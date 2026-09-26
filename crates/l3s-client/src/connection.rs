//! Attaching to a daemon: handshake, framed IO, and the background reader.
//!
//! The sequence on the wire is fixed:
//!
//! 1. client sends [`ClientMessage::Hello`] with its protocol revision;
//! 2. server replies [`ServerMessage::Welcome`] (an `error` means mismatch);
//! 3. server sends [`ServerMessage::Ready`] and [`ServerMessage::EndpointIdentity`];
//! 4. if `Ready::probe_terminal` is set, the client replies with
//!    [`ClientMessage::TerminalColors`] before the server will stream frames.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use l3s_protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, ShellDockLayout};

use crate::ConnectTarget;
use crate::error::{Error, Result};
use crate::transport::Stream;

/// Opaque identity for the server process a connection reached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointIdentity {
    /// Server boot identifier.
    pub boot_id: u64,
    /// Selected session name.
    pub session: String,
}

/// Events a [`Connection`] surfaces to its owner.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    /// The handshake completed.
    Ready {
        /// Whether the server wants terminal colors reported.
        probe_terminal: bool,
        /// The server's identity.
        identity: EndpointIdentity,
    },
    /// An application message from the server.
    Message(ServerMessage),
    /// The link ended; the string explains why.
    Disconnected {
        /// Human-readable reason.
        reason: String,
    },
}

/// An attached, version-checked link to a Luvus daemon.
pub struct Connection {
    outbound: Sender<ClientMessage>,
    inbound: Receiver<ClientEvent>,
    socket: PathBuf,
    identity: EndpointIdentity,
}

impl Connection {
    /// Attach to `target`, announcing an initial `cols` by `rows` viewport.
    pub fn open(target: &ConnectTarget, cols: u16, rows: u16) -> Result<Self> {
        let socket = target.resolve();
        let stream = Arc::new(Stream::connect(&socket)?);

        let (probe_terminal, identity) = handshake(&stream, cols, rows)?;

        let (outbound_tx, outbound_rx) = unbounded::<ClientMessage>();
        let (inbound_tx, inbound_rx) = unbounded::<ClientEvent>();

        spawn_writer(Arc::clone(&stream), outbound_rx);
        spawn_reader(Arc::clone(&stream), inbound_tx.clone());

        inbound_tx
            .send(ClientEvent::Ready {
                probe_terminal,
                identity: identity.clone(),
            })
            .ok();

        Ok(Self {
            outbound: outbound_tx,
            inbound: inbound_rx,
            socket,
            identity,
        })
    }

    /// The socket this connection is attached to.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// The server's identity, learned during the handshake.
    pub fn identity(&self) -> &EndpointIdentity {
        &self.identity
    }

    /// Queue a message to the server. Never blocks on socket IO.
    pub fn send(&self, message: ClientMessage) -> Result<()> {
        self.outbound.send(message).map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "writer stopped",
            ))
        })
    }

    /// Block until the next event arrives.
    pub fn recv(&self) -> Option<ClientEvent> {
        self.inbound.recv().ok()
    }

    /// Return the next event without blocking.
    pub fn try_recv(&self) -> Option<ClientEvent> {
        self.inbound.try_recv().ok()
    }

    /// Wait up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<ClientEvent> {
        self.inbound.recv_timeout(timeout).ok()
    }

    /// Ask the daemon to stop drawing its workspaces dock and instead project
    /// the workspace catalog plus the dock rectangle for this client to paint.
    ///
    /// Also claims the named-session chrome, so the endpoint leaves the whole
    /// session slot blank and exposes no hit target — the native shell draws
    /// that top-row chrome itself.
    ///
    /// Sending a non-default [`ShellDockLayout`] is also what marks this
    /// connection as a machine-capable shell; plain clients never call this.
    pub fn request_workspace_dock(&self) -> Result<()> {
        self.send(ClientMessage::ShellDockLayout(ShellDockLayout {
            owns_workspaces: true,
            owns_session_chrome: true,
            ..ShellDockLayout::default()
        }))
    }
}

fn handshake(stream: &Stream, cols: u16, rows: u16) -> Result<(bool, EndpointIdentity)> {
    let mut reader = stream;
    let mut writer = stream;

    l3s_protocol::write_message(
        &mut writer,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
        },
    )?;

    match l3s_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Welcome { version, error } => {
            if let Some(reason) = error {
                return Err(Error::Rejected(reason));
            }
            if version != PROTOCOL_VERSION {
                return Err(Error::VersionMismatch {
                    client: PROTOCOL_VERSION,
                    server: version,
                });
            }
        }
        _ => return Err(Error::Rejected("expected a welcome frame".into())),
    }

    let probe_terminal = match l3s_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Ready { probe_terminal } => probe_terminal,
        _ => return Err(Error::Rejected("expected a ready frame".into())),
    };

    let identity = match l3s_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::EndpointIdentity { boot_id, session } => {
            EndpointIdentity { boot_id, session }
        }
        _ => return Err(Error::Rejected("expected an endpoint identity".into())),
    };

    if probe_terminal {
        // This client paints its own surfaces, so it has no terminal palette to
        // report. The server waits for this reply before streaming frames.
        l3s_protocol::write_message(&mut writer, &ClientMessage::TerminalColors(None))?;
    }

    Ok((probe_terminal, identity))
}

fn spawn_writer(stream: Arc<Stream>, outbound: Receiver<ClientMessage>) {
    thread::Builder::new()
        .name("l3s-writer".into())
        .spawn(move || {
            let mut writer = &*stream;
            for message in outbound {
                if let Err(error) = l3s_protocol::write_message(&mut writer, &message) {
                    tracing::debug!(%error, "l3s writer stopped");
                    break;
                }
            }
        })
        .ok();
}

fn spawn_reader(stream: Arc<Stream>, inbound: Sender<ClientEvent>) {
    thread::Builder::new()
        .name("l3s-reader".into())
        .spawn(move || {
            let mut reader = &*stream;
            loop {
                match l3s_protocol::read_message::<_, ServerMessage>(&mut reader) {
                    Ok(message) => {
                        if inbound.send(ClientEvent::Message(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        inbound
                            .send(ClientEvent::Disconnected {
                                reason: error.to_string(),
                            })
                            .ok();
                        break;
                    }
                }
            }
        })
        .ok();
}
