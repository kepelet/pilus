//! A non-terminal reading of a pi session.
//!
//! Pi persists a conversation as a JSONL file: one typed object per line,
//! linked into a tree by `id`/`parentId`, grouped on disk by working
//! directory under `~/.pi/agent/sessions/--<cwd>--/`. This crate turns that
//! file into a flat, ordered [`Transcript`] a native surface can paint, and
//! keeps it current with [`Tail`].
//!
//! Nothing here knows about GPUI or the Luvus daemon, so the parser is
//! exercised headlessly in `tests/`.
//!
//! The wire vocabulary is documented upstream in pi's `docs/session-format.md`
//! and `docs/message-types.md`; the parser is deliberately tolerant of unknown
//! entry types and message roles so a newer pi still renders.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod discover;
mod entry;
mod tail;

pub use discover::{dir_name_for, find_by_id, latest, list, resolve, sessions_dir, sessions_root};
pub use entry::{Header, Item, ItemKind, ToolOutcome, Transcript, Usage};
pub use tail::{Error, FileSource, Result, Source, Tail};
