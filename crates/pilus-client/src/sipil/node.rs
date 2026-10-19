//! This process's iroh endpoint: where it dials sipil nodes and where they dial it.
//!
//! The window is not a tokio program, so the node owns a small runtime and
//! offers blocking calls, which the window runs on worker threads. The endpoint's
//! identity is a key under the pilus config directory; its public half is the
//! device ID other nodes allow-list or add as a contact.
//!
//! Inbound calls go to a [`Service`]. The node proves *who* is calling; whether
//! that device may call is the service's decision, since a sipil daemon, a pilus
//! window and a test each draw that line differently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use serde_json::Value;

use super::store;
use super::wire::{self, ALPN, Failure, REPLY_LIMIT, REQUEST_LIMIT};
use crate::netlog::{self, Kind};

/// How long one call may take, dialing included. A peer that is not there is
/// slower to give up than a daemon on loopback was.
const DEADLINE: Duration = Duration::from_secs(10);
/// How long a caller may take to send its request before the stream is dropped.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// Calls an inbound connection may have open at once.
const STREAMS_PER_CONNECTION: u32 = 16;
/// The most one line of a watch may weigh: a snapshot of the newest messages.
const FRAME_LIMIT: usize = 16 << 20;

/// Calls started and connections dialed since launch, for `PILUS_PERF`: what an
/// absent peer costs is the number of times it was dialed, so it is counted
/// where the dialing happens.
static CALLS: AtomicU64 = AtomicU64::new(0);
static DIALS: AtomicU64 = AtomicU64::new(0);

/// How many calls this process has started and how many fresh connections it
/// has dialed, in that order.
pub fn traffic() -> (u64, u64) {
    (CALLS.load(Ordering::Relaxed), DIALS.load(Ordering::Relaxed))
}

/// Which relays the endpoint dials out through.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RelayChoice {
    /// n0's public relays and discovery.
    #[default]
    Default,
    /// Dedicated or self-hosted relays.
    Urls(Vec<String>),
    /// Direct connections only; no relay and no discovery. For tests.
    Disabled,
}

/// What a node needs to start.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// Where `identity.key` lives.
    pub dir: PathBuf,
    /// Which relays to use.
    pub relay: RelayChoice,
}

/// Answers the calls other nodes make to this one.
pub trait Service: std::fmt::Debug + Send + Sync + 'static {
    /// Handle `method` from the device `from`, whose ID the handshake proved.
    fn handle(&self, from: &str, method: &str, params: Value) -> Result<Value, Failure>;
}

/// Why a call failed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CallError {
    /// The other side could not be reached, or dropped the call.
    #[error("{0}")]
    Unreachable(String),
    /// The other side did not answer in time.
    #[error("no answer")]
    Timeout,
    /// The other side answered with a refusal.
    #[error("{}", .0.message)]
    Refused(Failure),
    /// The other side answered with something that is not a reply.
    #[error("unreadable answer")]
    Malformed,
}

/// Why a node could not start.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The identity key could not be read or made.
    #[error("could not use the identity key: {0}")]
    Identity(String),
    /// The relay setting is not a relay URL.
    #[error("invalid relay `{0}`")]
    Relay(String),
    /// The runtime or the endpoint would not start.
    #[error("could not start the iroh endpoint: {0}")]
    Start(String),
}

/// A running iroh endpoint.
pub struct Node {
    runtime: tokio::runtime::Runtime,
    endpoint: Endpoint,
    id: String,
    connections: Mutex<HashMap<EndpointId, Connection>>,
    _router: Router,
}

impl Node {
    /// Bind the endpoint and start answering inbound calls with `service`.
    pub fn start(config: &NodeConfig, service: Arc<dyn Service>) -> Result<Arc<Self>, NodeError> {
        let key = load_or_create_key(&config.dir.join("identity.key"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("pilus-sipil-net")
            .build()
            .map_err(|error| NodeError::Start(error.to_string()))?;
        let builder = match &config.relay {
            RelayChoice::Default => Endpoint::builder(presets::N0),
            RelayChoice::Disabled => Endpoint::builder(presets::N0DisableRelay),
            RelayChoice::Urls(urls) => {
                let urls = urls
                    .iter()
                    .map(|url| {
                        url.parse::<RelayUrl>()
                            .map_err(|_| NodeError::Relay(url.clone()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Endpoint::builder(presets::N0).relay_mode(RelayMode::custom(urls))
            }
        }
        .secret_key(key);
        let (endpoint, router) = runtime
            .block_on(async {
                let endpoint = builder.bind().await?;
                let router = Router::builder(endpoint.clone())
                    .accept(ALPN, Inbound { service })
                    .spawn();
                Ok::<_, iroh::endpoint::BindError>((endpoint, router))
            })
            .map_err(|error| NodeError::Start(error.to_string()))?;
        Ok(Arc::new(Self {
            runtime,
            id: endpoint.id().to_string(),
            endpoint,
            connections: Mutex::new(HashMap::new()),
            _router: router,
        }))
    }

    /// This node's device ID: 64 hexadecimal characters.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The addresses this node can be reached at.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// A ticket for [`Node::addr`], to paste where the ID alone is not found.
    ///
    /// It also lists this machine's loopback address, so a node on the same
    /// machine is reached directly instead of through a LAN address the
    /// operating system may not let an app dial. Anyone else who dials that
    /// address reaches their own machine, and the handshake rejects it.
    pub fn ticket(&self) -> String {
        let addr = self.endpoint.addr();
        let port = addr.ip_addrs().find(|ip| ip.is_ipv4()).map(|ip| ip.port());
        let addr = match port {
            Some(port) => addr.with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port))),
            None => addr,
        };
        EndpointTicket::new(addr).to_string()
    }

    /// Call `method` on the node at `to` and wait for its answer.
    ///
    /// A connection is kept per peer. One that went stale while idle is
    /// replaced and the call retried, but only when the request never left: a
    /// request that was sent may have been run, and some calls (a new session)
    /// are not safe to run twice.
    pub fn call(
        &self,
        to: &EndpointAddr,
        method: &str,
        params: &Value,
    ) -> Result<Value, CallError> {
        CALLS.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let frame = wire::request(method, params);
        let outcome = self
            .runtime
            .block_on(async { tokio::time::timeout(DEADLINE, self.round_trip(to, &frame)).await });
        // A refusal is an answer, so the call got through.
        let reached = matches!(&outcome, Ok(Ok(_)) | Ok(Err(CallError::Refused(_))));
        netlog::record(
            Kind::Sipil,
            &to.id.fmt_short().to_string(),
            method,
            started,
            reached,
        );
        match outcome {
            Err(_) => {
                self.forget(&to.id);
                Err(CallError::Timeout)
            }
            Ok(Err(error)) => {
                self.forget(&to.id);
                Err(error)
            }
            Ok(Ok(bytes)) => match wire::parse_reply(&bytes) {
                Some(Ok(result)) => Ok(result),
                Some(Err(failure)) => Err(CallError::Refused(failure)),
                None => Err(CallError::Malformed),
            },
        }
    }

    /// Call `method` on the node at `to` and keep the stream open: the answer is
    /// many lines, read with [`WatchStream::next`], not one reply.
    ///
    /// Dialing and sending the request have the usual deadline; after that the
    /// stream lives as long as both sides want it. Dropping it hangs up.
    pub fn watch(
        self: &Arc<Self>,
        to: &EndpointAddr,
        method: &str,
        params: &Value,
    ) -> Result<WatchStream, CallError> {
        CALLS.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let frame = wire::request(method, params);
        let opened = self.runtime.block_on(async {
            tokio::time::timeout(DEADLINE, async {
                for attempt in 0..2 {
                    let connection = self.connection(to).await?;
                    match send_request(&connection, &frame).await {
                        Ok((send, recv)) => return Ok((send, recv)),
                        // Nothing complete was sent: the connection had died while idle.
                        Err(_) if attempt == 0 => self.forget(&to.id),
                        Err(error) => return Err(error),
                    }
                }
                Err(CallError::Unreachable(
                    "could not open a stream".to_string(),
                ))
            })
            .await
        });
        netlog::record(
            Kind::Sipil,
            &to.id.fmt_short().to_string(),
            method,
            started,
            matches!(opened, Ok(Ok(_))),
        );
        match opened {
            Err(_) => {
                self.forget(&to.id);
                Err(CallError::Timeout)
            }
            Ok(Err(error)) => {
                self.forget(&to.id);
                Err(error)
            }
            Ok(Ok((send, recv))) => Ok(WatchStream {
                node: Arc::clone(self),
                _send: send,
                recv,
                buffer: Vec::new(),
                ended: false,
            }),
        }
    }

    async fn round_trip(&self, to: &EndpointAddr, frame: &[u8]) -> Result<Vec<u8>, CallError> {
        for attempt in 0..2 {
            let connection = self.connection(to).await?;
            match send_request(&connection, frame).await {
                Ok((_send, mut recv)) => {
                    return recv
                        .read_to_end(REPLY_LIMIT)
                        .await
                        .map_err(|error| CallError::Unreachable(error.to_string()));
                }
                // Nothing complete was sent: the connection had died while idle.
                Err(_) if attempt == 0 => self.forget(&to.id),
                Err(error) => return Err(error),
            }
        }
        Err(CallError::Unreachable(
            "could not open a stream".to_string(),
        ))
    }

    async fn connection(&self, to: &EndpointAddr) -> Result<Connection, CallError> {
        if let Some(connection) = self.lock().get(&to.id)
            && connection.close_reason().is_none()
        {
            return Ok(connection.clone());
        }
        DIALS.fetch_add(1, Ordering::Relaxed);
        let connection = self
            .endpoint
            .connect(to.clone(), ALPN)
            .await
            .map_err(|error| CallError::Unreachable(error.to_string()))?;
        self.lock().insert(to.id, connection.clone());
        Ok(connection)
    }

    fn forget(&self, id: &EndpointId) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What [`WatchStream::next`] found.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// One JSON object the other side wrote.
    Value(Value),
    /// Nothing arrived in the time given.
    Idle,
    /// The other side finished the stream.
    Ended,
}

/// The answer to a call that does not end after one reply: one JSON object per
/// line, until the other side finishes the stream or this one is dropped.
pub struct WatchStream {
    node: Arc<Node>,
    _send: SendStream,
    recv: RecvStream,
    buffer: Vec<u8>,
    ended: bool,
}

impl WatchStream {
    /// The next line, waiting at most `wait` for it. A line that does not parse
    /// as JSON is [`CallError::Malformed`]; a stream that breaks is
    /// [`CallError::Unreachable`].
    pub fn next(&mut self, wait: Duration) -> Result<Line, CallError> {
        loop {
            if let Some(line) = take_line(&mut self.buffer) {
                return serde_json::from_slice(&line)
                    .map(Line::Value)
                    .map_err(|_| CallError::Malformed);
            }
            if self.ended {
                return Ok(Line::Ended);
            }
            if self.buffer.len() > FRAME_LIMIT {
                return Err(CallError::Malformed);
            }
            let recv = &mut self.recv;
            let mut chunk = vec![0u8; 64 * 1024];
            let read = self
                .node
                .runtime
                .block_on(async { tokio::time::timeout(wait, recv.read(&mut chunk)).await });
            match read {
                Err(_) => return Ok(Line::Idle),
                Ok(Ok(Some(count))) => self.buffer.extend_from_slice(&chunk[..count]),
                Ok(Ok(None)) => self.ended = true,
                Ok(Err(error)) => return Err(CallError::Unreachable(error.to_string())),
            }
        }
    }
}

impl Drop for WatchStream {
    fn drop(&mut self) {
        // Tell the other side nobody is reading, so it stops writing.
        let _ = self.recv.stop(0u32.into());
    }
}

/// Cut the first complete, non-empty line off `buffer`.
fn take_line(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    loop {
        let end = buffer.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<u8> = buffer.drain(..=end).collect();
        line.pop();
        if !line.iter().all(u8::is_ascii_whitespace) {
            return Some(line);
        }
    }
}

/// Open a stream and write the request; the stream pair is returned once the
/// whole request is on its way.
async fn send_request(
    connection: &Connection,
    frame: &[u8],
) -> Result<(SendStream, RecvStream), CallError> {
    let unreachable = |error: &dyn std::fmt::Display| CallError::Unreachable(error.to_string());
    let (mut send, recv) = connection.open_bi().await.map_err(|e| unreachable(&e))?;
    send.write_all(frame).await.map_err(|e| unreachable(&e))?;
    send.finish().map_err(|e| unreachable(&e))?;
    Ok((send, recv))
}

/// Answers inbound streams for the router.
#[derive(Debug)]
struct Inbound {
    service: Arc<dyn Service>,
}

impl ProtocolHandler for Inbound {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        connection.set_max_concurrent_bi_streams(STREAMS_PER_CONNECTION.into());
        let from = connection.remote_id().to_string();
        while let Ok((send, recv)) = connection.accept_bi().await {
            tokio::spawn(answer(Arc::clone(&self.service), from.clone(), send, recv));
        }
        Ok(())
    }
}

/// Read one request, run it, write the reply.
async fn answer(
    service: Arc<dyn Service>,
    from: String,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let request = tokio::time::timeout(REQUEST_DEADLINE, recv.read_to_end(REQUEST_LIMIT)).await;
    let outcome = match request {
        Ok(Ok(bytes)) => match wire::parse_request(&bytes) {
            Some((method, params)) => {
                // The service reads and writes small files; keep that off the
                // network threads.
                tokio::task::spawn_blocking(move || service.handle(&from, &method, params))
                    .await
                    .unwrap_or_else(|_| Err(Failure::new("internal", "Something went wrong.")))
            }
            None => Err(Failure::new(
                "bad_request",
                "A request is a JSON object with a method and optional params.",
            )),
        },
        Ok(Err(_)) | Err(_) => return,
    };
    if send.write_all(&wire::reply(&outcome)).await.is_ok() {
        let _ = send.finish();
    }
}

/// Load the identity at `path`, creating it on first run.
///
/// Losing the file changes the device ID, so every contact and every daemon
/// allow-list that named this device has to be told again.
pub fn load_or_create_key(path: &Path) -> Result<SecretKey, NodeError> {
    let identity = |error: &dyn std::fmt::Display| NodeError::Identity(error.to_string());
    match std::fs::read(path) {
        Ok(bytes) => {
            let array: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| NodeError::Identity(format!("{} is not 32 bytes", path.display())))?;
            Ok(SecretKey::from_bytes(&array))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| identity(&e))?;
            }
            let key = SecretKey::generate();
            store::write_private(path, &key.to_bytes()).map_err(|e| identity(&e))?;
            Ok(key)
        }
        Err(error) => Err(identity(&error)),
    }
}

/// The device ID this config directory's identity answers to, creating the
/// identity if there is none. Used before the node is up, to show the ID.
pub fn device_id(dir: &Path) -> Result<String, NodeError> {
    Ok(load_or_create_key(&dir.join("identity.key"))?
        .public()
        .to_string())
}
