//! Export Ephemeris notes into an Obsidian vault subfolder (ADR ephemeris-mry).
//!
//! Layout: `{vault}/{export_subdir}/{Notebook}/{Note}.md`
//! Optional PDF sibling `{Note}.pdf` is linked from the markdown when present.

use std::fs;
use std::path::{Path, PathBuf};

/// Default vault-relative export root (notebooks live under this).
pub const DEFAULT_EXPORT_SUBDIR: &str = "Ephemeris";

/// Payload for writing one note into the vault.
#[derive(Debug, Clone)]
pub struct VaultNoteExport {
    pub notebook_title: String,
    pub note_title: String,
    pub note_id: u32,
    pub created_at: u64,
    /// Machine-readable / OCR / transcribed text for the note body.
    pub text_content: Option<String>,
    /// If set, markdown embeds a wiki-style link to this vault-relative PDF.
    pub pdf_vault_rel: Option<String>,
}

/// Write (or overwrite) the markdown sidecar for `note` under the export root.
///
/// Returns the absolute path of the written `.md` file.
pub fn export_note_markdown(
    vault_path: &Path,
    export_subdir: &Path,
    note: &VaultNoteExport,
) -> crate::Result<PathBuf> {
    let notebook_dir = sanitize_component(&note.notebook_title);
    let note_stem = sanitize_component(&note.note_title);
    let dir = vault_path.join(export_subdir).join(&notebook_dir);
    fs::create_dir_all(&dir).map_err(crate::AppError::Io)?;

    let md_path = dir.join(format!("{note_stem}.md"));
    let body = render_markdown(note);
    atomic_write(&md_path, &body)?;
    Ok(md_path)
}

/// Sync every note in `notes` into the vault export tree.
pub fn sync_notes_to_vault(
    vault_path: &Path,
    export_subdir: &Path,
    notes: &[VaultNoteExport],
) -> crate::Result<usize> {
    let mut n = 0;
    for note in notes {
        export_note_markdown(vault_path, export_subdir, note)?;
        n += 1;
    }
    Ok(n)
}

fn render_markdown(note: &VaultNoteExport) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("title: \"{}\"\n", escape_yaml(&note.note_title)));
    out.push_str(&format!("ephemeris_note_id: {}\n", note.note_id));
    out.push_str(&format!(
        "notebook: \"{}\"\n",
        escape_yaml(&note.notebook_title)
    ));
    out.push_str(&format!("created: {}\n", note.created_at));
    out.push_str("source: ephemeris\n");
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", note.note_title));

    if let Some(ref pdf) = note.pdf_vault_rel {
        let stem = Path::new(pdf)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("note");
        out.push_str(&format!("![[{stem}.pdf]]\n\n"));
    }

    match note
        .text_content
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(text) => {
            out.push_str("## Text\n\n");
            out.push_str(text);
            out.push('\n');
        }
        None => {
            out.push_str("*No transcribed text yet.*\n");
        }
    }
    out
}

/// Sanitize a path component for cross-platform vault folders.
pub fn sanitize_component(name: &str) -> String {
    let trimmed = name.trim();
    let base = if trimmed.is_empty() {
        "Untitled"
    } else {
        trimmed
    };
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    out
}

fn escape_yaml(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn atomic_write(path: &Path, content: &str) -> crate::Result<()> {
    let tmp = path.with_extension("md.ephemeris-tmp");
    fs::write(&tmp, content).map_err(crate::AppError::Io)?;
    fs::rename(&tmp, path).map_err(crate::AppError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sanitize_replaces_forbidden() {
        assert_eq!(sanitize_component("a/b:c"), "a_b_c");
        assert_eq!(sanitize_component("  "), "Untitled");
    }

    #[test]
    fn export_writes_notebook_mirror() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path();
        let note = VaultNoteExport {
            notebook_title: "Work/Ideas".into(),
            note_title: "Meeting Notes".into(),
            note_id: 7,
            created_at: 1_700_000_000,
            text_content: Some("Hello from OCR".into()),
            pdf_vault_rel: None,
        };
        let path = export_note_markdown(vault, Path::new("Ephemeris"), &note).unwrap();
        assert!(path.ends_with("Ephemeris/Work_Ideas/Meeting Notes.md"));
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("ephemeris_note_id: 7"));
        assert!(body.contains("Hello from OCR"));
        assert!(body.contains("notebook: \"Work/Ideas\""));
    }

    #[test]
    fn export_links_pdf_when_provided() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "N".into(),
            note_title: "T".into(),
            note_id: 1,
            created_at: 0,
            text_content: None,
            pdf_vault_rel: Some("Ephemeris/N/T.pdf".into()),
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("![[T.pdf]]"));
        assert!(body.contains("No transcribed text yet"));
    }

    #[test]
    fn sync_multiple() {
        let tmp = TempDir::new().unwrap();
        let notes = vec![
            VaultNoteExport {
                notebook_title: "A".into(),
                note_title: "One".into(),
                note_id: 1,
                created_at: 0,
                text_content: None,
                pdf_vault_rel: None,
            },
            VaultNoteExport {
                notebook_title: "B".into(),
                note_title: "Two".into(),
                note_id: 2,
                created_at: 0,
                text_content: Some("x".into()),
                pdf_vault_rel: None,
            },
        ];
        let n = sync_notes_to_vault(tmp.path(), Path::new("Ephemeris"), &notes).unwrap();
        assert_eq!(n, 2);
        assert!(tmp.path().join("Ephemeris/A/One.md").exists());
        assert!(tmp.path().join("Ephemeris/B/Two.md").exists());
    }
}
