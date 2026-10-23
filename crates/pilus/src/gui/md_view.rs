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
//!
//! The text can be selected with the pointer (see `md_select`). A selection
//! offers to be copied, or sent to a running agent as a quoted excerpt.

use gpui::{
    AnyElement, ClipboardItem, Context, ListAlignment, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, SharedString, div, list, prelude::*, px,
};
use pilus_client::{DiffNote, NoteSide, SshTarget, TextFile};

use super::markdown::{self, Document};
use super::md_select::{self, Hits, Leaves, Selection, TextPoint};
use super::style::{self, RadiusExt};
use super::toast::ToastKind;
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

/// Whether `path` names an MDX file, which is markdown that also holds
/// `import`/`export` lines and JSX.
pub(super) fn is_mdx(path: &str) -> bool {
    is_markdown(path) && path.to_ascii_lowercase().ends_with(".mdx")
}

/// The longest file the editor opens.
const EDIT_LIMIT: usize = 1024 * 1024;

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
    /// The text the blocks were parsed from, which the editor starts from.
    source: String,
    /// Whether the file is MDX, so an edit is parsed as the same.
    mdx: bool,
    pub(super) truncated: bool,
    rows: Vec<Row>,
    list: ListState,
    /// What the rows were last built from; a change rebuilds them.
    built: Option<u64>,
    /// The text changed under the rows, which rebuild without losing the
    /// scroll position.
    stale: bool,
    /// Frames left to scroll the note form into view, as the Diff's own list
    /// does: a form just inserted has no measured height yet.
    reveal_composer: u8,
    selection: Option<Selection>,
    /// The pointer is down and moving the selection's end.
    dragging: bool,
    /// Where the text on screen landed, for mapping the pointer onto it.
    hits: Hits,
}

impl MdDoc {
    /// `path` is the file's name, which says whether it is read as markdown,
    /// and as MDX. Any other file is held for the editor alone: its text is not
    /// parsed, since there is no document to draw from it.
    pub(super) fn new(file: &TextFile, path: &str) -> Self {
        let mdx = is_mdx(path);
        let document = if is_markdown(path) {
            Document::parse(&file.text, mdx)
        } else {
            Document::parse("", false)
        };
        let rows = rows(&document, &[], None);
        Self {
            list: ListState::new(rows.len(), ListAlignment::Top, px(600.0)),
            document,
            source: file.text.clone(),
            mdx,
            truncated: file.truncated,
            rows,
            built: None,
            stale: false,
            reveal_composer: 0,
            selection: None,
            dragging: false,
            hits: Hits::default(),
        }
    }

    pub(super) fn source(&self) -> &str {
        &self.source
    }

    /// Whether the editor may take this file. A truncated read holds only the
    /// start of it, and saving that would cut the rest off the disk; a very
    /// long text makes the field lay out too much on every frame.
    pub(super) fn editable(&self) -> bool {
        !self.truncated && self.source.len() <= EDIT_LIMIT
    }

    /// Show `text` in place of the document, keeping the scroll position so a
    /// live preview does not jump back to the top with each keystroke.
    pub(super) fn replace_source(&mut self, text: String) {
        self.document = Document::parse(&text, self.mdx);
        self.source = text;
        self.selection = None;
        self.dragging = false;
        self.stale = true;
    }

    /// The selection, once it covers some text.
    fn selected(&self) -> Option<Selection> {
        self.selection.filter(|selection| !selection.is_collapsed())
    }

    /// Start a selection at `point`, or at the word or paragraph around it for
    /// a double or triple click.
    fn press(&mut self, point: Option<TextPoint>, clicks: usize) {
        let Some(point) = point else {
            self.selection = None;
            self.dragging = false;
            return;
        };
        self.selection = Some(match (clicks, self.document.leaf_text(point.leaf)) {
            (2, Some(text)) => {
                let word = md_select::word_range(text, point.offset);
                Selection {
                    anchor: TextPoint {
                        offset: word.start,
                        ..point
                    },
                    head: TextPoint {
                        offset: word.end,
                        ..point
                    },
                }
            }
            (3.., Some(text)) => Selection {
                anchor: TextPoint { offset: 0, ..point },
                head: TextPoint {
                    offset: text.len(),
                    ..point
                },
            },
            _ => Selection::collapsed(point),
        });
        // Only a single press goes on to drag: the end of a word selection
        // would otherwise jump to wherever the pointer first moves.
        self.dragging = clicks < 2;
    }

    /// Move the selection's end to `point`.
    fn drag_to(&mut self, point: Option<TextPoint>) {
        if let (Some(selection), Some(point)) = (self.selection.as_mut(), point) {
            selection.head = point;
        }
    }

    /// The pointer came up: a click that selected nothing clears the selection.
    fn release(&mut self) {
        self.dragging = false;
        if self
            .selection
            .is_some_and(|selection| selection.is_collapsed())
        {
            self.selection = None;
        }
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.dragging = false;
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
        if self.built != Some(stamp) || self.stale {
            let first = self.built.is_none();
            self.rows = rows(&self.document, notes, composing);
            self.built = Some(stamp);
            self.stale = false;
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

    pub(super) fn md_doc(&self, page: Page) -> Option<&MdDoc> {
        match page {
            Page::Files => self.files_panel.doc_view(),
            Page::Diff => self.diff_panel.md_view(),
        }
    }

    pub(super) fn md_doc_mut(&mut self, page: Page) -> Option<&mut MdDoc> {
        match page {
            Page::Files => self.files_panel.doc_view_mut(),
            Page::Diff => self.diff_panel.md_view_mut(),
        }
    }

    /// The scrolling list of `page`'s document, which takes the pointer to
    /// select text and floats the selection's actions over its foot.
    pub(super) fn md_list(&self, page: Page, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = self.md_doc(page) else {
            return div().into_any_element();
        };
        if doc.is_empty() && doc.rows.is_empty() {
            return self.diff_message("This file is empty").into_any_element();
        }
        // Rows report their text again as they are drawn.
        doc.hits.clear();
        let id = match page {
            Page::Files => "pilus-md-files",
            Page::Diff => "pilus-md-diff",
        };
        div()
            .id(id)
            .relative()
            .size_full()
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if let Some(doc) = this.md_doc_mut(page) {
                        let point = doc.hits.point_at(event.position, false);
                        doc.press(point, event.click_count);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let Some(doc) = this.md_doc_mut(page).filter(|doc| doc.dragging) else {
                    return;
                };
                // A button let go outside the window never reports its release.
                if event.pressed_button == Some(MouseButton::Left) {
                    let point = doc.hits.point_at(event.position, true);
                    doc.drag_to(point);
                } else {
                    doc.release();
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| this.md_release(page, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| this.md_release(page, cx)),
            )
            .child(
                list(
                    doc.list.clone(),
                    cx.processor(move |this: &mut PilusView, ix: usize, _window, cx| {
                        this.md_row(page, ix, cx)
                    }),
                )
                .size_full(),
            )
            .children(self.md_overlays(page, cx))
            .into_any_element()
    }

    fn md_release(&mut self, page: Page, cx: &mut Context<Self>) {
        if let Some(doc) = self.md_doc_mut(page).filter(|doc| doc.dragging) {
            doc.release();
            cx.notify();
        }
    }

    /// The open document's path, which a sent excerpt names.
    pub(super) fn md_path(&self, page: Page) -> Option<String> {
        match page {
            Page::Files => self.files_panel.doc_path().map(str::to_string),
            Page::Diff => self.diff_panel.selected_key().map(|(_, path)| path),
        }
    }

    pub(super) fn md_copy(&mut self, page: Page, cx: &mut Context<Self>) {
        let Some(text) = self
            .md_doc(page)
            .and_then(|doc| Some(doc.document.selected_text(&doc.selected()?)))
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.toast(ToastKind::Success, "Copied");
        cx.notify();
    }

    /// How many words the selection covers, once it covers some text.
    pub(super) fn md_selected_words(&self, page: Page) -> Option<usize> {
        let doc = self.md_doc(page).filter(|doc| !doc.dragging)?;
        let selection = doc.selected()?;
        Some(
            doc.document
                .selected_text(&selection)
                .split_whitespace()
                .count(),
        )
    }

    /// The selected text and the source lines it spans.
    pub(super) fn md_selected_excerpt(&self, page: Page) -> Option<(String, Option<(u32, u32)>)> {
        let doc = self.md_doc(page)?;
        let selection = doc.selected()?;
        Some((
            doc.document.selected_text(&selection),
            doc.document.selected_lines(&selection),
        ))
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
        let block_ix = block;
        let Some(block) = doc.document.blocks().get(block) else {
            return div().into_any_element();
        };
        let group = SharedString::from(format!("pilus-md-block-{ix}"));
        // Headings take room above them, so a section reads as a section; every
        // other block leaves a paragraph's gap below.
        let (before, after) = block.spacing();
        let unit = fonts.body / 13.0;
        let top = if ix == 0 { 12.0 } else { before * unit };
        let bottom = after * unit;
        let leaves = Leaves::new(
            block_ix,
            doc.selection,
            theme.selection_bg.into(),
            doc.hits.clone(),
        );
        let mut row = div()
            .id(("pilus-md-row", ix))
            .group(group.clone())
            .relative()
            .w_full()
            .px_3()
            .pt(px(top))
            .pb(px(bottom))
            .child(markdown::render_block(
                block,
                theme,
                fonts,
                &self.shared_settings,
                &leaves,
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
line with `n`. Notes stay in the daemon, so an agent can pick them up later, and
a note is anchored to *source lines*, so it survives the file being edited around
it. Long paragraphs wrap at a comfortable measure, with room between the lines.

## Checklist

- [x] Read the patch
- [ ] Run `cargo test`
- Send open notes to an agent
  - Pick the agent working in the same checkout
  - Or any other live agent

### Order

1. Read the description.
2. Walk the files, largest first.
3. Leave notes, then send them.

## Key bindings

| Key | Action |
| --- | --- |
| `j` / `k` | Next or previous line |
| `n` | Start a note |
| `a` | Send open notes |

> A note is anchored to source lines, not to the text it was written on.

```sh
cargo run -p pilus -- --probe
cargo run -p pilus --features visual-test -- --ui-shots-main
```

---

That is all there is to it.
";

/// A canned source file for the editor.
#[cfg(feature = "visual-test")]
const CODE_SAMPLE: &str = "//! Saving a text file back where it was read.

use std::path::Path;

/// Most bytes the editor opens.
const LIMIT: usize = 1024 * 1024;

pub fn write_text(path: &Path, text: &str) -> Result<(), String> {
    if text.len() > LIMIT {
        return Err(format!(\"{} bytes is too long\", text.len()));
    }
    std::fs::write(path, text).map_err(|error| error.to_string())
}
";

/// A page of diagrams: two that draw, and one that does not parse and stays
/// code.
#[cfg(feature = "visual-test")]
const DIAGRAM_SAMPLE: &str = "# Data flow

The window folds the daemon's frames into a surface, then paints it.

```mermaid
flowchart LR
  D[Daemon] -->|frames| S[Surface]
  S --> P{Dirty?}
  P -->|yes| G[Paint]
  P -->|no| I[Idle]
```

Who talks to whom:

```mermaid
sequenceDiagram
  participant U as User
  participant P as pilus
  participant D as Daemon
  U->>P: open file
  P->>D: attach
  D-->>P: frame
  Note right of P: fold into Surface
```

A diagram that does not parse stays readable:

```mermaid
this is not a diagram
```
";

/// A canned MDX page: front matter, ESM, components, and an expression.
#[cfg(feature = "visual-test")]
const MDX_SAMPLE: &str = "---
title: Launch notes
draft: true
---

import { Chart } from '../components/chart'
export const meta = { updated: 2026 }

# Launch notes

Numbers below come from the {meta.updated} run, and a <Badge /> marks drafts.

<Chart year=\"2026\" kind=\"bar\" />

<Callout type=\"warning\">

Check the **rollback** plan before shipping.

</Callout>

```ts
const ready: boolean = true
```
";

/// Shot-harness entry point: put the canned document on the Files tab.
#[cfg(feature = "visual-test")]
impl PilusView {
    /// Select from the opening paragraph into the checklist, as a drag would,
    /// which offers to copy it or write a note on it.
    pub(super) fn files_show_doc_selection(&mut self, cx: &mut Context<Self>) {
        use super::md_select::LeafId;

        if let Some(doc) = self.files_panel.doc_view_mut() {
            let at = |block, leaf, offset| TextPoint {
                leaf: LeafId { block, leaf },
                offset,
            };
            doc.selection = Some(Selection {
                anchor: at(2, 0, 40),
                head: at(4, 1, 7),
            });
        }
        cx.notify();
    }

    pub(super) fn files_show_doc_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc("/work/pilus/docs/review-guide.md", SAMPLE, cx);
    }

    /// The sample document with its source beside it, as the editor shows it
    /// once the column is wide enough.
    pub(super) fn files_split_doc_sample(&mut self, cx: &mut Context<Self>) {
        self.files_edit_doc_sample(cx);
        self.files_set_split();
        cx.notify();
    }

    /// The page of diagrams, drawn.
    pub(super) fn files_show_diagram_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc("/work/pilus/docs/data-flow.md", DIAGRAM_SAMPLE, cx);
    }

    /// A Rust file in the editor, as a click on it in the tree opens it.
    pub(super) fn files_edit_code_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc(
            "/work/pilus/crates/pilus-client/src/lib.rs",
            CODE_SAMPLE,
            cx,
        );
        self.files_begin_edit(cx);
        cx.notify();
    }

    /// An MDX page with its source beside it.
    pub(super) fn files_split_mdx_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc("/work/pilus/site/docs/launch.mdx", MDX_SAMPLE, cx);
        self.files_begin_edit(cx);
        self.files_set_split();
        cx.notify();
    }

    /// The sample document in the editor, with a line typed after the
    /// heading so the unsaved mark and the Save button show.
    pub(super) fn files_edit_doc_sample(&mut self, cx: &mut Context<Self>) {
        self.files_show_doc_sample(cx);
        self.files_begin_edit(cx);
        let text = format!(
            "{SAMPLE}
A line typed since the last save.
"
        );
        self.md_editor_input
            .update(cx, |input, cx| input.set_value_at_start(text, cx));
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
        Document::parse(SOURCE, false)
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
    fn a_source_file_is_held_unparsed_and_edits_up_to_a_limit() {
        let file = |text: String, truncated| TextFile { text, truncated };
        let code = MdDoc::new(&file("# not a heading\n".to_string(), false), "a.rs");
        assert!(code.document.blocks().is_empty());
        assert_eq!(code.source(), "# not a heading\n");
        assert!(code.editable());
        assert!(!MdDoc::new(&file("x".to_string(), true), "a.rs").editable());
        let long = file("x".repeat(EDIT_LIMIT + 1), false);
        assert!(!MdDoc::new(&long, "a.rs").editable());
    }

    #[test]
    fn only_mdx_files_are_mdx() {
        assert!(is_mdx("a/b.mdx"));
        assert!(is_mdx("B.MDX"));
        assert!(!is_mdx("a/b.md"));
        assert!(!is_mdx("mdx"));
    }

    #[test]
    fn an_edit_of_an_mdx_file_is_read_as_mdx() {
        let file = TextFile {
            text: "# One\n".to_string(),
            truncated: false,
        };
        let mut doc = MdDoc::new(&file, "post.mdx");
        // An expression block holds its code alone; as plain markdown it is a
        // paragraph that keeps the braces.
        doc.replace_source("{1 + 1}\n".to_string());
        let leaf = md_select::LeafId { block: 0, leaf: 0 };
        assert_eq!(doc.document.leaf_text(leaf), Some("1 + 1"));
        let mut plain = MdDoc::new(&file, "post.md");
        plain.replace_source("{1 + 1}\n".to_string());
        assert_eq!(plain.document.leaf_text(leaf), Some("{1 + 1}"));
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
        assert_eq!(block_for_line(&Document::parse("", false), 1), None);
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
        let empty = Document::parse("", false);
        let form = Composing {
            end: 1,
            editing: None,
        };
        assert_eq!(rows(&empty, &[], Some(form)), vec![Row::Composer]);
    }
}
