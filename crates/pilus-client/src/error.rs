//! Errors raised while locating and attaching to a Luvus daemon.

use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure in socket discovery, transport, or handshake.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying socket operation failed.
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),

    /// A framed message could not be encoded or decoded.
    #[error(transparent)]
    Protocol(#[from] pilus_protocol::FrameError),

    /// The target does not exist at the resolved path.
    #[error("no Luvus server at {path}")]
    NotFound {
        /// The socket path that was probed.
        path: PathBuf,
    },

    /// The server speaks a different protocol revision.
    #[error("protocol mismatch: client speaks {client}, server speaks {server}")]
    VersionMismatch {
        /// Revision this client understands.
        client: u32,
        /// Revision the server announced.
        server: u32,
    },

    /// The server refused the handshake with a message.
    #[error("server rejected the connection: {0}")]
    Rejected(String),

    /// The server closed the connection before the handshake completed.
    #[error("server closed the connection during the handshake")]
    HandshakeClosed,

    /// The target names a session that cannot be resolved.
    #[error("invalid session name: {0}")]
    InvalidSession(String),

    /// The daemon was not running and starting it failed.
    #[error("could not start Luvus server: {0}")]
    ServerStart(String),

    /// The JSON control API rejected the request or returned something
    /// unreadable.
    #[error("control API error: {0}")]
    Control(String),

    /// A saved remote machine could not be bridged: missing binary, failed
    /// SSH launch, or a remote command that exited before the handshake.
    #[error("{0}")]
    Machine(String),

    /// A project definition could not be saved or read.
    #[error("projects error: {0}")]
    Projects(String),

    /// The local "this node" connector could not start, stop, or be read.
    #[error("connector error: {0}")]
    Connector(String),
}
