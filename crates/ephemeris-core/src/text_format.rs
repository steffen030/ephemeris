//! Per-line text formatting shared by recognition, See-text, search and export.
//!
//! Each recognised handwriting line carries a [`LineFormat`] (block role,
//! indent, inline marks). [`lines_to_markdown`] is the single canonical
//! renderer that turns `(text, format)` lines into Obsidian-flavoured markdown;
//! that markdown is what `note.text_content` stores. Search and previews read
//! it back through `strip_inline_markdown` / `markdown_to_blocks`.

use serde::{Deserialize, Serialize};

/// Block-level role of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlockKind {
    #[default]
    Paragraph,
    /// Heading level; values outside `1..=3` are clamped when rendering.
    Heading(u8),
    Bullet,
    /// Ordered list item carrying its number.
    Numbered(u32),
    /// Task item; `true` when checked.
    Checkbox(bool),
}

/// Inline emphasis kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InlineKind {
    Highlight,
    Underline,
    Bold,
}

/// Emphasis over a half-open range of **char** offsets into the line text.
///
/// `start = 0, end = text.chars().count()` marks the whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineMark {
    pub kind: InlineKind,
    pub start: usize,
    pub end: usize,
}

/// Formatting attached to one recognised line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LineFormat {
    #[serde(default)]
    pub block: BlockKind,
    /// List nesting level (2 spaces each in markdown); ignored for non-lists.
    #[serde(default)]
    pub indent: u8,
    #[serde(default)]
    pub marks: Vec<InlineMark>,
    /// Force a blank line before this line even between plain paragraph lines.
    #[serde(default)]
    pub para_break_before: bool,
}

fn is_list(b: BlockKind) -> bool {
    matches!(
        b,
        BlockKind::Bullet | BlockKind::Numbered(_) | BlockKind::Checkbox(_)
    )
}

/// Marker open/close order (outermost first).
const KINDS: [InlineKind; 3] = [
    InlineKind::Bold,
    InlineKind::Highlight,
    InlineKind::Underline,
];

fn open_tag(k: InlineKind) -> &'static str {
    match k {
        InlineKind::Bold => "**",
        InlineKind::Highlight => "==",
        InlineKind::Underline => "<u>",
    }
}

fn close_tag(k: InlineKind) -> &'static str {
    match k {
        InlineKind::Bold => "**",
        InlineKind::Highlight => "==",
        InlineKind::Underline => "</u>",
    }
}

/// Clamp marks to `0..=len`, drop empty ones and merge overlapping / adjacent
/// marks of the same kind. Result is sorted by (kind order, start).
fn normalise_marks(marks: &[InlineMark], len: usize) -> Vec<InlineMark> {
    let mut out: Vec<InlineMark> = Vec::new();
    for kind in KINDS {
        let mut ranges: Vec<(usize, usize)> = marks
            .iter()
            .filter(|m| m.kind == kind)
            .map(|m| (m.start.min(len), m.end.min(len)))
            .filter(|(s, e)| s < e)
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        out.extend(
            merged
                .into_iter()
                .map(|(start, end)| InlineMark { kind, start, end }),
        );
    }
    out
}

/// Render `text` with inline marks. Offsets are chars, so umlauts and other
/// multi-byte characters never split.
fn render_inline(text: &str, marks: &[InlineMark]) -> String {
    render_inline_opts(text, marks, false)
}

/// Escape one char so it is literal inside a StyledText-safe markdown string.
fn push_escaped(out: &mut String, c: char) {
    if matches!(
        c,
        '\\' | '*' | '_' | '`' | '<' | '>' | '[' | ']' | '~' | '&'
    ) {
        out.push('\\');
    }
    out.push(c);
}

/// Render a line body for Slint `StyledText::from_markdown`.
///
/// Only the inline subset Slint parses is emitted: `**bold**` (highlight is
/// folded into bold, which renders as the e-ink highlight), `<u>underline</u>`.
/// Literal markdown-significant characters in `text` are backslash-escaped and
/// block syntax at the start of the line (`#`, `-`, `1.` ...) is neutralised,
/// so recognised text can never produce headings or unsupported tags.
pub fn styled_inline_markdown(text: &str, marks: &[InlineMark]) -> String {
    let folded: Vec<InlineMark> = marks
        .iter()
        .map(|m| InlineMark {
            kind: if m.kind == InlineKind::Highlight {
                InlineKind::Bold
            } else {
                m.kind
            },
            ..*m
        })
        .collect();
    let body = render_inline_opts(text, &folded, true);
    // Neutralise block syntax at the very start (after optional whitespace).
    let t = body.trim_start();
    let lead = body.len() - t.len();
    let mut chars = t.chars();
    let first = chars.next();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    let mut out = String::with_capacity(body.len() + 2);
    out.push_str(&body[..lead]);
    if matches!(first, Some('#' | '-' | '+')) {
        out.push('\\');
        out.push_str(t);
    } else if digits > 0 && matches!(t[digits..].chars().next(), Some('.' | ')')) {
        out.push_str(&t[..digits]);
        out.push('\\');
        out.push_str(&t[digits..]);
    } else {
        out.push_str(t);
    }
    out
}

fn render_inline_opts(text: &str, marks: &[InlineMark], escape: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let marks = normalise_marks(marks, chars.len());
    if marks.is_empty() {
        if escape {
            let mut out = String::with_capacity(text.len());
            text.chars().for_each(|c| push_escaped(&mut out, c));
            return out;
        }
        return text.to_string();
    }
    let active_at = |i: usize| -> [bool; 3] {
        let mut set = [false; 3];
        for m in &marks {
            if m.start <= i && i < m.end {
                set[KINDS.iter().position(|k| *k == m.kind).unwrap_or(0)] = true;
            }
        }
        set
    };
    let mut out = String::with_capacity(text.len() + 8);
    let mut cur = [false; 3];
    for (i, c) in chars.iter().enumerate() {
        let next = active_at(i);
        if next != cur {
            // Close from the innermost kind down to the outermost one that
            // ends here, then reopen survivors: output stays well nested.
            if let Some(j) = (0..3).find(|&j| cur[j] && !next[j]) {
                for k in (j..3).rev() {
                    if cur[k] {
                        out.push_str(close_tag(KINDS[k]));
                        cur[k] = false;
                    }
                }
            }
            for k in 0..3 {
                if next[k] && !cur[k] {
                    out.push_str(open_tag(KINDS[k]));
                    cur[k] = true;
                }
            }
        }
        if escape {
            push_escaped(&mut out, *c);
        } else {
            out.push(*c);
        }
    }
    for (k, on) in KINDS.iter().zip(cur).rev() {
        if on {
            out.push_str(close_tag(*k));
        }
    }
    out
}

/// True when a plain-text line would be parsed as a block construct.
fn starts_block_syntax(s: &str) -> bool {
    let t = s.trim_start();
    if t.starts_with('#')
        || t.starts_with("> ")
        || t.starts_with("```")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || matches!(t, "---" | "***" | "___")
    {
        return true;
    }
    t.find(". ")
        .is_some_and(|d| d > 0 && t[..d].chars().all(|c| c.is_ascii_digit()))
}

/// Render lines as Obsidian-flavoured markdown.
///
/// * Paragraph lines are joined with a single `\n` (so an all-paragraph page
///   reads like the raw recognised text) unless `para_break_before` is set,
///   which inserts a blank line.
/// * A blank line separates different blocks: headings, and any switch between
///   paragraph and list. Consecutive list items use a single `\n`.
/// * Lines with blank text are skipped.
/// * A paragraph line that would otherwise parse as a heading, list, quote,
///   fence or rule gets a leading backslash (understood by
///   `strip_inline_markdown`).
pub fn lines_to_markdown(lines: &[(String, LineFormat)]) -> String {
    let mut out = String::new();
    let mut prev: Option<BlockKind> = None;
    for (text, fmt) in lines {
        if text.trim().is_empty() {
            continue;
        }
        let inline = render_inline(text, &fmt.marks);
        let rendered = match fmt.block {
            BlockKind::Paragraph => {
                if starts_block_syntax(&inline) {
                    format!("\\{inline}")
                } else {
                    inline
                }
            }
            BlockKind::Heading(n) => format!("{} {inline}", "#".repeat(n.clamp(1, 3) as usize)),
            b => {
                let marker = match b {
                    BlockKind::Numbered(n) => format!("{n}. "),
                    BlockKind::Checkbox(true) => "- [x] ".to_string(),
                    BlockKind::Checkbox(false) => "- [ ] ".to_string(),
                    _ => "- ".to_string(),
                };
                format!("{}{marker}{inline}", "  ".repeat(fmt.indent as usize))
            }
        };
        if let Some(p) = prev {
            let tight = (p == BlockKind::Paragraph
                && fmt.block == BlockKind::Paragraph
                && !fmt.para_break_before)
                || (is_list(p) && is_list(fmt.block) && !fmt.para_break_before);
            out.push_str(if tight { "\n" } else { "\n\n" });
        }
        out.push_str(&rendered);
        prev = Some(fmt.block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_search::{markdown_to_blocks, MdBlockKind};

    fn l(t: &str, block: BlockKind) -> (String, LineFormat) {
        (
            t.to_string(),
            LineFormat {
                block,
                ..Default::default()
            },
        )
    }

    fn marked(t: &str, kind: InlineKind, s: usize, e: usize) -> (String, LineFormat) {
        (
            t.to_string(),
            LineFormat {
                marks: vec![InlineMark {
                    kind,
                    start: s,
                    end: e,
                }],
                ..Default::default()
            },
        )
    }

    #[test]
    fn paragraphs_single_newline_and_para_break() {
        let mut b = l("b", BlockKind::Paragraph);
        assert_eq!(
            lines_to_markdown(&[l("a", BlockKind::Paragraph), b.clone()]),
            "a\nb"
        );
        b.1.para_break_before = true;
        assert_eq!(
            lines_to_markdown(&[l("a", BlockKind::Paragraph), b]),
            "a\n\nb"
        );
    }

    #[test]
    fn every_block_kind() {
        let md = lines_to_markdown(&[
            l("H1", BlockKind::Heading(1)),
            l("H2", BlockKind::Heading(2)),
            l("H9", BlockKind::Heading(9)),
            l("bul", BlockKind::Bullet),
            (
                "nest".into(),
                LineFormat {
                    block: BlockKind::Numbered(3),
                    indent: 1,
                    ..Default::default()
                },
            ),
            l("todo", BlockKind::Checkbox(false)),
            l("done", BlockKind::Checkbox(true)),
            l("para", BlockKind::Paragraph),
        ]);
        assert_eq!(
            md,
            "# H1\n\n## H2\n\n### H9\n\n- bul\n  3. nest\n- [ ] todo\n- [x] done\n\npara"
        );
    }

    #[test]
    fn every_inline_kind_and_nesting() {
        assert_eq!(
            lines_to_markdown(&[marked("hello world", InlineKind::Highlight, 6, 11)]),
            "hello ==world=="
        );
        assert_eq!(
            lines_to_markdown(&[marked("abc", InlineKind::Underline, 0, 3)]),
            "<u>abc</u>"
        );
        assert_eq!(
            lines_to_markdown(&[marked("abc", InlineKind::Bold, 1, 99)]),
            "a**bc**"
        );
        let mut f = marked("abcd", InlineKind::Bold, 0, 3);
        f.1.marks.push(InlineMark {
            kind: InlineKind::Underline,
            start: 2,
            end: 4,
        });
        assert_eq!(lines_to_markdown(&[f]), "**ab<u>c</u>**<u>d</u>");
    }

    #[test]
    fn marks_merge_and_clamp() {
        let (t, mut f) = marked("abcdef", InlineKind::Highlight, 0, 2);
        f.marks.push(InlineMark {
            kind: InlineKind::Highlight,
            start: 2,
            end: 4,
        });
        f.marks.push(InlineMark {
            kind: InlineKind::Highlight,
            start: 3,
            end: 100,
        });
        f.marks.push(InlineMark {
            kind: InlineKind::Bold,
            start: 5,
            end: 5,
        });
        assert_eq!(lines_to_markdown(&[(t, f)]), "==abcdef==");
    }

    #[test]
    fn umlaut_char_offsets() {
        assert_eq!(
            lines_to_markdown(&[marked("Grüße süß", InlineKind::Highlight, 2, 5)]),
            "Gr==üße== süß"
        );
        assert_eq!(
            lines_to_markdown(&[marked("äö", InlineKind::Bold, 1, 2)]),
            "ä**ö**"
        );
    }

    #[test]
    fn escapes_leading_syntax() {
        assert_eq!(
            lines_to_markdown(&[l("# not heading", BlockKind::Paragraph)]),
            "\\# not heading"
        );
        assert_eq!(
            lines_to_markdown(&[l("1. nope", BlockKind::Paragraph)]),
            "\\1. nope"
        );
        assert_eq!(
            lines_to_markdown(&[l("- x", BlockKind::Paragraph)]),
            "\\- x"
        );
        assert_eq!(
            lines_to_markdown(&[l("plain - x", BlockKind::Paragraph)]),
            "plain - x"
        );
    }

    #[test]
    fn round_trip_through_markdown_to_blocks() {
        let md = lines_to_markdown(&[
            l("Titel", BlockKind::Heading(1)),
            marked("Wichtig für Üben", InlineKind::Highlight, 0, 8),
            l("# kein Titel", BlockKind::Paragraph),
            l("eins", BlockKind::Bullet),
            l("zwei", BlockKind::Numbered(2)),
            l("Milch", BlockKind::Checkbox(true)),
        ]);
        let blocks = markdown_to_blocks(&md);
        let got: Vec<_> = blocks.iter().map(|b| (b.kind, b.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (MdBlockKind::Heading1, "Titel"),
                (MdBlockKind::Paragraph, "Wichtig für Üben # kein Titel"),
                (MdBlockKind::Bullet, "eins"),
                (MdBlockKind::Numbered, "2. zwei"),
                (MdBlockKind::Bullet, "☑ Milch"),
            ]
        );
    }

    #[test]
    fn blank_lines_skipped() {
        assert_eq!(
            lines_to_markdown(&[
                l("a", BlockKind::Paragraph),
                l("  ", BlockKind::Paragraph),
                l("b", BlockKind::Paragraph)
            ]),
            "a\nb"
        );
    }
}
