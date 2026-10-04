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

    #[test]
    fn a_custom_source_drives_the_same_fold() {
        /// A source over an in-memory buffer, like a remote read would be.
        struct Memory(Vec<u8>);
        impl Source for Memory {
            fn byte_len(&mut self) -> Result<u64> {
                Ok(self.0.len() as u64)
            }
            fn read_from(&mut self, offset: u64) -> Result<Vec<u8>> {
                Ok(self.0[offset as usize..].to_vec())
            }
            fn label(&self) -> String {
                "memory".to_string()
            }
        }
        let tail = Tail::with_source(Box::new(Memory(message("a", "user", "hi").into_bytes())))
            .expect("tail");
        assert_eq!(tail.transcript().items.len(), 1);
        assert_eq!(tail.label(), "memory");
    }
}
