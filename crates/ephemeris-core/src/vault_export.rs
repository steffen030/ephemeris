//! Export Ephemeris notes into an Obsidian vault subfolder (ADR ephemeris-mry).
//!
//! Layout: `{vault}/{export_subdir}/Notes/{Notebook}/{…}/{Note}.md`
//! Unfiled notes: `{export_subdir}/Notes/{Note}.md` (empty notebook segments).
//! Nested Ephemeris notebooks become nested directories *under* `Notes/`, never
//! beside it. The PDF (+ hash sidecar) lives under `{note_dir}/_assets/` so the
//! notebook folder stays markdown-first; markdown embeds `![[_assets/{Note}.pdf]]`.

use crate::json_canvas::{new_canvas_id, CanvasNode, FileNode, JsonCanvas, TextNode};
use crate::pdf_export::pdf_hash_sidecar;
use std::fs;
use std::path::{Path, PathBuf};

/// Default vault-relative export root (`Notes/` lives under this).
pub const DEFAULT_EXPORT_SUBDIR: &str = "Ephemeris";

/// Folder under the export root that holds all note markdown/PDF exports.
pub const NOTES_EXPORT_FOLDER: &str = "Notes";

/// Folder under the export root that holds Obsidian JSON Canvas exports.
pub const CANVAS_EXPORT_FOLDER: &str = "Canvas";

/// Per-folder assets dir for note PDFs (and canvas PNGs).
pub const NOTE_ASSETS_FOLDER: &str = "_assets";
/// Payload for writing one note into the vault.
#[derive(Debug, Clone)]
pub struct VaultNoteExport {
    /// Display path for frontmatter (e.g. `"Work / Ideas"`).
    pub notebook_title: String,
    /// Sanitized notebook folder segments under `{export_subdir}/Notes/`
    /// (empty = unfiled, file lives directly in `Notes/`).
    pub notebook_segments: Vec<String>,
    pub note_title: String,
    pub note_id: u32,
    pub created_at: u64,
    /// Machine-readable / OCR / transcribed text for the note body.
    pub text_content: Option<String>,
    /// If set, markdown embeds a wiki-style link to this vault-relative PDF.
    pub pdf_vault_rel: Option<String>,
    /// Obsidian YAML frontmatter tags.
    pub tags: Vec<String>,
}

/// Absolute directory for a note under `{export_subdir}/Notes/{segments…}`.
pub fn notebook_export_dir(
    vault_path: &Path,
    export_subdir: &Path,
    segments: &[String],
) -> PathBuf {
    let mut dir = vault_path.join(export_subdir).join(NOTES_EXPORT_FOLDER);
    for seg in segments {
        dir.push(seg);
    }
    dir
}

/// Vault-relative path (`/` separators) for a file under the Notes tree.
pub fn notebook_export_rel(export_subdir: &Path, segments: &[String], file_name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in export_subdir.components() {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.push(NOTES_EXPORT_FOLDER.to_string());
    for seg in segments {
        parts.push(seg.clone());
    }
    parts.push(file_name.to_string());
    parts.join("/")
}

/// Absolute path for a note's PDF under `{note_dir}/_assets/{stem}.pdf`.
pub fn note_pdf_export_path(
    vault_path: &Path,
    export_subdir: &Path,
    segments: &[String],
    note_stem: &str,
) -> PathBuf {
    notebook_export_dir(vault_path, export_subdir, segments)
        .join(NOTE_ASSETS_FOLDER)
        .join(format!("{note_stem}.pdf"))
}

/// Vault-relative path for a note PDF under `_assets/`.
pub fn note_pdf_export_rel(export_subdir: &Path, segments: &[String], note_stem: &str) -> String {
    notebook_export_rel(
        export_subdir,
        segments,
        &format!("{NOTE_ASSETS_FOLDER}/{note_stem}.pdf"),
    )
}

/// Move legacy sibling `{stem}.pdf` (+ hash) into `_assets/`, or delete leftovers.
///
/// Older exports wrote PDF + `.ephemeris-hash` next to the markdown. Call this
/// before writing the new layout so Obsidian folders stay tidy and the
/// incremental hash sidecar keeps working across the move.
pub fn migrate_legacy_note_pdf(note_dir: &Path, note_stem: &str) {
    let legacy_pdf = note_dir.join(format!("{note_stem}.pdf"));
    let legacy_hash = pdf_hash_sidecar(&legacy_pdf);
    let assets_dir = note_dir.join(NOTE_ASSETS_FOLDER);
    let new_pdf = assets_dir.join(format!("{note_stem}.pdf"));
    let new_hash = pdf_hash_sidecar(&new_pdf);

    if legacy_pdf.exists() {
        if !new_pdf.exists() {
            let _ = fs::create_dir_all(&assets_dir);
            if fs::rename(&legacy_pdf, &new_pdf).is_ok() {
                if legacy_hash.exists() && !new_hash.exists() {
                    let _ = fs::rename(&legacy_hash, &new_hash);
                }
            } else {
                let _ = fs::remove_file(&legacy_pdf);
            }
        } else {
            let _ = fs::remove_file(&legacy_pdf);
        }
    }
    if legacy_hash.exists() {
        let _ = fs::remove_file(&legacy_hash);
    }
}

/// Obsidian wiki embed for a vault-relative PDF path.
fn pdf_wiki_embed(pdf_vault_rel: &str) -> String {
    let path = Path::new(pdf_vault_rel);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("note.pdf");
    let under_assets = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        == Some(NOTE_ASSETS_FOLDER);
    if under_assets {
        format!("![[{NOTE_ASSETS_FOLDER}/{name}]]")
    } else {
        format!("![[{name}]]")
    }
}
/// Write (or overwrite) the markdown sidecar for `note` under the export root.
///
/// Returns the absolute path of the written `.md` file.
pub fn export_note_markdown(
    vault_path: &Path,
    export_subdir: &Path,
    note: &VaultNoteExport,
) -> crate::Result<PathBuf> {
    let dir = notebook_export_dir(vault_path, export_subdir, &note.notebook_segments);
    fs::create_dir_all(&dir).map_err(crate::AppError::Io)?;

    let note_stem = sanitize_component(&note.note_title);
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

/// True when `path` is the export root or a file/dir inside it.
pub fn path_under_export_subdir(vault_root: &Path, path: &Path, export_subdir: &Path) -> bool {
    if export_subdir.as_os_str().is_empty() {
        return false;
    }
    let exclude_abs = vault_root.join(export_subdir);
    path == exclude_abs || path.starts_with(&exclude_abs)
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
    if !note.tags.is_empty() {
        out.push_str("tags:\n");
        for tag in &note.tags {
            out.push_str(&format!("  - {}\n", escape_yaml(tag)));
        }
    }
    out.push_str("source: ephemeris\n");
    out.push_str("ephemeris_format: 1\n");
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", note.note_title));

    if let Some(ref pdf) = note.pdf_vault_rel {
        out.push_str(&pdf_wiki_embed(pdf));
        out.push_str("\n\n");
    }

    match note
        .text_content
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(text) => {
            let demoted = demote_h1_to_h2(text);
            out.push_str(&demoted);
            out.push('\n');
        }
        None => {
            out.push_str("*No transcribed text yet.*\n");
        }
    }
    out
}

/// Demote recognized H1 lines (`# x` at line start) to H2.
fn demote_h1_to_h2(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("# ") {
            out.push('#');
            out.push_str(line);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Payload for writing one canvas into the vault as `.canvas` (+ optional PNGs).
#[derive(Debug, Clone)]
pub struct VaultCanvasExport {
    /// Display path for logging (e.g. `"Work / Ideas"`).
    pub folder_title: String,
    /// Sanitized folder segments under `{export_subdir}/Canvas/`.
    pub folder_segments: Vec<String>,
    pub canvas_title: String,
    pub canvas_id: u32,
    /// JSON Canvas document (may be augmented with PNG file nodes).
    pub doc: JsonCanvas,
    /// Optional ink PNG bytes keyed by text-node id → written under `_assets/`.
    pub ink_pngs: Vec<(String, Vec<u8>)>,
    /// When true, append a `file` node sibling for each ink PNG.
    pub include_ink_file_nodes: bool,
}

/// Absolute directory for a canvas under `{export_subdir}/Canvas/{segments…}`.
pub fn canvas_export_dir(vault_path: &Path, export_subdir: &Path, segments: &[String]) -> PathBuf {
    let mut dir = vault_path.join(export_subdir).join(CANVAS_EXPORT_FOLDER);
    for seg in segments {
        dir.push(seg);
    }
    dir
}

/// Vault-relative path for a file under the Canvas tree.
pub fn canvas_export_rel(export_subdir: &Path, segments: &[String], file_name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in export_subdir.components() {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.push(CANVAS_EXPORT_FOLDER.to_string());
    for seg in segments {
        parts.push(seg.clone());
    }
    parts.push(file_name.to_string());
    parts.join("/")
}

/// Write (or overwrite) a `.canvas` file and optional PNG assets.
///
/// Returns the absolute path of the written `.canvas` file.
pub fn export_canvas_json(
    vault_path: &Path,
    export_subdir: &Path,
    canvas: &VaultCanvasExport,
) -> crate::Result<PathBuf> {
    let dir = canvas_export_dir(vault_path, export_subdir, &canvas.folder_segments);
    fs::create_dir_all(&dir).map_err(crate::AppError::Io)?;

    let stem = sanitize_component(&canvas.canvas_title);
    let assets_dir = dir.join("_assets");
    if !canvas.ink_pngs.is_empty() {
        fs::create_dir_all(&assets_dir).map_err(crate::AppError::Io)?;
    }

    let mut doc = canvas.doc.clone();
    if canvas.include_ink_file_nodes {
        for (node_id, _) in &canvas.ink_pngs {
            let file_rel = canvas_export_rel(
                export_subdir,
                &canvas.folder_segments,
                &format!("_assets/{node_id}.png"),
            );
            // Place PNG file node near the matching text card when possible.
            let (x, y, width, height) = doc
                .nodes
                .iter()
                .find(|n| n.id() == node_id)
                .map(|n| (n.x() + n.width() + 20, n.y(), 200, n.height().min(160)))
                .unwrap_or((0, 0, 200, 120));
            doc.nodes.push(CanvasNode::File(FileNode {
                id: new_canvas_id(),
                x,
                y,
                width,
                height,
                file: file_rel,
                subpath: None,
                color: None,
            }));
            // Ensure the text node still carries markdown when empty.
            if let Some(CanvasNode::Text(TextNode { text, .. })) =
                doc.nodes.iter_mut().find(|n| n.id() == node_id)
            {
                if text.trim().is_empty() {
                    *text = String::new();
                }
            }
        }
    }

    for (node_id, png) in &canvas.ink_pngs {
        let png_path = assets_dir.join(format!("{node_id}.png"));
        atomic_write_bytes(&png_path, png)?;
    }

    let canvas_path = dir.join(format!("{stem}.canvas"));
    let body = doc
        .to_json_pretty()
        .map_err(|e| crate::AppError::Storage(format!("serialize .canvas: {e}")))?;
    atomic_write(&canvas_path, &body)?;
    Ok(canvas_path)
}

/// Sync every canvas in `canvases` into the vault export tree.
pub fn sync_canvases_to_vault(
    vault_path: &Path,
    export_subdir: &Path,
    canvases: &[VaultCanvasExport],
) -> crate::Result<usize> {
    let mut n = 0;
    for c in canvases {
        export_canvas_json(vault_path, export_subdir, c)?;
        n += 1;
    }
    Ok(n)
}

/// Sanitize a single path component for cross-platform vault folders.
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
    atomic_write_bytes(path, content.as_bytes())
}

fn atomic_write_bytes(path: &Path, content: &[u8]) -> crate::Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("dat");
    let tmp = path.with_extension(format!("{ext}.ephemeris-tmp"));
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
    fn export_writes_nested_notebook_dirs_under_notes() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path();
        let note = VaultNoteExport {
            notebook_title: "Work / Ideas".into(),
            notebook_segments: vec!["Work".into(), "Ideas".into()],
            note_title: "Meeting Notes".into(),
            note_id: 7,
            created_at: 1_700_000_000,
            text_content: Some("Hello from OCR".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(vault, Path::new("Ephemeris"), &note).unwrap();
        assert!(path.ends_with("Ephemeris/Notes/Work/Ideas/Meeting Notes.md"));
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("ephemeris_note_id: 7"));
        assert!(body.contains("ephemeris_format: 1"));
        assert!(body.contains("Hello from OCR"));
        assert!(body.contains("notebook: \"Work / Ideas\""));
        assert!(!body.contains("## Text"));
    }

    #[test]
    fn export_unfiled_lives_directly_in_notes() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Notes".into(),
            notebook_segments: vec![],
            note_title: "Loose".into(),
            note_id: 3,
            created_at: 0,
            text_content: None,
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        assert!(path.ends_with("Ephemeris/Notes/Loose.md"));
        assert!(!path.to_string_lossy().contains("Ephemeris/Notes/Notes/"));
    }

    #[test]
    fn export_links_pdf_when_provided() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "N".into(),
            notebook_segments: vec!["N".into()],
            note_title: "T".into(),
            note_id: 1,
            created_at: 0,
            text_content: None,
            pdf_vault_rel: Some("Ephemeris/Notes/N/_assets/T.pdf".into()),
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("![[_assets/T.pdf]]"));
        assert!(body.contains("No transcribed text yet"));
    }

    #[test]
    fn export_links_legacy_sibling_pdf_without_assets_prefix() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "N".into(),
            notebook_segments: vec!["N".into()],
            note_title: "T".into(),
            note_id: 1,
            created_at: 0,
            text_content: None,
            pdf_vault_rel: Some("Ephemeris/Notes/N/T.pdf".into()),
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("![[T.pdf]]"));
        assert!(!body.contains("![[_assets/T.pdf]]"));
    }

    #[test]
    fn note_pdf_export_path_under_assets() {
        let abs = note_pdf_export_path(
            Path::new("/vault"),
            Path::new("Ephemeris"),
            &["Work".into()],
            "Meeting Notes",
        );
        assert_eq!(
            abs,
            Path::new("/vault/Ephemeris/Notes/Work/_assets/Meeting Notes.pdf")
        );
        let rel = note_pdf_export_rel(Path::new("Ephemeris"), &["Work".into()], "Meeting Notes");
        assert_eq!(rel, "Ephemeris/Notes/Work/_assets/Meeting Notes.pdf");
    }

    #[test]
    fn migrate_legacy_note_pdf_moves_into_assets() {
        let tmp = TempDir::new().unwrap();
        let note_dir = tmp.path().join("Ephemeris/Notes/N");
        fs::create_dir_all(&note_dir).unwrap();
        let legacy_pdf = note_dir.join("T.pdf");
        let legacy_hash = pdf_hash_sidecar(&legacy_pdf);
        fs::write(&legacy_pdf, b"%PDF-legacy").unwrap();
        fs::write(&legacy_hash, "abc123").unwrap();

        migrate_legacy_note_pdf(&note_dir, "T");

        let new_pdf = note_dir.join("_assets/T.pdf");
        let new_hash = pdf_hash_sidecar(&new_pdf);
        assert!(new_pdf.exists());
        assert_eq!(fs::read(&new_pdf).unwrap(), b"%PDF-legacy");
        assert!(new_hash.exists());
        assert_eq!(fs::read_to_string(&new_hash).unwrap(), "abc123");
        assert!(!legacy_pdf.exists());
        assert!(!legacy_hash.exists());
    }

    #[test]
    fn migrate_legacy_note_pdf_removes_sibling_when_assets_exists() {
        let tmp = TempDir::new().unwrap();
        let note_dir = tmp.path().join("Ephemeris/Notes/N");
        let assets = note_dir.join("_assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("T.pdf"), b"%PDF-new").unwrap();
        fs::write(note_dir.join("T.pdf"), b"%PDF-old").unwrap();
        fs::write(pdf_hash_sidecar(&note_dir.join("T.pdf")), "old").unwrap();

        migrate_legacy_note_pdf(&note_dir, "T");

        assert_eq!(fs::read(assets.join("T.pdf")).unwrap(), b"%PDF-new");
        assert!(!note_dir.join("T.pdf").exists());
        assert!(!pdf_hash_sidecar(&note_dir.join("T.pdf")).exists());
    }

    #[test]
    fn sync_multiple() {
        let tmp = TempDir::new().unwrap();
        let notes = vec![
            VaultNoteExport {
                notebook_title: "A".into(),
                notebook_segments: vec!["A".into()],
                note_title: "One".into(),
                note_id: 1,
                created_at: 0,
                text_content: None,
                pdf_vault_rel: None,
                tags: vec![],
            },
            VaultNoteExport {
                notebook_title: "B".into(),
                notebook_segments: vec!["B".into()],
                note_title: "Two".into(),
                note_id: 2,
                created_at: 0,
                text_content: Some("x".into()),
                pdf_vault_rel: None,
                tags: vec![],
            },
        ];
        let n = sync_notes_to_vault(tmp.path(), Path::new("Ephemeris"), &notes).unwrap();
        assert_eq!(n, 2);
        assert!(tmp.path().join("Ephemeris/Notes/A/One.md").exists());
        assert!(tmp.path().join("Ephemeris/Notes/B/Two.md").exists());
    }

    #[test]
    fn notebook_export_rel_includes_notes_folder() {
        let rel = notebook_export_rel(
            Path::new("Ephemeris"),
            &["Work".into(), "Ideas".into()],
            "n.md",
        );
        assert_eq!(rel, "Ephemeris/Notes/Work/Ideas/n.md");
        let unfiled = notebook_export_rel(Path::new("Ephemeris"), &[], "loose.md");
        assert_eq!(unfiled, "Ephemeris/Notes/loose.md");
        let pdf = notebook_export_rel(
            Path::new("Ephemeris"),
            &["Work".into(), "Ideas".into()],
            "_assets/n.pdf",
        );
        assert_eq!(pdf, "Ephemeris/Notes/Work/Ideas/_assets/n.pdf");
    }

    #[test]
    fn export_canvas_writes_json_and_assets() {
        let tmp = TempDir::new().unwrap();
        let mut doc = JsonCanvas::default();
        doc.nodes.push(CanvasNode::Text(TextNode {
            id: "abc1234567890def".into(),
            x: 0,
            y: 0,
            width: 200,
            height: 100,
            text: "Hello".into(),
            color: None,
        }));
        let export = VaultCanvasExport {
            folder_title: "Work".into(),
            folder_segments: vec!["Work".into()],
            canvas_title: "Map".into(),
            canvas_id: 3,
            doc,
            ink_pngs: vec![("abc1234567890def".into(), vec![137, 80, 78, 71])],
            include_ink_file_nodes: true,
        };
        let path = export_canvas_json(tmp.path(), Path::new("Ephemeris"), &export).unwrap();
        assert!(path.ends_with("Ephemeris/Canvas/Work/Map.canvas"));
        assert!(tmp
            .path()
            .join("Ephemeris/Canvas/Work/_assets/abc1234567890def.png")
            .exists());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"type\": \"text\""));
        assert!(body.contains("Hello"));
        assert!(body.contains("_assets/abc1234567890def.png"));
    }

    #[test]
    fn canvas_export_rel_includes_canvas_folder() {
        let rel = canvas_export_rel(Path::new("Ephemeris"), &["A".into()], "x.canvas");
        assert_eq!(rel, "Ephemeris/Canvas/A/x.canvas");
    }

    #[test]
    fn path_under_export_detects_tree() {
        let vault = Path::new("/vault");
        let ex = Path::new("Ephemeris");
        assert!(path_under_export_subdir(
            vault,
            Path::new("/vault/Ephemeris"),
            ex
        ));
        assert!(path_under_export_subdir(
            vault,
            Path::new("/vault/Ephemeris/Work/n.md"),
            ex
        ));
        assert!(!path_under_export_subdir(
            vault,
            Path::new("/vault/Other/n.md"),
            ex
        ));
    }

    #[test]
    fn markdown_h1_demoted_to_h2() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "MyNote".into(),
            note_id: 1,
            created_at: 0,
            text_content: Some("# Heading 1\n## Heading 2\nRegular text".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("# MyNote"));
        assert!(body.contains("## Heading 1"));
        assert!(body.contains("## Heading 2"));
        assert!(body.contains("Regular text"));
    }

    #[test]
    fn markdown_highlight_and_underline_survive() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "Note".into(),
            note_id: 1,
            created_at: 0,
            text_content: Some("Some ==highlighted== and <u>underlined</u> text".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("==highlighted=="));
        assert!(body.contains("<u>underlined</u>"));
    }

    #[test]
    fn markdown_lists_survive() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "Note".into(),
            note_id: 1,
            created_at: 0,
            text_content: Some("- item 1\n- item 2\n  - nested\n1. ordered\n2. second".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("- item 1"));
        assert!(body.contains("- item 2"));
        assert!(body.contains("  - nested"));
        assert!(body.contains("1. ordered"));
        assert!(body.contains("2. second"));
    }

    #[test]
    fn markdown_checkboxes_survive() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "Note".into(),
            note_id: 1,
            created_at: 0,
            text_content: Some("- [ ] buy milk\n- [x] call bob".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("- [ ] buy milk"));
        assert!(body.contains("- [x] call bob"));
    }

    #[test]
    fn markdown_escaped_paragraph_survives() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "Note".into(),
            note_id: 1,
            created_at: 0,
            text_content: Some("\\# not a heading\nRegular paragraph".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("\\# not a heading"));
        assert!(body.contains("Regular paragraph"));
    }

    #[test]
    fn frontmatter_includes_tags_when_set() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "N".into(),
            notebook_segments: vec![],
            note_title: "Tagged".into(),
            note_id: 9,
            created_at: 0,
            text_content: None,
            pdf_vault_rel: None,
            tags: vec!["work".into(), "ideas".into()],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("tags:\n  - work\n  - ideas\n"));
    }

    #[test]
    fn frontmatter_includes_ephemeris_format() {
        let tmp = TempDir::new().unwrap();
        let note = VaultNoteExport {
            notebook_title: "Test".into(),
            notebook_segments: vec![],
            note_title: "Note".into(),
            note_id: 42,
            created_at: 1_700_000_000,
            text_content: Some("Body text".into()),
            pdf_vault_rel: None,
            tags: vec![],
        };
        let path = export_note_markdown(tmp.path(), Path::new("Ephemeris"), &note).unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("ephemeris_format: 1"));
        assert!(body.contains("title: \"Note\""));
        assert!(body.contains("ephemeris_note_id: 42"));
    }
}
