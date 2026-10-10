//! A markdown file drawn as a document, in the right sidebar.
//!
//! Two pages show one: the Files tab, for a file Git has no change for, and the
//! Diff, as the rendered counterpart of a changed file's patch. Both hand this
//! module the parsed [`Document`]; it lays the blocks out as the rows of a
//! virtualized list, and the Diff also gives it the file's review notes.
//!
//! A note is anchored to source lines, and a rendered block remembers the lines
//! it came from, so a note sits under the block its last line falls in, and a
//! block's comment button starts a note over that block's lines. The row model
//! is pure so the placement can be tested without a window.

use gpui::{
    AnyElement, Context, ListAlignment, ListState, MouseButton, MouseDownEvent, SharedString, div,
    list, prelude::*, px,
};
use pilus_client::{DiffNote, NoteSide, SshTarget, TextFile};

use super::markdown::{self, Document};
use super::style::{self, RadiusExt};
use super::{PilusView, icon};

/// Whether `path` names a markdown file, by extension.
pub(super) fn is_markdown(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown" | "mkd" | "mdx"
            )
    })
}

/// One row of the document list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Row {
    Block(usize),
    /// A note, by its index in the notes the rows were built from.
    Note(usize),
    /// The note form.
    Composer,
    /// Heads the notes that have no block to sit under.
    Stray,
}

/// The note form's place in the document.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Composing<'a> {
    /// The last source line the new note covers.
    pub(super) end: u32,
    /// The note being edited, whose card the form replaces.
    pub(super) editing: Option<&'a str>,
}

/// The block `line` belongs to: the one that covers it, or the nearest above
/// when it falls in a gap, so a note on a blank line stays with the text it
/// follows. A line before the first block belongs to the first.
pub(super) fn block_for_line(document: &Document, line: u32) -> Option<usize> {
    let blocks = document.blocks();
    if blocks.is_empty() {
        return None;
    }
    Some(
        blocks
            .iter()
            .rposition(|block| block.first_line <= line)
            .unwrap_or(0),
    )
}

/// The rows to draw: every block, each followed by the notes that end in it,
/// and the form where it belongs. Notes on the old side of a diff, or with
/// nowhere to sit, close the list under their own heading.
pub(super) fn rows(
    document: &Document,
    notes: &[DiffNote],
    composing: Option<Composing<'_>>,
) -> Vec<Row> {
    let count = document.blocks().len();
    let mut under: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut stray = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        match (note.side, block_for_line(document, note.end_line)) {
            (NoteSide::New, Some(block)) => under[block].push(index),
            _ => stray.push(index),
        }
    }
    for group in &mut under {
        group.sort_by_key(|&index| (notes[index].start_line, notes[index].end_line, index));
    }
    let editing = composing
        .and_then(|form| form.editing)
        .and_then(|id| notes.iter().position(|note| note.id == id));
    let new_form_block = composing
        .filter(|form| form.editing.is_none())
        .map(|form| block_for_line(document, form.end));
    let mut rows = Vec::new();
    let note_row = |index: usize| {
        if editing == Some(index) {
            Row::Composer
        } else {
            Row::Note(index)
        }
    };
    for (block, group) in under.iter().enumerate() {
        rows.push(Row::Block(block));
        rows.extend(group.iter().map(|&index| note_row(index)));
        if new_form_block == Some(Some(block)) {
            rows.push(Row::Composer);
        }
    }
    if !stray.is_empty() {
        rows.push(Row::Stray);
        rows.extend(stray.iter().map(|&index| note_row(index)));
    }
    // A new note on a file with no blocks has no block to follow.
    if new_form_block == Some(None) {
        rows.push(Row::Composer);
    }
    rows
}

/// A file's blocks and the list that scrolls them.
pub(super) struct MdDoc {
    document: Document,
    pub(super) truncated: bool,
    rows: Vec<Row>,
    list: ListState,
    /// What the rows were last built from; a change rebuilds them.
    built: Option<u64>,
    /// Frames left to scroll the note form into view, as the Diff's own list
    /// does: a form just inserted has no measured height yet.
    reveal_composer: u8,
}

impl MdDoc {
    pub(super) fn new(file: &TextFile) -> Self {
        let document = Document::parse(&file.text);
        let rows = rows(&document, &[], None);
        Self {
            list: ListState::new(rows.len(), ListAlignment::Top, px(600.0)),
            document,
            truncated: file.truncated,
            rows,
            built: None,
            reveal_composer: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.document.blocks().is_empty()
    }

    /// Ask for the note form to be scrolled into view over the next frames.
    pub(super) fn reveal_composer(&mut self) {
        self.reveal_composer = 2;
    }

    /// Bring the rows up to date with the notes and the form. `stamp` stands
    /// for everything they depend on, so an unchanged page rebuilds nothing.
    /// Returns whether another frame is wanted to finish a scroll.
    pub(super) fn sync(
        &mut self,
        stamp: u64,
        notes: &[DiffNote],
        composing: Option<Composing<'_>>,
    ) -> bool {
        if self.built != Some(stamp) {
            let first = self.built.is_none();
            self.rows = rows(&self.document, notes, composing);
            self.built = Some(stamp);
            if first {
                self.list.reset(self.rows.len());
            } else {
                self.list.splice(0..self.list.item_count(), self.rows.len());
            }
        }
        let mut again = false;
        if self.reveal_composer > 0 {
            self.reveal_composer -= 1;
            again = self.reveal_composer > 0;
            if let Some(row) = self.rows.iter().position(|row| *row == Row::Composer) {
                self.list.scroll_to_reveal_item(row);
            }
        }
        again
    }

    /// Scroll by `pixels`, down when positive.
    pub(super) fn scroll_by(&self, pixels: f32) {
        self.list.scroll_by(px(pixels));
    }

    /// Jump to the start or the end.
    pub(super) fn scroll_to_edge(&self, end: bool) {
        match end {
            false => self.list.scroll_to_reveal_item(0),
            true => self
                .list
                .scroll_to_reveal_item(self.rows.len().saturating_sub(1)),
        }
    }
}

/// Whose document a list draws, since each page keeps its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Page {
    Files,
    Diff,
}

impl PilusView {
    /// Where to read a file of `endpoint`: `Ok(None)` on this machine, the SSH
    /// profile for a remote one, or why there is no way to reach it.
    pub(super) fn read_target(
        &self,
        endpoint: &super::EndpointId,
    ) -> Result<Option<SshTarget>, String> {
        match endpoint.machine_id() {
            None => Ok(None),
            Some(id) => self
                .machine_target_by_key(id)
                .map(Some)
                .ok_or_else(|| "This machine has no SSH profile to read files over".to_string()),
        }
    }

    fn md_doc(&self, page: Page) -> Option<&MdDoc> {
        match page {
            Page::Files => self.files_panel.doc_view(),
            Page::Diff => self.diff_panel.md_view(),
        }
    }

    /// The scrolling list of `page`'s document.
    pub(super) fn md_list(&self, page: Page, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = self.md_doc(page) else {
            return div().into_any_element();
        };
        if doc.is_empty() && doc.rows.is_empty() {
            return self.diff_message("This file is empty").into_any_element();
        }
        list(
            doc.list.clone(),
            cx.processor(move |this: &mut PilusView, ix: usize, _window, cx| {
                this.md_row(page, ix, cx)
            }),
        )
        .size_full()
        .into_any_element()
    }

    fn md_row(&self, page: Page, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = self.md_doc(page) else {
            return div().into_any_element();
        };
        let Some(&row) = doc.rows.get(ix) else {
            return div().into_any_element();
        };
        match row {
            Row::Block(block) => self.md_block_row(page, doc, ix, block, cx),
            Row::Note(index) => match self.diff_panel.file_note(index) {
                Some(note) => self.diff_note_card(note, 0.0, cx),
                None => div().into_any_element(),
            },
            Row::Composer => self.diff_composer(0.0, cx),
            Row::Stray => div()
                .px_3()
                .pt_3()
                .child(style::section_label(
                    self.theme,
                    "NOTES ON LINES NOT SHOWN",
                    style::Fonts::resolve(&self.shared_settings).label,
                ))
                .into_any_element(),
        }
    }

    fn md_block_row(
        &self,
        page: Page,
        doc: &MdDoc,
        ix: usize,
        block: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(block) = doc.document.blocks().get(block) else {
            return div().into_any_element();
        };
        let group = SharedString::from(format!("pilus-md-block-{ix}"));
        // Headings take room above them, so a section reads as a section.
        let top = if block.is_heading() { 12.0 } else { 2.0 };
        let mut row = div()
            .id(("pilus-md-row", ix))
            .group(group.clone())
            .relative()
            .w_full()
            .px_3()
            .pt(px(top))
            .pb(px(2.0))
            .child(markdown::render_block(
                block,
                theme,
                fonts,
                &self.shared_settings,
            ));
        if page == Page::Diff {
            let (first, last) = (block.first_line, block.last_line);
            let size = style::scaled(&self.shared_settings, 18.0);
            row = row.child(
                div()
                    .id(("pilus-md-comment", ix))
                    .absolute()
                    .right(px(10.0))
                    .top(px(top))
                    .w(px(size))
                    .h(px(size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .radius_sm()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .opacity(0.0)
                    .group_hover(group, |style| style.opacity(1.0))
                    .child(icon(
                        include_bytes!("../../assets/icons/message-square.svg"),
                        size * 0.65,
                        theme.muted,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.diff_comment_on_lines(first, last, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        row.into_any_element()
    }
}

/// A canned document for the shot harness: every block kind the renderer
/// draws, with line numbers the sample notes point at.
#[cfg(feature = "visual-test")]
pub(super) const SAMPLE: &str = "---
title: Review guide
status: draft
---

# Reviewing a change

Start from the **Diff** tab: pick a file, read the patch, and leave a note on any
line with `n`. Notes stay in the daemon, so an agent can pick them up later.

## Checklist

- [x] Read the patch
- [ ] Run `cargo test`
- Send open notes to an agent

## Key bindings

| Key | Action |
| --- | --- |
| `j` / `k` | Next or previous line |
| `n` | Start a note |
| `a` | Send open notes |

> A note is anchored to source lines, not to the text it was written on.

```sh
cargo run -p pilus -- --probe
```
";

/// Shot-harness entry point: put the canned document on the Files tab.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn files_show_doc_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc("/work/pilus/docs/review-guide.md", SAMPLE, cx);
    }
}

#[cfg(test)]
mod tests {
    use fixtures::note;

    use super::*;

    /// Test notes without pulling in the daemon's JSON.
    mod fixtures {
        use pilus_client::{DiffLayer, DiffNote, NoteKind, NoteSide, NoteState};

        pub(super) fn note(id: &str, side: NoteSide, start: u32, end: u32) -> DiffNote {
            DiffNote {
                id: id.to_string(),
                path: "README.md".to_string(),
                layer: DiffLayer::Worktree,
                side,
                start_line: start,
                end_line: end,
                kind: NoteKind::Question,
                state: NoteState::Open,
                body: String::new(),
                deliveries: 0,
            }
        }
    }

    // Lines: 1 title; 3-4 paragraph; 6-7 list.
    const SOURCE: &str = "# Title\n\nFirst paragraph\nstill first.\n\n- one\n- two\n";

    fn document() -> Document {
        Document::parse(SOURCE)
    }

    #[test]
    fn markdown_is_recognized_by_extension() {
        assert!(is_markdown("docs/README.md"));
        assert!(is_markdown("NOTES.MARKDOWN"));
        assert!(is_markdown("a/b.mdx"));
        assert!(!is_markdown("src/main.rs"));
        assert!(!is_markdown("docs/md"));
        assert!(!is_markdown(".md"));
    }

    #[test]
    fn a_line_belongs_to_the_block_covering_it_or_the_one_above() {
        let document = document();
        assert_eq!(block_for_line(&document, 1), Some(0));
        assert_eq!(block_for_line(&document, 4), Some(1));
        // Line 2 is blank: it stays with the heading above.
        assert_eq!(block_for_line(&document, 2), Some(0));
        assert_eq!(block_for_line(&document, 7), Some(2));
        // Past the end, the last block.
        assert_eq!(block_for_line(&document, 99), Some(2));
        assert_eq!(block_for_line(&Document::parse(""), 1), None);
    }

    #[test]
    fn without_notes_every_block_is_a_row() {
        assert_eq!(
            rows(&document(), &[], None),
            vec![Row::Block(0), Row::Block(1), Row::Block(2)]
        );
    }

    #[test]
    fn a_note_sits_under_the_block_its_last_line_ends_in() {
        let notes = [
            note("a", NoteSide::New, 3, 4),
            // Starts in the paragraph, ends in the list: it belongs to the list.
            note("b", NoteSide::New, 4, 6),
        ];
        assert_eq!(
            rows(&document(), &notes, None),
            vec![
                Row::Block(0),
                Row::Block(1),
                Row::Note(0),
                Row::Block(2),
                Row::Note(1),
            ]
        );
    }

    #[test]
    fn notes_under_one_block_follow_the_source_order() {
        let notes = [
            note("late", NoteSide::New, 4, 4),
            note("early", NoteSide::New, 3, 3),
        ];
        assert_eq!(
            rows(&document(), &notes, None),
            vec![
                Row::Block(0),
                Row::Block(1),
                Row::Note(1),
                Row::Note(0),
                Row::Block(2),
            ]
        );
    }

    #[test]
    fn old_side_notes_close_the_list_under_a_heading() {
        let notes = [note("gone", NoteSide::Old, 3, 3)];
        assert_eq!(
            rows(&document(), &notes, None),
            vec![
                Row::Block(0),
                Row::Block(1),
                Row::Block(2),
                Row::Stray,
                Row::Note(0)
            ]
        );
    }

    #[test]
    fn the_new_note_form_follows_its_block_and_that_blocks_notes() {
        let notes = [note("a", NoteSide::New, 3, 4)];
        let form = Composing {
            end: 4,
            editing: None,
        };
        assert_eq!(
            rows(&document(), &notes, Some(form)),
            vec![
                Row::Block(0),
                Row::Block(1),
                Row::Note(0),
                Row::Composer,
                Row::Block(2)
            ]
        );
    }

    #[test]
    fn editing_swaps_the_form_in_for_the_notes_card() {
        let notes = [note("a", NoteSide::New, 3, 4)];
        let form = Composing {
            end: 4,
            editing: Some("a"),
        };
        assert_eq!(
            rows(&document(), &notes, Some(form)),
            vec![Row::Block(0), Row::Block(1), Row::Composer, Row::Block(2)]
        );
    }

    #[test]
    fn a_form_for_a_note_that_is_gone_draws_nothing() {
        let form = Composing {
            end: 4,
            editing: Some("deleted"),
        };
        assert!(!rows(&document(), &[], Some(form)).contains(&Row::Composer));
    }

    #[test]
    fn an_empty_file_still_has_somewhere_for_the_form() {
        let empty = Document::parse("");
        let form = Composing {
            end: 1,
            editing: None,
        };
        assert_eq!(rows(&empty, &[], Some(form)), vec![Row::Composer]);
    }
}
