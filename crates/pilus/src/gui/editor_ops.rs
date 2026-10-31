//! Line editing for the Files tab's editor: indent, comment, move, copy and
//! delete whole lines, the chords a code editor is expected to have.
//!
//! The window binds each chord in the `MdEditor` key context (see
//! `app_key_bindings`), and the editor div hands the action to its field as an
//! [`Op`]. The work here is pure: it reads the text and the selection and
//! returns one [`Change`], which the field applies as a single undo step. So
//! every operation is testable without a window, and a read-only field can
//! refuse the ones that edit before any text moves.

use std::ops::Range;

gpui::actions!(
    editor,
    [
        /// Indent the selected lines, or insert an indent at the caret.
        Indent,
        /// Take one indent off the selected lines.
        Outdent,
        /// Comment or uncomment the selected lines.
        ToggleComment,
        /// Select the caret's line; again, the next one too.
        SelectLine,
        /// Delete the selected lines.
        DeleteLine,
        /// Swap the selected lines with the line above.
        MoveLineUp,
        /// Swap the selected lines with the line below.
        MoveLineDown,
        /// Copy the selected lines above themselves.
        CopyLineUp,
        /// Copy the selected lines below themselves.
        CopyLineDown,
        /// Start a new line under the caret's, at its indent.
        NewLineBelow,
        /// Start a new line over the caret's, at its indent.
        NewLineAbove
    ]
);

/// How many lines [`indent_unit`] reads before it decides.
const SCAN_LINES: usize = 2000;
/// How wide a tab counts when an outdent takes spaces off.
const TAB_WIDTH: usize = 4;

/// One editing operation, as the chord that asked for it names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Op {
    Indent,
    Outdent,
    ToggleComment,
    SelectLine,
    DeleteLine,
    MoveUp,
    MoveDown,
    CopyUp,
    CopyDown,
    NewLineBelow,
    NewLineAbove,
}

impl Op {
    /// Whether the operation changes the text, so a read-only field refuses it.
    pub(super) fn edits(self) -> bool {
        !matches!(self, Self::SelectLine)
    }
}

/// What an operation does: `range` of the old text gives way to `insert`, and
/// the selection lands at `anchor`..`head` in the new text. An empty range
/// with nothing to insert only moves the selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Change {
    pub(super) range: Range<usize>,
    pub(super) insert: String,
    pub(super) anchor: usize,
    pub(super) head: usize,
}

/// What the file says about itself: how it indents, and how it comments a line.
pub(super) struct Style<'a> {
    pub(super) unit: &'a str,
    pub(super) comment: Option<&'a str>,
}

/// Run `op` over `text` with the selection at `anchor`..`head`. `None` when it
/// has nothing to do, such as moving the first line up.
pub(super) fn apply(
    op: Op,
    text: &str,
    anchor: usize,
    head: usize,
    style: &Style<'_>,
) -> Option<Change> {
    let anchor = floor(text, anchor);
    let head = floor(text, head);
    let sel = anchor.min(head)..anchor.max(head);
    match op {
        Op::Indent => indent(text, sel, anchor, head, style.unit),
        Op::Outdent => outdent(text, sel, anchor, head, style.unit),
        Op::ToggleComment => toggle_comment(text, sel, anchor, head, style.comment?),
        Op::SelectLine => Some(select_line(text, sel)),
        Op::DeleteLine => delete_line(text, sel),
        Op::MoveUp => move_up(text, sel, anchor, head),
        Op::MoveDown => move_down(text, sel, anchor, head),
        Op::CopyUp => Some(copy_up(text, sel, anchor, head)),
        Op::CopyDown => Some(copy_down(text, sel, anchor, head)),
        Op::NewLineBelow => Some(new_line(text, head, true)),
        Op::NewLineAbove => Some(new_line(text, head, false)),
    }
}

/// What Enter inserts at `at`: a newline and the indent of the line it breaks,
/// so the new line starts where the old one did.
pub(super) fn newline(text: &str, at: usize) -> String {
    let at = floor(text, at);
    let start = line_start(text, at);
    let indent = leading(&text[start..]).min(at - start);
    format!("\n{}", &text[start..start + indent])
}

/// How `text` indents: a tab when more lines open with tabs than spaces, and
/// otherwise two spaces or four, from the narrowest indent it uses.
pub(super) fn indent_unit(text: &str) -> String {
    let (mut tabs, mut spaces, mut narrowest) = (0usize, 0usize, usize::MAX);
    for line in text.lines().take(SCAN_LINES) {
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let width = line.len() - line.trim_start_matches(' ').len();
        // One space is a block comment's ` *`, not an indent.
        if width >= 2 && width < line.len() {
            spaces += 1;
            narrowest = narrowest.min(width);
        }
    }
    if tabs > spaces {
        return "\t".to_string();
    }
    " ".repeat(if narrowest == 2 { 2 } else { 4 })
}

fn floor(text: &str, at: usize) -> usize {
    text.floor_char_boundary(at.min(text.len()))
}

/// The first byte of the line holding `at`.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |newline| newline + 1)
}

/// The newline that ends the line holding `at`, or the end of the text.
fn line_end(text: &str, at: usize) -> usize {
    text[at..]
        .find('\n')
        .map_or(text.len(), |newline| at + newline)
}

/// Bytes of spaces and tabs a line opens with.
fn leading(line: &str) -> usize {
    let line = line.split('\n').next().unwrap_or_default();
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The whole lines a selection touches, without the last one's newline. A
/// selection that ends at the start of a line leaves that line out, as a
/// selection of whole lines does.
fn lines(text: &str, sel: &Range<usize>) -> Range<usize> {
    let start = line_start(text, sel.start);
    let last = if sel.end > sel.start && sel.end == line_start(text, sel.end) {
        sel.end - 1
    } else {
        sel.end
    };
    start..line_end(text, last.max(start))
}

/// One line's edit: `remove` bytes at `at`, counted from the line's start,
/// give way to `insert`.
struct LineEdit {
    at: usize,
    remove: usize,
    insert: String,
}

/// Where one line of an [`edit_lines`] run started before and after, and
/// what was done to it, as (at, removed, inserted) byte counts.
#[derive(Clone, Copy)]
struct Moved {
    old: usize,
    new: usize,
    done: Option<(usize, usize, usize)>,
}

/// Edit each selected line with `edit`, carrying the selection along with the
/// text it covered. `None` when no line changed.
fn edit_lines(
    text: &str,
    sel: &Range<usize>,
    anchor: usize,
    head: usize,
    edit: impl Fn(&str) -> Option<LineEdit>,
) -> Option<Change> {
    let span = lines(text, sel);
    let mut out = String::with_capacity(span.len() + 16);
    let mut map: Vec<Moved> = Vec::new();
    let mut old = span.start;
    let mut changed = false;
    for (ix, line) in text[span.clone()].split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let new_start = span.start + out.len();
        let done = edit(line).map(|line_edit| {
            out.push_str(&line[..line_edit.at]);
            out.push_str(&line_edit.insert);
            out.push_str(&line[line_edit.at + line_edit.remove..]);
            changed = true;
            (line_edit.at, line_edit.remove, line_edit.insert.len())
        });
        if done.is_none() {
            out.push_str(line);
        }
        map.push(Moved {
            old,
            new: new_start,
            done,
        });
        old += line.len() + 1;
    }
    if !changed {
        return None;
    }
    let place = |at: usize| {
        let Some(line) = map.iter().rev().find(|line| line.old <= at).or(map.first()) else {
            return at;
        };
        let column = at.saturating_sub(line.old);
        let column = match line.done {
            Some((edit_at, remove, insert)) if column >= edit_at => {
                if column < edit_at + remove {
                    edit_at + insert
                } else {
                    column - remove + insert
                }
            }
            _ => column,
        };
        line.new + column
    };
    Some(Change {
        range: span,
        insert: out,
        anchor: place(anchor),
        head: place(head),
    })
}

fn indent(text: &str, sel: Range<usize>, anchor: usize, head: usize, unit: &str) -> Option<Change> {
    // A caret, or a selection inside one line, takes an indent in its place:
    // a tab, or spaces to the next stop.
    if !text[sel.clone()].contains('\n') {
        let insert = if unit == "\t" {
            "\t".to_string()
        } else {
            let column = sel.start - line_start(text, sel.start);
            let width = unit.len().max(1);
            " ".repeat(width - column % width)
        };
        let at = sel.start + insert.len();
        return Some(Change {
            range: sel,
            insert,
            anchor: at,
            head: at,
        });
    }
    edit_lines(text, &sel, anchor, head, |line| {
        (!line.is_empty()).then(|| LineEdit {
            at: 0,
            remove: 0,
            insert: unit.to_string(),
        })
    })
}

fn outdent(
    text: &str,
    sel: Range<usize>,
    anchor: usize,
    head: usize,
    unit: &str,
) -> Option<Change> {
    let width = if unit == "\t" { TAB_WIDTH } else { unit.len() };
    edit_lines(text, &sel, anchor, head, |line| {
        let remove = if line.starts_with('\t') {
            1
        } else {
            (line.len() - line.trim_start_matches(' ').len()).min(width)
        };
        (remove > 0).then(|| LineEdit {
            at: 0,
            remove,
            insert: String::new(),
        })
    })
}

fn toggle_comment(
    text: &str,
    sel: Range<usize>,
    anchor: usize,
    head: usize,
    token: &str,
) -> Option<Change> {
    let span = lines(text, &sel);
    let filled: Vec<&str> = text[span]
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .collect();
    // Every line already commented means uncomment; any line not, comment
    // them all, so a mixed selection never comes out half and half.
    let commented = filled
        .iter()
        .all(|line| line[leading(line)..].starts_with(token));
    // The marks line up at the shallowest indent, as an editor puts them.
    let indent = filled.iter().map(|line| leading(line)).min()?;
    edit_lines(text, &sel, anchor, head, |line| {
        if line.trim().is_empty() {
            return None;
        }
        if commented {
            let at = leading(line);
            let rest = &line[at + token.len()..];
            Some(LineEdit {
                at,
                remove: token.len() + usize::from(rest.starts_with(' ')),
                insert: String::new(),
            })
        } else {
            Some(LineEdit {
                at: indent,
                remove: 0,
                insert: format!("{token} "),
            })
        }
    })
}

fn select_line(text: &str, sel: Range<usize>) -> Change {
    let span = lines(text, &sel);
    let mut target = span.start..(span.end + 1).min(text.len());
    // Whole lines already selected: take the next one as well.
    if sel == target && target.end < text.len() {
        target.end = (line_end(text, target.end) + 1).min(text.len());
    }
    Change {
        range: target.start..target.start,
        insert: String::new(),
        anchor: target.start,
        head: target.end,
    }
}

fn delete_line(text: &str, sel: Range<usize>) -> Option<Change> {
    let span = lines(text, &sel);
    // The line's own newline goes with it; the last line takes the one before.
    let (range, caret) = if span.end < text.len() {
        (span.start..span.end + 1, span.start)
    } else if span.start > 0 {
        (span.start - 1..span.end, line_start(text, span.start - 1))
    } else {
        (span.clone(), 0)
    };
    if range.is_empty() {
        return None;
    }
    Some(Change {
        range,
        insert: String::new(),
        anchor: caret,
        head: caret,
    })
}

fn move_up(text: &str, sel: Range<usize>, anchor: usize, head: usize) -> Option<Change> {
    let span = lines(text, &sel);
    if span.start == 0 {
        return None;
    }
    let above_start = line_start(text, span.start - 1);
    let above = &text[above_start..span.start - 1];
    let shift = above.len() + 1;
    Some(Change {
        insert: format!("{}\n{above}", &text[span.clone()]),
        range: above_start..span.end,
        anchor: anchor - shift,
        head: head - shift,
    })
}

fn move_down(text: &str, sel: Range<usize>, anchor: usize, head: usize) -> Option<Change> {
    let span = lines(text, &sel);
    if span.end >= text.len() {
        return None;
    }
    let below_end = line_end(text, span.end + 1);
    let below = &text[span.end + 1..below_end];
    let shift = below.len() + 1;
    Some(Change {
        insert: format!("{below}\n{}", &text[span.clone()]),
        range: span.start..below_end,
        anchor: anchor + shift,
        head: head + shift,
    })
}

/// The copy goes above, and the selection stays on the upper of the two.
fn copy_up(text: &str, sel: Range<usize>, anchor: usize, head: usize) -> Change {
    let span = lines(text, &sel);
    Change {
        insert: format!("{}\n", &text[span.clone()]),
        range: span.start..span.start,
        anchor,
        head,
    }
}

/// The copy goes below, and the selection follows it there.
fn copy_down(text: &str, sel: Range<usize>, anchor: usize, head: usize) -> Change {
    let span = lines(text, &sel);
    let shift = span.len() + 1;
    Change {
        insert: format!("\n{}", &text[span.clone()]),
        range: span.end..span.end,
        anchor: anchor + shift,
        head: head + shift,
    }
}

fn new_line(text: &str, head: usize, below: bool) -> Change {
    let start = line_start(text, head);
    let indent = &text[start..start + leading(&text[start..])];
    let (at, insert, caret) = if below {
        let at = line_end(text, head);
        let insert = format!("\n{indent}");
        let caret = at + insert.len();
        (at, insert, caret)
    } else {
        (start, format!("{indent}\n"), start + indent.len())
    };
    Change {
        range: at..at,
        insert,
        anchor: caret,
        head: caret,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACES: Style<'static> = Style {
        unit: "    ",
        comment: Some("//"),
    };

    /// Run `op` on `text`, whose selection is marked `[` (anchor) and `|`
    /// (head), or a lone `|` for a caret, and give the result marked the same.
    fn run(op: Op, marked: &str) -> String {
        let (text, anchor, head) = unmark(marked);
        match apply(op, &text, anchor, head, &SPACES) {
            Some(change) => {
                let mut out = text.clone();
                out.replace_range(change.range, &change.insert);
                mark(&out, change.anchor, change.head)
            }
            None => marked.to_string(),
        }
    }

    fn unmark(marked: &str) -> (String, usize, usize) {
        let head = marked.find('|').unwrap_or(marked.len());
        let anchor = marked.find('[');
        let text = marked.replace(['[', '|'], "");
        let (anchor, head) = match anchor {
            Some(anchor) if anchor < head => (anchor, head - 1),
            Some(anchor) => (anchor - 1, head),
            None => (head, head),
        };
        (text, anchor, head)
    }

    fn mark(text: &str, anchor: usize, head: usize) -> String {
        let mut out = text.to_string();
        if anchor == head {
            out.insert(head.min(out.len()), '|');
            return out;
        }
        // Insert the later mark first so the earlier offset stays valid.
        if anchor > head {
            out.insert(anchor.min(out.len()), '[');
            out.insert(head, '|');
        } else {
            out.insert(head.min(out.len()), '|');
            out.insert(anchor, '[');
        }
        out
    }

    #[test]
    fn a_caret_indents_to_the_next_stop() {
        assert_eq!(run(Op::Indent, "ab|c"), "ab  |c");
        assert_eq!(run(Op::Indent, "|abc"), "    |abc");
    }

    #[test]
    fn a_selection_over_lines_indents_each_and_keeps_its_text() {
        assert_eq!(
            run(Op::Indent, "[one\n\ntwo|\nthree"),
            "    [one\n\n    two|\nthree"
        );
    }

    #[test]
    fn outdent_takes_one_level_off_each_line() {
        assert_eq!(run(Op::Outdent, "      a[b\n  c|d\ne"), "  a[b\nc|d\ne");
        assert_eq!(run(Op::Outdent, "\t\tx|"), "\tx|");
        assert_eq!(run(Op::Outdent, "x|"), "x|");
    }

    #[test]
    fn comments_line_up_at_the_shallowest_indent_and_come_off_again() {
        let commented = run(Op::ToggleComment, "  [a\n\n    b|");
        assert_eq!(commented, "  // [a\n\n  //   b|");
        assert_eq!(run(Op::ToggleComment, &commented), "  [a\n\n    b|");
    }

    #[test]
    fn a_half_commented_selection_comments_every_line() {
        assert_eq!(run(Op::ToggleComment, "[// a\nb|"), "// [// a\n// b|");
    }

    #[test]
    fn a_file_with_no_line_comment_is_left_alone() {
        let (text, anchor, head) = unmark("a|");
        let style = Style {
            unit: "  ",
            comment: None,
        };
        assert_eq!(apply(Op::ToggleComment, &text, anchor, head, &style), None);
    }

    #[test]
    fn select_line_takes_the_line_then_the_next() {
        assert_eq!(run(Op::SelectLine, "a\nb|c\nd"), "a\n[bc\n|d");
        assert_eq!(run(Op::SelectLine, "a\n[bc\n|d"), "a\n[bc\nd|");
    }

    #[test]
    fn delete_line_takes_its_newline() {
        assert_eq!(run(Op::DeleteLine, "a\nb|c\nd"), "a\n|d");
        assert_eq!(run(Op::DeleteLine, "a\nb|c"), "|a");
        assert_eq!(run(Op::DeleteLine, "o|nly"), "|");
    }

    #[test]
    fn lines_move_past_their_neighbours_with_the_selection() {
        assert_eq!(run(Op::MoveUp, "a\nb|c\nd"), "b|c\na\nd");
        assert_eq!(run(Op::MoveDown, "a\nb|c\nd"), "a\nd\nb|c");
        assert_eq!(run(Op::MoveUp, "a|\nb"), "a|\nb");
        assert_eq!(run(Op::MoveDown, "a\nb|"), "a\nb|");
        // The selection's end runs past the text and is held at its end.
        assert_eq!(run(Op::MoveDown, "[a\nb\n|c"), "c\n[a\nb|");
    }

    #[test]
    fn copies_land_above_or_below() {
        assert_eq!(run(Op::CopyUp, "a\nb|\nc"), "a\nb|\nb\nc");
        assert_eq!(run(Op::CopyDown, "a\nb|\nc"), "a\nb\nb|\nc");
    }

    #[test]
    fn new_lines_keep_the_indent() {
        assert_eq!(run(Op::NewLineBelow, "  a|b\nc"), "  ab\n  |\nc");
        assert_eq!(run(Op::NewLineAbove, "x\n  a|b"), "x\n  |\n  ab");
        assert_eq!(newline("    let a", 9), "\n    ");
        // A break inside the indent keeps only what is before it.
        assert_eq!(newline("    a", 2), "\n  ");
    }

    #[test]
    fn the_indent_unit_follows_the_file() {
        assert_eq!(indent_unit("a\n\tb\n\tc"), "\t");
        assert_eq!(indent_unit("a\n  b\n    c"), "  ");
        assert_eq!(indent_unit("a\n    b\n /* c */"), "    ");
        assert_eq!(indent_unit("plain"), "    ");
    }
}
