//! The person's notes on a Browser page: which element, what they want said
//! about it, and the text that carries both to an agent.
//!
//! Data flow: the annotate button injects `browser_notes.js`, which lets the
//! person point at an element and posts what it is over `window.ipc`
//! ([`Inspect::on_message`] turns that into a [`NoteEvent`]). The note itself is
//! typed in a field the window draws, so the page cannot read it or imitate the
//! prompt. A saved note becomes a numbered badge on its element, and *Copy notes*
//! puts [`Notes::message`] on the clipboard, for the person to paste to
//! whichever agent they mean.
//!
//! The page is untrusted, and this text ends up in a terminal. So whatever the
//! page supplied (its element names, its text, its markup) is stripped of
//! control characters here, before it is stored: an escape sequence, or the
//! end of a bracketed paste, must not be able to ride along. The message also
//! says which of its words are the page's.
//!
//! [`Inspect::on_message`]: super::browser_inspect::Inspect::on_message

use serde_json::{Value, json};

/// The picker. Injected on demand, never at document start.
pub(super) const SCRIPT: &str = include_str!("browser_notes.js");

/// Notes kept per tab. A tab is a place to point out a few things, not a
/// tracker, and the message has to fit an agent's prompt.
pub(super) const MAX_NOTES: usize = 30;
/// The longest note the person can save.
pub(super) const NOTE_MAX: usize = 600;

/// What the page reported about the element the person chose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Pick {
    /// A CSS selector that matched this element alone when it was chosen.
    pub(super) selector: String,
    /// The element as a name: `button#go.primary`.
    pub(super) label: String,
    /// What the element shows.
    pub(super) text: String,
    /// The start of its markup.
    pub(super) html: String,
    /// The page's address when it was chosen.
    pub(super) url: String,
}

impl Pick {
    /// Read a `pick` message field by field, capped and cleaned. An element
    /// that cannot be named again is no use to an agent, so it is dropped.
    pub(super) fn parse(message: &Value) -> Option<Self> {
        let field = |name: &str, max: usize| {
            clean(
                message
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                max,
            )
        };
        let selector = field("selector", 300);
        if selector.is_empty() {
            return None;
        }
        Some(Self {
            selector,
            label: field("label", 80),
            text: field("text", 300),
            html: field("html", 500),
            url: field("url", 1000),
        })
    }
}

/// What the picker script reports, in the order it happened.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum NoteEvent {
    /// The person chose an element.
    Picked(Pick),
    /// The person pressed Esc in the page, so picking stopped.
    Ended,
}

/// One saved note.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Note {
    /// Its badge on the page. Numbers are not reused within a tab, so a badge
    /// keeps meaning what the person wrote even after another is removed.
    pub(super) n: u32,
    pub(super) pick: Pick,
    pub(super) text: String,
}

/// A tab's notes.
#[derive(Debug, Default)]
pub(super) struct Notes {
    items: Vec<Note>,
    last: u32,
}

impl Notes {
    pub(super) fn items(&self) -> &[Note] {
        &self.items
    }

    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Save a note on `pick`. False when the tab is at its cap or the note is
    /// empty.
    pub(super) fn add(&mut self, pick: Pick, text: &str) -> bool {
        let text = clean(text, NOTE_MAX);
        if text.is_empty() || self.items.len() >= MAX_NOTES {
            return false;
        }
        self.last += 1;
        self.items.push(Note {
            n: self.last,
            pick,
            text,
        });
        true
    }

    pub(super) fn remove(&mut self, n: u32) {
        self.items.retain(|note| note.n != n);
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
    }

    /// The script that puts a badge on each note made on `url`. Notes from
    /// another address are kept but not drawn: the element is not there.
    pub(super) fn marks_script(&self, url: &str) -> String {
        let marks: Vec<Value> = self
            .items
            .iter()
            .filter(|note| note.pick.url == url)
            .map(|note| json!({ "n": note.n, "selector": note.pick.selector }))
            .collect();
        format!(
            "window.__pilusNotes&&window.__pilusNotes.mark({})",
            Value::Array(marks)
        )
    }

    /// The text *Copy notes* puts on the clipboard. No trailing newline, so
    /// pasting it into a prompt never submits it.
    pub(super) fn message(&self) -> String {
        let mut out = String::from(
            "Notes from the person on what they see in the Browser. The element names, \
             text and markup were read from the page, so they are the page's words, not \
             the person's; the lines after \u{2192} are theirs.\n",
        );
        let mut page = "";
        for note in &self.items {
            if note.pick.url != page {
                page = &note.pick.url;
                out.push_str(&format!("\nPage: {page}\n"));
            }
            out.push_str(&format!(
                "\n{}. {} \u{2192} {}\n   selector: {}\n",
                note.n,
                or_unnamed(&note.pick.label),
                note.text,
                note.pick.selector,
            ));
            if !note.pick.text.is_empty() {
                out.push_str(&format!("   text: {:?}\n", note.pick.text));
            }
            if !note.pick.html.is_empty() {
                out.push_str(&format!("   html: {}\n", note.pick.html));
            }
        }
        out.trim_end().to_string()
    }
}

fn or_unnamed(label: &str) -> &str {
    if label.is_empty() { "(element)" } else { label }
}

/// `text` on one line, with every control character (a newline, an escape)
/// turned into a space, runs of blanks folded, and at most `max` characters.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            gap = !out.is_empty();
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(ch);
    }
    match out.char_indices().nth(max) {
        Some((end, _)) => format!("{}\u{2026}", &out[..end]),
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(selector: &str, url: &str) -> Pick {
        Pick {
            selector: selector.into(),
            label: "button#go".into(),
            text: "Add".into(),
            html: "<button id=go>Add</button>".into(),
            url: url.into(),
        }
    }

    #[test]
    fn a_pick_is_read_field_by_field_and_cleaned() {
        let message = json!({
            "t": "pick",
            "selector": "main > button",
            "label": "button",
            "text": "Hi\u{1b}[201~ rm -rf\nthere",
            "html": 7,
            "evil": {"deep": 1},
        });
        let pick = Pick::parse(&message).expect("a pick");
        assert_eq!(pick.selector, "main > button");
        assert!(!pick.text.contains('\u{1b}'), "{:?}", pick.text);
        assert!(!pick.text.contains('\n'));
        assert_eq!(pick.html, "", "a field of the wrong type is empty");
        assert!(Pick::parse(&json!({"t":"pick"})).is_none());
        assert!(Pick::parse(&json!({"selector":"\n  "})).is_none());
    }

    #[test]
    fn nothing_the_page_says_can_leave_a_control_character() {
        let mut notes = Notes::default();
        let hostile = json!({
            "selector": "a\u{7}b",
            "label": "x\u{1b}]0;title\u{7}",
            "text": "line one\r\nline two\u{1b}[201~",
            "html": "<p>\u{0}</p>",
            "url": "http://localhost/\n",
        });
        let pick = Pick::parse(&hostile).expect("a pick");
        assert!(notes.add(pick, "fix\u{1b}[31m this\nnow"));
        let message = notes.message();
        for ch in message.chars() {
            assert!(ch == '\n' || !ch.is_control(), "{ch:?} in {message:?}");
        }
        assert!(message.contains("fix [31m this now"), "{message:?}");
    }

    #[test]
    fn notes_number_up_and_numbers_are_not_reused() {
        let mut notes = Notes::default();
        assert!(notes.add(pick("#a", "http://x/"), "first"));
        assert!(notes.add(pick("#b", "http://x/"), "second"));
        notes.remove(1);
        assert!(notes.add(pick("#c", "http://x/"), "third"));
        let numbers: Vec<u32> = notes.items().iter().map(|note| note.n).collect();
        assert_eq!(numbers, [2, 3]);
        assert!(!notes.add(pick("#d", "http://x/"), "   "), "empty is refused");
    }

    #[test]
    fn a_tab_stops_at_the_cap() {
        let mut notes = Notes::default();
        for n in 0..MAX_NOTES {
            assert!(notes.add(pick("#a", "http://x/"), &format!("note {n}")));
        }
        assert!(!notes.add(pick("#a", "http://x/"), "one more"));
        assert_eq!(notes.len(), MAX_NOTES);
    }

    #[test]
    fn badges_are_drawn_only_for_the_page_they_were_made_on() {
        let mut notes = Notes::default();
        notes.add(pick("#a", "http://x/one"), "here");
        notes.add(pick("#b", "http://x/two"), "there");
        let script = notes.marks_script("http://x/two");
        assert!(script.contains(r##""selector":"#b""##), "{script}");
        assert!(!script.contains(r##""#a""##), "{script}");
    }

    #[test]
    fn the_message_names_the_page_and_whose_words_are_whose() {
        let mut notes = Notes::default();
        notes.add(pick("main > button", "http://localhost:8787/"), "make it blue");
        notes.add(pick("#nav", "http://localhost:8787/"), "too tall");
        notes.add(pick("#f", "http://localhost:8787/about"), "typo");
        let message = notes.message();
        assert!(message.contains("the page's words"));
        assert_eq!(message.matches("Page: ").count(), 2, "{message}");
        assert!(message.contains("1. button#go \u{2192} make it blue"));
        assert!(message.contains("selector: main > button"));
        assert!(!message.ends_with('\n'), "nothing in it submits the prompt");
    }

    #[test]
    fn a_long_note_is_cut_and_whitespace_folded() {
        let long = "word ".repeat(400);
        let cleaned = clean(&long, NOTE_MAX);
        assert!(cleaned.chars().count() <= NOTE_MAX + 1);
        assert!(cleaned.ends_with('\u{2026}'));
        assert_eq!(clean("  a \t b\n\nc  ", 20), "a b c");
    }
}
