//! Watching one conversation on a daemon, instead of polling its transcript.
//!
//! `watch` is the one call that does not answer once (see `../sipil`'s README):
//! the stream stays open and carries an event per line. The first is a
//! snapshot; every other applies on top of it, so a [`Watched`] is all a window
//! needs to keep: hand it each [`WatchEvent`] and it holds the transcript, what
//! the conversation is, and the answer being written. A daemon from before the
//! call answers `unknown_method`, which [`Watching::unsupported`] recognises, and
//! the window polls as it always did.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::node::{Line, WatchStream};
use super::wire::Failure;
use super::{Activity, ConversationInfo, ItemRole, SipilError, Target, TranscriptItem, call_error};

/// A tool call that has not finished, and what it has printed so far.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct RunningTool {
    /// The call's id, which its result will name.
    #[serde(rename = "callId")]
    pub call_id: String,
    /// The tool.
    #[serde(default)]
    pub name: String,
    /// The end of its output so far. The daemon cuts it to what a result shows,
    /// and says so at the start when it has.
    #[serde(default)]
    pub output: String,
}

/// One thing that happened in the watched conversation.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WatchEvent {
    /// The newest lines, what the conversation is, and the answer written so far.
    /// Replaces everything held.
    Snapshot {
        /// The newest lines, oldest first.
        items: Vec<TranscriptItem>,
        /// Whether older lines exist.
        #[serde(default)]
        more: bool,
        /// What the conversation is.
        #[serde(default)]
        conversation: Option<Box<ConversationInfo>>,
        /// The answer being written, if one is.
        #[serde(default)]
        live: Option<String>,
        /// The calls running, with what each has printed.
        #[serde(default)]
        tools: Vec<RunningTool>,
    },
    /// A line was committed.
    Item {
        /// The line.
        item: TranscriptItem,
    },
    /// More of the answer being written.
    Delta {
        /// What to append.
        text: String,
    },
    /// The whole answer being written so far, when a change was not an append.
    Live {
        /// What replaces the answer held.
        text: String,
    },
    /// The conversation started or stopped working.
    Status {
        /// What it is now.
        conversation: Box<ConversationInfo>,
    },
    /// A running call printed something.
    Output {
        /// The call.
        #[serde(rename = "callId")]
        call_id: String,
        /// The tool.
        #[serde(default)]
        name: String,
        /// Its output so far, which replaces what is held for it.
        text: String,
    },
    /// A `subagent` call's conversation now exists.
    Subagent {
        /// The call.
        #[serde(rename = "callId")]
        call_id: String,
        /// The conversation it started.
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
    /// The daemon is still there.
    Ping,
    /// An event from a newer daemon than this pilus.
    #[serde(other)]
    Unknown,
}

/// A conversation as a watch has told it so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Watched {
    /// The newest lines, oldest first.
    pub items: Vec<TranscriptItem>,
    /// Whether older lines exist than those held.
    pub more: bool,
    /// What the conversation is.
    pub info: Option<ConversationInfo>,
    /// The answer being written, which is not yet a line.
    pub live: String,
    /// The calls running, in the order they started, with what each has printed.
    pub tools: Vec<RunningTool>,
}

impl Watched {
    /// Fold one event in.
    pub fn apply(&mut self, event: WatchEvent) {
        match event {
            WatchEvent::Snapshot {
                items,
                more,
                conversation,
                live,
                tools,
            } => {
                self.items = items;
                self.more = more;
                self.info = conversation.map(|info| *info);
                self.live = live.unwrap_or_default();
                self.tools = tools;
            }
            WatchEvent::Item { item } => {
                // A finished answer replaces the one being written.
                if item.role == ItemRole::Assistant {
                    self.live.clear();
                }
                // A result is the end of what its call printed.
                if let Some(call) = &item.call_id {
                    self.tools.retain(|tool| tool.call_id != *call);
                }
                match self.items.binary_search_by_key(&item.id, |held| held.id) {
                    Ok(at) => self.items[at] = item,
                    Err(at) => self.items.insert(at, item),
                }
            }
            WatchEvent::Delta { text } => self.live.push_str(&text),
            WatchEvent::Live { text } => self.live = text,
            WatchEvent::Status { conversation } => {
                if conversation.status == Activity::Idle {
                    self.live.clear();
                    self.tools.clear();
                }
                self.info = Some(*conversation);
            }
            WatchEvent::Output {
                call_id,
                name,
                text,
            } => match self.tools.iter_mut().find(|tool| tool.call_id == call_id) {
                Some(tool) => tool.output = text,
                None => self.tools.push(RunningTool {
                    call_id,
                    name,
                    output: text,
                }),
            },
            WatchEvent::Subagent {
                call_id,
                conversation_id,
            } => {
                let started = self
                    .items
                    .iter_mut()
                    .flat_map(|item| item.calls.iter_mut())
                    .find(|call| call.id == call_id)
                    .and_then(|call| call.subagent.as_mut());
                if let Some(subagent) = started
                    && subagent.conversation.is_none()
                {
                    subagent.conversation = Some(conversation_id);
                }
            }
            WatchEvent::Ping | WatchEvent::Unknown => {}
        }
    }
}

/// What [`Watching::next`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Something happened.
    Event(Box<WatchEvent>),
    /// Nothing arrived in the time given.
    Idle,
    /// The daemon finished the watch.
    Ended,
}

/// An open watch of one conversation. Dropping it hangs up.
pub struct Watching {
    stream: WatchStream,
    target: Target,
}

impl Watching {
    pub(super) fn new(stream: WatchStream, target: Target) -> Self {
        Self { stream, target }
    }

    /// Whether `error` is an older daemon saying it has no `watch`.
    pub fn unsupported(error: &SipilError) -> bool {
        matches!(error, SipilError::Refused { code, .. } if code == "unknown_method")
    }

    /// The next event, waiting at most `wait` for one.
    ///
    /// A line the daemon sends as an error (the conversation is gone, this
    /// device is no longer allowed) is a [`SipilError::Refused`], and the watch is
    /// over.
    pub fn next(&mut self, wait: Duration) -> Result<Next, SipilError> {
        match self.stream.next(wait) {
            Ok(Line::Idle) => Ok(Next::Idle),
            Ok(Line::Ended) => Ok(Next::Ended),
            Ok(Line::Value(value)) => read(&value),
            Err(error) => Err(call_error(&self.target, error)),
        }
    }
}

/// One line of the stream: an event, the end, or the daemon's refusal.
fn read(value: &Value) -> Result<Next, SipilError> {
    if let Some(event) = value.get("event") {
        return serde_json::from_value(event.clone())
            .map(|event| Next::Event(Box::new(event)))
            .map_err(|_| SipilError::Malformed);
    }
    if value.get("result").is_some() {
        return Ok(Next::Ended);
    }
    if let Some(error) = value.get("error") {
        let text = |key: &str| error.get(key).and_then(Value::as_str).map(str::to_string);
        let failure = Failure {
            code: text("code").unwrap_or_else(|| "unknown".to_string()),
            message: text("message").unwrap_or_else(|| "The daemon refused.".to_string()),
        };
        return Err(SipilError::Refused {
            code: failure.code,
            message: failure.message,
        });
    }
    Err(SipilError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(id: u64, role: &str, text: &str) -> Value {
        json!({"id": id, "role": role, "text": text, "tools": []})
    }

    fn event(value: Value) -> WatchEvent {
        serde_json::from_value(value).expect("an event")
    }

    fn info(status: &str) -> Value {
        json!({"id": "7", "name": "Roadmap", "role": "session", "status": status})
    }

    #[test]
    fn a_snapshot_replaces_everything_and_events_build_on_it() {
        let mut watched = Watched::default();
        watched.apply(event(json!({
            "type": "snapshot", "items": [line(1, "user", "hi")], "more": true,
            "conversation": info("working"), "live": "Hel"
        })));
        assert_eq!(watched.items.len(), 1);
        assert!(watched.more);
        assert_eq!(watched.live, "Hel");
        assert_eq!(watched.info.as_ref().unwrap().status, Activity::Working);

        watched.apply(event(json!({"type": "delta", "text": "lo"})));
        assert_eq!(watched.live, "Hello");
        watched.apply(event(json!({"type": "live", "text": "Help"})));
        assert_eq!(
            watched.live, "Help",
            "a live event replaces rather than appends"
        );

        // The finished answer replaces the one being written.
        watched.apply(event(
            json!({"type": "item", "item": line(2, "assistant", "Hello back.")}),
        ));
        assert_eq!(watched.live, "");
        assert_eq!(
            watched.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [1, 2]
        );

        // A new snapshot, as after falling behind, drops what was held.
        watched.apply(event(json!({"type": "snapshot", "items": []})));
        assert_eq!(
            (watched.items.len(), watched.more, watched.info),
            (0, false, None)
        );
    }

    #[test]
    fn a_line_seen_twice_is_held_once_and_a_late_one_lands_in_order() {
        let mut watched = Watched::default();
        for id in [1, 3, 3, 2] {
            watched.apply(event(
                json!({"type": "item", "item": line(id, "user", &format!("m{id}"))}),
            ));
        }
        assert_eq!(
            watched.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn going_idle_clears_an_answer_that_was_never_finished() {
        let mut watched = Watched {
            live: "half an ans".to_string(),
            ..Watched::default()
        };
        watched.apply(event(
            json!({"type": "status", "conversation": info("working")}),
        ));
        assert_eq!(watched.live, "half an ans");
        watched.apply(event(
            json!({"type": "status", "conversation": info("idle")}),
        ));
        assert_eq!(watched.live, "");
    }

    #[test]
    fn a_subagent_event_fills_in_the_call_it_names() {
        let mut watched = Watched::default();
        watched.apply(event(json!({"type": "item", "item": {
            "id": 5, "role": "assistant", "text": "", "tools": ["subagent"],
            "calls": [{"id": "c1", "name": "subagent", "subagent": {"agent": "researcher", "task": "look"}}]
        }})));
        watched.apply(event(
            json!({"type": "subagent", "callId": "c1", "conversationId": "12"}),
        ));
        let call = &watched.items[0].calls[0];
        assert_eq!(
            call.subagent.as_ref().unwrap().conversation.as_deref(),
            Some("12")
        );
        // An event for a call that is not held changes nothing.
        let before = watched.clone();
        watched.apply(event(
            json!({"type": "subagent", "callId": "nope", "conversationId": "9"}),
        ));
        assert_eq!(watched, before);
    }

    #[test]
    fn a_running_calls_output_replaces_itself_and_ends_with_its_result() {
        let mut watched = Watched::default();
        watched.apply(event(json!({"type": "snapshot", "items": [], "tools": [
            {"callId": "c1", "name": "bash", "output": "one\n"}
        ]})));
        assert_eq!(watched.tools.len(), 1);

        watched.apply(event(
            json!({"type": "output", "callId": "c1", "name": "bash", "text": "one\ntwo\n"}),
        ));
        watched.apply(event(
            json!({"type": "output", "callId": "c2", "name": "read", "text": ""}),
        ));
        assert_eq!(
            watched
                .tools
                .iter()
                .map(|tool| (tool.call_id.as_str(), tool.output.as_str()))
                .collect::<Vec<_>>(),
            [("c1", "one\ntwo\n"), ("c2", "")],
            "an update replaces the output, and calls keep the order they started in"
        );

        // c1's result ends it; c2 is still running.
        let mut result = line(9, "tool", "one\ntwo\n");
        result["callId"] = json!("c1");
        watched.apply(event(json!({"type": "item", "item": result})));
        assert_eq!(watched.tools.len(), 1);
        assert_eq!(watched.tools[0].call_id, "c2");

        // Going idle ends whatever never got a result.
        watched.apply(event(
            json!({"type": "status", "conversation": info("idle")}),
        ));
        assert!(watched.tools.is_empty());
    }

    #[test]
    fn an_event_from_a_newer_daemon_is_ignored_not_fatal() {
        let mut watched = Watched::default();
        let unknown = event(json!({"type": "tool_output", "callId": "c1", "text": "x"}));
        assert_eq!(unknown, WatchEvent::Unknown);
        watched.apply(unknown);
        watched.apply(event(json!({"type": "ping"})));
        assert_eq!(watched, Watched::default());
    }

    #[test]
    fn lines_are_events_the_end_or_the_daemons_refusal() {
        assert_eq!(
            read(&json!({"event": {"type": "ping"}})),
            Ok(Next::Event(Box::new(WatchEvent::Ping)))
        );
        assert_eq!(read(&json!({"result": {}})), Ok(Next::Ended));
        let refused = read(&json!({"error": {"code": "unknown_method", "message": "No."}}))
            .expect_err("a refusal");
        assert!(Watching::unsupported(&refused));
        assert!(!Watching::unsupported(&SipilError::Malformed));
        assert_eq!(read(&json!({"nonsense": 1})), Err(SipilError::Malformed));
        assert_eq!(
            read(&json!({"event": {"type": "snapshot"}})),
            Err(SipilError::Malformed),
            "a snapshot without its items is not an event"
        );
    }
}
