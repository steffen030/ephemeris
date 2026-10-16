//! Row model for the formatted See-text view.
//!
//! Pure data (no Slint): [`rows_from_cache`] builds rows from a page's
//! recognition cache, [`rows_from_markdown`] from stored `note.text_content`
//! (fallback when no cache is loaded). Each row carries `styled_md`, a
//! StyledText-safe inline markdown string (`**bold**`, `<u>..</u>` only, user
//! text escaped) and `plain` for the fallback when Slint rejects it.

use crate::note_search::strip_inline_markdown;
use crate::recognition_cache::{PageRecognitionCache, PENDING_LINE_MARKER};
use crate::text_format::{styled_inline_markdown, BlockKind, InlineKind, InlineMark, LineFormat};

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
    /// True when `line_index` is a real cache line the editor may correct
    /// (false for markdown-derived and still-empty pending rows).
    pub editable: bool,
    /// A highlight / underline mark covers the whole trimmed line.
    pub whole_highlight: bool,
    pub whole_underline: bool,
    /// StyledText-safe inline markdown.
    pub styled_md: String,
    /// Plain text fallback.
    pub plain: String,
}

fn is_list_kind(k: i32) -> bool {
    matches!(k, KIND_BULLET | KIND_NUMBERED | KIND_CHECKBOX)
}

/// Char range of `text` without leading / trailing whitespace.
fn trimmed_range(text: &str) -> (usize, usize) {
    let n = text.chars().count();
    let lead = text.chars().take_while(|c| c.is_whitespace()).count();
    let trail = text.chars().rev().take_while(|c| c.is_whitespace()).count();
    (lead.min(n), n.saturating_sub(trail).max(lead.min(n)))
}

/// True when a `kind` mark spans the whole trimmed (non-empty) line.
fn has_whole_mark(text: &str, marks: &[InlineMark], kind: InlineKind) -> bool {
    let (s, e) = trimmed_range(text);
    s < e
        && marks
            .iter()
            .any(|m| m.kind == kind && m.start <= s && m.end >= e)
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
        let (mut whole_highlight, mut whole_underline) = (false, false);
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
            whole_highlight = has_whole_mark(text, &f.marks, InlineKind::Highlight);
            whole_underline = has_whole_mark(text, &f.marks, InlineKind::Underline);
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
            editable: l.is_completed(),
            whole_highlight,
            whole_underline,
            styled_md,
            plain,
        });
    }
    rows
}

/// Convert stored inline markdown (`**`, `==`, `*em*` / `_em_`, `<u>`,
/// backslash escapes) to the StyledText-safe subset. Anything else
/// markdown-significant is escaped.
pub fn markdown_inline_to_styled(s: &str) -> String {
    inline_to_styled(s, false)
}

/// Char indices of single `*` / `_` that form a `*em*` / `_em_` pair.
/// Delimiters must hug the text (`* a *` is literal) and `_` must not sit
/// inside a word (`snake_case_name` is literal), as in CommonMark.
fn emphasis_delims(c: &[char]) -> Vec<usize> {
    let word = |i: Option<usize>| {
        i.and_then(|i| c.get(i))
            .is_some_and(|ch| ch.is_alphanumeric())
    };
    let space = |i: Option<usize>| i.and_then(|i| c.get(i)).is_none_or(|ch| ch.is_whitespace());
    let single = |i: usize, m: char| {
        c[i] == m
            && c.get(i + 1) != Some(&m)
            && (i == 0 || c[i - 1] != m)
            && (i == 0 || c[i - 1] != '\\')
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let m = c[i];
        let opens = (m == '*' || m == '_')
            && single(i, m)
            && !space(Some(i + 1))
            && !(m == '_' && word(i.checked_sub(1)));
        if opens {
            let close = (i + 2..c.len())
                .find(|&j| single(j, m) && !space(Some(j - 1)) && !(m == '_' && word(Some(j + 1))));
            if let Some(j) = close {
                out.extend([i, j]);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn inline_to_styled(s: &str, drop_bold: bool) -> String {
    let c: Vec<char> = s.chars().collect();
    let emphasis = emphasis_delims(&c);
    let mut out = String::with_capacity(s.len() + 4);
    let mut i = 0;
    let starts = |i: usize, pat: &str| {
        pat.chars()
            .enumerate()
            .all(|(k, p)| c.get(i + k) == Some(&p))
    };
    while i < c.len() {
        if emphasis.contains(&i) {
            // StyledText only parses `*em*`.
            out.push('*');
            i += 1;
        } else if starts(i, "**") || starts(i, "==") {
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
            editable: false,
            whole_highlight: false,
            whole_underline: false,
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

/// Editor state for one line: plain text (no markup) plus format chips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEdit {
    pub text: String,
    pub block: BlockKind,
    /// List nesting; clamped to `0..=MAX_INDENT` and ignored for non-lists.
    pub indent: u8,
    /// Whole-line highlight / underline chips.
    pub highlight: bool,
    pub underline: bool,
}

pub const MAX_INDENT: u8 = 3;

/// Block chip index (same numbering as `KIND_*`) to a [`BlockKind`].
/// `number` is kept for Numbered when positive, else 1; `checked` is the
/// previous checkbox state.
pub fn block_from_kind(kind: i32, number: i32, checked: bool) -> BlockKind {
    match kind {
        KIND_H1 => BlockKind::Heading(1),
        KIND_H2 => BlockKind::Heading(2),
        KIND_H3 => BlockKind::Heading(3),
        KIND_BULLET => BlockKind::Bullet,
        KIND_NUMBERED => BlockKind::Numbered(number.max(1) as u32),
        KIND_CHECKBOX => BlockKind::Checkbox(checked),
        _ => BlockKind::Paragraph,
    }
}

/// Initial editor state for cache line `idx` (effective text and format).
pub fn line_edit_state(cache: &PageRecognitionCache, idx: usize) -> Option<LineEdit> {
    let l = cache.lines.get(idx)?;
    let f = l.effective_format();
    let text = l.effective_text();
    Some(LineEdit {
        text: text.to_string(),
        block: f.block,
        indent: f.indent.min(MAX_INDENT),
        highlight: has_whole_mark(text, &f.marks, InlineKind::Highlight),
        underline: has_whole_mark(text, &f.marks, InlineKind::Underline),
    })
}

/// Save an editor result as the user's correction for line `idx`.
///
/// Newlines in the text become spaces. `user_text` is set when the text
/// differs from the effective text or a text correction already exists;
/// `user_format` is always set. Mark handling:
/// - Highlight / Underline chips are whole-line marks over the trimmed text.
///   A kind whose chip state is unchanged keeps its existing (possibly
///   partial) marks, unless the text changed.
/// - When the text changed, char offsets of old marks are meaningless, so
///   every mark that was not whole-line is dropped and whole-line marks
///   (any kind) are recomputed over the new trimmed text.
///
/// Returns false when `idx` is out of range.
pub fn apply_line_edit(cache: &mut PageRecognitionCache, idx: usize, edit: &LineEdit) -> bool {
    let Some(l) = cache.lines.get(idx) else {
        return false;
    };
    let old_text = l.effective_text().to_string();
    let mut fmt: LineFormat = l.effective_format().clone();
    let had_user_text = l.user_text.is_some();
    let new_text = edit.text.replace(['\r', '\n'], " ");
    let changed = new_text != old_text;

    let (ns, ne) = trimmed_range(&new_text);
    let whole = |kind| InlineMark {
        kind,
        start: ns,
        end: ne,
    };
    if changed {
        let was_whole = |m: &InlineMark| has_whole_mark(&old_text, std::slice::from_ref(m), m.kind);
        let keep: Vec<InlineKind> = fmt
            .marks
            .iter()
            .filter(|m| was_whole(m))
            .map(|m| m.kind)
            .collect();
        fmt.marks = Vec::new();
        if ns < ne {
            for k in [
                InlineKind::Bold,
                InlineKind::Highlight,
                InlineKind::Underline,
            ] {
                if keep.contains(&k) {
                    fmt.marks.push(whole(k));
                }
            }
        }
    }
    for (kind, want) in [
        (InlineKind::Highlight, edit.highlight),
        (InlineKind::Underline, edit.underline),
    ] {
        let has = has_whole_mark(&new_text, &fmt.marks, kind);
        if has == want {
            continue;
        }
        fmt.marks.retain(|m| m.kind != kind);
        if want && ns < ne {
            fmt.marks.push(whole(kind));
        }
    }

    fmt.block = edit.block;
    let is_list = matches!(
        edit.block,
        BlockKind::Bullet | BlockKind::Numbered(_) | BlockKind::Checkbox(_)
    );
    fmt.indent = if is_list {
        edit.indent.min(MAX_INDENT)
    } else {
        0
    };
    let text = (changed || had_user_text).then_some(new_text);
    cache.set_line_correction(idx, text, Some(fmt));
    true
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

    #[test]
    fn markdown_rows_italic_in_every_row_kind() {
        let rows = rows_from_markdown(
            "- [ ] call *Anna*\n- [x] _paid_ rent\n- a *b* c\n3. *x*\n# Big *one*\nplain *em* and **bold**",
        );
        assert_eq!(rows[0].styled_md, "call *Anna*");
        assert_eq!(rows[0].plain, "call Anna");
        assert_eq!(rows[1].styled_md, "*paid* rent");
        assert_eq!(rows[2].styled_md, "a *b* c");
        assert_eq!(rows[3].styled_md, "*x*");
        assert_eq!(rows[4].styled_md, "**Big *one***");
        assert_eq!(rows[5].styled_md, "plain *em* and **bold**");
    }

    #[test]
    fn markdown_rows_lone_or_intraword_markers_stay_literal() {
        let rows = rows_from_markdown(
            "snake_case_name\n2 * 3 * 4\na* b*\n\\*not em\\*\nfile_v2.txt and *open",
        );
        assert_eq!(rows[0].styled_md, "snake\\_case\\_name");
        assert_eq!(rows[1].styled_md, "2 \\* 3 \\* 4");
        assert_eq!(rows[2].styled_md, "a\\* b\\*");
        assert_eq!(rows[3].styled_md, "\\*not em\\*");
        assert_eq!(rows[4].styled_md, "file\\_v2.txt and \\*open");
    }

    fn edit_of(c: &PageRecognitionCache, i: usize) -> LineEdit {
        line_edit_state(c, i).unwrap()
    }

    fn mk(kind: InlineKind, start: usize, end: usize) -> InlineMark {
        InlineMark { kind, start, end }
    }

    #[test]
    fn edit_text_sets_user_text_and_recomputes_whole_marks() {
        let mut f = fmt(BlockKind::Paragraph);
        f.marks = vec![
            mk(InlineKind::Highlight, 0, 5), // whole line
            mk(InlineKind::Underline, 0, 2), // partial
        ];
        let mut c = cache(vec![line("hello", f)]);
        let mut e = edit_of(&c, 0);
        assert!(e.highlight && !e.underline);
        e.text = "hello brave world".into();
        assert!(apply_line_edit(&mut c, 0, &e));
        let l = &c.lines[0];
        assert_eq!(l.user_text.as_deref(), Some("hello brave world"));
        // Whole-line highlight follows the new length; partial underline dropped.
        assert_eq!(
            l.user_format.as_ref().unwrap().marks,
            vec![mk(InlineKind::Highlight, 0, 17)]
        );
        assert_eq!(l.text, "hello");
    }

    #[test]
    fn unchanged_text_keeps_partial_marks_and_no_user_text() {
        let mut f = fmt(BlockKind::Paragraph);
        f.marks = vec![mk(InlineKind::Underline, 0, 2)];
        let mut c = cache(vec![line("hello", f)]);
        let mut e = edit_of(&c, 0);
        e.block = BlockKind::Heading(2);
        apply_line_edit(&mut c, 0, &e);
        let l = &c.lines[0];
        assert!(l.user_text.is_none());
        let uf = l.user_format.as_ref().unwrap();
        assert_eq!(uf.block, BlockKind::Heading(2));
        assert_eq!(uf.marks, vec![mk(InlineKind::Underline, 0, 2)]);
    }

    #[test]
    fn chips_toggle_whole_line_marks_over_trimmed_text() {
        let mut c = cache(vec![line("  hi there ", fmt(BlockKind::Paragraph))]);
        let mut e = edit_of(&c, 0);
        e.underline = true;
        apply_line_edit(&mut c, 0, &e);
        assert_eq!(
            c.lines[0].user_format.as_ref().unwrap().marks,
            vec![mk(InlineKind::Underline, 2, 10)]
        );
        assert!(edit_of(&c, 0).underline);
        let mut e = edit_of(&c, 0);
        e.underline = false;
        apply_line_edit(&mut c, 0, &e);
        assert!(c.lines[0].user_format.as_ref().unwrap().marks.is_empty());
    }

    #[test]
    fn indent_clamps_and_only_applies_to_lists() {
        let mut c = cache(vec![line("a", fmt(BlockKind::Paragraph))]);
        let mut e = edit_of(&c, 0);
        e.block = BlockKind::Bullet;
        e.indent = 9;
        apply_line_edit(&mut c, 0, &e);
        assert_eq!(c.lines[0].user_format.as_ref().unwrap().indent, MAX_INDENT);
        e.block = BlockKind::Heading(1);
        apply_line_edit(&mut c, 0, &e);
        assert_eq!(c.lines[0].user_format.as_ref().unwrap().indent, 0);
    }

    #[test]
    fn numbered_defaults_to_one() {
        assert_eq!(
            block_from_kind(KIND_NUMBERED, 0, false),
            BlockKind::Numbered(1)
        );
        assert_eq!(
            block_from_kind(KIND_NUMBERED, 4, false),
            BlockKind::Numbered(4)
        );
        assert_eq!(
            block_from_kind(KIND_CHECKBOX, 0, true),
            BlockKind::Checkbox(true)
        );
        assert_eq!(block_from_kind(KIND_H3, 0, false), BlockKind::Heading(3));
        assert_eq!(block_from_kind(KIND_PARA, 0, false), BlockKind::Paragraph);
    }

    #[test]
    fn reset_clears_correction_and_rows_flag_editable() {
        let mut c = cache(vec![line("a", fmt(BlockKind::Paragraph))]);
        let mut e = edit_of(&c, 0);
        e.text = "b".into();
        e.block = BlockKind::Bullet;
        apply_line_edit(&mut c, 0, &e);
        assert_eq!(c.completed_markdown(), "- b");
        let rows = rows_from_cache(&c);
        assert!(rows[0].editable);
        c.clear_line_correction(0);
        assert_eq!(c.completed_markdown(), "a");
        assert!(!apply_line_edit(&mut c, 5, &e));
        assert!(!rows_from_markdown("x")[0].editable);
    }
}
