//! The pilus extension's RPC protocol: newline-delimited JSON over a unix
//! socket pi serves next to its session file.
//!
//! The file tail tells the window what pi *wrote*; this channel reports what
//! pi *is* — busy, blocked, mid-stream — and takes commands (prompt, abort,
//! model and thinking changes, catalog) without round-tripping the daemon's
//! terminal. Every type here is data only: IO lives in the GUI's reader
//! thread, so the parser stays testable without a socket.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::entry::{Item, Transcript};

/// The protocol revision the extension writes in its hello and sidecar.
pub const PROTOCOL: u32 = 1;

/// The extension's agent directory: `PI_CODING_AGENT_DIR` when pi is
/// relocated, else the default `~/.pi/agent`.
pub fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    home().join(".pi/agent")
}

/// The user's home directory, or the current directory when unset.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The RPC socket the extension listens on for one session.
pub fn socket_path(agent_dir: &Path, session_id: &str) -> PathBuf {
    agent_dir.join("pilus").join(format!("{session_id}.sock"))
}

/// The sidecar pi writes next to the socket, used to reach it remotely.
pub fn sidecar_path(agent_dir: &Path, session_id: &str) -> PathBuf {
    agent_dir.join("pilus").join(format!("{session_id}.json"))
}

/// Where the socket is advertised from, so a remote client can reach the
/// extension without guessing paths.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    pub protocol: u32,
    /// pi's node process, for liveness checks.
    pub pid: u64,
    /// Absolute path of the socket.
    pub socket: String,
    /// pi's node binary, which can relay stdio onto the socket.
    pub exec_path: String,
    /// The pane this pi runs in; `None` on extensions older than the pane
    /// field, or a pi outside the daemon's panes.
    pub pane: Option<String>,
    /// The session the socket streams; the file name says it too, but a
    /// scanned sidecar needs it without trusting the name.
    pub session_id: Option<String>,
}

impl Sidecar {
    /// Parse one sidecar document. Anything unexpected is `None`: a stale or
    /// foreign file is indistinguishable from a missing one.
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// pi's current model, as reported by `state` and `models`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    /// The context window in tokens; absent on models that do not say.
    pub context_window: Option<u64>,
    #[serde(default)]
    pub reasoning: bool,
}

/// The context-window occupancy `state` reports.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Tokens used; null right after a compaction, before pi re-counts.
    pub tokens: Option<u64>,
    #[serde(default)]
    pub context_window: u64,
    /// The fraction used (0–1), when pi computed it.
    pub percent: Option<f64>,
}

/// A snapshot of what pi is doing, pushed whenever it changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PiState {
    /// A turn is running.
    #[serde(default)]
    pub busy: bool,
    /// pi is waiting on the user (a permission prompt, a question).
    #[serde(default)]
    pub blocked: bool,
    /// Prompts are queued behind the current turn.
    #[serde(default)]
    pub pending: bool,
    /// The selected model; null before pi has decided.
    pub model: Option<PiModel>,
    /// The selected thinking level.
    pub thinking: Option<String>,
    /// Context-window occupancy.
    pub context: Option<ContextUsage>,
}

/// Which part of a tool run an event reports.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Start,
    Update,
    End,
}

/// A tool run's progress, pushed as pi produces output.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolEvent {
    pub phase: ToolPhase,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Output so far; empty while the tool is still silent.
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

/// The wire shape: one object per line, discriminated by `type`. Kept
/// private so the public [`RpcEvent`] does not inherit serde quirks like
/// `stream`'s nested payload.
#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum Wire {
    Hello {
        #[allow(dead_code)]
        protocol: u32,
        session_id: String,
        session_file: String,
        pid: u64,
    },
    State(Box<PiState>),
    Stream {
        message: Value,
    },
    StreamEnd,
    Tool(Box<ToolEvent>),
    Response {
        id: String,
        ok: bool,
        error: Option<String>,
        data: Option<Value>,
    },
}

/// One parsed line from the extension.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcEvent {
    /// The socket's greeting; `session_file` ties the channel to the file
    /// the reader tails.
    Hello {
        session_id: String,
        session_file: String,
        pid: u64,
    },
    /// pi's state changed.
    State(Box<PiState>),
    /// A partial assistant message while pi streams (the same shape a
    /// session-file `message` entry carries).
    Stream(Value),
    /// The streaming turn ended; the file is about to hold it.
    StreamEnd,
    /// A tool run started, produced output, or finished.
    Tool(Box<ToolEvent>),
    /// The reply to a command, keyed by the id the caller sent.
    Response {
        id: String,
        ok: bool,
        error: Option<String>,
        data: Option<Value>,
    },
}

/// Parse one line of the socket's output. Unknown types and malformed JSON
/// are `None`: the extension may be newer than this build, and a bad line
/// must not kill the channel.
pub fn parse_event(line: &str) -> Option<RpcEvent> {
    let wire = serde_json::from_str::<Wire>(line.trim()).ok()?;
    Some(match wire {
        Wire::Hello {
            session_id,
            session_file,
            pid,
            ..
        } => RpcEvent::Hello {
            session_id,
            session_file,
            pid,
        },
        Wire::State(state) => RpcEvent::State(state),
        Wire::Stream { message } => RpcEvent::Stream(message),
        Wire::StreamEnd => RpcEvent::StreamEnd,
        Wire::Tool(tool) => RpcEvent::Tool(tool),
        Wire::Response {
            id,
            ok,
            error,
            data,
        } => RpcEvent::Response {
            id,
            ok,
            error,
            data,
        },
    })
}

/// How a prompt lands when pi is already working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverAs {
    /// Interrupt the current turn and feed the prompt in.
    Steer,
    /// Run the prompt after the current turn finishes.
    FollowUp,
}

/// A command the client sends, one object per line with an id.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcCommand {
    Prompt {
        text: String,
        deliver_as: Option<DeliverAs>,
    },
    Abort,
    /// Ask pi to open a fresh session, like typing `/new` in the editor.
    New,
    SetModel {
        provider: String,
        model: String,
    },
    SetThinking {
        level: String,
    },
    Models,
    State,
}

impl RpcCommand {
    /// The JSON line to write, with `id` as the caller-assigned request id.
    /// `deliverAs` is omitted when the prompt is not mid-turn.
    pub fn to_line(&self, id: &str) -> String {
        let mut object = Map::new();
        object.insert("id".to_string(), json!(id));
        match self {
            RpcCommand::Prompt { text, deliver_as } => {
                object.insert("cmd".to_string(), json!("prompt"));
                object.insert("text".to_string(), json!(text));
                if let Some(deliver_as) = deliver_as {
                    object.insert(
                        "deliverAs".to_string(),
                        json!(match deliver_as {
                            DeliverAs::Steer => "steer",
                            DeliverAs::FollowUp => "followUp",
                        }),
                    );
                }
            }
            RpcCommand::Abort => {
                object.insert("cmd".to_string(), json!("abort"));
            }
            RpcCommand::New => {
                object.insert("cmd".to_string(), json!("new"));
            }
            RpcCommand::SetModel { provider, model } => {
                object.insert("cmd".to_string(), json!("set_model"));
                object.insert("provider".to_string(), json!(provider));
                object.insert("model".to_string(), json!(model));
            }
            RpcCommand::SetThinking { level } => {
                object.insert("cmd".to_string(), json!("set_thinking"));
                object.insert("level".to_string(), json!(level));
            }
            RpcCommand::Models => {
                object.insert("cmd".to_string(), json!("models"));
            }
            RpcCommand::State => {
                object.insert("cmd".to_string(), json!("state"));
            }
        }
        Value::Object(object).to_string()
    }
}

/// The `models` response's catalog. Anything that does not look like a
/// model is skipped rather than failing the list.
pub fn models_from(data: &Value) -> Vec<PiModel> {
    data.get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| serde_json::from_value(model.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Fold pi's partial assistant message into transcript items, the same way
/// the file fold renders it once it lands. The synthetic entry id keeps the
/// items self-consistent for rendering only.
pub fn live_items(message: &Value) -> Vec<Item> {
    let mut transcript = Transcript::default();
    transcript.push_line(
        &json!({"type": "message", "id": "live", "timestamp": "", "message": message}).to_string(),
    );
    transcript.items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::ItemKind;

    const HELLO: &str = "{\"type\":\"hello\",\"protocol\":1,\"sessionId\":\"01a1063d-0f35-7510-98af-3ad569b0ebed\",\"sessionFile\":\"/tmp/s/2026-10-04T09-27-07-317Z_01a1063d-0f35-7510-98af-3ad569b0ebed.jsonl\",\"pid\":31755}";
    const STATE: &str = "{\"type\":\"state\",\"busy\":false,\"blocked\":false,\"pending\":false,\"model\":{\"provider\":\"opencode-go\",\"id\":\"muse-spark-1.3-contributor\",\"name\":\"Muse Spark 1.3 Contributor\",\"contextWindow\":1048576,\"reasoning\":true},\"thinking\":\"high\",\"context\":{\"tokens\":7367,\"contextWindow\":1048576,\"percent\":0.7025718688964844}}";
    const TOOL_START: &str = "{\"type\":\"tool\",\"phase\":\"start\",\"toolCallId\":\"call_01a1063e009976799781a6b7\",\"toolName\":\"bash\",\"output\":\"\",\"isError\":false}";
    const TOOL_END: &str = "{\"type\":\"tool\",\"phase\":\"end\",\"toolCallId\":\"call_01a1063e009976799781a6b7\",\"toolName\":\"bash\",\"output\":\"tool-ok\\n\",\"isError\":false}";
    const MODELS_REPLY: &str = "{\"type\":\"response\",\"id\":\"2\",\"ok\":true,\"data\":{\"models\":[{\"provider\":\"minimax\",\"id\":\"MiniMax-M2.7\",\"name\":\"MiniMax-M2.7\",\"contextWindow\":204800,\"reasoning\":true}]}}";
    const ERROR_REPLY: &str =
        "{\"type\":\"response\",\"id\":\"5\",\"ok\":false,\"error\":\"unknown command bogus\"}";

    #[test]
    fn the_fixture_lines_parse() {
        let Some(RpcEvent::Hello {
            session_id,
            session_file,
            pid,
        }) = parse_event(HELLO)
        else {
            panic!("hello");
        };
        assert_eq!(session_id, "01a1063d-0f35-7510-98af-3ad569b0ebed");
        assert!(session_file.ends_with(".jsonl"));
        assert_eq!(pid, 31755);

        let Some(RpcEvent::State(state)) = parse_event(STATE) else {
            panic!("state");
        };
        assert!(!state.busy);
        assert_eq!(state.thinking.as_deref(), Some("high"));
        let model = state.model.expect("model");
        assert_eq!(model.provider, "opencode-go");
        assert_eq!(model.id, "muse-spark-1.3-contributor");
        assert_eq!(model.context_window, Some(1048576));
        let context = state.context.expect("context");
        assert_eq!(context.tokens, Some(7367));
        assert_eq!(context.context_window, 1048576);
        assert!(context.percent.is_some());

        let Some(RpcEvent::Tool(tool)) = parse_event(TOOL_START) else {
            panic!("tool start");
        };
        assert_eq!(tool.phase, ToolPhase::Start);
        assert_eq!(tool.tool_call_id, "call_01a1063e009976799781a6b7");
        assert_eq!(tool.tool_name, "bash");

        let Some(RpcEvent::Tool(tool)) = parse_event(TOOL_END) else {
            panic!("tool end");
        };
        assert_eq!(tool.phase, ToolPhase::End);
        assert_eq!(tool.output, "tool-ok\n");

        assert_eq!(
            parse_event("{\"type\":\"stream_end\"}"),
            Some(RpcEvent::StreamEnd)
        );

        let Some(RpcEvent::Response { id, ok, data, .. }) = parse_event(MODELS_REPLY) else {
            panic!("models reply");
        };
        assert_eq!(id, "2");
        assert!(ok);
        let models = models_from(&data.expect("data"));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "MiniMax-M2.7");

        let Some(RpcEvent::Response { id, ok, error, .. }) = parse_event(ERROR_REPLY) else {
            panic!("error reply");
        };
        assert_eq!(id, "5");
        assert!(!ok);
        assert_eq!(error.as_deref(), Some("unknown command bogus"));
    }

    #[test]
    fn sparse_state_and_unknown_lines_are_safe() {
        // `model` and `context.tokens` may be null right after a compaction.
        let Some(RpcEvent::State(state)) = parse_event(
            "{\"type\":\"state\",\"busy\":true,\"model\":null,\"context\":{\"tokens\":null}}",
        ) else {
            panic!("sparse state");
        };
        assert!(state.busy);
        assert!(state.model.is_none());
        assert_eq!(state.context.and_then(|c| c.tokens), None);

        assert_eq!(parse_event("{\"type\":\"mystery\"}"), None);
        assert_eq!(parse_event("not json"), None);
        assert_eq!(parse_event(""), None);
        assert_eq!(parse_event("{\"type\":\"state\""), None);
    }

    #[test]
    fn commands_serialize_to_the_protocol_shape() {
        let as_json = |command: &RpcCommand, id: &str| {
            serde_json::from_str::<Value>(&command.to_line(id)).expect("json")
        };
        assert_eq!(
            as_json(
                &RpcCommand::Prompt {
                    text: "hi".to_string(),
                    deliver_as: None,
                },
                "1",
            ),
            json!({"id": "1", "cmd": "prompt", "text": "hi"}),
        );
        assert_eq!(
            as_json(
                &RpcCommand::Prompt {
                    text: "hi".to_string(),
                    deliver_as: Some(DeliverAs::Steer),
                },
                "2",
            ),
            json!({"id": "2", "cmd": "prompt", "text": "hi", "deliverAs": "steer"}),
        );
        assert_eq!(
            as_json(
                &RpcCommand::Prompt {
                    text: "hi".to_string(),
                    deliver_as: Some(DeliverAs::FollowUp),
                },
                "3",
            ),
            json!({"id": "3", "cmd": "prompt", "text": "hi", "deliverAs": "followUp"}),
        );
        assert_eq!(
            as_json(&RpcCommand::Abort, "4"),
            json!({"id": "4", "cmd": "abort"}),
        );
        assert_eq!(
            as_json(
                &RpcCommand::SetModel {
                    provider: "anthropic".to_string(),
                    model: "opus".to_string(),
                },
                "5",
            ),
            json!({"id": "5", "cmd": "set_model", "provider": "anthropic", "model": "opus"}),
        );
        assert_eq!(
            as_json(
                &RpcCommand::SetThinking {
                    level: "high".to_string(),
                },
                "6",
            ),
            json!({"id": "6", "cmd": "set_thinking", "level": "high"}),
        );
        assert_eq!(
            as_json(&RpcCommand::Models, "7"),
            json!({"id": "7", "cmd": "models"}),
        );
        assert_eq!(
            as_json(&RpcCommand::State, "8"),
            json!({"id": "8", "cmd": "state"}),
        );
        assert_eq!(
            as_json(&RpcCommand::New, "9"),
            json!({"id": "9", "cmd": "new"}),
        );
    }

    #[test]
    fn live_items_fold_like_the_file() {
        let items = live_items(&json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "answer"},
                {"type": "toolCall", "id": "t1", "name": "read", "arguments": {"path": "a"}},
            ],
        }));
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0].kind, ItemKind::Thinking { .. }));
        assert!(matches!(items[1].kind, ItemKind::Assistant { .. }));
        assert!(matches!(items[2].kind, ItemKind::ToolCall { .. }));
    }

    #[test]
    fn a_sidecar_parses() {
        let sidecar = Sidecar::parse(
            "{\"protocol\":1,\"pid\":31755,\"socket\":\"/tmp/x.sock\",\"execPath\":\"/usr/bin/node\",\"pane\":\"86\",\"sessionId\":\"abc-123\"}",
        )
        .expect("sidecar");
        assert_eq!(sidecar.protocol, 1);
        assert_eq!(sidecar.pid, 31755);
        assert_eq!(sidecar.socket, "/tmp/x.sock");
        assert_eq!(sidecar.exec_path, "/usr/bin/node");
        assert_eq!(sidecar.pane.as_deref(), Some("86"));
        assert_eq!(sidecar.session_id.as_deref(), Some("abc-123"));

        // Older extensions write no pane or session id; both stay `None`.
        let old = Sidecar::parse(
            "{\"protocol\":1,\"pid\":31755,\"socket\":\"/tmp/x.sock\",\"execPath\":\"/usr/bin/node\"}",
        )
        .expect("old sidecar");
        assert!(old.pane.is_none());
        assert!(old.session_id.is_none());

        // A pi outside the daemon's panes reports a null pane.
        let free = Sidecar::parse(
            "{\"protocol\":1,\"pid\":31755,\"socket\":\"/tmp/x.sock\",\"execPath\":\"/usr/bin/node\",\"pane\":null,\"sessionId\":\"abc-123\"}",
        )
        .expect("free sidecar");
        assert!(free.pane.is_none());

        assert!(Sidecar::parse("{}").is_none());
    }
}
