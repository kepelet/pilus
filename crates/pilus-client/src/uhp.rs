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
    /// Delegated token for a paired `uhp access` gateway; the owner socket
    /// omits `auth` entirely.
    auth: Option<String>,
}

impl UhpClient {
    /// Bind to the UHP socket at `path`.
    pub fn connect(path: &Path) -> Self {
        Self {
            transport: ControlTransport::Socket(path.to_path_buf()),
            next_id: 0,
            auth: None,
        }
    }

    /// Bind to a `luvus uhp access` loopback gateway over TCP.
    pub fn connect_tcp(address: impl Into<String>) -> Self {
        Self {
            transport: ControlTransport::Tcp(address.into()),
            next_id: 0,
            auth: None,
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
            auth: None,
        }
    }

    /// Attach a delegated token, sent as `auth` on every request.
    ///
    /// This is what a client paired through a `luvus uhp access` gateway uses;
    /// the owner socket leaves `auth` off.
    #[must_use]
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth = Some(token.into());
        self
    }

    /// Build the next request line, including `auth` when a token is set.
    fn envelope(&mut self, method: &str, params: Value) -> String {
        self.next_id = self.next_id.wrapping_add(1);
        let mut request = json!({
            "id": self.next_id.to_string(),
            "method": method,
            "params": params,
        });
        if let Some(auth) = &self.auth
            && let Some(object) = request.as_object_mut()
        {
            object.insert("auth".to_string(), Value::String(auth.clone()));
        }
        request.to_string()
    }

    /// Discover the server's protocol version, method registry, event catalog,
    /// and — on a paired gateway — the effective access projection.
    pub fn capabilities(&mut self) -> Result<Capabilities> {
        let result = self.request("uhp.capabilities", json!({}))?;
        Ok(Capabilities::from_json(&result))
    }

    /// Send one request on a fresh connection and return its `result`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let line = self.envelope(method, params);
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

    /// Open the general event subscription from "now": replaying older
    /// sequences requires a snapshot reconciliation the caller does not need,
    /// since a fresh subscription is followed by a full refresh.
    pub fn subscribe(&mut self) -> Result<EventStream> {
        self.subscribe_after(None)
    }

    /// Open the general event subscription, replaying retained events after
    /// `after_sequence` before live delivery when it is set.
    ///
    /// A cursor older than the retained window comes back as
    /// [`Error::Control`] carrying `resync_required`, so the caller can take a
    /// fresh snapshot first.
    pub fn subscribe_after(&mut self, after_sequence: Option<u64>) -> Result<EventStream> {
        let params = match after_sequence {
            Some(sequence) => json!({ "after_sequence": sequence }),
            None => json!({}),
        };
        let line = self.envelope("events.subscribe", params);
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
        let line = self.envelope("terminal.backend.observe", params);
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

/// The server's advertised UHP surface, from `uhp.capabilities`.
///
/// Keep every field tolerant: a newer daemon may add methods or access fields,
/// and an older one may omit them. Discovery, not the Luvus release number,
/// decides what a caller may do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    /// Protocol identity, e.g. `luvus-uhp 1.0`.
    pub protocol_version: String,
    /// Advertised method names.
    pub methods: Vec<String>,
    /// `read_only` or `control`, when reached through a paired gateway.
    pub access_mode: Option<String>,
    /// The gateway's effective allow-list; empty means the owner endpoint.
    pub allowed_methods: Vec<String>,
}

impl Capabilities {
    fn from_json(result: &Value) -> Self {
        fn names(value: Option<&Value>) -> Vec<String> {
            value
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| match item {
                    Value::String(name) => Some(name.clone()),
                    Value::Object(object) => object
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
                .collect()
        }

        // `protocol` is `{name, major, minor}`; older or hand-rolled replies
        // may carry a flat version string instead.
        fn version(result: &Value) -> String {
            if let Some(protocol) = result.get("protocol") {
                let name = protocol
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("luvus-uhp");
                if let (Some(major), Some(minor)) = (
                    protocol.get("major").and_then(Value::as_u64),
                    protocol.get("minor").and_then(Value::as_u64),
                ) {
                    return format!("{name} {major}.{minor}");
                }
            }
            ["protocol_version", "uhp_version", "version"]
                .iter()
                .find_map(|key| result.get(*key).and_then(Value::as_str))
                .unwrap_or_default()
                .to_string()
        }

        let access = result.get("access");
        Self {
            protocol_version: version(result),
            methods: names(result.get("methods")),
            access_mode: access
                .and_then(|access| access.get("mode"))
                .and_then(Value::as_str)
                .map(str::to_string),
            allowed_methods: names(access.and_then(|access| access.get("allowed_methods"))),
        }
    }

    /// Whether `method` may be called.
    ///
    /// The owner endpoint advertises no access projection, so an empty
    /// allow-list means the full method registry is available.
    pub fn allows(&self, method: &str) -> bool {
        self.allowed_methods.is_empty() || self.allowed_methods.iter().any(|item| item == method)
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
