//! Notes a reader leaves on excerpts of a markdown file.
//!
//! A selection in the document view can become a note: the quoted text, the
//! source lines it came from, and what the reader wants done about it. Notes
//! are the app's own (the daemon's review notes are anchored to a Git layer,
//! which an unchanged file does not have), so they are kept per machine and
//! file in `md-notes.json` beside the settings, and survive a restart.
//!
//! The reader collects several, then hands any of them to an agent as one
//! prompt. A note remembers who it was sent to, so a second pass over a long
//! document does not resend what already went. The store and the prompt are
//! pure so they can be tested without a window.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "md-notes.json";

/// One note: an excerpt of a file and the reader's words about it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ExcerptNote {
    pub(super) id: u64,
    /// The machine the file lives on, as the window keys its endpoints.
    pub(super) endpoint: String,
    pub(super) path: String,
    /// The first and last source line the excerpt covers.
    pub(super) lines: Option<(u32, u32)>,
    pub(super) quote: String,
    pub(super) body: String,
    /// The agent the note was last sent to.
    #[serde(default)]
    pub(super) sent_to: Option<String>,
}

/// What a new note is made of.
pub(super) struct NewNote {
    pub(super) endpoint: String,
    pub(super) path: String,
    pub(super) lines: Option<(u32, u32)>,
    pub(super) quote: String,
    pub(super) body: String,
}

/// Every note, in the order they were written.
#[derive(Default, Deserialize, Serialize)]
pub(super) struct NoteBook {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    notes: Vec<ExcerptNote>,
    /// Where it saves; a book with none keeps its notes in memory.
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl NoteBook {
    /// The book stored in `root`. A missing or unreadable file is an empty
    /// book: notes are a convenience, and a corrupt one must not stop the app.
    pub(super) fn load(root: &Path) -> Self {
        let file = root.join(FILE);
        let mut book: Self = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        book.file = Some(file);
        // Ids are never reused, even if the stored counter fell behind.
        let highest = book.notes.iter().map(|note| note.id).max().unwrap_or(0);
        book.next_id = book.next_id.max(highest + 1);
        book
    }

    /// A book that never touches the disk.
    #[cfg(feature = "visual-test")]
    pub(super) fn in_memory() -> Self {
        Self::default()
    }

    /// Write the book out, through a temporary file so a crash cannot leave
    /// half of it.
    pub(super) fn save(&self) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let staging = file.with_extension("json.tmp");
        std::fs::write(&staging, text)?;
        std::fs::rename(&staging, file)
    }

    /// The notes on one file, oldest first.
    pub(super) fn for_doc<'a>(
        &'a self,
        endpoint: &'a str,
        path: &'a str,
    ) -> impl Iterator<Item = &'a ExcerptNote> {
        self.notes
            .iter()
            .filter(move |note| note.endpoint == endpoint && note.path == path)
    }

    pub(super) fn get(&self, id: u64) -> Option<&ExcerptNote> {
        self.notes.iter().find(|note| note.id == id)
    }

    pub(super) fn add(&mut self, new: NewNote) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.notes.push(ExcerptNote {
            id,
            endpoint: new.endpoint,
            path: new.path,
            lines: new.lines,
            quote: new.quote,
            body: new.body,
            sent_to: None,
        });
        id
    }

    /// Rewrite a note's words. Editing makes it worth sending again.
    pub(super) fn edit(&mut self, id: u64, body: String) {
        if let Some(note) = self.notes.iter_mut().find(|note| note.id == id) {
            note.body = body;
            note.sent_to = None;
        }
    }

    pub(super) fn remove(&mut self, id: u64) {
        self.notes.retain(|note| note.id != id);
    }

    pub(super) fn mark_sent(&mut self, ids: &[u64], agent: &str) {
        for note in self.notes.iter_mut().filter(|note| ids.contains(&note.id)) {
            note.sent_to = Some(agent.to_string());
        }
    }
}

/// The prompt that hands an agent `notes` on the file `path`. Each excerpt is
/// quoted, so the agent can tell the reader's words from the document's.
pub(super) fn notes_prompt(path: &str, notes: &[&ExcerptNote]) -> String {
    let mut prompt = match notes {
        [_] => format!("A note on {path}:\n"),
        _ => format!("{} notes on {path}:\n", notes.len()),
    };
    for (index, note) in notes.iter().enumerate() {
        prompt.push('\n');
        if notes.len() > 1 {
            write!(prompt, "{}. ", index + 1).ok();
        }
        match note.lines {
            Some((first, last)) if first != last => writeln!(prompt, "Lines {first}-{last}:"),
            Some((line, _)) => writeln!(prompt, "Line {line}:"),
            None => writeln!(prompt, "Excerpt:"),
        }
        .ok();
        for line in note.quote.lines() {
            if line.is_empty() {
                prompt.push_str(">\n");
            } else {
                writeln!(prompt, "> {line}").ok();
            }
        }
        writeln!(prompt, "{}", note.body.trim()).ok();
    }
    prompt.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(path: &str, body: &str) -> NewNote {
        NewNote {
            endpoint: "local".to_string(),
            path: path.to_string(),
            lines: Some((3, 5)),
            quote: "first\n\nsecond".to_string(),
            body: body.to_string(),
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-md-notes-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn notes_are_listed_per_file_in_the_order_written() {
        let mut book = NoteBook::default();
        let a = book.add(new("/r/a.md", "one"));
        let _ = book.add(new("/r/b.md", "elsewhere"));
        let c = book.add(new("/r/a.md", "two"));
        let ids: Vec<u64> = book
            .for_doc("local", "/r/a.md")
            .map(|note| note.id)
            .collect();
        assert_eq!(ids, vec![a, c]);
        // The same path on another machine is another file.
        assert_eq!(book.for_doc("mac-mini", "/r/a.md").count(), 0);
    }

    #[test]
    fn an_id_is_never_reused_after_a_removal() {
        let mut book = NoteBook::default();
        let first = book.add(new("a.md", "x"));
        book.remove(first);
        assert_ne!(book.add(new("a.md", "y")), first);
    }

    #[test]
    fn editing_a_note_makes_it_unsent_again() {
        let mut book = NoteBook::default();
        let id = book.add(new("a.md", "x"));
        book.mark_sent(&[id], "pilus");
        assert_eq!(
            book.get(id).and_then(|n| n.sent_to.as_deref()),
            Some("pilus")
        );
        book.edit(id, "y".to_string());
        let note = book.get(id).expect("note");
        assert_eq!((note.body.as_str(), note.sent_to.as_deref()), ("y", None));
    }

    #[test]
    fn a_saved_book_comes_back_with_its_notes_and_counter() {
        let dir = scratch_dir("roundtrip");
        let mut book = NoteBook::load(&dir);
        let id = book.add(new("a.md", "keep"));
        book.mark_sent(&[id], "orca");
        book.save().expect("save");
        let mut again = NoteBook::load(&dir);
        let note = again.get(id).expect("note survives");
        assert_eq!(note.body, "keep");
        assert_eq!(note.sent_to.as_deref(), Some("orca"));
        assert!(again.add(new("a.md", "next")) > id);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_an_empty_book() {
        let dir = scratch_dir("corrupt");
        std::fs::write(dir.join(FILE), "{ not json").expect("write");
        let book = NoteBook::load(&dir);
        assert_eq!(book.for_doc("local", "a.md").count(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn one_note_reads_as_a_single_quoted_excerpt() {
        let mut book = NoteBook::default();
        let id = book.add(new("/r/README.md", "  Reword this.  "));
        let note = book.get(id).expect("note");
        assert_eq!(
            notes_prompt("/r/README.md", &[note]),
            "A note on /r/README.md:\n\nLines 3-5:\n> first\n>\n> second\nReword this."
        );
    }

    #[test]
    fn several_notes_are_numbered_in_one_prompt() {
        let mut book = NoteBook::default();
        let a = book.add(new("a.md", "first ask"));
        let mut single = new("a.md", "second ask");
        single.lines = Some((9, 9));
        single.quote = "one line".to_string();
        let b = book.add(single);
        let notes = [book.get(a).expect("a"), book.get(b).expect("b")];
        assert_eq!(
            notes_prompt("a.md", &notes),
            "2 notes on a.md:\n\n1. Lines 3-5:\n> first\n>\n> second\nfirst ask\n\n2. Line 9:\n> one line\nsecond ask"
        );
    }
}
