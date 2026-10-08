//! Full-text search over Obsidian vault notes (ADR ephemeris-mry, ephemeris-be4.3).
//!
//! Uses an in-memory SQLite FTS5 index rebuilt from markdown files on demand.

use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};

/// A single search hit with a readable snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Stable id: vault-relative path for vault notes, or caller-defined for others.
    pub id: String,
    /// Source kind: `"vault"`, `"local_note"`, `"task"`, `"calendar"`, …
    pub source: String,
    pub title: String,
    pub snippet: String,
    /// Optional filesystem / vault-relative path (for opening vault markdown).
    pub path: Option<String>,
}

/// In-memory FTS5 index of vault markdown notes.
pub struct VaultFtsIndex {
    conn: Connection,
}

impl VaultFtsIndex {
    /// Open an empty in-memory FTS5 index.
    pub fn open_in_memory() -> crate::Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| crate::AppError::Storage(format!("fts open: {e}")))?;
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE vault_notes USING fts5(
                path UNINDEXED,
                title,
                body,
                tokenize = 'porter unicode61'
            );
            "#,
        )
        .map_err(|e| crate::AppError::Storage(format!("fts schema: {e}")))?;
        Ok(VaultFtsIndex { conn })
    }

    /// Drop all indexed documents.
    pub fn clear(&mut self) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM vault_notes", [])
            .map_err(|e| crate::AppError::Storage(format!("fts clear: {e}")))?;
        Ok(())
    }

    /// Number of documents currently indexed.
    pub fn len(&self) -> crate::Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM vault_notes", [], |row| row.get(0))
            .map_err(|e| crate::AppError::Storage(format!("fts count: {e}")))?;
        Ok(n as usize)
    }

    /// Returns true when the index has no documents.
    pub fn is_empty(&self) -> crate::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Clear and rebuild the index from every `.md` file under `vault_path`.
    ///
    /// If `vault_path` is missing or not a directory, the index is cleared and
    /// `Ok(0)` is returned (so callers can safely reindex after disconnect).
    pub fn reindex_vault(&mut self, vault_path: &Path) -> crate::Result<usize> {
        self.reindex_vaults(&[vault_path])
    }

    /// Clear and rebuild the index from one or more vault directories.
    ///
    /// Document `path` / `id` is the vault-relative path when a single vault is
    /// indexed, or `abspath` when multiple vaults are merged (so open-by-path
    /// stays unambiguous).
    pub fn reindex_vaults(&mut self, vault_paths: &[&Path]) -> crate::Result<usize> {
        self.clear()?;
        let multi = vault_paths.len() > 1;
        let mut count = 0usize;
        for vault_path in vault_paths {
            if !vault_path.is_dir() {
                tracing::warn!("Vault path {:?} is not a directory; skipping", vault_path);
                continue;
            }
            let files = list_markdown_files(vault_path)?;
            for full in files {
                let rel = full
                    .strip_prefix(vault_path)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| full.to_string_lossy().replace('\\', "/"));
                let id = if multi {
                    full.to_string_lossy().replace('\\', "/")
                } else {
                    rel
                };
                let Ok(body) = fs::read_to_string(&full) else {
                    continue;
                };
                let title = full
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string();
                self.conn
                    .execute(
                        "INSERT INTO vault_notes(path, title, body) VALUES (?1, ?2, ?3)",
                        params![id, title, body],
                    )
                    .map_err(|e| crate::AppError::Storage(format!("fts insert: {e}")))?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Full-text query; returns hits with snippets (max `limit`).
    ///
    /// Falls back to case-insensitive substring scan when FTS MATCH yields
    /// nothing (short tokens, empty index, tokenizer edge cases).
    pub fn search(&self, query: &str, limit: usize) -> crate::Result<Vec<SearchHit>> {
        let raw = query.trim();
        if raw.is_empty() {
            return Ok(Vec::new());
        }

        let q = sanitize_fts_query(raw);
        let mut hits = Vec::new();
        if !q.is_empty() {
            let fts_result = (|| -> crate::Result<Vec<SearchHit>> {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT path, title,
                                snippet(vault_notes, 2, '', '', '…', 12) AS snip
                         FROM vault_notes
                         WHERE vault_notes MATCH ?1
                         ORDER BY rank
                         LIMIT ?2",
                    )
                    .map_err(|e| crate::AppError::Storage(format!("fts prepare: {e}")))?;
                let rows = stmt
                    .query_map(params![q, limit as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| crate::AppError::Storage(format!("fts query: {e}")))?;
                let mut out = Vec::new();
                for row in rows {
                    let (path, title, snippet) =
                        row.map_err(|e| crate::AppError::Storage(format!("fts row: {e}")))?;
                    out.push(SearchHit {
                        id: path.clone(),
                        source: "vault".to_string(),
                        title,
                        snippet: snippet.trim().to_string(),
                        path: Some(path),
                    });
                }
                Ok(out)
            })();
            match fts_result {
                Ok(found) => hits = found,
                Err(e) => {
                    tracing::warn!("FTS MATCH failed for {q:?}: {e}; using LIKE fallback");
                }
            }
        }

        if hits.is_empty() {
            hits = self.search_like(raw, limit)?;
        }
        Ok(hits)
    }

    /// Substring fallback over indexed title+body (case-insensitive).
    ///
    /// Uses Rust-side filtering rather than SQL `lower()`/`LIKE` on the FTS5
    /// virtual table — those are unreliable across SQLite builds for fts5.
    fn search_like(&self, query: &str, limit: usize) -> crate::Result<Vec<SearchHit>> {
        let q_lower = query.to_lowercase();
        let mut stmt = self
            .conn
            .prepare("SELECT path, title, body FROM vault_notes")
            .map_err(|e| crate::AppError::Storage(format!("fts like prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| crate::AppError::Storage(format!("fts like query: {e}")))?;
        let mut hits = Vec::new();
        for row in rows {
            if hits.len() >= limit {
                break;
            }
            let (path, title, body) =
                row.map_err(|e| crate::AppError::Storage(format!("fts like row: {e}")))?;
            if title.to_lowercase().contains(&q_lower) || body.to_lowercase().contains(&q_lower) {
                hits.push(SearchHit {
                    id: path.clone(),
                    source: "vault".to_string(),
                    title,
                    snippet: make_snippet(&body, &q_lower),
                    path: Some(path),
                });
            }
        }
        Ok(hits)
    }

    /// Read a vault note from disk.
    ///
    /// `rel_or_abs` may be a vault-relative path or an absolute path (used when
    /// multiple vaults are indexed together).
    pub fn read_vault_file(vault_path: &Path, rel_or_abs: &str) -> crate::Result<String> {
        let candidate = PathBuf::from(rel_or_abs);
        let full = if candidate.is_absolute() {
            candidate
        } else {
            vault_path.join(rel_or_abs)
        };
        fs::read_to_string(&full).map_err(crate::AppError::Io)
    }
}

/// Kind of a display block for the in-app markdown viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdBlockKind {
    Heading1,
    Heading2,
    Heading3,
    Paragraph,
    Bullet,
    Numbered,
    Quote,
    Code,
    Rule,
}

/// One rendered block of markdown (plain text + role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdBlock {
    pub kind: MdBlockKind,
    pub text: String,
}

/// Strip common inline markdown markers for list/title display.
///
/// Keeps the readable words; drops `**bold**`, `*italic*`, `` `code` ``,
/// `~~strike~~`, wikilinks and `[label](url)` wrappers.
pub fn strip_inline_markdown(s: &str) -> String {
    let s = simplify_wikilinks(s);
    let s = simplify_md_links(&s);
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // **bold** or __bold__
        if (chars[i] == '*' || chars[i] == '_') && chars.get(i + 1) == Some(&chars[i]) {
            let marker = chars[i];
            if let Some(end) = find_closing_pair(&chars, i + 2, marker, true) {
                for &c in &chars[i + 2..end] {
                    out.push(c);
                }
                i = end + 2;
                continue;
            }
        }
        // *italic* or _italic_ (single)
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = find_closing_pair(&chars, i + 1, marker, false) {
                for &c in &chars[i + 1..end] {
                    out.push(c);
                }
                i = end + 1;
                continue;
            }
        }
        // ~~strike~~
        if chars[i] == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_closing_pair(&chars, i + 2, '~', true) {
                for &c in &chars[i + 2..end] {
                    out.push(c);
                }
                i = end + 2;
                continue;
            }
        }
        // `code`
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let end = i + 1 + end;
                for &c in &chars[i + 1..end] {
                    out.push(c);
                }
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    // Collapse leftover odd markers and tidy whitespace.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn find_closing_pair(chars: &[char], start: usize, marker: char, doubled: bool) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == marker {
            if doubled {
                if chars.get(i + 1) == Some(&marker) {
                    return Some(i);
                }
            } else {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Parse markdown into display blocks for the eink viewer.
pub fn markdown_to_blocks(md: &str) -> Vec<MdBlock> {
    let mut blocks = Vec::new();
    let mut in_code = false;
    let mut code_buf = String::new();
    let mut para_buf = String::new();
    let mut in_frontmatter = false;
    let mut seen_content = false;

    let flush_para = |buf: &mut String, blocks: &mut Vec<MdBlock>| {
        let t = buf.trim().to_string();
        buf.clear();
        if !t.is_empty() {
            blocks.push(MdBlock {
                kind: MdBlockKind::Paragraph,
                text: strip_inline_markdown(&t),
            });
        }
    };

    for line in md.lines() {
        let trimmed = line.trim_end();
        let trim_start = trimmed.trim_start();

        if !seen_content && trim_start == "---" {
            in_frontmatter = !in_frontmatter;
            continue;
        }
        if in_frontmatter {
            continue;
        }
        seen_content = true;

        if trim_start.starts_with("```") {
            if in_code {
                blocks.push(MdBlock {
                    kind: MdBlockKind::Code,
                    text: code_buf.trim_end().to_string(),
                });
                code_buf.clear();
                in_code = false;
            } else {
                flush_para(&mut para_buf, &mut blocks);
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push_str(trimmed);
            code_buf.push('\n');
            continue;
        }

        if trim_start.is_empty() {
            flush_para(&mut para_buf, &mut blocks);
            continue;
        }

        if trim_start == "---" || trim_start == "***" || trim_start == "___" {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Rule,
                text: String::new(),
            });
            continue;
        }

        if let Some(rest) = trim_start.strip_prefix("### ") {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Heading3,
                text: strip_inline_markdown(rest),
            });
            continue;
        }
        if let Some(rest) = trim_start.strip_prefix("## ") {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Heading2,
                text: strip_inline_markdown(rest),
            });
            continue;
        }
        if let Some(rest) = trim_start.strip_prefix("# ") {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Heading1,
                text: strip_inline_markdown(rest),
            });
            continue;
        }

        if let Some(rest) = trim_start.strip_prefix("> ") {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Quote,
                text: strip_inline_markdown(rest),
            });
            continue;
        }

        if let Some(rest) = trim_start
            .strip_prefix("- ")
            .or_else(|| trim_start.strip_prefix("* "))
            .or_else(|| trim_start.strip_prefix("+ "))
        {
            flush_para(&mut para_buf, &mut blocks);
            blocks.push(MdBlock {
                kind: MdBlockKind::Bullet,
                text: strip_inline_markdown(rest),
            });
            continue;
        }

        // Ordered list "1. item"
        if let Some(dot) = trim_start.find(". ") {
            let (num, rest) = trim_start.split_at(dot);
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                flush_para(&mut para_buf, &mut blocks);
                blocks.push(MdBlock {
                    kind: MdBlockKind::Numbered,
                    text: format!("{}. {}", num, strip_inline_markdown(&rest[2..])),
                });
                continue;
            }
        }

        if !para_buf.is_empty() {
            para_buf.push(' ');
        }
        para_buf.push_str(trim_start);
    }

    if in_code && !code_buf.is_empty() {
        blocks.push(MdBlock {
            kind: MdBlockKind::Code,
            text: code_buf.trim_end().to_string(),
        });
    }
    flush_para(&mut para_buf, &mut blocks);
    blocks
}

/// Turn markdown into readable plain text for the in-app viewer.
pub fn markdown_to_plain(md: &str) -> String {
    markdown_to_blocks(md)
        .into_iter()
        .map(|b| match b.kind {
            MdBlockKind::Heading1 | MdBlockKind::Heading2 | MdBlockKind::Heading3 => b.text,
            MdBlockKind::Bullet => format!("• {}", b.text),
            MdBlockKind::Numbered => b.text,
            MdBlockKind::Quote => format!("“{}”", b.text),
            MdBlockKind::Code => b.text,
            MdBlockKind::Rule => "────────".to_string(),
            MdBlockKind::Paragraph => b.text,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn simplify_wikilinks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        if let Some(end) = rest.find("]]") {
            let inner = &rest[..end];
            let label = inner.split('|').next_back().unwrap_or(inner);
            out.push_str(label);
            rest = &rest[end + 2..];
        } else {
            out.push_str("[[");
            break;
        }
    }
    out.push_str(rest);
    out
}

fn simplify_md_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some((label, next)) = parse_md_link(&chars, i) {
                out.push_str(&label);
                i = next;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn parse_md_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    // [label](url)
    if chars.get(start)? != &'[' {
        return None;
    }
    let mut i = start + 1;
    let mut label = String::new();
    while i < chars.len() && chars[i] != ']' {
        label.push(chars[i]);
        i += 1;
    }
    if chars.get(i)? != &']' || chars.get(i + 1)? != &'(' {
        return None;
    }
    i += 2;
    while i < chars.len() && chars[i] != ')' {
        i += 1;
    }
    if chars.get(i)? != &')' {
        return None;
    }
    Some((label, i + 1))
}

/// Build an FTS MATCH query from free text (AND of tokenized terms).
fn sanitize_fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .map(|t| {
            let cleaned: String = t
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            cleaned
        })
        .filter(|t| t.len() >= 2)
        // Prefix match so partial typing still hits ("meet" → meeting).
        .map(|t| format!("{t}*"))
        .collect();
    tokens.join(" ")
}

fn list_markdown_files(dir: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_md(dir, &mut files)?;
    Ok(files)
}

fn walk_md(dir: &Path, files: &mut Vec<PathBuf>) -> crate::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(crate::AppError::Io)? {
        let entry = entry.map_err(crate::AppError::Io)?;
        let path = entry.path();
        if path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false);
            if !hidden {
                walk_md(&path, files)?;
            }
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            files.push(path);
        }
    }
    Ok(())
}

/// Substring search over local note titles / text (no FTS required).
pub fn search_local_notes<'a, I>(notes: I, query: &str, limit: usize) -> Vec<SearchHit>
where
    I: IntoIterator<Item = (String, String, Option<&'a str>)>, // id, title, text?
{
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (id, title, text) in notes {
        let title_l = title.to_lowercase();
        let body = text.unwrap_or("").to_lowercase();
        if title_l.contains(&q) || body.contains(&q) {
            let snippet = if body.contains(&q) {
                make_snippet(text.unwrap_or(""), &q)
            } else {
                title.clone()
            };
            hits.push(SearchHit {
                id,
                source: "local_note".to_string(),
                title,
                snippet,
                path: None,
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// Substring search over task titles.
pub fn search_tasks<I>(tasks: I, query: &str, limit: usize) -> Vec<SearchHit>
where
    I: IntoIterator<Item = (String, String, bool)>, // id, title, done
{
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (id, title, done) in tasks {
        if title.to_lowercase().contains(&q) {
            let status = if done { "done" } else { "open" };
            hits.push(SearchHit {
                id,
                source: "task".to_string(),
                title: title.clone(),
                snippet: format!("Task ({status})"),
                path: None,
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// Substring search over calendar event titles.
pub fn search_events<I>(events: I, query: &str, limit: usize) -> Vec<SearchHit>
where
    I: IntoIterator<Item = (String, String)>, // id, title
{
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (id, title) in events {
        if title.to_lowercase().contains(&q) {
            hits.push(SearchHit {
                id,
                source: "calendar".to_string(),
                title: title.clone(),
                snippet: "Calendar event".to_string(),
                path: None,
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

fn make_snippet(text: &str, q_lower: &str) -> String {
    let lower = text.to_lowercase();
    let Some(pos) = lower.find(q_lower) else {
        return text.chars().take(80).collect();
    };
    let start = pos.saturating_sub(40);
    let end = (pos + q_lower.len() + 40).min(text.len());
    // Align to char boundaries
    let start = text
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text
        .char_indices()
        .find(|(i, _)| *i >= end)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let mut snip = String::new();
    if start > 0 {
        snip.push('…');
    }
    snip.push_str(text[start..end].trim());
    if end < text.len() {
        snip.push('…');
    }
    snip
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_md(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn fts_finds_vault_note_with_snippet() {
        let tmp = TempDir::new().unwrap();
        write_md(
            tmp.path(),
            "Projects/Alpha.md",
            "# Alpha\n\nDiscuss the ephemeris ink engine tomorrow.",
        );
        write_md(tmp.path(), "Inbox.md", "# Inbox\n\nBuy milk");

        let mut idx = VaultFtsIndex::open_in_memory().unwrap();
        let n = idx.reindex_vault(tmp.path()).unwrap();
        assert_eq!(n, 2);

        let hits = idx.search("ephemeris ink", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_deref(), Some("Projects/Alpha.md"));
        assert!(hits[0].snippet.to_lowercase().contains("ink"));
    }

    #[test]
    fn markdown_to_plain_strips_markup() {
        let plain = markdown_to_plain("# Hello\n\n**Bold** and [[Wiki|Label]] and [a](http://x)");
        assert!(plain.contains("Hello"));
        assert!(plain.contains("Bold"));
        assert!(plain.contains("Label"));
        assert!(plain.contains('a'));
        assert!(!plain.contains("**"));
        assert!(!plain.contains("http"));
    }

    #[test]
    fn strip_inline_removes_bold_markers() {
        let s = strip_inline_markdown(
            "**3. Airflow DAG import errors** violates the Controllability principle",
        );
        assert!(!s.contains("**"));
        assert!(s.starts_with("3. Airflow"));
        assert!(s.contains("Controllability"));
    }

    #[test]
    fn markdown_to_blocks_headings_and_bullets() {
        let blocks = markdown_to_blocks("# Title\n\n- one\n- two\n\nPara **bold**");
        assert_eq!(blocks[0].kind, MdBlockKind::Heading1);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[1].kind, MdBlockKind::Bullet);
        assert_eq!(blocks[1].text, "one");
        assert!(blocks.iter().any(|b| b.kind == MdBlockKind::Paragraph
            && b.text.contains("bold")
            && !b.text.contains("**")));
    }

    #[test]
    fn search_local_and_tasks() {
        let notes = vec![
            ("1".into(), "Meeting".into(), Some("agenda for sprint")),
            ("2".into(), "Other".into(), None),
        ];
        let hits = search_local_notes(notes, "sprint", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "local_note");

        let tasks = vec![("t1".into(), "Buy milk".into(), false)];
        let th = search_tasks(tasks, "milk", 10);
        assert_eq!(th.len(), 1);
        assert_eq!(th[0].source, "task");
    }
}
