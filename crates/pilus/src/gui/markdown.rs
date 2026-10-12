//! Markdown rendering for documents and GitHub bodies.
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

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, SharedString, StyledText, TextRun, div, font,
    prelude::*, px,
};
use markdown::mdast::Node;

use crate::settings::Settings;

use super::style::{self, Fonts, RadiusExt};
use super::theme::Theme;

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
}

impl Block {
    pub(super) fn is_heading(&self) -> bool {
        matches!(self.node, Node::Heading(_))
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
                Block {
                    node,
                    first_line,
                    last_line,
                }
            })
            .collect();
        Self { blocks }
    }

    pub(super) fn blocks(&self) -> &[Block] {
        &self.blocks
    }
}

/// Paint one block of a document at the body size.
pub(super) fn render_block(
    block: &Block,
    theme: Theme,
    fonts: Fonts,
    settings: &Rc<RefCell<Settings>>,
) -> AnyElement {
    let chrome: SharedString = style::chrome_font(settings).family;
    let mono = style::mono_family(settings);
    self::block(&block.node, theme, fonts, &chrome, &mono, fonts.body)
}

/// Render `source` as a vertical stack of blocks.
pub(super) fn render(
    source: &str,
    theme: Theme,
    fonts: Fonts,
    settings: &Rc<RefCell<Settings>>,
) -> AnyElement {
    let chrome = style::chrome_font(settings);
    let chrome_family: SharedString = chrome.family.clone();
    let mono = style::mono_family(settings);
    let nodes = parse(source, false);
    let mut column = div().flex().flex_col().gap_2();
    for node in &nodes {
        column = column.child(block(node, theme, fonts, &chrome_family, &mono, fonts.body));
    }
    column.into_any_element()
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

/// Render one block node at the given base size.
fn block(
    node: &Node,
    theme: Theme,
    fonts: Fonts,
    chrome: &str,
    mono: &str,
    size: f32,
) -> AnyElement {
    match node {
        Node::Paragraph(paragraph) => inline(&paragraph.children, theme, chrome, mono, size, None),
        Node::Heading(heading) => {
            let scale = match heading.depth {
                1 => 1.5,
                2 => 1.3,
                3 => 1.15,
                _ => 1.0,
            };
            inline(
                &heading.children,
                theme,
                chrome,
                mono,
                size * scale,
                Some(FontWeight::BOLD),
            )
        }
        Node::Code(code) => code_card(
            code.lang.as_deref().filter(|lang| !lang.is_empty()),
            &code.value,
            theme,
            fonts,
            mono,
        ),
        Node::Yaml(front) => code_card(Some("front matter"), &front.value, theme, fonts, mono),
        Node::Toml(front) => code_card(Some("front matter"), &front.value, theme, fonts, mono),
        Node::Blockquote(quote) => {
            let mut inner = div().flex().flex_col().gap_2().flex_1().min_w(px(0.0));
            for child in &quote.children {
                inner = inner.child(block(child, theme, fonts, chrome, mono, size));
            }
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(
                    div()
                        .w(px(2.0))
                        .flex_shrink_0()
                        .radius_full()
                        .bg(theme.border),
                )
                .child(inner)
                .into_any_element()
        }
        Node::List(list) => {
            let mut column = div().flex().flex_col().gap_2();
            for (index, item) in list.children.iter().enumerate() {
                let bullet = match (list.ordered, list.start) {
                    (true, Some(start)) => format!("{}.", start as usize + index),
                    (true, None) => format!("{}.", index + 1),
                    (false, _) => "•".to_string(),
                };
                column = column.child(list_item(item, bullet, theme, fonts, chrome, mono, size));
            }
            column.into_any_element()
        }
        Node::ThematicBreak(_) => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.border)
            .into_any_element(),
        Node::Table(table) => {
            let mut column = div().flex().flex_col().gap_1();
            for (index, row) in table.children.iter().enumerate() {
                let Node::TableRow(row) = row else {
                    continue;
                };
                // The first row names the columns, so it reads as a header.
                let header = index == 0;
                let mut line = div().flex().flex_row().gap_3();
                if header {
                    line = line.pb_1().border_b_1().border_color(theme.border);
                }
                for cell in &row.children {
                    let Node::TableCell(cell) = cell else {
                        continue;
                    };
                    line = line.child(div().flex_1().min_w(px(0.0)).child(inline(
                        &cell.children,
                        theme,
                        chrome,
                        mono,
                        size,
                        header.then_some(FontWeight::BOLD),
                    )));
                }
                column = column.child(line);
            }
            column.into_any_element()
        }
        Node::Html(html) => div()
            .text_size(px(size))
            .text_color(theme.muted)
            .child(html.value.clone())
            .into_any_element(),
        Node::Text(text) => div()
            .text_size(px(size))
            .text_color(theme.fg)
            .whitespace_normal()
            .child(text.value.clone())
            .into_any_element(),
        other => match other.children() {
            Some(children) => {
                let mut column = div().flex().flex_col().gap_2();
                for child in children {
                    column = column.child(block(child, theme, fonts, chrome, mono, size));
                }
                column.into_any_element()
            }
            None => div().into_any_element(),
        },
    }
}

/// A monospace card for code or front matter, with its language or kind
/// above it.
fn code_card(
    label: Option<&str>,
    value: &str,
    theme: Theme,
    fonts: Fonts,
    mono: &str,
) -> AnyElement {
    let mut card = div()
        .flex()
        .flex_col()
        .gap_1()
        .mx_1()
        .radius_md()
        .bg(theme.panel_dim)
        .px_3()
        .py_2()
        .overflow_hidden()
        .text_size(px(fonts.secondary))
        .text_color(theme.fg)
        .whitespace_normal()
        .font_family(mono.to_string());
    if let Some(label) = label {
        card = card.child(
            div()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(label.to_string()),
        );
    }
    card.child(value.to_string()).into_any_element()
}

/// One list item: a marker beside its first block, with later blocks aligned.
fn list_item(
    item: &Node,
    bullet: String,
    theme: Theme,
    fonts: Fonts,
    chrome: &str,
    mono: &str,
    size: f32,
) -> AnyElement {
    let Node::ListItem(item) = item else {
        return div().into_any_element();
    };
    let mut body = div().flex().flex_col().gap_2().flex_1().min_w(px(0.0));
    for child in &item.children {
        body = body.child(block(child, theme, fonts, chrome, mono, size));
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
                .flex_shrink_0()
                .text_size(px(size))
                .text_color(theme.muted)
                .child(marker),
        )
        .child(body)
        .into_any_element()
}

/// A paragraph: inline runs in one wrapping [`Text`], or a wrapping row of
/// prose and code chips when it contains inline code.
fn inline(
    nodes: &[Node],
    theme: Theme,
    chrome: &str,
    mono: &str,
    size: f32,
    weight: Option<FontWeight>,
) -> AnyElement {
    let mut builder = Inline::new(chrome, theme);
    builder.children(nodes, Style::default());
    let atoms = builder.into_atoms();
    let mut line = div()
        .w_full()
        .text_size(px(size))
        .text_color(theme.fg)
        .whitespace_normal();
    if let Some(weight) = weight {
        line = line.font_weight(weight);
    }
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
        line = line.child(StyledText::new(text).with_runs(runs));
    } else {
        let mut row = div().flex().flex_row().flex_wrap().items_baseline();
        for atom in atoms {
            match atom {
                Atom::Prose { text, runs } => {
                    // `min_w(0)` lets a prose run shrink below its natural
                    // width so it wraps; a flex item's default `min-width:
                    // auto` would hold it at full width and overflow.
                    row = row.child(
                        div()
                            .min_w(px(0.0))
                            .child(StyledText::new(text).with_runs(runs)),
                    );
                }
                Atom::Code { value, trailing } => {
                    let chip = div()
                        .px_1()
                        .radius_sm()
                        .bg(theme.panel_dim)
                        .font_family(mono.to_string())
                        .child(value);
                    row = row.child(if trailing.is_empty() {
                        chip.into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .child(chip)
                            .child(trailing)
                            .into_any_element()
                    });
                }
            }
        }
        line = line.child(row);
    }
    line.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

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
