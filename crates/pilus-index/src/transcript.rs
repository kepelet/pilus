//! A session's conversation, read from the end of its transcript for a window
//! to draw.
//!
//! Like [`last_reply_local`](crate::last_reply_local) this reads on demand and
//! keeps nothing: the caller owns the permission (`Settings::index_agents`),
//! and what comes back is shown and dropped. Only the end of the file is read,
//! a *window* of [`WINDOW`] bytes to begin with, so a transcript of many
//! megabytes costs one seek. A reader that wants older entries asks again with
//! a larger window, up to [`MAX_WINDOW`], and the entries it gets end with the
//! ones it had. Reasoning is left out; a tool call and its result are one
//! entry. A line that does not parse is skipped, because a
//! transcript being written can end mid-line.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use serde_json::Value;

use pilus_client::SshTarget;

use crate::error::{Error, Result};
use crate::model::Agent;
use crate::parse::iso_seconds;
use crate::scan::run_remote;

/// How much of the end of a transcript is read at first.
pub const WINDOW: u64 = 4 * 1024 * 1024;
/// The most that is read, however far back a reader asks.
pub const MAX_WINDOW: u64 = 128 * 1024 * 1024;
/// Longest text kept for a message, in characters.
const TEXT_CHARS: usize = 24_000;
/// Longest tool result kept, in characters.
const OUTPUT_CHARS: usize = 6_000;
/// Longest one-line summary of a tool call, in characters.
const SUMMARY_CHARS: usize = 160;

/// One thing said or done in a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entry {
    /// What the person asked.
    User { text: String, at: i64 },
    /// What the agent answered, as Markdown.
    Assistant { text: String, at: i64 },
    /// A tool the agent ran, with what it returned once it has.
    Tool {
        name: String,
        /// The arguments that matter, on one line.
        summary: String,
        output: Option<String>,
        failed: bool,
        at: i64,
    },
}

/// The newest part of a session's conversation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// The file goes on before the window that was read.
    pub earlier: bool,
}

/// The conversation in the last `window` bytes of the local transcript at `path`.
pub fn read_transcript_local(path: &str, agent: Agent, window: u64) -> Result<Transcript> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(window.min(MAX_WINDOW));
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(from_tail(agent, &bytes, start > 0))
}

/// The conversation in the last `window` bytes of the transcript at `path` on a
/// remote host, cut out by `tail` there so only that crosses the wire. The path
/// travels on standard input, never inside the command.
pub fn read_transcript_remote(
    target: &SshTarget,
    path: &str,
    agent: Agent,
    window: u64,
) -> Result<Transcript> {
    if path.contains('\n') {
        return Err(Error::Remote("a path with a line break".to_string()));
    }
    let window = window.min(MAX_WINDOW);
    let script = format!("IFS= read -r p; tail -c {window} \"$p\"");
    let bytes = run_remote(target, &script, &format!("{path}\n"))?;
    // A full window means the file goes on before it.
    let cut = bytes.len() as u64 >= window;
    Ok(from_tail(agent, &bytes, cut))
}

/// Entries of the end of a transcript. `cut` says the bytes start somewhere
/// inside the file, so the first line is a fragment.
fn from_tail(agent: Agent, bytes: &[u8], cut: bool) -> Transcript {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    if cut {
        lines.next();
    }
    let mut transcript = entries_in(agent, lines);
    transcript.earlier |= cut;
    transcript
}

/// Fold transcript `lines` into entries.
pub fn entries_in<'a>(agent: Agent, lines: impl Iterator<Item = &'a str>) -> Transcript {
    let mut entries: Vec<Entry> = Vec::new();
    // Where each tool call sits in `entries`, by call id, for its result.
    let mut calls: HashMap<String, usize> = HashMap::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some((role, content)) = message_of(agent, &value) else {
            continue;
        };
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso_seconds)
            .unwrap_or(0);
        match (role, content) {
            ("user", Value::String(text)) => push_user(&mut entries, text, at),
            ("assistant", Value::String(text)) => push_assistant(&mut entries, text, at),
            (_, Value::Array(blocks)) => {
                for block in blocks {
                    fold_block(&mut entries, &mut calls, role, block, at);
                }
            }
            _ => {}
        }
        // A pi tool result is a message of its own, with its call's id beside
        // the content rather than in a block.
        if role == "toolResult" {
            let id = value
                .get("message")
                .and_then(|message| message.get("toolCallId"))
                .and_then(Value::as_str);
            let failed = value
                .get("message")
                .and_then(|message| message.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let (Some(id), Some(Value::Array(blocks))) = (id, Some(content)) {
                attach(&mut entries, &calls, id, &blocks_text(blocks), failed);
            }
        }
    }
    Transcript {
        entries,
        earlier: false,
    }
}

/// The role and content of a line that is a conversation message.
fn message_of(agent: Agent, value: &Value) -> Option<(&str, &Value)> {
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let kind = value.get("type")?.as_str()?;
    let wanted = match agent {
        Agent::Claude => kind == "user" || kind == "assistant",
        Agent::Pi => kind == "message",
    };
    if !wanted {
        return None;
    }
    let message = value.get("message")?;
    let role = message.get("role")?.as_str()?;
    Some((role, message.get("content")?))
}

/// One content block. `tool_use` and `toolCall` are the same thing in the two
/// agents' words, as are `tool_result` blocks (Claude's, inside a user line)
/// and pi's `toolResult` messages (handled by the caller).
fn fold_block(
    entries: &mut Vec<Entry>,
    calls: &mut HashMap<String, usize>,
    role: &str,
    block: &Value,
    at: i64,
) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            match role {
                "user" => push_user(entries, text, at),
                "assistant" => push_assistant(entries, text, at),
                _ => {}
            }
        }
        Some("tool_use" | "toolCall") => {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let input = block.get("input").or_else(|| block.get("arguments"));
            entries.push(Entry::Tool {
                summary: summary_of(input),
                name,
                output: None,
                failed: false,
                at,
            });
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                calls.insert(id.to_string(), entries.len() - 1);
            }
        }
        Some("tool_result") => {
            let id = block.get("tool_use_id").and_then(Value::as_str);
            let failed = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = match block.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(blocks)) => blocks_text(blocks),
                _ => String::new(),
            };
            if let Some(id) = id {
                attach(entries, calls, id, &text, failed);
            }
        }
        _ => {}
    }
}

/// Give the call `id` its result.
fn attach(
    entries: &mut [Entry],
    calls: &HashMap<String, usize>,
    id: &str,
    text: &str,
    failed: bool,
) {
    if let Some(Entry::Tool {
        output,
        failed: was,
        ..
    }) = calls.get(id).and_then(|at| entries.get_mut(*at))
    {
        *output = Some(clip(text.trim(), OUTPUT_CHARS));
        *was = failed;
    }
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a user line is the harness talking (a slash command, a hook, a
/// reminder) rather than the person.
fn is_tagged(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

fn push_user(entries: &mut Vec<Entry>, text: &str, at: i64) {
    let text = text.trim();
    if text.is_empty() || is_tagged(text) {
        return;
    }
    entries.push(Entry::User {
        text: clip(text, TEXT_CHARS),
        at,
    });
}

fn push_assistant(entries: &mut Vec<Entry>, text: &str, at: i64) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    entries.push(Entry::Assistant {
        text: clip(text, TEXT_CHARS),
        at,
    });
}

/// The arguments of a call that say what it did: its command, file or
/// pattern, else the start of its JSON.
fn summary_of(input: Option<&Value>) -> String {
    const KEYS: [&str; 8] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ];
    let Some(input) = input else {
        return String::new();
    };
    let picked = KEYS
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    let flat = picked.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&flat, SUMMARY_CHARS)
}

/// `text` cut to `limit` characters, with an ellipsis when it was cut.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(agent: Agent, lines: &[&str]) -> Transcript {
        entries_in(agent, lines.iter().copied())
    }

    #[test]
    fn a_claude_turn_pairs_its_tool_call_with_the_result_and_skips_the_noise() {
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-21T02:05:00.000Z","message":{"role":"user","content":"Fix the build"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo  build"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"error[E0425]","is_error":true}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"queue-operation"}"#,
            "not json",
        ];
        let transcript = read(Agent::Claude, &lines);
        assert_eq!(transcript.entries.len(), 3);
        assert!(
            matches!(&transcript.entries[0], Entry::User { text, at } if text == "Fix the build" && *at == 1_792_548_300)
        );
        assert!(
            matches!(&transcript.entries[1], Entry::Assistant { text, .. } if text == "Looking.")
        );
        assert_eq!(
            transcript.entries[2],
            Entry::Tool {
                name: "Bash".to_string(),
                summary: "cargo build".to_string(),
                output: Some("error[E0425]".to_string()),
                failed: true,
                at: 0,
            }
        );
    }

    #[test]
    fn a_pi_tool_result_is_a_message_of_its_own() {
        let lines = [
            r#"{"type":"session","id":"x"}"#,
            r#"{"type":"message","message":{"role":"system","content":""}}"#,
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"look"}]}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"src/main.rs"}}]}}"#,
            r#"{"type":"message","message":{"role":"toolResult","toolCallId":"c1","content":[{"type":"text","text":"fn main() {}"}],"isError":false}}"#,
        ];
        let transcript = read(Agent::Pi, &lines);
        assert_eq!(transcript.entries.len(), 2);
        assert_eq!(
            transcript.entries[1],
            Entry::Tool {
                name: "read".to_string(),
                summary: "src/main.rs".to_string(),
                output: Some("fn main() {}".to_string()),
                failed: false,
                at: 0,
            }
        );
    }

    #[test]
    fn a_window_cut_mid_file_drops_its_first_fragment() {
        let tail =
            "ll\"}}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        let cut = from_tail(Agent::Claude, tail.as_bytes(), true);
        assert_eq!(cut.entries.len(), 1);
        assert!(cut.earlier);
        let whole = from_tail(
            Agent::Claude,
            tail.lines().nth(1).unwrap().as_bytes(),
            false,
        );
        assert_eq!(whole.entries, cut.entries);
        assert!(!whole.earlier);
    }
}
