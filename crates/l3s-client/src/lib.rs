//! Connection layer for `l3s`.
//!
//! [`ConnectTarget`] resolves a Luvus client socket, [`connect`] performs the
//! version-checked handshake, and the returned [`Connection`] streams
//! [`ServerMessage`]s to the UI while accepting [`ClientMessage`]s back.
//! [`Surface`] folds those messages into the cell grid a window paints.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod ansi;
mod connection;
mod control;
mod discovery;
mod error;
mod surface;
mod transport;
mod uhp;
pub use connection::{ClientEvent, Connection, EndpointIdentity};
pub use control::{Agent, AgentFeed, BarSegment, BarWidget, ControlClient, FeedUpdate, Tab};
pub use discovery::{
    active_session, api_socket_path_for, client_socket_path_for, config_dir, describe,
    list_sessions, session_dir_for, validate_session,
};
pub use error::{Error, Result};
pub use l3s_protocol as protocol;
pub use surface::{Surface, SurfaceChange};
pub use uhp::{Event, EventStream, Frame, Observer, Terminal, UhpClient};

use std::path::PathBuf;

/// Where `l3s` should look for a Luvus daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectTarget {
    /// Explicit named session, or `None` for the default session.
    pub session: Option<String>,
    /// Explicit client-socket path, overriding discovery.
    pub socket: Option<PathBuf>,
}

impl Default for ConnectTarget {
    fn default() -> Self {
        Self::local()
    }
}

impl ConnectTarget {
    /// The default session on this machine.
    ///
    /// Honors `$LUVUS_SESSION` so `l3s` launched from inside a pane attaches to
    /// the same session as the tooling around it.
    pub fn local() -> Self {
        Self {
            session: active_session(),
            socket: None,
        }
    }

    /// A named session on this machine.
    pub fn session(name: impl Into<String>) -> Self {
        let name = name.into();
        let name = (name != "default").then_some(name);
        Self {
            session: name,
            socket: None,
        }
    }

    /// An explicit client-socket path.
    pub fn socket(path: impl Into<PathBuf>) -> Self {
        Self {
            session: None,
            socket: Some(path.into()),
        }
    }

    /// The client socket this target refers to.
    pub fn resolve(&self) -> PathBuf {
        match &self.socket {
            Some(path) => path.clone(),
            None => client_socket_path_for(self.session.as_deref()),
        }
    }

    /// A short human-readable label for window titles and errors.
    pub fn label(&self) -> String {
        match (&self.session, &self.socket) {
            (Some(name), _) => name.clone(),
            (None, Some(path)) => path.display().to_string(),
            (None, None) => "default".to_string(),
        }
    }
}

/// Attach to `target` with an initial `cols` by `rows` viewport.
pub fn connect(target: &ConnectTarget, cols: u16, rows: u16) -> Result<Connection> {
    Connection::open(target, cols, rows)
}
