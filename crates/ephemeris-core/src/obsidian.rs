use crate::Note;
use std::fs;
use std::path::{Path, PathBuf};

/// Obsidian vault filesystem reader.
pub struct ObsidianVault {
    vault_path: PathBuf,
}

impl ObsidianVault {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ObsidianVault {
            vault_path: path.into(),
        }
    }

    /// List all markdown files in the vault.
    pub fn list_files(&self) -> crate::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        self.walk_directory(&self.vault_path, &mut files)?;
        Ok(files)
    }

    /// Read all notes from the vault directory.
    pub fn read_notes(&self) -> crate::Result<Vec<Note>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut notes = Vec::new();
        let files = self.list_files()?;

        for path in files {
            if let Ok(_) = fs::read_to_string(&path) {
                let title = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string();
                notes.push(Note::new(title, now));
            }
        }

        Ok(notes)
    }

    /// Read markdown file contents.
    pub fn read_file(&self, path: impl AsRef<Path>) -> crate::Result<String> {
        let full_path = self.vault_path.join(path);
        fs::read_to_string(&full_path).map_err(crate::AppError::Io)
    }

    /// Extract frontmatter (YAML between --- markers) from markdown.
    pub fn extract_frontmatter(&self, markdown: &str) -> crate::Result<String> {
        if !markdown.starts_with("---") {
            return Ok(String::new());
        }

        let rest = &markdown[3..];
        if let Some(end) = rest.find("---") {
            Ok(rest[..end].trim().to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Walk directory recursively, collecting markdown files.
    fn walk_directory(&self, dir: &Path, files: &mut Vec<PathBuf>) -> crate::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if !self.is_hidden(&path) {
                    self.walk_directory(&path, files)?;
                }
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                files.push(path);
            }
        }

        Ok(())
    }

    /// Check if a path is hidden (starts with a dot).
    fn is_hidden(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with('.'))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn setup_vault(_name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let notes_dir = dir.path().join("notes");
        fs::create_dir(&notes_dir).expect("create notes dir");
        dir
    }

    #[test]
    fn vault_creation() {
        let temp = setup_vault("test");
        let _vault = ObsidianVault::new(temp.path());
    }

    #[test]
    fn list_files_empty_vault() {
        let temp = setup_vault("empty");
        let vault = ObsidianVault::new(temp.path());
        let files = vault.list_files().expect("should list files");
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn read_notes_empty() {
        let temp = setup_vault("empty");
        let vault = ObsidianVault::new(temp.path());
        let notes = vault.read_notes().expect("should read");
        assert_eq!(notes.len(), 0);
    }

    #[test]
    fn list_markdown_files() {
        let temp = setup_vault("with_files");
        let vault_path = temp.path();

        let mut file1 = fs::File::create(vault_path.join("note1.md")).expect("create note1");
        file1.write_all(b"# Note 1\nContent").expect("write note1");

        let mut file2 = fs::File::create(vault_path.join("note2.md")).expect("create note2");
        file2
            .write_all(b"# Note 2\nOther content")
            .expect("write note2");

        let vault = ObsidianVault::new(vault_path);
        let files = vault.list_files().expect("should list files");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn read_notes_from_vault() {
        let temp = setup_vault("notes");
        let vault_path = temp.path();

        let mut file = fs::File::create(vault_path.join("test.md")).expect("create file");
        file.write_all(b"# Test Note\nContent").expect("write file");

        let vault = ObsidianVault::new(vault_path);
        let notes = vault.read_notes().expect("should read notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "test");
    }

    #[test]
    fn read_file_contents() {
        let temp = setup_vault("read");
        let vault_path = temp.path();
        let content = "# Heading\nBody content";

        let mut file = fs::File::create(vault_path.join("readme.md")).expect("create file");
        file.write_all(content.as_bytes()).expect("write file");

        let vault = ObsidianVault::new(vault_path);
        let text = vault.read_file("readme.md").expect("should read file");
        assert_eq!(text, content);
    }

    #[test]
    fn extract_frontmatter_present() {
        let vault = ObsidianVault::new("/tmp");
        let markdown = "---\ntitle: Test\ndate: 2024-01-01\n---\n# Content";
        let fm = vault.extract_frontmatter(markdown).expect("should extract");
        assert_eq!(fm, "title: Test\ndate: 2024-01-01");
    }

    #[test]
    fn extract_frontmatter_absent() {
        let vault = ObsidianVault::new("/tmp");
        let markdown = "# Content\nNo frontmatter";
        let fm = vault.extract_frontmatter(markdown).expect("should extract");
        assert_eq!(fm, "");
    }

    #[test]
    fn skip_hidden_directories() {
        let temp = setup_vault("hidden");
        let vault_path = temp.path();

        let mut file1 = fs::File::create(vault_path.join("visible.md")).expect("create file");
        file1.write_all(b"Visible").expect("write file");

        let hidden_dir = vault_path.join(".hidden");
        fs::create_dir(&hidden_dir).expect("create hidden dir");
        let mut file2 = fs::File::create(hidden_dir.join("secret.md")).expect("create hidden file");
        file2.write_all(b"Hidden").expect("write hidden file");

        let vault = ObsidianVault::new(vault_path);
        let files = vault.list_files().expect("should list files");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.md"));
    }
}
