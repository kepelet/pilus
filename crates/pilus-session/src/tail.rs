//! Tailing a session as pi appends to it.
//!
//! Pi writes complete JSONL lines and flushes them, so a reader can follow the
//! file by byte offset. [`Tail`] keeps the transcript folded so far and reads
//! only new bytes on each [`Tail::poll`], holding back a trailing partial line
//! (and any partial UTF-8 sequence) until its newline arrives.
//!
//! The bytes come from a [`Source`], so the same fold works for a local file
//! and a session on another machine read over SSH.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::entry::Transcript;

/// A failure reading a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read session {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read session: {0}")]
    Source(String),
}

/// Result alias for the session reader.
pub type Result<T> = std::result::Result<T, Error>;

/// Where a session's bytes come from.
///
/// Both methods are called on the reader's own thread, never the GUI thread,
/// so a `Source` may block (an SSH round trip) without stalling a window.
pub trait Source: Send {
    /// Total byte length currently available.
    fn byte_len(&mut self) -> Result<u64>;
    /// Read every byte from `offset` to the current end.
    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>>;
    /// A human label for the header and error text.
    fn label(&self) -> String;
}

/// A session file on the local filesystem.
pub struct FileSource {
    path: PathBuf,
    file: File,
}

impl FileSource {
    /// Open `path` for reading.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = File::open(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path, file })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Source for FileSource {
    fn byte_len(&mut self) -> Result<u64> {
        self.file
            .metadata()
            .map(|meta| meta.len())
            .map_err(|source| Error::Io {
                path: self.path.clone(),
                source,
            })
    }

    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_to_end(&mut bytes))
            .map_err(|source| Error::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(bytes)
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// A session and the transcript folded from it.
pub struct Tail {
    source: Box<dyn Source>,
    /// Byte offset read up to.
    offset: u64,
    /// Bytes after the last newline, waiting for more.
    partial: Vec<u8>,
    transcript: Transcript,
}

impl Tail {
    /// Open a local session file and fold everything already written.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_source(Box::new(FileSource::open(path)?))
    }

    /// Fold from any byte source, reading what is already available.
    pub fn with_source(source: Box<dyn Source>) -> Result<Self> {
        let mut tail = Self {
            source,
            offset: 0,
            partial: Vec::new(),
            transcript: Transcript::default(),
        };
        tail.poll()?;
        Ok(tail)
    }

    /// A human label for the source being tailed.
    pub fn label(&self) -> String {
        self.source.label()
    }

    /// The transcript folded so far.
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The lowest item index changed since the last call, or `None` when
    /// only metadata moved. A caller sends `items[from..]` as the delta;
    /// `None` means `from = items.len()` — no item moved, but metadata may
    /// still need delivery. Taking the mark resets it.
    pub fn take_changed_from(&mut self) -> Option<usize> {
        self.transcript.take_changed_from()
    }

    /// Read bytes appended since the last call.
    ///
    /// Returns whether the transcript changed. A source that shrank (rotation
    /// or truncation) is re-read from the start, so a replaced session is not
    /// silently missed.
    pub fn poll(&mut self) -> Result<bool> {
        let length = self.source.byte_len()?;
        if length < self.offset {
            self.rewind();
        }
        if length == self.offset {
            return Ok(false);
        }
        let appended = self.source.read_from(self.offset)?;
        if appended.is_empty() {
            return Ok(false);
        }
        self.offset += appended.len() as u64;
        self.partial.extend_from_slice(&appended);
        Ok(self.drain_lines())
    }

    /// Reset to an empty transcript and re-read from the start.
    fn rewind(&mut self) {
        self.transcript = Transcript::default();
        // Everything downstream of item 0 changes, so the delta spans all.
        self.transcript.mark_changed(0);
        self.offset = 0;
        self.partial.clear();
    }

    /// Fold every complete line in `partial`, keeping any trailing fragment.
    fn drain_lines(&mut self) -> bool {
        let mut changed = false;
        let mut start = 0;
        while let Some(relative) = self.partial[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + relative;
            let line = String::from_utf8_lossy(&self.partial[start..end]);
            changed |= self.transcript.push_line(&line);
            start = end + 1;
        }
        if start > 0 {
            self.partial.drain(..start);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal message line with the given role and text.
    fn message(id: &str, role: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"message\",\"id\":\"{id}\",\"parentId\":null,\"timestamp\":\"t\",\
             \"message\":{{\"role\":\"{role}\",\"content\":\"{text}\",\"timestamp\":0}}}}\n"
        )
    }

    #[test]
    fn poll_reads_appended_lines_only_once() {
        let path =
            std::env::temp_dir().join(format!("pilus-session-tail-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, message("a", "user", "hello")).expect("write header");

        let mut tail = Tail::open(&path).expect("open");
        assert_eq!(tail.transcript().items.len(), 1);

        // Nothing new: a poll reports no change.
        assert!(!tail.poll().expect("poll idle"));

        // A partial line is held back until its newline lands.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen");
        file.write_all(b"{\"type\":\"message\",\"id\":\"b\"")
            .expect("partial write");
        assert!(!tail.poll().expect("poll partial"));
        assert_eq!(tail.transcript().items.len(), 1);

        file.write_all(
            b",\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":\"world\",\
              \"timestamp\":0}}\n",
        )
        .expect("finish line");
        assert!(tail.poll().expect("poll complete"));
        assert_eq!(tail.transcript().items.len(), 2);

        // Re-reading never duplicates.
        assert!(!tail.poll().expect("poll stable"));
        assert_eq!(tail.transcript().items.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_file_is_read_from_the_start() {
        let path =
            std::env::temp_dir().join(format!("pilus-session-reset-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            message("a", "user", "a long first conversation line"),
        )
        .expect("write");
        let mut tail = Tail::open(&path).expect("open");
        assert_eq!(tail.transcript().items.len(), 1);

        // A shorter replacement (rotation/truncation) must be re-read whole.
        std::fs::write(&path, message("b", "assistant", "hi")).expect("overwrite");
        assert!(tail.poll().expect("poll after truncation"));
        assert_eq!(tail.transcript().items.len(), 1);
        match &tail.transcript().items[0].kind {
            crate::entry::ItemKind::Assistant { text, .. } => assert_eq!(text, "hi"),
            other => panic!("expected assistant, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    /// A source over a shared in-memory buffer, like a remote read would be.
    /// The caller keeps the `Arc` so it can append after boxing.
    struct Memory(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Memory {
        fn with(text: String) -> (std::sync::Arc<std::sync::Mutex<Vec<u8>>>, Self) {
            let bytes = std::sync::Arc::new(std::sync::Mutex::new(text.into_bytes()));
            (bytes.clone(), Self(bytes))
        }
    }

    impl Source for Memory {
        fn byte_len(&mut self) -> Result<u64> {
            Ok(self.0.lock().map_or(0, |bytes| bytes.len() as u64))
        }
        fn read_from(&mut self, offset: u64) -> Result<Vec<u8>> {
            Ok(self
                .0
                .lock()
                .map(|bytes| bytes[offset as usize..].to_vec())
                .unwrap_or_default())
        }
        fn label(&self) -> String {
            "memory".to_string()
        }
    }

    /// Append `line` to the shared buffer and poll, asserting a change.
    fn grow(tail: &mut Tail, bytes: &std::sync::Mutex<Vec<u8>>, line: &str) {
        if let Ok(mut buffer) = bytes.lock() {
            buffer.extend_from_slice(line.as_bytes());
        }
        assert!(tail.poll().expect("poll"));
    }

    #[test]
    fn a_custom_source_drives_the_same_fold() {
        let tail = Tail::with_source(Box::new(Memory(
            std::sync::Arc::new(std::sync::Mutex::new(
                message("a", "user", "hi").into_bytes(),
            )),
        )))
        .expect("tail");
        assert_eq!(tail.transcript().items.len(), 1);
        assert_eq!(tail.label(), "memory");
    }

    #[test]
    fn appended_lines_mark_the_first_new_index() {
        let (bytes, memory) = Memory::with(message("a", "user", "hi"));
        let mut tail = Tail::with_source(Box::new(memory)).expect("tail");
        // The initial fold already marked every item; drain it first.
        tail.take_changed_from();

        grow(&mut tail, &bytes, &message("b", "user", "one"));
        grow(&mut tail, &bytes, &message("c", "assistant", "two"));
        assert_eq!(tail.take_changed_from(), Some(1));
    }

    #[test]
    fn an_outcome_attaching_to_an_earlier_call_marks_its_index() {
        let call = "{\"type\":\"message\",\"id\":\"c\",\"timestamp\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"t1\",\"name\":\"read\",\"arguments\":{}}],\"timestamp\":0}}\n";
        let (bytes, memory) = Memory::with(format!("{}{}", message("a", "user", "go"), call));
        let mut tail = Tail::with_source(Box::new(memory)).expect("tail");
        tail.take_changed_from();

        grow(
            &mut tail,
            &bytes,
            "{\"type\":\"message\",\"id\":\"r\",\"timestamp\":\"t\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"t1\",\"content\":\"done\",\"timestamp\":0}}\n",
        );
        // The result rewrote the call at index 1, not the tail.
        assert_eq!(tail.take_changed_from(), Some(1));
    }

    #[test]
    fn a_usage_only_line_changes_no_item() {
        let (bytes, memory) = Memory::with(message("a", "user", "hi"));
        let mut tail = Tail::with_source(Box::new(memory)).expect("tail");
        tail.take_changed_from();

        grow(
            &mut tail,
            &bytes,
            "{\"type\":\"usage\",\"id\":\"u\",\"timestamp\":\"t\",\"usage\":{\"input\":5}}\n",
        );
        assert_eq!(tail.take_changed_from(), None);
        assert_eq!(tail.transcript().usage.input, 5);
    }

    #[test]
    fn a_rewind_marks_the_whole_transcript() {
        let (bytes, memory) = Memory::with(message("a", "user", "a long first line"));
        let mut tail = Tail::with_source(Box::new(memory)).expect("tail");
        tail.take_changed_from();

        // A shorter replacement forces a rewind and a from-0 delta.
        if let Ok(mut buffer) = bytes.lock() {
            buffer.clear();
            buffer.extend_from_slice(message("b", "assistant", "hi").as_bytes());
        }
        assert!(tail.poll().expect("poll"));
        assert_eq!(tail.take_changed_from(), Some(0));
    }
}
