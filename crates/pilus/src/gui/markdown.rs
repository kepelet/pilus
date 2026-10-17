//! Markdown rendering for documents.
//!
//! This module parses CommonMark plus GFM with `markdown` (the markdown-rs
//! crate) and paints its mdast as GPUI elements: paragraphs and inline runs
//! (bold, italic, code, links), headings, fenced code blocks, lists, block
//! quotes, tables, and rules.
//!
//! A file is drawn a block at a time: [`Document`] keeps the parsed blocks
//! with the source lines each covers, so the viewer can virtualize the list
//! and set a review note under the block its lines fall in.
//!
//! Inline styling rides one [`Text`] per paragraph, so wrapping stays natural;
//! each run carries its own font and color. A paragraph with inline code lays
//! out as a wrapping row instead, so each code span can wear a padded, rounded
//! chip that a text run's flat background cannot draw.
//!
//! Text is selectable (see `md_select`). Each block knows the plain text of its
//! leaves, the runs a selection can cover, so a copy never depends on what the
//! list has on screen; the painter numbers the leaves it draws in the same order
//! and reports where each landed.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, Div, FontStyle, FontWeight, Hsla, SharedString, StyledText, TextRun, canvas, div,
    font, prelude::*, px, relative,
};
use markdown::mdast::Node;

use crate::settings::Settings;

use super::md_select::{LeafId, Leaves, Selection};
use super::style::{self, Fonts, RadiusExt};
use super::theme::Theme;

/// Line height of running text, as a multiple of its size.
const PROSE_LEADING: f32 = 1.65;
/// Line height of headings, which wrap rarely and read tighter.
const HEADING_LEADING: f32 = 1.3;
/// Line height of code, which is denser than prose.
const CODE_LEADING: f32 = 1.5;

/// Parse `source` into its top-level nodes.
///
/// Front matter is only read for a file: in a chat reply a leading `---` is a
/// rule, and treating it as front matter would swallow the text after it.
fn parse(source: &str, front_matter: bool) -> Vec<Node> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.frontmatter = front_matter;
    match markdown::to_mdast(source, &options) {
        Ok(Node::Root(root)) => root.children,
        Ok(node) => vec![node],
        // A parse failure falls back to the raw text, never a blank reply.
        Err(_) => vec![Node::Text(markdown::mdast::Text {
            value: source.to_string(),
            position: None,
        })],
    }
}

/// One top-level block of a [`Document`] and the source lines it covers.
pub(super) struct Block {
    node: Node,
    /// First and last source line, 1-based, as a review note counts them.
    pub(super) first_line: u32,
    pub(super) last_line: u32,
    /// The plain text of each selectable leaf, in the order the painter
    /// numbers them.
    leaves: Vec<String>,
}

impl Block {
    pub(super) fn is_heading(&self) -> bool {
        matches!(self.node, Node::Heading(_))
    }

    /// Room above and below the block at the default text size: a heading
    /// pulls away from what precedes it and sits close to what it introduces,
    /// and every other block leaves a paragraph's gap.
    pub(super) fn spacing(&self) -> (f32, f32) {
        if self.is_heading() {
            (14.0, 6.0)
        } else {
            (0.0, 12.0)
        }
    }
}

/// A parsed file: its blocks in source order.
pub(super) struct Document {
    blocks: Vec<Block>,
}

impl Document {
    pub(super) fn parse(source: &str) -> Self {
        let mut line = 1;
        let blocks = parse(source, true)
            .into_iter()
            .map(|node| {
                let (first_line, last_line) = node
                    .position()
                    .map(|at| (at.start.line as u32, at.end.line as u32))
                    // A node without a position sits where the last one ended.
                    .unwrap_or((line, line));
                line = last_line;
                let mut leaves = Vec::new();
                collect_leaves(&node, &mut leaves);
                Block {
                    node,
                    first_line,
                    last_line,
                    leaves,
                }
            })
            .collect();
        Self { blocks }
    }

    pub(super) fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The text `selection` covers: a block's leaves on separate lines, and a
    /// blank line between blocks.
    pub(super) fn selected_text(&self, selection: &Selection) -> String {
        let (start, end) = selection.ordered();
        let mut blocks = Vec::new();
        for (ix, block) in self
            .blocks
            .iter()
            .enumerate()
            .take(end.leaf.block + 1)
            .skip(start.leaf.block)
        {
            let parts: Vec<&str> = block
                .leaves
                .iter()
                .enumerate()
                .filter_map(|(leaf, text)| {
                    let range = selection.range_in(LeafId { block: ix, leaf }, text.len())?;
                    text.get(range)
                })
                .collect();
            if !parts.is_empty() {
                blocks.push(parts.join("\n"));
            }
        }
        blocks.join("\n\n")
    }

    /// The plain text of one leaf.
    pub(super) fn leaf_text(&self, leaf: LeafId) -> Option<&str> {
        self.blocks
            .get(leaf.block)?
            .leaves
            .get(leaf.leaf)
            .map(String::as_str)
    }

    /// The source lines from where `selection` starts to where it ends.
    pub(super) fn selected_lines(&self, selection: &Selection) -> Option<(u32, u32)> {
        let (start, end) = selection.ordered();
        let first = self.blocks.get(start.leaf.block)?.first_line;
        let last = self.blocks.get(end.leaf.block)?.last_line;
        Some((first, last))
    }
}

/// The plain text of the leaves under `node`, in the order [`block`] paints
/// them. A leaf is one run of text a selection can cover.
fn collect_leaves(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::Paragraph(paragraph) => out.push(flat_text(&paragraph.children)),
        Node::Heading(heading) => out.push(flat_text(&heading.children)),
        Node::Code(code) => out.push(code.value.clone()),
        Node::Yaml(front) => out.push(front.value.clone()),
        Node::Toml(front) => out.push(front.value.clone()),
        Node::Html(html) => out.push(html.value.clone()),
        Node::Text(text) => out.push(text.value.clone()),
        Node::Table(table) => {
            for row in &table.children {
                let Node::TableRow(row) = row else {
                    continue;
                };
                for cell in &row.children {
                    if let Node::TableCell(cell) = cell {
                        out.push(flat_text(&cell.children));
                    }
                }
            }
        }
        Node::ThematicBreak(_) => {}
        other => {
            for child in other.children().into_iter().flatten() {
                collect_leaves(child, out);
            }
        }
    }
}

/// What a paragraph's atoms add up to: its text with the styling removed.
fn flat_text(nodes: &[Node]) -> String {
    let mut builder = Inline::new("", Theme::default());
    builder.children(nodes, Style::default());
    builder.into_atoms().iter().map(Atom::text).collect()
}

/// Paint one block of a document at the body size. `leaves` numbers the
/// block's selectable text and receives where it landed.
pub(super) fn render_block(
    block: &Block,
    theme: Theme,
    fonts: Fonts,
    settings: &Rc<RefCell<Settings>>,
    leaves: &Leaves,
) -> AnyElement {
    let paint = Paint {
        theme,
        fonts,
        chrome: style::chrome_font(settings).family,
        mono: style::mono_family(settings),
        leaves,
        plain: &block.leaves,
    };
    self::block(&block.node, &paint)
}

/// Everything painting a block needs besides the node.
struct Paint<'a> {
    theme: Theme,
    fonts: Fonts,
    chrome: SharedString,
    mono: String,
    leaves: &'a Leaves,
    /// The block's leaf texts from the parse, which the painter's own
    /// numbering must agree with.
    plain: &'a [String],
}

impl Paint<'_> {
    /// Claim the next leaf for the text `text`.
    fn leaf(&self, text: &str) -> LeafId {
        let leaf = self.leaves.next_leaf();
        debug_assert_eq!(
            self.plain.get(leaf.leaf).map(String::as_str),
            Some(text),
            "the painter and the parse disagree on leaf {}",
            leaf.leaf
        );
        leaf
    }

    /// A run of `len` bytes in `family`, undecorated.
    fn run(&self, len: usize, family: &str, color: Hsla) -> TextRun {
        TextRun {
            len,
            font: font(family.to_string()),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    /// Selectable text: `text` set in `runs`, the part of leaf `leaf` from
    /// byte `start` on. It tints what the selection covers, and reports its
    /// layout once measured so a pointer can be mapped back onto it.
    fn segment(&self, leaf: LeafId, start: usize, text: String, runs: Vec<TextRun>) -> Div {
        if text.is_empty() {
            return div();
        }
        let runs = self.leaves.tinted(leaf, start, text.len(), runs);
        let styled = StyledText::new(text).with_runs(runs);
        let layout = styled.layout().clone();
        let hits = self.leaves.hits().clone();
        // The probe is a sibling painted after the text, so by the time it
        // runs the text has been measured and its layout can be read.
        div().relative().child(styled).child(
            canvas(
                move |_, _, _| hits.report(leaf, start, layout),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }
}

/// One inline run's inherited styling.
#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: bool,
}

/// One atom of a paragraph: prose that flows as text, or an inline code span
/// that needs its own padded, rounded chip.
enum Atom {
    Prose {
        text: String,
        runs: Vec<TextRun>,
    },
    /// An inline code span, carrying any punctuation that abuts it so a wrap
    /// cannot strand that punctuation at the start of the next line.
    Code {
        value: String,
        trailing: String,
    },
}

impl Atom {
    /// The text this atom adds to its paragraph's plain text.
    fn text(&self) -> String {
        match self {
            Atom::Prose { text, .. } => text.clone(),
            Atom::Code { value, trailing } => format!("{value}{trailing}"),
        }
    }
}

/// Builds a paragraph's atoms in one pass.
struct Inline<'a> {
    atoms: Vec<Atom>,
    text: String,
    runs: Vec<TextRun>,
    chrome: &'a str,
    fg: Hsla,
    accent: Hsla,
    muted: Hsla,
}

impl<'a> Inline<'a> {
    fn new(chrome: &'a str, theme: Theme) -> Self {
        Self {
            atoms: Vec::new(),
            text: String::new(),
            runs: Vec::new(),
            chrome,
            fg: theme.fg.into(),
            accent: theme.accent.into(),
            muted: theme.muted.into(),
        }
    }

    /// Append `value` under `style`.
    fn push(&mut self, value: &str, style: Style) {
        if value.is_empty() {
            return;
        }
        // Inline code becomes its own atom: its chip is padded and rounded,
        // which a text run's flat background cannot draw.
        if style.code {
            self.flush();
            self.atoms.push(Atom::Code {
                value: value.to_string(),
                trailing: String::new(),
            });
            return;
        }
        let mut face = font(self.chrome);
        face.weight = if style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        face.style = if style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let color = if style.link {
            self.accent
        } else if style.strike {
            self.muted
        } else {
            self.fg
        };
        self.runs.push(TextRun {
            len: value.len(),
            font: face,
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
        self.text.push_str(value);
    }

    /// Close the prose run in progress, if any.
    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let mut text = std::mem::take(&mut self.text);
        let mut runs = std::mem::take(&mut self.runs);
        // Prose that abuts the previous chip — its punctuation and the space
        // after it — is glued to that chip, so a wrap cannot strand a comma
        // at the start of the next line or leave a leading space on it.
        if let Some(Atom::Code { trailing, .. }) = self.atoms.last_mut() {
            let glued = glued_prefix_len(&text);
            if glued > 0 {
                trailing.push_str(&text[..glued]);
                text = text[glued..].to_string();
                runs = drop_runs(runs, glued);
            }
        }
        if !text.is_empty() {
            self.atoms.push(Atom::Prose { text, runs });
        }
    }

    /// Walk one inline node, threading `style` into its children.
    fn walk(&mut self, node: &Node, style: Style) {
        match node {
            // A newline inside a paragraph is a soft break: it reads as a
            // space, and only a `Break` node is a line break.
            Node::Text(text) => self.push(&text.value.replace('\n', " "), style),
            Node::InlineCode(code) => self.push(
                &code.value,
                Style {
                    code: true,
                    ..style
                },
            ),
            Node::Strong(strong) => self.children(
                &strong.children,
                Style {
                    bold: true,
                    ..style
                },
            ),
            Node::Emphasis(emphasis) => self.children(
                &emphasis.children,
                Style {
                    italic: true,
                    ..style
                },
            ),
            Node::Delete(delete) => self.children(
                &delete.children,
                Style {
                    strike: true,
                    ..style
                },
            ),
            Node::Link(link) => self.children(
                &link.children,
                Style {
                    link: true,
                    ..style
                },
            ),
            Node::LinkReference(link) => self.children(&link.children, style),
            Node::Image(image) => {
                let label = if image.alt.is_empty() {
                    "image".to_string()
                } else {
                    image.alt.clone()
                };
                self.push(
                    &format!("[{label}]"),
                    Style {
                        link: true,
                        ..style
                    },
                );
            }
            Node::Break(_) => self.push("\n", style),
            Node::Html(html) => self.push(&html.value, style),
            Node::InlineMath(math) => self.push(
                &math.value,
                Style {
                    code: true,
                    ..style
                },
            ),
            Node::FootnoteReference(reference) => {
                let label = reference.label.as_deref().unwrap_or_default();
                self.push(&format!("[{label}]"), style)
            }
            _ => {}
        }
    }

    fn children(&mut self, nodes: &[Node], style: Style) {
        for node in nodes {
            self.walk(node, style);
        }
    }

    /// The atoms, once every child has been folded in.
    fn into_atoms(mut self) -> Vec<Atom> {
        self.flush();
        self.atoms
    }
}

/// How many leading bytes of `text` glue to a preceding chip: the run up to
/// the first whitespace, plus that whitespace, so the chip keeps the spacing.
fn glued_prefix_len(text: &str) -> usize {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    match text[end..].chars().next() {
        Some(space) => end + space.len_utf8(),
        None => end,
    }
}

/// Drop the runs covering the first `at` bytes of `text`, trimming a run that
/// straddles the boundary.
fn drop_runs(mut runs: Vec<TextRun>, at: usize) -> Vec<TextRun> {
    let mut remaining = at;
    let mut kept = Vec::new();
    for mut run in runs.drain(..) {
        if remaining >= run.len {
            remaining -= run.len;
        } else {
            run.len -= remaining;
            remaining = 0;
            kept.push(run);
        }
    }
    kept
}

/// The runs covering bytes `from..to` of the text they style.
fn slice_runs(runs: &[TextRun], from: usize, to: usize) -> Vec<TextRun> {
    let mut slice = Vec::new();
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let (low, high) = (from.max(at), to.min(end));
        if low < high {
            let mut piece = run.clone();
            piece.len = high - low;
            slice.push(piece);
        }
        at = end;
    }
    slice
}

/// Render one block node.
fn block(node: &Node, paint: &Paint) -> AnyElement {
    let theme = paint.theme;
    let size = paint.fonts.body;
    match node {
        Node::Paragraph(paragraph) => {
            inline(&paragraph.children, paint, size, false, PROSE_LEADING)
        }
        Node::Heading(heading) => {
            let scale = match heading.depth {
                1 => 1.6,
                2 => 1.35,
                3 => 1.15,
                _ => 1.0,
            };
            let title = inline(
                &heading.children,
                paint,
                size * scale,
                true,
                HEADING_LEADING,
            );
            // The two largest headings sit on a rule, so a section reads as one.
            if heading.depth <= 2 {
                div()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(title)
                    .into_any_element()
            } else {
                title
            }
        }
        Node::Code(code) => code_card(
            code.lang.as_deref().filter(|lang| !lang.is_empty()),
            &code.value,
            paint,
        ),
        Node::Yaml(front) => code_card(Some("front matter"), &front.value, paint),
        Node::Toml(front) => code_card(Some("front matter"), &front.value, paint),
        Node::Blockquote(quote) => {
            let mut inner = div().flex().flex_col().gap(px(8.0)).flex_1().min_w(px(0.0));
            for child in &quote.children {
                inner = inner.child(block(child, paint));
            }
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(
                    div()
                        .w(px(3.0))
                        .flex_shrink_0()
                        .radius_full()
                        .bg(theme.border),
                )
                .child(inner)
                .into_any_element()
        }
        Node::List(list) => {
            // A loose list (blank lines between items) breathes more.
            let gap = if list.spread { 8.0 } else { 4.0 };
            let mut column = div().flex().flex_col().gap(px(gap));
            for (index, item) in list.children.iter().enumerate() {
                let bullet = match (list.ordered, list.start) {
                    (true, Some(start)) => format!("{}.", start as usize + index),
                    (true, None) => format!("{}.", index + 1),
                    (false, _) => "•".to_string(),
                };
                column = column.child(list_item(item, bullet, paint));
            }
            column.into_any_element()
        }
        Node::ThematicBreak(_) => div()
            .my_1()
            .h(px(1.0))
            .w_full()
            .bg(theme.border)
            .into_any_element(),
        Node::Table(table) => {
            let mut grid = div()
                .flex()
                .flex_col()
                .radius_md()
                .border_1()
                .border_color(theme.border)
                .overflow_hidden();
            for (index, row) in table.children.iter().enumerate() {
                let Node::TableRow(row) = row else {
                    continue;
                };
                // The first row names the columns, so it reads as a header.
                let header = index == 0;
                let mut line = div().flex().flex_row();
                if header {
                    line = line.bg(theme.panel_dim);
                } else {
                    line = line.border_t_1().border_color(theme.border);
                }
                for cell in &row.children {
                    let Node::TableCell(cell) = cell else {
                        continue;
                    };
                    line = line.child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .px_2()
                            .py(px(5.0))
                            .child(inline(&cell.children, paint, size, header, PROSE_LEADING)),
                    );
                }
                grid = grid.child(line);
            }
            grid.into_any_element()
        }
        Node::Html(html) => {
            let leaf = paint.leaf(&html.value);
            let run = paint.run(html.value.len(), &paint.chrome, theme.muted.into());
            paint
                .segment(leaf, 0, html.value.clone(), vec![run])
                .text_size(px(size))
                .line_height(relative(PROSE_LEADING))
                .whitespace_normal()
                .into_any_element()
        }
        Node::Text(text) => {
            let leaf = paint.leaf(&text.value);
            let run = paint.run(text.value.len(), &paint.chrome, theme.fg.into());
            paint
                .segment(leaf, 0, text.value.clone(), vec![run])
                .text_size(px(size))
                .line_height(relative(PROSE_LEADING))
                .whitespace_normal()
                .into_any_element()
        }
        other => match other.children() {
            Some(children) => {
                let mut column = div().flex().flex_col().gap(px(8.0));
                for child in children {
                    column = column.child(block(child, paint));
                }
                column.into_any_element()
            }
            None => div().into_any_element(),
        },
    }
}

/// A monospace card for code or front matter, with its language or kind
/// above it. Code is set a step below the secondary text, so it reads as an
/// aside to the prose around it.
fn code_card(label: Option<&str>, value: &str, paint: &Paint) -> AnyElement {
    let theme = paint.theme;
    let leaf = paint.leaf(value);
    let mut card = div()
        .flex()
        .flex_col()
        .gap_1()
        .radius_md()
        .bg(theme.panel_dim)
        .px_3()
        .py_2()
        .overflow_hidden()
        .text_size(px(paint.fonts.label))
        .text_color(theme.fg)
        .line_height(relative(CODE_LEADING))
        .whitespace_normal();
    if let Some(label) = label {
        card = card.child(div().text_color(theme.muted).child(label.to_string()));
    }
    let run = paint.run(value.len(), &paint.mono, theme.fg.into());
    card.child(paint.segment(leaf, 0, value.to_string(), vec![run]))
        .into_any_element()
}

/// One list item: a marker beside its first block, with later blocks aligned.
fn list_item(item: &Node, bullet: String, paint: &Paint) -> AnyElement {
    let Node::ListItem(item) = item else {
        return div().into_any_element();
    };
    let gap = if item.spread { 8.0 } else { 4.0 };
    let mut body = div().flex().flex_col().gap(px(gap)).flex_1().min_w(px(0.0));
    for child in &item.children {
        body = body.child(block(child, paint));
    }
    let marker = if let Some(true) = item.checked {
        "☑".to_string()
    } else if let Some(false) = item.checked {
        "☐".to_string()
    } else {
        bullet
    };
    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(
            div()
                .flex()
                .justify_end()
                .flex_shrink_0()
                .min_w(px(16.0))
                .text_size(px(paint.fonts.body))
                .line_height(relative(PROSE_LEADING))
                .text_color(paint.theme.muted)
                .child(marker),
        )
        .child(body)
        .into_any_element()
}

/// A paragraph: inline runs in one wrapping [`Text`], or a wrapping row of
/// prose and code chips when it contains inline code. `bold` sets the whole
/// paragraph in bold, as a heading or a table header is.
fn inline(nodes: &[Node], paint: &Paint, size: f32, bold: bool, leading: f32) -> AnyElement {
    let theme = paint.theme;
    let mut builder = Inline::new(&paint.chrome, theme);
    builder.children(
        nodes,
        Style {
            bold,
            ..Style::default()
        },
    );
    let atoms = builder.into_atoms();
    let whole: String = atoms.iter().map(Atom::text).collect();
    let leaf = paint.leaf(&whole);
    let line = div()
        .w_full()
        .text_size(px(size))
        .text_color(theme.fg)
        .line_height(relative(leading))
        .whitespace_normal();
    if atoms.iter().all(|atom| matches!(atom, Atom::Prose { .. })) {
        let mut text = String::new();
        let mut runs = Vec::new();
        for atom in atoms {
            if let Atom::Prose {
                text: chunk,
                runs: chunk_runs,
            } = atom
            {
                text.push_str(&chunk);
                runs.extend(chunk_runs);
            }
        }
        return line
            .child(paint.segment(leaf, 0, text, runs).w_full())
            .into_any_element();
    }
    // A code chip is a step smaller than the prose around it.
    let chip_size = size * paint.fonts.secondary / paint.fonts.body;
    let mut row = div().flex().flex_row().flex_wrap().items_baseline();
    let mut start = 0;
    for atom in atoms {
        let length = atom.text().len();
        match atom {
            Atom::Prose { text, runs } => {
                // One item per word, each keeping the space after it, so the
                // row wraps like running text: a prose run as a single item
                // would fill the row and strand the chip on a line of its own.
                let mut at = 0;
                for word in text.split_inclusive(char::is_whitespace) {
                    let end = at + word.len();
                    row = row.child(paint.segment(
                        leaf,
                        start + at,
                        word.to_string(),
                        slice_runs(&runs, at, end),
                    ));
                    at = end;
                }
            }
            Atom::Code { value, trailing } => {
                let run = paint.run(value.len(), &paint.mono, theme.fg.into());
                let chip = div()
                    .px_1()
                    .radius_sm()
                    .bg(theme.panel_dim)
                    .text_size(px(chip_size))
                    .line_height(relative(1.4))
                    .child(paint.segment(leaf, start, value.clone(), vec![run]));
                row = row.child(if trailing.is_empty() {
                    chip.into_any_element()
                } else {
                    let run = paint.run(trailing.len(), &paint.chrome, theme.fg.into());
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .child(chip)
                        .child(paint.segment(leaf, start + value.len(), trailing, vec![run]))
                        .into_any_element()
                });
            }
        }
        start += length;
    }
    line.child(row).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::md_select::TextPoint;

    fn atoms(source: &str) -> Vec<Atom> {
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).expect("parse");
        let Node::Root(root) = root else {
            panic!("expected root");
        };
        let Node::Paragraph(paragraph) = &root.children[0] else {
            panic!("expected paragraph");
        };
        let mut inline = Inline::new("Chrome", Theme::default());
        inline.children(&paragraph.children, Style::default());
        inline.into_atoms()
    }

    #[test]
    fn inline_code_becomes_its_own_atom() {
        let atoms = atoms("**bold** and *italic* and `code` and [link](https://example.com)");
        assert_eq!(atoms.len(), 3);
        // A prose atom's runs must cover its text, or `StyledText` panics.
        for atom in &atoms {
            if let Atom::Prose { text, runs } = atom {
                let covered: usize = runs.iter().map(|run| run.len).sum();
                assert_eq!(covered, text.len());
            }
        }
        assert!(matches!(&atoms[0], Atom::Prose { text, .. } if text.contains("bold")));
        assert!(matches!(
            &atoms[1],
            Atom::Code { value, trailing } if value == "code" && trailing == " "
        ));
        assert!(matches!(&atoms[2], Atom::Prose { text, .. } if text.contains("link")));
    }

    #[test]
    fn punctuation_glues_to_the_chip() {
        let atoms = atoms("Run `cargo test`, then read the output.");
        assert_eq!(atoms.len(), 3);
        assert!(matches!(&atoms[0], Atom::Prose { text, .. } if text == "Run "));
        assert!(matches!(
            &atoms[1],
            Atom::Code { value, trailing } if value == "cargo test" && trailing == ", "
        ));
        // The space rides with the comma, so no prose atom starts with
        // whitespace and a wrap cannot indent the continuation.
        assert!(matches!(&atoms[2], Atom::Prose { text, .. } if text == "then read the output."));
    }

    #[test]
    fn a_document_keeps_each_blocks_source_lines() {
        let source = "# Title\n\nFirst paragraph\nstill first.\n\n- one\n- two\n";
        let document = Document::parse(source);
        let lines: Vec<(u32, u32)> = document
            .blocks()
            .iter()
            .map(|block| (block.first_line, block.last_line))
            .collect();
        assert_eq!(lines, vec![(1, 1), (3, 4), (6, 7)]);
        assert!(document.blocks()[0].is_heading());
        assert!(!document.blocks()[1].is_heading());
    }

    #[test]
    fn a_file_reads_front_matter_but_a_chat_reply_does_not() {
        let source = "---\ntitle: Notes\n---\n\nBody\n";
        let file = Document::parse(source);
        assert!(matches!(file.blocks()[0].node, Node::Yaml(_)));
        assert_eq!(
            (file.blocks()[0].first_line, file.blocks()[0].last_line),
            (1, 3)
        );
        // As a reply, the leading rule stays a rule.
        assert!(matches!(parse(source, false)[0], Node::ThematicBreak(_)));
    }

    #[test]
    fn a_soft_line_break_reads_as_a_space() {
        let soft = atoms("first line\nsecond line");
        assert!(matches!(&soft[0], Atom::Prose { text, .. } if text == "first line second line"));
        let hard = atoms("first  \nsecond");
        assert!(matches!(&hard[0], Atom::Prose { text, .. } if text.contains('\n')));
    }

    const MIXED: &str = "Hello `code`, world\n\n- one\n- two\n\n| a | b |\n|---|---|\n| c | d |\n\n```rs\nfn x() {}\n```\n";

    #[test]
    fn each_block_lists_the_text_of_its_leaves_in_painting_order() {
        let document = Document::parse(MIXED);
        let leaves: Vec<Vec<&str>> = document
            .blocks()
            .iter()
            .map(|block| block.leaves.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            leaves,
            vec![
                vec!["Hello code, world"],
                vec!["one", "two"],
                vec!["a", "b", "c", "d"],
                vec!["fn x() {}"],
            ]
        );
    }

    #[test]
    fn selected_text_runs_across_leaves_and_blocks() {
        let document = Document::parse(MIXED);
        let at = |block, leaf, offset| TextPoint {
            leaf: LeafId { block, leaf },
            offset,
        };
        let selection = Selection {
            anchor: at(1, 1, 2),
            head: at(0, 0, 6),
        };
        assert_eq!(document.selected_text(&selection), "code, world\n\none\ntw");
        assert_eq!(
            document.selected_lines(&selection).map(|lines| lines.0),
            Some(1)
        );
        assert_eq!(
            document.selected_lines(&Selection::collapsed(at(0, 0, 1))),
            Some((1, 1))
        );
        let inside = Selection {
            anchor: at(2, 1, 0),
            head: at(2, 2, 1),
        };
        assert_eq!(document.selected_text(&inside), "b\nc");
    }

    #[test]
    fn slicing_runs_keeps_each_pieces_styling_and_length() {
        let atoms = atoms("plain **bold** tail");
        let Atom::Prose { text, runs } = &atoms[0] else {
            panic!("expected prose");
        };
        let slice = slice_runs(runs, 3, 9);
        assert_eq!(slice.iter().map(|run| run.len).sum::<usize>(), 6);
        assert_eq!(&text[3..9], "in bol");
        assert_eq!(slice.len(), 2);
        assert_ne!(slice[0].font.weight, slice[1].font.weight);
        assert!(slice_runs(runs, 4, 4).is_empty());
    }

    #[test]
    fn an_empty_file_has_no_blocks() {
        assert!(Document::parse("").blocks().is_empty());
    }

    #[test]
    fn a_code_free_paragraph_stays_one_text() {
        let atoms = atoms("just **bold** prose");
        assert!(atoms.iter().all(|atom| matches!(atom, Atom::Prose { .. })));
    }
}
