//! Row model for the formatted See-text view.
//!
//! Pure data (no Slint): [`rows_from_cache`] builds rows from a page's
//! recognition cache, [`rows_from_markdown`] from stored `note.text_content`
//! (fallback when no cache is loaded). Each row carries `styled_md`, a
//! StyledText-safe inline markdown string (`**bold**`, `<u>..</u>` only, user
//! text escaped) and `plain` for the fallback when Slint rejects it.

use crate::note_search::strip_inline_markdown;
use crate::recognition_cache::{PageRecognitionCache, PENDING_LINE_MARKER};
use crate::text_format::{styled_inline_markdown, BlockKind, InlineKind};

pub const KIND_H1: i32 = 0;
pub const KIND_H2: i32 = 1;
pub const KIND_H3: i32 = 2;
pub const KIND_PARA: i32 = 3;
pub const KIND_BULLET: i32 = 4;
pub const KIND_NUMBERED: i32 = 5;
pub const KIND_CHECKBOX: i32 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeeTextRow {
    /// 0=h1 1=h2 2=h3 3=para 4=bullet 5=numbered 6=checkbox
    pub kind: i32,
    pub number: i32,
    pub checked: bool,
    pub indent: i32,
    pub gap_before: bool,
    pub pending: bool,
    pub stale: bool,
    /// Index into the cache's `lines` (0 for markdown-derived rows: position).
    pub line_index: i32,
    /// StyledText-safe inline markdown.
    pub styled_md: String,
    /// Plain text fallback.
    pub plain: String,
}

fn is_list_kind(k: i32) -> bool {
    matches!(k, KIND_BULLET | KIND_NUMBERED | KIND_CHECKBOX)
}

fn bold_heading(inner: &str) -> String {
    if inner.trim().is_empty() {
        inner.to_string()
    } else {
        format!("**{inner}**")
    }
}

/// Rows for every cache line (pending lines show [`PENDING_LINE_MARKER`]).
pub fn rows_from_cache(cache: &PageRecognitionCache) -> Vec<SeeTextRow> {
    let mut rows: Vec<SeeTextRow> = Vec::new();
    for (i, l) in cache.lines.iter().enumerate() {
        let (kind, number, checked, indent, para_break, styled_md, plain, pending);
        if l.is_completed() {
            let f = l.effective_format();
            let text = l.effective_text();
            let (k, n, c) = match f.block {
                BlockKind::Paragraph => (KIND_PARA, 0, false),
                BlockKind::Heading(h) => (KIND_H1 + (h.clamp(1, 3) as i32 - 1), 0, false),
                BlockKind::Bullet => (KIND_BULLET, 0, false),
                BlockKind::Numbered(n) => (KIND_NUMBERED, n as i32, false),
                BlockKind::Checkbox(c) => (KIND_CHECKBOX, 0, c),
            };
            kind = k;
            number = n;
            checked = c;
            indent = if is_list_kind(k) { f.indent as i32 } else { 0 };
            para_break = f.para_break_before;
            styled_md = if kind <= KIND_H3 {
                // StyledText has no font-weight: headings are bold via markup.
                let marks: Vec<_> = f
                    .marks
                    .iter()
                    .filter(|m| m.kind == InlineKind::Underline)
                    .copied()
                    .collect();
                bold_heading(&styled_inline_markdown(text, &marks))
            } else {
                styled_inline_markdown(text, &f.marks)
            };
            plain = text.to_string();
            pending = l.pending;
        } else {
            kind = KIND_PARA;
            number = 0;
            checked = false;
            indent = 0;
            para_break = false;
            styled_md = PENDING_LINE_MARKER.to_string();
            plain = PENDING_LINE_MARKER.to_string();
            pending = true;
        }
        // Same spacing rule as `lines_to_markdown`.
        let gap_before = rows.last().is_some_and(|p| {
            let tight = (p.kind == KIND_PARA && kind == KIND_PARA && !para_break)
                || (is_list_kind(p.kind) && is_list_kind(kind) && !para_break);
            !tight
        });
        rows.push(SeeTextRow {
            kind,
            number,
            checked,
            indent,
            gap_before,
            pending,
            stale: l.user_stale,
            line_index: i as i32,
            styled_md,
            plain,
        });
    }
    rows
}

/// Convert stored inline markdown (`**`, `==`, `<u>`, backslash escapes) to
/// the StyledText-safe subset. Anything else markdown-significant is escaped.
pub fn markdown_inline_to_styled(s: &str) -> String {
    inline_to_styled(s, false)
}

fn inline_to_styled(s: &str, drop_bold: bool) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    let mut i = 0;
    let starts = |i: usize, pat: &str| {
        pat.chars()
            .enumerate()
            .all(|(k, p)| c.get(i + k) == Some(&p))
    };
    while i < c.len() {
        if starts(i, "**") || starts(i, "==") {
            if !drop_bold {
                out.push_str("**");
            }
            i += 2;
        } else if starts(i, "<u>") {
            out.push_str("<u>");
            i += 3;
        } else if starts(i, "</u>") {
            out.push_str("</u>");
            i += 4;
        } else if c[i] == '\\' && i + 1 < c.len() && c[i + 1].is_ascii_punctuation() {
            out.push('\\');
            out.push(c[i + 1]);
            i += 2;
        } else {
            if matches!(
                c[i],
                '\\' | '*' | '_' | '`' | '<' | '>' | '[' | ']' | '~' | '&'
            ) {
                out.push('\\');
            }
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// Rows parsed from stored markdown (`lines_to_markdown` output).
pub fn rows_from_markdown(md: &str) -> Vec<SeeTextRow> {
    let mut rows: Vec<SeeTextRow> = Vec::new();
    let mut blank = false;
    for (idx, raw) in md.lines().enumerate() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            blank = true;
            continue;
        }
        let lead = line.len() - line.trim_start().len();
        let t = line.trim_start();
        let mut kind = KIND_PARA;
        let (mut number, mut checked, mut indent) = (0, false, 0);
        let mut body = t;
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            kind = KIND_H1 + (hashes.min(3) as i32 - 1);
            body = t[hashes + 1..].trim_start();
        } else if let Some(rest) = ["- [ ] ", "* [ ] ", "+ [ ] "]
            .iter()
            .find_map(|p| t.strip_prefix(p))
        {
            kind = KIND_CHECKBOX;
            body = rest;
            indent = (lead / 2) as i32;
        } else if let Some(rest) = ["- [x] ", "- [X] ", "* [x] ", "+ [x] "]
            .iter()
            .find_map(|p| t.strip_prefix(p))
        {
            kind = KIND_CHECKBOX;
            checked = true;
            body = rest;
            indent = (lead / 2) as i32;
        } else if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|p| t.strip_prefix(p)) {
            kind = KIND_BULLET;
            body = rest;
            indent = (lead / 2) as i32;
        } else {
            let d = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if d > 0 && t[d..].starts_with(". ") {
                kind = KIND_NUMBERED;
                number = t[..d].parse().unwrap_or(1);
                body = t[d + 2..].trim_start();
                indent = (lead / 2) as i32;
            }
        }
        rows.push(SeeTextRow {
            kind,
            number,
            checked,
            indent,
            gap_before: blank && !rows.is_empty(),
            pending: false,
            stale: false,
            line_index: idx as i32,
            styled_md: if kind <= KIND_H3 {
                bold_heading(&inline_to_styled(body, true))
            } else {
                markdown_inline_to_styled(body)
            },
            plain: strip_inline_markdown(body),
        });
        blank = false;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition_cache::CachedLine;
    use crate::text_format::{InlineKind, InlineMark, LineFormat};

    fn line(text: &str, fmt: LineFormat) -> CachedLine {
        CachedLine {
            stroke_ids_hash: 0,
            stroke_ids: vec![],
            text: text.into(),
            raw_text: None,
            confidence: 0.9,
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            pending: false,
            format: fmt,
            user_text: None,
            user_format: None,
            user_stale: false,
        }
    }

    fn fmt(block: BlockKind) -> LineFormat {
        LineFormat {
            block,
            ..Default::default()
        }
    }

    fn cache(lines: Vec<CachedLine>) -> PageRecognitionCache {
        let mut c = PageRecognitionCache::new("v");
        c.lines = lines;
        c
    }

    #[test]
    fn cache_rows_blocks_indent_gap_pending_stale() {
        let mut pend = line("", LineFormat::default());
        pend.pending = true;
        let mut stale = line("old", fmt(BlockKind::Paragraph));
        stale.user_stale = true;
        let mut nested = fmt(BlockKind::Bullet);
        nested.indent = 1;
        let rows = rows_from_cache(&cache(vec![
            line("Title", fmt(BlockKind::Heading(1))),
            line("a", fmt(BlockKind::Bullet)),
            line("b", nested),
            line("t", fmt(BlockKind::Checkbox(true))),
            line("3rd", fmt(BlockKind::Numbered(3))),
            line("p", fmt(BlockKind::Paragraph)),
            pend,
            stale,
        ]));
        let k: Vec<i32> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(k, [0, 4, 4, 6, 5, 3, 3, 3]);
        assert_eq!(rows[2].indent, 1);
        assert!(rows[3].checked);
        assert_eq!(rows[4].number, 3);
        // gaps: heading->list yes, list->list no, list->para yes, para->pending no
        let g: Vec<bool> = rows.iter().map(|r| r.gap_before).collect();
        assert_eq!(g, [false, true, false, false, false, true, false, false]);
        assert!(rows[6].pending);
        assert_eq!(rows[6].styled_md, PENDING_LINE_MARKER);
        assert!(rows[7].stale && !rows[6].stale);
        assert_eq!(rows[7].line_index, 7);
        // heading text carries no '#'
        assert_eq!(rows[0].styled_md, "**Title**");
    }

    #[test]
    fn highlight_becomes_bold_underline_kept() {
        let mut f = fmt(BlockKind::Paragraph);
        f.marks = vec![
            InlineMark {
                kind: InlineKind::Highlight,
                start: 0,
                end: 1,
            },
            InlineMark {
                kind: InlineKind::Underline,
                start: 2,
                end: 3,
            },
        ];
        let rows = rows_from_cache(&cache(vec![line("d e", f)]));
        assert_eq!(rows[0].styled_md, "**d** <u>e</u>");
    }

    #[test]
    fn user_text_is_escaped() {
        let f = fmt(BlockKind::Paragraph);
        let rows = rows_from_cache(&cache(vec![
            line("a*b_c`d<e>", f.clone()),
            line("# not heading", f.clone()),
            line("1. not list", f.clone()),
            line("- dash", f),
        ]));
        assert_eq!(rows[0].styled_md, "a\\*b\\_c\\`d\\<e\\>");
        assert_eq!(rows[1].styled_md, "\\# not heading");
        assert_eq!(rows[2].styled_md, "1\\. not list");
        assert_eq!(rows[3].styled_md, "\\- dash");
    }

    #[test]
    fn markdown_rows_acceptance_example() {
        let rows = rows_from_markdown("# A\n\n- b\n  - c\n\n==d== <u>e</u>");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, KIND_H1);
        assert_eq!(rows[0].styled_md, "**A**");
        assert_eq!((rows[1].kind, rows[1].indent), (KIND_BULLET, 0));
        assert_eq!((rows[2].kind, rows[2].indent), (KIND_BULLET, 1));
        assert!(!rows[2].gap_before);
        assert!(rows[3].gap_before);
        assert_eq!(rows[3].styled_md, "**d** <u>e</u>");
        assert_eq!(rows[3].plain, "d e");
    }

    #[test]
    fn markdown_rows_checkbox_numbered_escape() {
        let rows = rows_from_markdown("- [ ] todo\n- [x] done\n7. seven\n\\# hash\nsnake_case <b>");
        assert_eq!((rows[0].kind, rows[0].checked), (KIND_CHECKBOX, false));
        assert!(rows[1].checked);
        assert_eq!((rows[2].kind, rows[2].number), (KIND_NUMBERED, 7));
        assert_eq!(rows[3].styled_md, "\\# hash");
        assert_eq!(rows[4].styled_md, "snake\\_case \\<b\\>");
    }
}
