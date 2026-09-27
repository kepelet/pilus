//! Client for the Luvus UHP automation socket.
//!
//! UHP is the daemon's public automation protocol: LF-terminated JSON on the
//! owner-only API socket. Ordinary calls answer exactly one request per
//! connection for `observe`, surfaced as `Observer::next_frame`, which
//! `--probe-uhp` uses to print one PTY's rendered screen.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::{Value, json};

use crate::control::ControlTransport;
use crate::error::{Error, Result};
use crate::transport::{SshTarget, Stream};

/// One live PTY as reported by `terminal.backend.inventory`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Terminal {
    /// Random id for one successful PTY lifetime.
    pub terminal_id: String,
    /// Current pane route; survives moves and reorders.
    pub pane_id: String,
    /// The child's own title, from its escape output.
    pub title: String,
    /// The pane's working directory.
    pub cwd: String,
    /// The owning workspace's name.
    pub workspace: String,
}

/// One complete screen replacement from an observe stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Frame {
    /// Identifies the captured text; frames are deduplicated on it.
    pub revision: u64,
    /// Row count the frame promises.
    pub lines: u16,
    /// Whether the server dropped rows to fit the capture bound.
    pub truncated: bool,
    /// Styled rows joined by `\n`.
    pub text: String,
}

/// A synchronous UHP session client.
///
/// Every call opens its own connection, matching the daemon's one-request-per
/// -connection contract; the observe stream keeps its connection until dropped.
#[derive(Clone, Debug)]
pub struct UhpClient {
    transport: ControlTransport,
    next_id: u64,
}

impl UhpClient {
    /// Bind to the UHP socket at `path`.
    pub fn connect(path: &Path) -> Self {
        Self {
            transport: ControlTransport::Socket(path.to_path_buf()),
            next_id: 0,
        }
    }

    /// Bind to a remote machine's UHP surface over SSH.
    ///
    /// `luvus uhp proxy` forwards one request per invocation, so streaming
    /// methods (`events.subscribe`, `terminal.backend.observe`) are not
    /// reachable this way; ordinary request/response methods are.
    pub fn connect_ssh(target: SshTarget) -> Self {
        Self {
            transport: ControlTransport::Ssh(target),
            next_id: 0,
        }
    }

    /// Send one request on a fresh connection and return its `result`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({
            "id": self.next_id.to_string(),
            "method": method,
            "params": params,
        })
        .to_string();
        let stream = self.transport.stream()?;
        {
            let mut writer = &stream;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        let response = read_json(&stream)?;
        if let Some(error) = response.get("error") {
            return Err(Error::Control(format!(
                "{method}: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("uhp error")
            )));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// List live terminals plus the server generation that identifies them.
    pub fn inventory(&mut self) -> Result<(String, Vec<Terminal>)> {
        let result = self.request("terminal.backend.inventory", json!({}))?;
        let generation = result
            .get("server_generation")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let terminals = result
            .get("terminals")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok((
            generation,
            terminals.iter().filter_map(terminal_from).collect(),
        ))
    }

    /// Open the general event subscription.
    ///
    /// Subscribes from "now": replaying older sequences requires a snapshot
    /// reconciliation the caller does not need, since a fresh subscription is
    /// followed by a full refresh.
    pub fn subscribe(&mut self) -> Result<EventStream> {
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({
            "id": self.next_id.to_string(),
            "method": "events.subscribe",
            "params": {},
        })
        .to_string();
        let stream = self.transport.stream()?;
        {
            let mut writer = &stream;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(EventStream {
            reader: BufReader::new(stream),
        })
    }

    /// Open a read-only observe stream for one terminal.
    pub fn observe(
        &mut self,
        generation: &str,
        terminal: &Terminal,
        lines: u16,
        ansi: bool,
    ) -> Result<Observer> {
        let params = json!({
            "server_generation": generation,
            "terminal_id": terminal.terminal_id,
            "pane_id": terminal.pane_id,
            "lines": lines,
            "ansi": ansi,
        });
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({
            "id": self.next_id.to_string(),
            "method": "terminal.backend.observe",
            "params": params,
        })
        .to_string();
        let stream = self.transport.stream()?;
        {
            let mut writer = &stream;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(Observer {
            reader: BufReader::new(stream),
        })
    }
}

/// An open `terminal.backend.observe` stream.
///
/// The first frame is sent immediately; later frames arrive only when the
/// terminal emits output, so [`Observer::next_frame`] blocks until then.
pub struct Observer {
    reader: BufReader<Stream>,
}

impl Observer {
    /// Read the next screen replacement, or `None` when the stream ends.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let value: Value =
                serde_json::from_str(&line).map_err(|error| Error::Control(error.to_string()))?;
            if let Some(error) = value.get("error") {
                return Err(Error::Control(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("uhp error")
                        .to_string(),
                ));
            }
            match value.get("event").and_then(Value::as_str) {
                Some("terminal.frame") => {
                    let data = value.get("data").cloned().unwrap_or(Value::Null);
                    return Ok(Some(frame_from(&data)));
                }
                Some("terminal.resync_required") => {
                    return Err(Error::Control(
                        "terminal stream fell behind and must resync".to_string(),
                    ));
                }
                // The stream acknowledgment (`terminal_backend_stream`) and any
                // event we do not consume yet are skipped.
                _ => continue,
            }
        }
    }
}

/// One sequenced event from an active subscription.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Event {
    /// Catalog name, e.g. `pane.agent_status_changed`.
    pub name: String,
    /// Monotonic server sequence; replay cursors use it.
    pub sequence: u64,
    /// The event's data payload, interpreted per event name.
    pub data: Value,
}

/// A live `events.subscribe` stream.
///
/// The subscription accepts only `after_sequence`, so every event in the
/// catalog arrives here and the caller filters by name.
pub struct EventStream {
    reader: BufReader<Stream>,
}

impl EventStream {
    /// Read the next event, or `None` when the server closes the stream.
    ///
    /// A `resync_required` reply (replay history no longer retained) surfaces
    /// as an error so the caller can resubscribe from a fresh snapshot.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let value: Value =
                serde_json::from_str(&line).map_err(|error| Error::Control(error.to_string()))?;
            if let Some(error) = value.get("error") {
                return Err(Error::Control(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("uhp error")
                        .to_string(),
                ));
            }
            // Skip the `subscription_started` acknowledgment.
            let Some(name) = value.get("event").and_then(Value::as_str) else {
                continue;
            };
            return Ok(Some(Event {
                name: name.to_string(),
                sequence: value.get("sequence").and_then(Value::as_u64).unwrap_or(0),
                data: value.get("data").cloned().unwrap_or(Value::Null),
            }));
        }
    }
}

fn read_json(stream: &Stream) -> Result<Value> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|error| Error::Control(error.to_string()))
}

fn terminal_from(value: &Value) -> Option<Terminal> {
    let text = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Terminal {
        terminal_id: value.get("terminal_id")?.as_str()?.to_string(),
        pane_id: value.get("pane_id")?.as_str()?.to_string(),
        title: text("terminal_title"),
        cwd: text("cwd"),
        workspace: value
            .get("workspace")
            .and_then(|workspace| workspace.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn frame_from(data: &Value) -> Frame {
    Frame {
        revision: data
            .get("content_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        lines: data.get("lines").and_then(Value::as_u64).unwrap_or(0) as u16,
        truncated: data
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text: data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}
