use crate::Note;

/// Obsidian vault filesystem reader.
pub struct ObsidianVault {
    #[allow(dead_code)]
    vault_path: String,
}

impl ObsidianVault {
    pub fn new(path: impl Into<String>) -> Self {
        ObsidianVault {
            vault_path: path.into(),
        }
    }

    pub fn read_notes(&self) -> crate::Result<Vec<Note>> {
        Ok(vec![])
    }

    pub fn extract_frontmatter(&self, _markdown: &str) -> crate::Result<String> {
        Ok("---".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_creation() {
        let _vault = ObsidianVault::new("/path/to/vault");
    }

    #[test]
    fn read_notes_empty() {
        let vault = ObsidianVault::new("/test");
        let notes = vault.read_notes().expect("should read");
        assert_eq!(notes.len(), 0);
    }
}
