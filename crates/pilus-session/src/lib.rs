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
mod rpc;
mod tail;

pub use discover::{
    SessionListing, dir_name_for, find_any_by_id, find_by_id, find_by_partial_id, first_prompt_in,
    id_from_path, latest, list, listing, resolve, sessions_dir, sessions_root,
};
pub use entry::{Header, Item, ItemKind, ToolOutcome, Transcript, TranscriptMeta, Usage};
pub use rpc::{
    ContextUsage, DeliverAs, PROTOCOL, PiModel, PiState, RpcCommand, RpcEvent, Sidecar, ToolEvent,
    ToolPhase, agent_dir, live_items, models_from, parse_event, sidecar_path, socket_path,
};
pub use tail::{Error, FileSource, Result, Source, Tail};
