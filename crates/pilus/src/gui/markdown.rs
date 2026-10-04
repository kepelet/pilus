//! Markdown rendering for the Pi Session transcript.
//!
//! Pi mostly replies in Markdown (CommonMark plus GFM). This module parses a
//! reply with `markdown` (the markdown-rs crate) and paints its mdast as GPUI
//! elements: paragraphs and inline runs (bold, italic, code, links), headings,
//! fenced code blocks, lists, block quotes, tables, and rules.
//!
//! Inline styling rides one [`Text`] per paragraph, so wrapping stays natural;
//! each run carries its own font and color.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, SharedString, StyledText, TextRun, div, font,
    prelude::*, px,
};
use markdown::mdast::Node;

use crate::settings::Settings;

use super::style::{self, Fonts};
use super::theme::Theme;

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
    let nodes = match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(Node::Root(root)) => root.children,
        Ok(node) => vec![node],
        // A parse failure falls back to the raw text, never a blank reply.
        Err(_) => vec![Node::Text(markdown::mdast::Text {
            value: source.to_string(),
            position: None,
        })],
    };
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

/// Builds a paragraph's text and its runs in one pass.
struct Inline<'a> {
    text: String,
    runs: Vec<TextRun>,
    chrome: &'a str,
    mono: &'a str,
    fg: Hsla,
    accent: Hsla,
    muted: Hsla,
    code_bg: Hsla,
}

impl<'a> Inline<'a> {
    fn new(chrome: &'a str, mono: &'a str, theme: Theme, code_bg: Hsla) -> Self {
        Self {
            text: String::new(),
            runs: Vec::new(),
            chrome,
            mono,
            fg: theme.fg.into(),
            accent: theme.accent.into(),
            muted: theme.muted.into(),
            code_bg,
        }
    }

    /// Append `value` under `style`.
    fn push(&mut self, value: &str, style: Style) {
        if value.is_empty() {
            return;
        }
        let mut face = font(if style.code { self.mono } else { self.chrome });
        // Inline code reads through its face and tint, not its weight; bold
        // mono at body size looks heavier than the prose around it.
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
            background_color: style.code.then_some(self.code_bg),
            underline: None,
            strikethrough: None,
        });
        self.text.push_str(value);
    }

    /// Walk one inline node, threading `style` into its children.
    fn walk(&mut self, node: &Node, style: Style) {
        match node {
            Node::Text(text) => self.push(&text.value, style),
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

    /// The styled text, once every child has been folded in.
    fn element(self) -> StyledText {
        StyledText::new(self.text).with_runs(self.runs)
    }
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
        Node::Code(code) => {
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .rounded_md()
                .bg(theme.panel_dim)
                .px_3()
                .py_2()
                .overflow_hidden()
                .text_size(px(fonts.secondary))
                .text_color(theme.fg)
                .whitespace_normal()
                .font_family(mono.to_string());
            if let Some(lang) = code.lang.as_deref().filter(|lang| !lang.is_empty()) {
                card = card.child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(lang.to_string()),
                );
            }
            card.child(code.value.clone()).into_any_element()
        }
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
                        .rounded_full()
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
            for row in &table.children {
                let Node::TableRow(row) = row else {
                    continue;
                };
                let mut line = div().flex().flex_row().gap_3();
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
                        None,
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

/// A paragraph: inline runs in one wrapping [`Text`].
fn inline(
    nodes: &[Node],
    theme: Theme,
    chrome: &str,
    mono: &str,
    size: f32,
    weight: Option<FontWeight>,
) -> AnyElement {
    let code_bg: Hsla = theme.panel_dim.into();
    let mut builder = Inline::new(chrome, mono, theme, code_bg);
    builder.children(nodes, Style::default());
    let text = builder.element();
    let mut line = div()
        .w_full()
        .text_size(px(size))
        .text_color(theme.fg)
        .whitespace_normal()
        .child(text);
    if let Some(weight) = weight {
        line = line.font_weight(weight);
    }
    line.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_runs_cover_the_whole_text() {
        let source = "**bold** and *italic* and `code` and [link](https://example.com)";
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).expect("parse");
        let Node::Root(root) = root else {
            panic!("expected root");
        };
        let Node::Paragraph(paragraph) = &root.children[0] else {
            panic!("expected paragraph");
        };
        let theme = Theme::default();
        let mut inline = Inline::new("Chrome", "Mono", theme, theme.panel_dim.into());
        inline.children(&paragraph.children, Style::default());
        // A run list that does not cover the text panics inside `StyledText`.
        let covered: usize = inline.runs.iter().map(|run| run.len).sum();
        assert_eq!(covered, inline.text.len());
        assert!(inline.text.contains("bold"));
        assert!(inline.text.contains("code"));
    }
}
