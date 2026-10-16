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
    pub fn reindex_vault(
        &mut self,
        vault_path: &Path,
        exclude_subdir: Option<&Path>,
    ) -> crate::Result<usize> {
        self.reindex_vaults(&[vault_path], exclude_subdir)
    }

    /// Clear and rebuild the index from one or more vault directories.
    ///
    /// Document `path` / `id` is the vault-relative path when a single vault is
    /// indexed, or `abspath` when multiple vaults are merged (so open-by-path
    /// stays unambiguous).
    ///
    /// When `exclude_subdir` is set (vault-relative, e.g. `Ephemeris`), that
    /// tree is skipped so local note exports are not indexed as vault notes.
    pub fn reindex_vaults(
        &mut self,
        vault_paths: &[&Path],
        exclude_subdir: Option<&Path>,
    ) -> crate::Result<usize> {
        self.clear()?;
        let multi = vault_paths.len() > 1;
        let mut count = 0usize;
        for vault_path in vault_paths {
            if !vault_path.is_dir() {
                tracing::warn!("Vault path {:?} is not a directory; skipping", vault_path);
                continue;
            }
            let files = list_markdown_files(vault_path, exclude_subdir)?;
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
    /// Tappable wiki / markdown link (`link_target` set).
    Link,
}

/// One rendered block of markdown (plain text + role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdBlock {
    pub kind: MdBlockKind,
    /// Plain text for fallback / list display.
    pub text: String,
    /// Inline markdown safe for Slint `StyledText::from_markdown` (emphasis, etc.).
    pub styled_md: String,
    /// Wiki path or relative vault path when `kind == Link`; empty otherwise.
    pub link_target: String,
}

impl MdBlock {
    fn new(kind: MdBlockKind, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            kind,
            styled_md: text.clone(),
            text,
            link_target: String::new(),
        }
    }

    fn styled(kind: MdBlockKind, plain: impl Into<String>, styled_md: impl Into<String>) -> Self {
        Self {
            kind,
            text: plain.into(),
            styled_md: styled_md.into(),
            link_target: String::new(),
        }
    }

    fn link(label: impl Into<String>, target: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            kind: MdBlockKind::Link,
            styled_md: format!("**{label}**"),
            text: label,
            link_target: target.into(),
        }
    }
}

/// Strip common inline markdown markers for list/title display.
///
/// Keeps the readable words; drops `**bold**`, `*italic*`, `` `code` ``,
/// `~~strike~~`, `==highlight==`, `<u>underline</u>`, wikilinks and
/// `[label](url)` wrappers. A backslash before a markdown-significant char
/// (as emitted by `lines_to_markdown`) is dropped.
pub fn strip_inline_markdown(s: &str) -> String {
    let s = simplify_wikilinks(s);
    let s = simplify_md_links(&s);
    let s = s.replace("<u>", "").replace("</u>", "");
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
        // \# escaped markdown char: keep the char, drop the backslash
        if chars[i] == '\\'
            && chars
                .get(i + 1)
                .is_some_and(|c| "\\#-*+>`_~=.1234567890".contains(*c))
        {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // ==highlight==
        if chars[i] == '=' && chars.get(i + 1) == Some(&'=') {
            if let Some(end) = find_closing_pair(&chars, i + 2, '=', true) {
                for &c in &chars[i + 2..end] {
                    out.push(c);
                }
                i = end + 2;
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

/// Fold Obsidian `==highlight==` into `**bold**` for Slint StyledText.
fn obsidian_highlights_to_bold(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '=' && chars.get(i + 1) == Some(&'=') {
            if let Some(end) = chars[i + 2..].iter().position(|&c| c == '=') {
                let end = i + 2 + end;
                if chars.get(end + 1) == Some(&'=') {
                    out.push_str("**");
                    for &c in &chars[i + 2..end] {
                        out.push(c);
                    }
                    out.push_str("**");
                    i = end + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip YAML frontmatter (`---` … `---`) so the MD viewer skips metadata.
fn strip_yaml_frontmatter(md: &str) -> &str {
    let mut lines = md.lines();
    let Some(first) = lines.next() else {
        return md;
    };
    if first.trim() != "---" {
        return md;
    }
    let mut consumed = first.len();
    if md.as_bytes().get(consumed) == Some(&b'\n') {
        consumed += 1;
    } else if md.as_bytes().get(consumed) == Some(&b'\r') {
        consumed += 1;
        if md.as_bytes().get(consumed) == Some(&b'\n') {
            consumed += 1;
        }
    }
    for line in lines {
        let line_len = line.len();
        let mut step = line_len;
        if md.as_bytes().get(consumed + step) == Some(&b'\n') {
            step += 1;
        } else if md.as_bytes().get(consumed + step) == Some(&b'\r') {
            step += 1;
            if md.as_bytes().get(consumed + step) == Some(&b'\n') {
                step += 1;
            }
        }
        consumed += step;
        if line.trim() == "---" {
            return md.get(consumed..).unwrap_or("");
        }
    }
    md
}

/// Rewrite Obsidian `[[Note|Alias]]` / `[[Note]]` into `[Alias](wiki:Note)` so
/// pulldown-cmark emits ordinary links we can follow in the viewer.
fn expand_wikilinks(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        if let Some(end) = rest.find("]]") {
            let inner = &rest[..end];
            let (target, label) = match inner.split_once('|') {
                Some((t, l)) => (t.trim(), l.trim()),
                None => (inner.trim(), inner.trim()),
            };
            if !target.is_empty() {
                out.push('[');
                out.push_str(label);
                out.push_str("](wiki:");
                out.push_str(target);
                out.push(')');
            } else {
                out.push_str("[[");
                out.push_str(inner);
                out.push_str("]]");
            }
            rest = &rest[end + 2..];
        } else {
            out.push_str("[[");
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Parse markdown into display blocks for the eink viewer via `pulldown-cmark`.
pub fn markdown_to_blocks(md: &str) -> Vec<MdBlock> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let body = strip_yaml_frontmatter(md);
    let expanded = expand_wikilinks(body);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut blocks = Vec::new();
    let mut plain = String::new();
    let mut styled = String::new();
    let mut kind_stack: Vec<MdBlockKind> = Vec::new();
    let mut list_is_ordered: Vec<bool> = Vec::new();
    let mut ordered_index: Vec<u64> = Vec::new();
    let mut link_dest: Option<String> = None;
    let mut link_plain = String::new();
    let mut link_styled = String::new();
    let mut task_checked: Option<bool> = None;
    let mut in_table = false;
    let mut table_row = String::new();

    let flush_text_block = |kind: MdBlockKind,
                            plain: &mut String,
                            styled: &mut String,
                            task: &mut Option<bool>,
                            blocks: &mut Vec<MdBlock>| {
        let raw = plain.trim().to_string();
        let raw_styled = styled.trim().to_string();
        plain.clear();
        styled.clear();
        if raw.is_empty() && !matches!(kind, MdBlockKind::Rule) {
            *task = None;
            return;
        }
        // Obsidian ==highlight== is not CommonMark; fold to bold for StyledText
        // and strip for plain (matches legacy strip_inline_markdown).
        let p = strip_inline_markdown(&raw);
        let s = obsidian_highlights_to_bold(if raw_styled.is_empty() {
            &raw
        } else {
            &raw_styled
        });
        let (p, s) = match (*task, kind) {
            (Some(true), MdBlockKind::Bullet) => (format!("☑ {p}"), format!("☑ {s}")),
            (Some(false), MdBlockKind::Bullet) => (format!("☐ {p}"), format!("☐ {s}")),
            _ => (p, s),
        };
        *task = None;
        blocks.push(MdBlock::styled(kind, p, s));
    };

    for event in Parser::new_ext(&expanded, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let k = match level {
                    pulldown_cmark::HeadingLevel::H1 => MdBlockKind::Heading1,
                    pulldown_cmark::HeadingLevel::H2 => MdBlockKind::Heading2,
                    _ => MdBlockKind::Heading3,
                };
                kind_stack.push(k);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(k) = kind_stack.pop() {
                    // Headings: plain only (bold weight from kind in UI).
                    let p = strip_inline_markdown(plain.trim());
                    plain.clear();
                    styled.clear();
                    blocks.push(MdBlock::new(k, p));
                    task_checked = None;
                }
            }
            Event::Start(Tag::Paragraph) => {
                kind_stack.push(MdBlockKind::Paragraph);
            }
            Event::End(TagEnd::Paragraph) => {
                if let Some(k) = kind_stack.pop() {
                    flush_text_block(k, &mut plain, &mut styled, &mut task_checked, &mut blocks);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                kind_stack.push(MdBlockKind::Quote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(k) = kind_stack.pop() {
                    flush_text_block(k, &mut plain, &mut styled, &mut task_checked, &mut blocks);
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                kind_stack.push(MdBlockKind::Code);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(k) = kind_stack.pop() {
                    let t = plain.trim_end().to_string();
                    plain.clear();
                    styled.clear();
                    if !t.is_empty() {
                        blocks.push(MdBlock::new(k, t));
                    }
                }
            }
            Event::Start(Tag::List(start)) => {
                list_is_ordered.push(start.is_some());
                ordered_index.push(start.unwrap_or(1));
            }
            Event::End(TagEnd::List(_)) => {
                list_is_ordered.pop();
                ordered_index.pop();
            }
            Event::Start(Tag::Item) => {
                let ordered = list_is_ordered.last().copied().unwrap_or(false);
                if ordered {
                    let n = ordered_index.last().copied().unwrap_or(1);
                    if let Some(idx) = ordered_index.last_mut() {
                        *idx += 1;
                    }
                    plain.push_str(&format!("{n}. "));
                    styled.push_str(&format!("{n}. "));
                    kind_stack.push(MdBlockKind::Numbered);
                } else {
                    kind_stack.push(MdBlockKind::Bullet);
                }
            }
            Event::End(TagEnd::Item) => {
                if let Some(k) = kind_stack.pop() {
                    flush_text_block(k, &mut plain, &mut styled, &mut task_checked, &mut blocks);
                }
            }
            Event::Start(Tag::Table(_)) => {
                in_table = true;
                table_row.clear();
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                if !table_row.trim().is_empty() {
                    blocks.push(MdBlock::new(
                        MdBlockKind::Code,
                        table_row.trim().to_string(),
                    ));
                }
                table_row.clear();
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {}
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                table_row.push('\n');
            }
            Event::Start(Tag::TableCell) => {
                if !table_row.is_empty() && !table_row.ends_with('\n') {
                    table_row.push_str(" | ");
                }
            }
            Event::End(TagEnd::TableCell) => {}
            Event::Rule => {
                blocks.push(MdBlock::new(MdBlockKind::Rule, String::new()));
            }
            Event::TaskListMarker(checked) => {
                task_checked = Some(checked);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_dest = Some(dest_url.to_string());
                link_plain.clear();
                link_styled.clear();
            }
            Event::End(TagEnd::Link) => {
                let label_plain = strip_inline_markdown(link_plain.trim());
                link_plain.clear();
                link_styled.clear();
                if let Some(dest) = link_dest.take() {
                    let is_wiki = dest.starts_with("wiki:");
                    let target = dest
                        .strip_prefix("wiki:")
                        .unwrap_or(dest.as_str())
                        .to_string();
                    if is_wiki && !label_plain.is_empty() {
                        // Wiki links become their own tappable rows.
                        blocks.push(MdBlock::link(label_plain, target));
                    } else if !label_plain.is_empty() {
                        // External / relative MD links: keep label in the
                        // surrounding block; still emit a follow-row for .md.
                        plain.push_str(&label_plain);
                        styled.push_str("**");
                        styled.push_str(&label_plain);
                        styled.push_str("**");
                        if target.ends_with(".md") || !target.contains("://") {
                            blocks.push(MdBlock::link(label_plain, target));
                        }
                    }
                }
            }
            Event::Start(Tag::Emphasis) => styled.push('*'),
            Event::End(TagEnd::Emphasis) => styled.push('*'),
            Event::Start(Tag::Strong) => styled.push_str("**"),
            Event::End(TagEnd::Strong) => styled.push_str("**"),
            Event::Start(Tag::Strikethrough) => styled.push_str("~~"),
            Event::End(TagEnd::Strikethrough) => styled.push_str("~~"),
            Event::Code(c) => {
                let c = c.as_ref();
                plain.push_str(c);
                styled.push('`');
                styled.push_str(c);
                styled.push('`');
                if in_table {
                    table_row.push_str(c);
                }
            }
            Event::Text(t) => {
                let t = t.as_ref();
                if in_table {
                    table_row.push_str(t);
                } else if link_dest.is_some() {
                    link_plain.push_str(t);
                    link_styled.push_str(t);
                } else {
                    plain.push_str(t);
                    styled.push_str(t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_table {
                    // ignore
                } else {
                    plain.push(' ');
                    styled.push(' ');
                }
            }
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }
    blocks
}

/// Resolve an Obsidian wiki target (`Note`, `Folder/Note`, `Note.md`) to a
/// vault-relative `.md` path, or `None` if not found.
pub fn resolve_wiki_target(
    vault_root: &Path,
    target: &str,
    exclude_subdir: Option<&Path>,
) -> Option<String> {
    let target = target.trim().trim_start_matches('/').replace('\\', "/");
    if target.is_empty() {
        return None;
    }
    let with_ext = if target.ends_with(".md") {
        target.clone()
    } else {
        format!("{target}.md")
    };
    let direct = vault_root.join(&with_ext);
    if direct.is_file() {
        return Some(with_ext);
    }
    let stem = Path::new(&target)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(target.as_str())
        .to_lowercase();
    let files = list_markdown_files(vault_root, exclude_subdir).ok()?;
    let mut matches: Vec<String> = files
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(vault_root).ok()?;
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let file_stem = rel
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if file_stem == stem {
                Some(rel_s)
            } else {
                None
            }
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
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
            MdBlockKind::Link => {
                if b.link_target.is_empty() {
                    b.text
                } else {
                    format!("{} → {}", b.text, b.link_target)
                }
            }
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

/// List `.md` files under `vault_root`, skipping hidden dirs and optional export tree.
pub fn list_markdown_files(
    vault_root: &Path,
    exclude_subdir: Option<&Path>,
) -> crate::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_md(vault_root, vault_root, exclude_subdir, &mut files)?;
    Ok(files)
}

fn walk_md(
    vault_root: &Path,
    dir: &Path,
    exclude_subdir: Option<&Path>,
    files: &mut Vec<PathBuf>,
) -> crate::Result<()> {
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
            if hidden {
                continue;
            }
            if let Some(ex) = exclude_subdir {
                if crate::path_under_export_subdir(vault_root, &path, ex) {
                    continue;
                }
            }
            walk_md(vault_root, &path, exclude_subdir, files)?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Some(ex) = exclude_subdir {
                if crate::path_under_export_subdir(vault_root, &path, ex) {
                    continue;
                }
            }
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
        let n = idx.reindex_vault(tmp.path(), None).unwrap();
        assert_eq!(n, 2);

        let hits = idx.search("ephemeris ink", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_deref(), Some("Projects/Alpha.md"));
        assert!(hits[0].snippet.to_lowercase().contains("ink"));
    }

    #[test]
    fn fts_skips_export_subdir() {
        let tmp = TempDir::new().unwrap();
        write_md(tmp.path(), "Real.md", "# Real\n\nfindme-vault");
        write_md(
            tmp.path(),
            "Ephemeris/Notes/Local.md",
            "# Local export\n\nfindme-export",
        );

        let mut idx = VaultFtsIndex::open_in_memory().unwrap();
        let n = idx
            .reindex_vault(tmp.path(), Some(Path::new("Ephemeris")))
            .unwrap();
        assert_eq!(n, 1);
        assert!(idx.search("findme-vault", 10).unwrap().len() == 1);
        assert!(idx.search("findme-export", 10).unwrap().is_empty());
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
