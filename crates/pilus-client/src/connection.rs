//! Attaching to a daemon: handshake, framed IO, and the background reader.
//!
//! The sequence on the wire is fixed:
//!
//! 1. client sends [`ClientMessage::Hello`] with its protocol revision;
//! 2. server replies [`ServerMessage::Welcome`] (an `error` means mismatch);
//! 3. server sends [`ServerMessage::Ready`] and [`ServerMessage::EndpointIdentity`];
//! 4. if `Ready::probe_terminal` is set, the client replies with
//!    [`ClientMessage::TerminalColors`] before the server will stream frames.

use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use pilus_protocol::{
    ClientMessage, FALLBACK_PROTOCOL_VERSION, PROTOCOL_VERSION, ServerMessage, ShellDockLayout,
    SurfaceInterest, set_decode_version,
};

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
    /// Kept so dropping the connection can terminate a spawned SSH bridge.
    stream: Arc<Stream>,
    label: String,
    identity: EndpointIdentity,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // A remote reader may be parked in `read`; killing the child closes
        // its stdout and lets that thread exit. Local sockets are a no-op.
        self.stream.terminate();
    }
}

impl Connection {
    /// Attach to `target`, announcing an initial `cols` by `rows` viewport.
    ///
    /// The pinned revision is tried first, then [`FALLBACK_PROTOCOL_VERSION`],
    /// so one build attaches to a daemon that has not been updated yet. The
    /// daemon closes the link when it rejects a revision, so each attempt dials
    /// a fresh socket.
    pub fn open(target: &ConnectTarget, cols: u16, rows: u16) -> Result<Self> {
        let socket = target.resolve();
        let label = socket.display().to_string();
        Self::attach_negotiated(&label, cols, rows, || Stream::connect(&socket))
    }

    /// Attach by trying each supported protocol revision against a freshly
    /// built stream. Shared by the local socket and the SSH bridge, which both
    /// can rebuild their transport cheaply.
    pub fn attach_negotiated(
        label: &str,
        cols: u16,
        rows: u16,
        mut connect: impl FnMut() -> Result<Stream>,
    ) -> Result<Self> {
        let mut last_server = None;
        for version in candidate_versions() {
            let stream = connect()?;
            match Self::attach(stream, label, cols, rows, version) {
                Ok(connection) => return Ok(connection),
                Err(Error::VersionMismatch { server, .. }) => last_server = Some(server),
                Err(error) => return Err(error),
            }
        }
        // Neither supported revision matched. Report it against the newest one
        // so the message names what this build speaks, not the last attempt.
        Err(Error::VersionMismatch {
            client: PROTOCOL_VERSION,
            server: last_server.unwrap_or_default(),
        })
    }

    /// Attach over an already-connected stream at the pinned revision.
    ///
    /// `label` is a human-readable endpoint name used in errors and status
    /// text. A remote SSH bridge hands its child pipes in here; the handshake
    /// and frame loop are identical to the local socket path.
    pub fn open_stream(
        stream: Stream,
        label: impl Into<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let label = label.into();
        Self::attach(stream, &label, cols, rows, PROTOCOL_VERSION)
    }

    /// Handshake at `version` and spawn the reader/writer threads.
    fn attach(stream: Stream, label: &str, cols: u16, rows: u16, version: u32) -> Result<Self> {
        let stream = Arc::new(stream);
        let (probe_terminal, identity) =
            handshake(&stream, cols, rows, version).map_err(|error| {
                match stream.stderr_tail() {
                    Some(tail) => Error::Machine(format!("{error}: {tail}")),
                    None => error,
                }
            })?;

        let (outbound_tx, outbound_rx) = unbounded::<ClientMessage>();
        let (inbound_tx, inbound_rx) = unbounded::<ClientEvent>();

        spawn_writer(Arc::clone(&stream), outbound_rx);
        spawn_reader(Arc::clone(&stream), inbound_tx.clone(), version);

        inbound_tx
            .send(ClientEvent::Ready {
                probe_terminal,
                identity: identity.clone(),
            })
            .ok();

        Ok(Self {
            outbound: outbound_tx,
            inbound: inbound_rx,
            stream,
            label: label.to_string(),
            identity,
        })
    }

    /// The endpoint label this connection was opened with.
    pub fn label(&self) -> &str {
        &self.label
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

    /// Tell the server how much of this endpoint's surface we want.
    ///
    /// Inactive endpoints are suspended: they keep owning their panes and PTY
    /// sizes but stop streaming frames, so a saved machine costs one SSH
    /// process and a small reader rather than a second render loop.
    pub fn set_interest(&self, interest: SurfaceInterest) -> Result<()> {
        self.send(ClientMessage::SurfaceInterest(interest))
    }

    /// Ask for a fresh candidate frame tagged with `ticket`.
    ///
    /// The two-phase handoff: the current surface stays interactive until the
    /// matching `PreparedFrame` arrives, so a slow or failed destination never
    /// blanks a usable window.
    pub fn prepare_surface(&self, ticket: u64, cols: u16, rows: u16) -> Result<()> {
        self.send(ClientMessage::PrepareSurface { ticket, cols, rows })
    }

    /// Send a lightweight liveness check; the reply echoes `nonce`.
    pub fn health_check(&self, nonce: u64) -> Result<()> {
        self.send(ClientMessage::HealthCheck { nonce })
    }
}

/// Protocol revisions to try, newest first.
fn candidate_versions() -> [u32; 2] {
    [PROTOCOL_VERSION, FALLBACK_PROTOCOL_VERSION]
}

fn handshake(
    stream: &Stream,
    cols: u16,
    rows: u16,
    version: u32,
) -> Result<(bool, EndpointIdentity)> {
    let mut reader = stream;
    let mut writer = stream;

    pilus_protocol::write_message(
        &mut writer,
        &ClientMessage::Hello {
            version,
            cols,
            rows,
        },
    )?;

    match pilus_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Welcome {
            version: server,
            error,
        } => {
            if let Some(reason) = error {
                // The server reports its revision even when it rejects. A
                // mismatch is retryable at another revision, so surface it as
                // one; any other rejection is final.
                if reason.contains("protocol version mismatch") {
                    return Err(Error::VersionMismatch {
                        client: version,
                        server,
                    });
                }
                return Err(Error::Rejected(reason));
            }
            if server != version {
                return Err(Error::VersionMismatch {
                    client: version,
                    server,
                });
            }
        }
        _ => return Err(Error::Rejected("expected a welcome frame".into())),
    }

    let probe_terminal = match pilus_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Ready { probe_terminal } => probe_terminal,
        _ => return Err(Error::Rejected("expected a ready frame".into())),
    };

    let identity = match pilus_protocol::read_message::<_, ServerMessage>(&mut reader)? {
        ServerMessage::EndpointIdentity { boot_id, session } => {
            EndpointIdentity { boot_id, session }
        }
        _ => return Err(Error::Rejected("expected an endpoint identity".into())),
    };

    if probe_terminal {
        // This client paints its own surfaces, so it has no terminal palette to
        // report. The server waits for this reply before streaming frames.
        pilus_protocol::write_message(&mut writer, &ClientMessage::TerminalColors(None))?;
    }

    Ok((probe_terminal, identity))
}

fn spawn_writer(stream: Arc<Stream>, outbound: Receiver<ClientMessage>) {
    thread::Builder::new()
        .name("pilus-writer".into())
        .spawn(move || {
            let mut writer = &*stream;
            for message in outbound {
                if let Err(error) = pilus_protocol::write_message(&mut writer, &message) {
                    tracing::debug!(%error, "pilus writer stopped");
                    break;
                }
            }
        })
        .ok();
}

fn spawn_reader(stream: Arc<Stream>, inbound: Sender<ClientEvent>, version: u32) {
    thread::Builder::new()
        .name("pilus-reader".into())
        .spawn(move || {
            // Frames are positional; tell this thread's decoder which shape the
            // negotiated daemon sends.
            set_decode_version(version);
            let mut reader = &*stream;
            loop {
                match pilus_protocol::read_message::<_, ServerMessage>(&mut reader) {
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A scripted peer on a child process's stdio, proving a remote bridge is
    /// handshaken exactly like a local socket. Raw bincode frames are written
    /// with `printf` so the test needs no helper binary.
    #[test]
    fn handshakes_over_a_spawned_child_stream() {
        // Welcome carries the protocol revision as a bincode varint; `\024` is
        // 20, matching [`PROTOCOL_VERSION`].
        let script = r"head -c 8 >/dev/null; printf '\003\000\000\000\000\024\000'; printf '\002\000\000\000\013\000'; printf '\012\000\000\000\022\001\007default'; sleep 1";
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        let stream = Stream::spawn(&mut command).expect("spawn the scripted peer");
        let connection = Connection::open_stream(stream, "scripted", 120, 40).expect("handshake");
        assert_eq!(connection.identity().session, "default");
        assert_eq!(connection.identity().boot_id, 1);
    }
}
