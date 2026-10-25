//! The client for sipil, the durable agent daemon, and for other pilus windows,
//! which are sipils too.
//!
//! Every node speaks the same small protocol over iroh (see [`wire`]): a sipil
//! daemon (a Node service over pi-durable) serves agents and sessions, and a
//! pilus window serves chat with its contacts. A node is addressed
//! by its device ID, so there is no URL and no port, and who is calling is
//! whatever device ID the handshake proved. None of this touches
//! `pilus-protocol`. Every call blocks, with a deadline; the window runs them on
//! a worker thread, never on the GUI thread.
//!
//! Provider keys and the model are the daemon's own business, set on its
//! machine. There is no call here to read, set or choose either.

use std::str::FromStr;
use std::sync::Arc;

use iroh::{EndpointAddr, EndpointId};
use iroh_tickets::endpoint::EndpointTicket;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod backoff;
pub mod channels;
pub mod chat;
pub mod contacts;
pub mod history;
pub mod mention;
pub mod node;
pub mod offline;
pub mod peer;
mod store;
pub mod watch;
pub mod wire;

pub use backoff::{Backoff, Policy};
pub use channels::{Channel, Channels, Learned};
pub use chat::{Chats, Cursor, Line};
pub use contacts::{Contact, Directory, Pairing, Request};
pub use node::{CallError, Node, NodeConfig, RelayChoice, device_id};
pub use offline::{Offline, Queued, RememberedAgent};
pub use peer::{Conversation, Delivery, Event, Local, LocalOptions, SharedDaemon};
pub use watch::{Next, RunningTool, WatchEvent, Watched, Watching};
pub use wire::{API_VERSION, Failure};

use node::CallError as Call;

/// Why a call to a sipil failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SipilError {
    /// The address is not something to dial.
    #[error("{0}")]
    Address(String),
    /// The network side of pilus is not running.
    #[error("{0}")]
    Offline(String),
    /// Nothing answered at the address.
    #[error("Could not reach {target}: {reason}")]
    Unreachable {
        /// The device that was dialed, shortened.
        target: String,
        /// The transport's own words.
        reason: String,
    },
    /// Something answered, but it is not a sipil.
    #[error("{target} answered, but it is not a sipil")]
    NotSipil {
        /// The device that was dialed, shortened.
        target: String,
    },
    /// The node is a pilus window where a daemon was wanted, or the reverse.
    #[error("{0}")]
    WrongKind(String),
    /// The node speaks another protocol revision than this pilus.
    #[error("That sipil speaks API {found}, but this pilus needs {API_VERSION}")]
    Version {
        /// The revision the node reported.
        found: u32,
    },
    /// The node refused the call, with its own explanation.
    #[error("{message}")]
    Refused {
        /// Its machine-readable code, such as `unauthorized`.
        code: String,
        /// Text meant for the person.
        message: String,
    },
    /// The node answered with a body this client does not understand.
    #[error("That sipil sent an answer pilus could not read")]
    Malformed,
}

impl SipilError {
    /// Whether the node knows who this device is and does not allow it.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if code == "unauthorized")
    }
}

/// What kind of node answered.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A sipil daemon: agents, sessions and keys.
    #[default]
    Daemon,
    /// A pilus window: a person to chat with.
    Pilus,
}

/// What a node reports about itself, to anyone who asks.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Health {
    /// Always `sipil`: every node is one.
    pub name: String,
    /// Whether it is a daemon or a pilus window.
    #[serde(default)]
    pub kind: NodeKind,
    /// The node's release.
    pub version: String,
    /// The protocol revision.
    pub api: u32,
    /// Whether the asker is allowed to use it.
    #[serde(default)]
    pub allowed: bool,
    /// What a pilus window calls its owner, if it says.
    #[serde(default)]
    pub nickname: Option<String>,
    /// For a daemon: what its primary agent is called.
    #[serde(default)]
    pub label: Option<String>,
}

/// Where a call goes: a device, and what is known of how to reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    addr: EndpointAddr,
    text: String,
}

impl Target {
    /// The device ID.
    pub fn id(&self) -> String {
        self.addr.id.to_string()
    }

    /// The address to dial.
    pub fn addr(&self) -> &EndpointAddr {
        &self.addr
    }

    /// What to remember so this target can be parsed again: the ticket when one
    /// was given, else the bare ID.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The ticket, if one was given. A bare ID carries no addresses.
    pub fn ticket(&self) -> Option<String> {
        (self.text != self.id()).then(|| self.text.clone())
    }

    /// The device ID shortened for sentences.
    pub fn short(&self) -> String {
        self.id().chars().take(8).collect()
    }
}

/// Read a device ID (64 hexadecimal characters) or a ticket, as pasted.
pub fn parse_target(input: &str) -> Result<Target, SipilError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(SipilError::Address(
            "Enter the device ID or ticket of the sipil.".to_string(),
        ));
    }
    if text.contains("://") || text.contains(char::is_whitespace) {
        return Err(SipilError::Address(
            "sipil is reached by device ID now, not by address. Paste the device ID or ticket it printed.".to_string(),
        ));
    }
    let is_id = text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit());
    if is_id {
        let id = EndpointId::from_str(&text.to_ascii_lowercase())
            .map_err(|_| SipilError::Address(format!("{text} is not a device ID.")))?;
        return Ok(Target {
            text: id.to_string(),
            addr: EndpointAddr::new(id),
        });
    }
    EndpointTicket::from_str(text)
        .map(|ticket| Target {
            addr: ticket.endpoint_addr().clone(),
            text: text.to_string(),
        })
        .map_err(|_| {
            SipilError::Address("That is not a device ID (64 hexadecimal characters).".to_string())
        })
}

/// `HH:MM` for a Unix time, in this machine's local time.
///
/// The offset is read once from `date +%z`, because the standard library has no
/// local time zone and reading one directly would need `unsafe`. It is the
/// offset now, so a line from before a daylight-saving change is an hour off.
pub fn clock_label(unix_secs: u64) -> String {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let offset = *OFFSET.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|output| parse_offset(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or(0)
    });
    let local = (i64::try_from(unix_secs).unwrap_or(0) + offset).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, local % 3600 / 60)
}

/// Seconds east of UTC from a `+HHMM` or `-HHMM` offset.
fn parse_offset(text: &str) -> Option<i64> {
    let text = text.trim();
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = text.get(1..5)?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) || text.len() != 5 {
        return None;
    }
    let (hours, minutes): (i64, i64) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Whether an agent or session has work in flight.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    /// Waiting for input.
    #[default]
    Idle,
    /// Running a turn.
    Working,
}

/// What an agent is to the user. Every agent of a daemon is one the person talks
/// to; the field stays because the daemon still sends it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// An agent the person talks to.
    Primary,
}

/// A model, as sipil names it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// The provider id.
    pub provider: String,
    /// The model within it.
    pub model_id: String,
}

/// A primary agent of a daemon.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    /// The conversation id.
    pub id: String,
    /// What to call it.
    pub name: String,
    /// What it is to the user.
    pub role: Role,
    /// Whether it is working.
    pub status: Activity,
    /// The model it runs, once one is chosen.
    pub model: Option<ModelRef>,
    /// Why its latest turn failed, until a later turn succeeds.
    #[serde(default)]
    pub error: Option<String>,
}

/// A model a daemon's keys unlock.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelDetail {
    /// The provider id.
    #[serde(default)]
    pub provider: String,
    /// What the provider is called.
    #[serde(default)]
    pub provider_name: String,
    /// The model within it.
    #[serde(default)]
    pub model_id: String,
    /// What the model is called.
    #[serde(default)]
    pub name: String,
}

/// An agent a daemon runs: what it is for and what it is given.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AgentDetail {
    /// The profile id, which names it when a session is started.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// The primary agent, or one it hands work to.
    pub role: Role,
    /// What it is for, in a sentence.
    #[serde(default)]
    pub summary: String,
    /// What it is told.
    #[serde(default)]
    pub instructions: String,
    /// The tools it is given, in the order the model sees them.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The extensions those tools come from.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// The model it runs on. A daemon from before this field leaves it out.
    #[serde(default)]
    pub model: Option<ModelDetail>,
}

/// What a daemon says about itself to a device it allows: the `about` call. A
/// daemon from before the call answers `unknown_method`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct About {
    /// What its primary agent is called.
    #[serde(default)]
    pub name: String,
    /// The machine it runs on.
    #[serde(default)]
    pub hostname: String,
    /// Its operating system.
    #[serde(default)]
    pub platform: String,
    /// Its device ID.
    #[serde(default)]
    pub device_id: String,
    /// Its release.
    #[serde(default)]
    pub version: String,
    /// The directory its agents' tools start in.
    #[serde(default)]
    pub workspace: String,
    /// Where it keeps its data.
    #[serde(default)]
    pub home: String,
    /// The model a conversation runs on when none is chosen, once a key is set.
    #[serde(default)]
    pub model: Option<ModelDetail>,
    /// The primary agent, then the agents it hands work to.
    #[serde(default)]
    pub agents: Vec<AgentDetail>,
    /// Its logo, as SVG text.
    #[serde(default)]
    pub logo: Option<String>,
}

/// A top-level conversation.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Session {
    /// The conversation id.
    pub id: String,
    /// The first thing said in it.
    pub title: String,
    /// Whether it is working.
    pub status: Activity,
    /// The model it runs, once one has been chosen.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// Why its latest turn failed, until a later turn succeeds.
    #[serde(default)]
    pub error: Option<String>,
}

/// An open piece of work on a daemon's board: what its primary agent is
/// carrying for the person (`../sipil/src/work.ts`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkItem {
    /// Its number on the board.
    pub id: u64,
    /// What the work is.
    pub title: String,
    /// `todo`, `doing`, `waiting` or `blocked`.
    pub state: String,
    /// Who is on it: the person, the agent, or a coding agent and where.
    #[serde(default)]
    pub owner: String,
    /// The next step.
    #[serde(default)]
    pub next: String,
    /// What it waits on, when waiting or blocked.
    #[serde(default)]
    pub waiting_on: String,
    /// When it last changed, in Unix milliseconds.
    #[serde(default)]
    pub updated_at: u64,
}

impl WorkItem {
    /// Whether it needs someone before it can go on.
    pub fn stuck(&self) -> bool {
        self.state == "waiting" || self.state == "blocked"
    }
}

/// A piece of work the agent took off its board, with how it ended.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClosedWork {
    /// Its number on the board.
    pub id: u64,
    /// What the work was.
    pub title: String,
    /// How it ended.
    #[serde(default)]
    pub outcome: String,
    /// When it was closed, in Unix milliseconds.
    #[serde(default)]
    pub closed_at: u64,
}

/// A daemon's board: the open work in the order the agent keeps it (what waits
/// on someone first), and what it closed lately.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct WorkBoard {
    /// Open items, blocked and waiting first.
    #[serde(default)]
    pub open: Vec<WorkItem>,
    /// Recently closed items, newest first.
    #[serde(default)]
    pub closed: Vec<ClosedWork>,
}

/// Recurring work a daemon's primary agent does on a schedule
/// (`../sipil/src/routines.ts`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    /// Its number.
    pub id: u64,
    /// What it is called.
    pub name: String,
    /// Its cron schedule.
    pub schedule: String,
    /// The time zone the schedule is read in.
    #[serde(default)]
    pub timezone: String,
    /// What each run does.
    #[serde(default)]
    pub prompt: String,
    /// Whether its runs are stopped.
    #[serde(default)]
    pub paused: bool,
    /// When it runs next, in Unix milliseconds.
    #[serde(default)]
    pub next_run_at: Option<u64>,
    /// When it last ran, in Unix milliseconds.
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// How the last run went: `reported`, `quiet` or `failed`.
    #[serde(default)]
    pub last_outcome: Option<String>,
}

/// Who said a transcript line.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ItemRole {
    /// The person.
    User,
    /// The model.
    Assistant,
    /// A tool's result.
    Tool,
    /// Another person, in a chat with a contact.
    Peer,
}

/// A call an assistant line made, with what it was given.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCall {
    /// The call's id, which its result names.
    pub id: String,
    /// The tool.
    pub name: String,
    /// One line on what the call is about: the command, the path, the task.
    #[serde(default)]
    pub summary: String,
    /// The arguments as JSON, cut to a readable length.
    #[serde(default)]
    pub args: String,
}

/// One line of a conversation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranscriptItem {
    /// The entry id, which only grows: a new line has a larger one.
    pub id: u64,
    /// Who said it.
    pub role: ItemRole,
    /// What was said. For a failed assistant line, the reason it failed.
    pub text: String,
    /// The tools an assistant line called, by name.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The same calls with their arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ToolCall>,
    /// On a tool result: the call it answers.
    #[serde(default, rename = "callId", skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// On a tool result: whether the call failed.
    #[serde(
        default,
        rename = "isError",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub is_error: bool,
    /// On a reply: the line it answers, as the sender quoted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Quote>,
    /// Set on an assistant line the provider failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Unix seconds when it was said, when the node knows.
    #[serde(default)]
    pub at: Option<u64>,
    /// Who said it when that is not simply "you" or "the other side": a person
    /// in a channel, or an agent in a chat between people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// What a reply answers: who said the line and the start of it, copied, because
/// each side numbers its lines on its own and a quote has to read alike on both.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Quote {
    /// Who said it, as the replier calls them.
    pub who: String,
    /// What was said, cut to [`QUOTE_LIMIT`] characters.
    pub text: String,
}

/// The most characters of a line a quote keeps.
pub const QUOTE_LIMIT: usize = 240;

impl Quote {
    /// A quote of `text` by `who`: one run of words, cut where it gets long.
    pub fn of(who: &str, text: &str) -> Self {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = if flat.chars().count() > QUOTE_LIMIT {
            let cut: String = flat.chars().take(QUOTE_LIMIT).collect();
            format!("{cut}…")
        } else {
            flat
        };
        Self {
            who: who.chars().filter(|c| !c.is_control()).take(60).collect(),
            text,
        }
    }

    /// Read a quote off a request, if it carries a usable one. It comes from the
    /// network, so it is cut and cleaned again here.
    pub fn from_params(params: &Value) -> Option<Self> {
        let quote = params.get("reply")?;
        let who = quote.get("who")?.as_str()?;
        let text = quote.get("text")?.as_str()?;
        let quote = Self::of(who, text);
        (!quote.text.is_empty()).then_some(quote)
    }

    /// The message with this quote in front, for a conversation that has no
    /// field for one: a Markdown block quote, which an agent reads as it is.
    pub fn prefix(&self, message: &str) -> String {
        format!("> {}: {}\n\n{message}", self.who, self.text)
    }
}

/// A conversation on a daemon, whichever list carries it, so a page learns what
/// it opened from this.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationInfo {
    /// The conversation id.
    pub id: String,
    /// What it goes by.
    pub name: String,
    /// What kind of conversation it is.
    pub role: ConversationRole,
    /// Whether it is working.
    pub status: Activity,
    /// The model it runs, once one is chosen.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The agent it runs as, when it was started as one.
    #[serde(default)]
    pub profile: Option<String>,
    /// Why its latest turn failed, until a later turn succeeds.
    #[serde(default)]
    pub error: Option<String>,
}

/// What kind of conversation a [`ConversationInfo`] is.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConversationRole {
    /// The primary agent's.
    Primary,
    /// A session.
    Session,
}

/// A handle on one sipil daemon.
#[derive(Clone)]
pub struct Sipil {
    node: Arc<Node>,
    target: Target,
}

impl Sipil {
    /// Prepare calls to the daemon at `address` (a device ID or a ticket) from
    /// the pilus node, which must be running.
    pub fn new(address: &str) -> Result<Self, SipilError> {
        let local = peer::local().ok_or_else(|| {
            SipilError::Offline("pilus is not online yet, so it cannot reach a sipil.".to_string())
        })?;
        Self::with_node(local.node(), address)
    }

    /// Prepare calls from `node`, for a node that is not the process's own.
    pub fn with_node(node: Arc<Node>, address: &str) -> Result<Self, SipilError> {
        Ok(Self {
            node,
            target: parse_target(address)?,
        })
    }

    /// Where calls go.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Check that the address is a sipil daemon this pilus can talk to and is
    /// allowed to use.
    pub fn connect(&self) -> Result<Health, SipilError> {
        let health = probe(&self.node, &self.target)?;
        match health.kind {
            NodeKind::Daemon => {}
            NodeKind::Pilus => {
                return Err(SipilError::WrongKind(
                    "That is another pilus, not a sipil agent. Add it as a contact to chat with it."
                        .to_string(),
                ));
            }
        }
        if !health.allowed {
            return Err(SipilError::Refused {
                code: "unauthorized".to_string(),
                message: not_allowed(self.node.id()),
            });
        }
        Ok(health)
    }

    /// The daemon's agents.
    pub fn agents(&self) -> Result<Vec<Agent>, SipilError> {
        Ok(self.call::<Agents>("agents", &json!({}))?.agents)
    }

    /// What this daemon says about itself.
    pub fn about(&self) -> Result<About, SipilError> {
        self.call("about", &json!({}))
    }

    /// The primary agent's board of open work. A daemon from before the board
    /// refuses it with `unknown_method`.
    pub fn work(&self) -> Result<WorkBoard, SipilError> {
        self.call("work", &json!({}))
    }

    /// The primary agent's routines, each with when it runs next. A daemon
    /// from before routines refuses it with `unknown_method`.
    pub fn routines(&self) -> Result<Vec<Routine>, SipilError> {
        Ok(self.call::<Routines>("routines", &json!({}))?.routines)
    }

    /// The top-level conversations.
    pub fn sessions(&self) -> Result<Vec<Session>, SipilError> {
        Ok(self.call::<Sessions>("sessions", &json!({}))?.sessions)
    }

    /// Start a conversation of its own.
    pub fn create_session(&self) -> Result<Session, SipilError> {
        self.call("sessions.create", &json!({}))
    }

    /// Say something to a conversation. sipil answers once it has taken the
    /// message, not when the model has replied: read [`Sipil::transcript`] for
    /// that. `request_id` makes a retry land on the same message instead of a
    /// second one. The daemon decides which model answers.
    pub fn send_message(
        &self,
        conversation: &str,
        content: &str,
        request_id: &str,
    ) -> Result<(), SipilError> {
        let params = json!({
            "id": conversation_id(conversation)?,
            "content": content,
            "requestId": request_id,
        });
        self.call::<Ack>("message", &params).map(|_| ())
    }

    /// The newest lines of a conversation, oldest first.
    pub fn transcript(&self, conversation: &str) -> Result<Vec<TranscriptItem>, SipilError> {
        Ok(self.transcript_of(conversation)?.0)
    }

    /// The newest lines of a conversation, and what the daemon says it is (an
    /// older daemon says nothing).
    pub fn transcript_of(
        &self,
        conversation: &str,
    ) -> Result<(Vec<TranscriptItem>, Option<ConversationInfo>), SipilError> {
        let params = json!({ "id": conversation_id(conversation)? });
        let found = self.call::<Transcript>("transcript", &params)?;
        Ok((found.items, found.conversation))
    }

    /// A page of a conversation's lines: the newest, or with `before` the ones
    /// before the line with that id.
    pub fn transcript_page(
        &self,
        conversation: &str,
        before: Option<u64>,
    ) -> Result<TranscriptPage, SipilError> {
        let mut params = json!({ "id": conversation_id(conversation)? });
        if let (Some(before), Some(object)) = (before, params.as_object_mut()) {
            object.insert("before".to_string(), json!(before));
        }
        let found = self.call::<Transcript>("transcript", &params)?;
        Ok(TranscriptPage {
            items: found.items,
            more: found.more,
            conversation: found.conversation,
        })
    }

    /// Follow a conversation as it happens, instead of polling its transcript.
    /// The first event is a snapshot. A daemon from before the call refuses it
    /// with `unknown_method` (see [`Watching::unsupported`]).
    pub fn watch(&self, conversation: &str) -> Result<Watching, SipilError> {
        let params = json!({ "id": conversation_id(conversation)? });
        let stream = self
            .node
            .watch(self.target.addr(), "watch", &params)
            .map_err(|error| call_error(&self.target, error))?;
        Ok(Watching::new(stream, self.target.clone()))
    }

    /// Stop a conversation's current work.
    pub fn abort(&self, conversation: &str) -> Result<(), SipilError> {
        let params = json!({ "id": conversation_id(conversation)? });
        self.call::<Ack>("abort", &params).map(|_| ())
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: &Value) -> Result<T, SipilError> {
        let result = self
            .node
            .call(self.target.addr(), method, params)
            .map_err(|error| call_error(&self.target, error))?;
        serde_json::from_value(result).map_err(|_| SipilError::Malformed)
    }
}

/// Ask the node at `target` how it is. Any node answers, daemon or pilus, allowed
/// or not.
pub fn probe(node: &Node, target: &Target) -> Result<Health, SipilError> {
    let result = node
        .call(target.addr(), "health", &json!({}))
        .map_err(|error| match error {
            // A pilus that has switched itself off answers `unavailable`.
            Call::Refused(Failure { code, .. }) if code == "unavailable" => {
                SipilError::Unreachable {
                    target: target.short(),
                    reason: "it is not taking calls".to_string(),
                }
            }
            // A node that refuses even `health`, or answers with something
            // else, is the wrong program.
            Call::Refused(_) | Call::Malformed => SipilError::NotSipil {
                target: target.short(),
            },
            other => call_error(target, other),
        })?;
    let health: Health = serde_json::from_value(result).map_err(|_| SipilError::NotSipil {
        target: target.short(),
    })?;
    if health.name != "sipil" {
        return Err(SipilError::NotSipil {
            target: target.short(),
        });
    }
    if health.api != API_VERSION {
        return Err(SipilError::Version { found: health.api });
    }
    Ok(health)
}

/// What to tell a person whose device a daemon does not know yet.
pub fn not_allowed(device_id: &str) -> String {
    format!(
        "This device is not allowed to use that sipil yet. On the machine running sipil, run: npm run connect -- --device-id {device_id}"
    )
}

/// Put a node's failure in this module's words.
fn call_error(target: &Target, error: Call) -> SipilError {
    match error {
        Call::Refused(Failure { code, message }) => SipilError::Refused { code, message },
        Call::Malformed => SipilError::Malformed,
        Call::Timeout => SipilError::Unreachable {
            target: target.short(),
            reason: "no answer".to_string(),
        },
        Call::Unreachable(reason) => SipilError::Unreachable {
            target: target.short(),
            reason,
        },
    }
}

#[derive(Deserialize)]
struct Agents {
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct Sessions {
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Routines {
    routines: Vec<Routine>,
}

#[derive(Deserialize)]
struct Transcript {
    items: Vec<TranscriptItem>,
    #[serde(default)]
    more: bool,
    #[serde(default)]
    conversation: Option<ConversationInfo>,
}

/// A page of a conversation's lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptPage {
    /// The lines, oldest first.
    pub items: Vec<TranscriptItem>,
    /// Whether the daemon has lines before the first of these. A daemon from
    /// before paging never says so.
    pub more: bool,
    /// What the daemon says the conversation is (an older daemon says nothing).
    pub conversation: Option<ConversationInfo>,
}

/// An answer whose body does not matter, only that it was not a refusal.
#[derive(Deserialize)]
struct Ack {}

/// A conversation id is a number; anything else would end up in the path.
fn conversation_id(id: &str) -> Result<&str, SipilError> {
    if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(SipilError::Address(format!(
            "{id:?} is not a conversation id."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "26e9ba62c5fedd08fce5f6eb4468d342f56712cea92c4efab93049ebd892258d";

    #[test]
    fn a_device_id_is_a_target_in_any_case() {
        let target = parse_target(&format!("  {}  ", ID.to_uppercase())).unwrap();
        assert_eq!(target.id(), ID);
        assert_eq!(target.text(), ID);
        assert_eq!(target.ticket(), None);
        assert_eq!(target.short(), "26e9ba62");
    }

    #[test]
    fn a_ticket_is_a_target_and_is_kept_to_dial_by() {
        let id = EndpointId::from_str(ID).unwrap();
        let ticket = EndpointTicket::new(EndpointAddr::new(id)).to_string();
        let target = parse_target(&ticket).unwrap();
        assert_eq!(target.id(), ID);
        assert_eq!(target.ticket().as_deref(), Some(ticket.as_str()));
    }

    #[test]
    fn urls_and_junk_are_not_targets_and_the_old_address_says_why() {
        assert!(parse_target("").is_err());
        assert!(parse_target("nonsense").is_err());
        assert!(parse_target(&ID[..63]).is_err());
        let SipilError::Address(message) = parse_target("http://127.0.0.1:4747").unwrap_err()
        else {
            panic!("an address error");
        };
        assert!(message.contains("device ID"), "{message}");
    }

    #[test]
    fn a_zone_offset_is_read_in_both_directions() {
        assert_eq!(parse_offset("+0700\n"), Some(25_200));
        assert_eq!(parse_offset("-0330"), Some(-12_600));
        assert_eq!(parse_offset("UTC"), None);
        assert_eq!(parse_offset("+07"), None);
    }

    #[test]
    fn only_a_number_is_a_conversation_id() {
        assert_eq!(conversation_id("17"), Ok("17"));
        assert!(conversation_id("").is_err());
        assert!(conversation_id("1/../providers").is_err());
        assert!(conversation_id("1?x=y").is_err());
    }
}
