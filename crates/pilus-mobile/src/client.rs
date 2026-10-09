//! The device's client: an iroh endpoint, a host connection per ALPN, and UHP.
//!
//! A [`Client`] is a synchronous facade over a private tokio runtime so the
//! FFI layer — and any Swift caller — never sees `async`. The two QUIC
//! connections map to the connector's two surfaces: `CONTROL_ALPN` for
//! enrollment and token refresh, `UHP_ALPN` for ordinary UHP requests where
//! each bidirectional stream is one LF-terminated request/reply.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl};
use iroh_tickets::endpoint::EndpointTicket;
use serde_json::{Value, json};

use crate::store::{self, Enrollment};
use crate::{CONTROL_ALPN, HUB_ALPN, UHP_ALPN};

/// The deadline for one connect, one write, or one reply.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Largest reply the client will buffer. UHP frames are bounded server-side;
/// this only needs to clear `uhp.capabilities`, the biggest payload.
const REPLY_LIMIT: usize = 8 * 1024 * 1024;

/// Failures the app can surface to the user.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Storage or filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The ticket string was not a valid `EndpointTicket`.
    #[error("the ticket is not valid: {0}")]
    Ticket(String),

    /// The iroh endpoint or connection failed.
    #[error("transport error: {0}")]
    Transport(String),

    /// The peer's control channel rejected a request.
    #[error("{0}")]
    Control(String),

    /// The gateway answered a UHP request with an error.
    #[error("{0}")]
    Uhp(String),

    /// A reply could not be decoded.
    #[error("bad reply: {0}")]
    Reply(String),

    /// The device has not enrolled with this host yet.
    #[error("not enrolled")]
    NotEnrolled,

    /// The persisted identity or enrollment file is broken.
    #[error("store error: {0}")]
    Store(String),

    /// A bounded wait expired.
    #[error("{0} timed out")]
    Timeout(&'static str),
}

/// Which of the connector's surfaces a call travels on.
#[derive(Clone, Copy)]
enum Channel {
    Control,
    Uhp,
    Hub,
}

impl Channel {
    fn alpn(self) -> &'static [u8] {
        match self {
            Self::Control => CONTROL_ALPN,
            Self::Uhp => UHP_ALPN,
            Self::Hub => HUB_ALPN,
        }
    }
}

/// A bound, retrying connection per ALPN.
struct Connections {
    control: Option<Connection>,
    uhp: Option<Connection>,
    hub: Option<Connection>,
}

impl Connections {
    fn get(&self, channel: Channel) -> Option<&Connection> {
        match channel {
            Channel::Control => self.control.as_ref(),
            Channel::Uhp => self.uhp.as_ref(),
            Channel::Hub => self.hub.as_ref(),
        }
    }

    fn set(&mut self, channel: Channel, connection: Connection) {
        match channel {
            Channel::Control => self.control = Some(connection),
            Channel::Uhp => self.uhp = Some(connection),
            Channel::Hub => self.hub = Some(connection),
        }
    }

    fn take(&mut self, channel: Channel) -> Option<Connection> {
        match channel {
            Channel::Control => self.control.take(),
            Channel::Uhp => self.uhp.take(),
            Channel::Hub => self.hub.take(),
        }
    }
}

/// The open halves of a `hub.watch` stream.
///
/// Held outside the tokio runtime between calls; every read still runs inside
/// `block_on`, which is what keeps the stream's driver alive.
struct Watch {
    reader: tokio::io::BufReader<iroh::endpoint::RecvStream>,
    send: iroh::endpoint::SendStream,
}

/// A connected device session.
///
/// Created by [`Client::connect`] (fresh ticket) or [`Client::resume`]
/// (saved enrollment). Calls are synchronous and internally serialized where
/// they must be; the app drives them from background threads.
pub struct Client {
    runtime: tokio::runtime::Runtime,
    dir: PathBuf,
    endpoint: Endpoint,
    host: EndpointAddr,
    ticket: String,
    connections: Mutex<Connections>,
    watch: Mutex<Option<Watch>>,
    token: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl Client {
    /// The device's `EndpointId`, creating the identity key on first call.
    ///
    /// This is the string a host can put in `pilus-agent serve --iroh --allow`.
    pub fn device_id(dir: &Path) -> Result<String, Error> {
        let key = store::load_or_create_key(dir)?;
        Ok(key.public().to_string())
    }

    /// Bind the endpoint for `ticket` without enrolling or dialing yet.
    ///
    /// Connections are opened lazily on first use so a stale host address
    /// still lets the app show its onboarding state before a request fails.
    /// `relay` names a self-hosted relay to use instead of the n0 public
    /// ones; `api_secret` rides along as a `token` query parameter, the
    /// authentication shape self-hosted relays expose (iroh carries no relay
    /// credentials itself). Empty strings mean the defaults.
    pub fn connect(dir: &Path, ticket: &str, relay: &str, api_secret: &str) -> Result<Self, Error> {
        let parsed = EndpointTicket::from_str(ticket.trim())
            .map_err(|error| Error::Ticket(error.to_string()))?;
        Self::bind(
            dir,
            ticket.trim().to_string(),
            parsed.endpoint_addr().clone(),
            relay,
            api_secret,
        )
    }

    /// Reconnect to the host in the saved enrollment under `dir`.
    pub fn resume(dir: &Path, relay: &str, api_secret: &str) -> Result<Self, Error> {
        let enrollment = Enrollment::load(dir).ok_or(Error::NotEnrolled)?;
        let parsed = EndpointTicket::from_str(enrollment.ticket.trim())
            .map_err(|error| Error::Ticket(error.to_string()))?;
        let client = Self::bind(
            dir,
            enrollment.ticket,
            parsed.endpoint_addr().clone(),
            relay,
            api_secret,
        )?;
        *client
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(enrollment.token);
        Ok(client)
    }

    fn bind(
        dir: &Path,
        ticket: String,
        host: EndpointAddr,
        relay: &str,
        api_secret: &str,
    ) -> Result<Self, Error> {
        let key = store::load_or_create_key(dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .map_err(Error::Io)?;
        let mut builder = Endpoint::builder(presets::N0).secret_key(key);
        if !relay.is_empty() {
            let url = relay_url_with_secret(relay, api_secret)?;
            builder = builder.relay_mode(RelayMode::custom([url]));
        }
        let endpoint = runtime
            .block_on(builder.bind())
            .map_err(|error| Error::Transport(error.to_string()))?;
        Ok(Self {
            runtime,
            dir: dir.to_path_buf(),
            endpoint,
            host,
            ticket,
            connections: Mutex::new(Connections {
                control: None,
                uhp: None,
                hub: None,
            }),
            watch: Mutex::new(None),
            token: Mutex::new(None),
            next_id: AtomicU64::new(0),
        })
    }

    /// The host's `EndpointId` from the ticket.
    pub fn host_id(&self) -> String {
        self.host.id.to_string()
    }

    /// The device's own `EndpointId` on the live endpoint.
    pub fn endpoint_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Whether a delegated token is held.
    pub fn is_authorized(&self) -> bool {
        self.lock_token().is_some()
    }

    /// Enroll this device with `code`, then collect the delegated token.
    ///
    /// The code is the one `pilus-agent serve --iroh` prints; a success writes
    /// `enrollment.json` so later launches can [`Client::resume`].
    pub fn enroll(&self, code: &str, label: &str) -> Result<(), Error> {
        let reply = self.control_request(&json!({
            "type": "enroll",
            "code": code.trim(),
            "label": label,
        }))?;
        if reply.get("type").and_then(Value::as_str) != Some("enrolled") {
            return Err(Error::Control(
                reply
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("enrollment was rejected")
                    .to_string(),
            ));
        }
        self.authorize()
    }

    /// Fetch (or refresh) the delegated token over the control channel.
    ///
    /// Only an already-enrolled device can call this; the connector matches
    /// the peer by `EndpointId`.
    pub fn authorize(&self) -> Result<(), Error> {
        let reply = self.control_request(&json!({ "type": "token" }))?;
        let token = reply
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Reply("no token in reply".to_string()))?
            .to_string();
        *self.lock_token() = Some(token.clone());
        Enrollment {
            ticket: self.ticket.clone(),
            endpoint_id: self.host_id(),
            token,
        }
        .save(&self.dir)?;
        Ok(())
    }

    /// The saved enrollment, if the device has one.
    pub fn enrollment(dir: &Path) -> Option<Enrollment> {
        Enrollment::load(dir)
    }

    /// Forget the enrollment record. The identity key stays — it is only a
    /// lookup key the host may revoke on its side.
    pub fn forget(dir: &Path) {
        Enrollment::forget(dir);
    }

    /// Send one ordinary UHP request and return its `result` value.
    ///
    /// Errors inside the reply's `error` object surface as [`Error::Uhp`].
    /// A transport failure drops the cached connection and retries once on a
    /// fresh one, since a restarted host leaves dead QUIC sessions behind.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        match self.uhp_request(method, &params) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.drop_connection(Channel::Uhp);
                self.uhp_request(method, &params).map_err(|_| error)
            }
        }
    }

    fn uhp_request(&self, method: &str, params: &Value) -> Result<Value, Error> {
        let token = self.lock_token().clone().ok_or(Error::NotEnrolled)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let line = json!({
            "id": id,
            "method": method,
            "params": params,
            "auth": token,
        });
        let reply = self.round_trip(Channel::Uhp, line.to_string().as_bytes())?;
        let value: Value =
            serde_json::from_slice(&reply).map_err(|error| Error::Reply(error.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(Error::Uhp(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("uhp error")
                    .to_string(),
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One request on the hub channel (`{"id","machine","method","params"}`).
    ///
    /// The hub authenticates by `EndpointId`, so the frame carries no token.
    /// On an old connector with no hub ALPN the connect fails and the caller
    /// falls back to [`Client::request`]; anything else retries once on a
    /// fresh connection like [`Client::request`] does.
    pub fn hub_request(&self, method: &str, machine: &str, params: Value) -> Result<Value, Error> {
        match self.hub_call(method, machine, &params) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.drop_connection(Channel::Hub);
                self.hub_call(method, machine, &params).map_err(|_| error)
            }
        }
    }

    fn hub_call(&self, method: &str, machine: &str, params: &Value) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let line = json!({
            "id": id,
            "machine": machine,
            "method": method,
            "params": params,
        });
        let reply = self.round_trip(Channel::Hub, line.to_string().as_bytes())?;
        let value: Value =
            serde_json::from_slice(&reply).map_err(|error| Error::Reply(error.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(Error::Uhp(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("hub error")
                    .to_string(),
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Open a `hub.watch` stream and return the subscription's first reply.
    ///
    /// Only one watch can be live at a time — a second open replaces the
    /// first, and the connector holds the stream open past the usual one
    /// request/one reply shape, relaying change signals until it closes.
    pub fn hub_watch_open(&self) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let line = json!({ "id": id, "method": "hub.watch" }).to_string();
        let (value, watch) = self.runtime.block_on(async {
            let connection = self.connection(Channel::Hub).await?;
            let (mut send, recv) = timeout("watch", async {
                connection
                    .open_bi()
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))
            })
            .await?;
            send.write_all(line.as_bytes())
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
            send.write_all(b"\n")
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
            let mut reader = tokio::io::BufReader::new(recv);
            let mut first = String::new();
            timeout("watch reply", async {
                tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut first)
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))
            })
            .await?;
            let value: Value = serde_json::from_str(first.trim())
                .map_err(|error| Error::Reply(error.to_string()))?;
            Ok::<(Value, Watch), Error>((value, Watch { reader, send }))
        })?;
        if let Some(error) = value.get("error") {
            return Err(Error::Uhp(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("watch was refused")
                    .to_string(),
            ));
        }
        *self
            .watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(watch);
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The next signal line on the open watch, decoded as JSON.
    ///
    /// Blocks until a line arrives or the stream ends; an end or error drops
    /// the watch so the caller can reopen it. Returns `Error::NotEnrolled`
    /// when no watch is open.
    pub fn hub_watch_next(&self) -> Result<Value, Error> {
        loop {
            let mut guard = self
                .watch
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(watch) = guard.as_mut() else {
                return Err(Error::NotEnrolled);
            };
            let mut line = String::new();
            let read = self.runtime.block_on(async {
                timeout("watch signal", async {
                    tokio::io::AsyncBufReadExt::read_line(&mut watch.reader, &mut line)
                        .await
                        .map_err(|error| Error::Transport(error.to_string()))
                })
                .await
            });
            match read {
                Ok(0) | Err(_) => {
                    *guard = None;
                    self.drop_connection(Channel::Hub);
                    return Err(Error::Transport("watch ended".to_string()));
                }
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    return serde_json::from_str(line.trim())
                        .map_err(|error| Error::Reply(error.to_string()));
                }
            }
        }
    }

    /// Close the open watch, if any.
    pub fn hub_watch_close(&self) {
        if let Some(mut watch) = self
            .watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = self.runtime.block_on(async move {
                let _ = watch.send.finish();
            });
        }
    }

    /// One request on the control channel.
    fn control_request(&self, request: &Value) -> Result<Value, Error> {
        let reply = self.round_trip(Channel::Control, request.to_string().as_bytes())?;
        let value: Value =
            serde_json::from_slice(&reply).map_err(|error| Error::Reply(error.to_string()))?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            return Err(Error::Control(
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("control error")
                    .to_string(),
            ));
        }
        Ok(value)
    }

    /// Write one LF-terminated frame on a fresh stream and read to EOF.
    fn round_trip(&self, channel: Channel, frame: &[u8]) -> Result<Vec<u8>, Error> {
        self.runtime.block_on(async {
            let connection = self.connection(channel).await?;
            let result = timeout("request", async {
                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))?;
                send.write_all(frame)
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))?;
                send.write_all(b"\n")
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))?;
                send.finish()
                    .map_err(|error| Error::Transport(error.to_string()))?;
                recv.read_to_end(REPLY_LIMIT)
                    .await
                    .map_err(|error| Error::Transport(error.to_string()))
            })
            .await;
            match result {
                Ok(bytes) => Ok(bytes),
                Err(error) => {
                    self.drop_connection(channel);
                    Err(error)
                }
            }
        })
    }

    /// The cached connection for `channel`, dialing once when missing.
    async fn connection(&self, channel: Channel) -> Result<Connection, Error> {
        {
            let guard = self.lock_connections();
            if let Some(connection) = guard.get(channel)
                && connection.close_reason().is_none()
            {
                return Ok(connection.clone());
            }
        }
        // Give the endpoint a short window to reach a relay before dialing;
        // the ticket's direct addresses may still win the race afterwards.
        let _ = tokio::time::timeout(Duration::from_secs(5), self.endpoint.online()).await;
        let host = self.host.clone();
        let endpoint = self.endpoint.clone();
        let connection = timeout("connect", async move {
            endpoint
                .connect(host, channel.alpn())
                .await
                .map_err(|error| Error::Transport(error.to_string()))
        })
        .await?;
        self.lock_connections().set(channel, connection.clone());
        Ok(connection)
    }

    fn drop_connection(&self, channel: Channel) {
        if let Some(connection) = self.lock_connections().take(channel) {
            connection.close(0u32.into(), b"retry");
        }
    }

    fn lock_connections(&self) -> std::sync::MutexGuard<'_, Connections> {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_token(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The app's API secret rides to a self-hosted relay as a `token` query
/// parameter — iroh itself defines no relay authentication, and this is the
/// shape self-hosted relays document. Leaves `relay` untouched when the
/// secret is empty.
fn relay_url_with_secret(relay: &str, api_secret: &str) -> Result<RelayUrl, Error> {
    let mut url = relay.trim().to_string();
    if !api_secret.is_empty() {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str("token=");
        url.push_str(&encode_query_value(api_secret));
    }
    url.parse()
        .map_err(|error| Error::Transport(format!("invalid relay `{relay}`: {error}")))
}

/// Percent-encode the characters a query value can't carry verbatim.
fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Await `future`, returning [`Error::Timeout`] past [`IO_TIMEOUT`].
async fn timeout<T, F>(what: &'static str, future: F) -> Result<T, Error>
where
    F: Future<Output = Result<T, Error>>,
{
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| Error::Timeout(what))?
}
