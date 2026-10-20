//! Connection layer for `pilus`.
//!
//! [`ConnectTarget`] resolves a Luvus client socket, [`connect`] performs the
//! version-checked handshake, and the returned [`Connection`] streams
//! [`ServerMessage`]s to the UI while accepting [`ClientMessage`]s back.
//! [`Surface`] folds those messages into the cell grid a window paints.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod agent_service;
pub mod agents;
pub mod ansi;
mod arrival;
mod autostart;
mod commit;
mod connection;
mod connector;
mod control;
mod diff;
mod discovery;
mod download;
mod entries;
mod error;
mod files;
pub mod folders;
mod git;
mod git_status;
mod github;
mod github_account;
mod machines;
mod mission;
pub mod netlog;
mod ports;
pub mod process;
mod read;
mod relay;
pub mod sipil;
mod stage;
mod surface;
mod tasks;
mod transport;
mod uhp;
mod upload;
mod write;
pub use agents::AgentConfig;
pub use agents::AgentModel;
/// Display labels for pi thinking levels (`high` to `High`).
pub use agents::effort_label;
/// Raw pi thinking levels for display labels (`High` to `high`).
pub use agents::thinking_level;
pub use arrival::Arrival;
pub use autostart::{delete_session, ensure_server, ensure_server_for, stop_session};
pub use commit::{CommitDetail, CommitFile, show_commit};
pub use connection::{ClientEvent, Connection, EndpointIdentity};
pub use connector::agent_binary;
pub use control::{
    Agent, AgentFeed, AgentSession, Appearance, AutomationSummary, BarSegment, BarWidget,
    ControlClient, ControlTransport, FeedScope, FeedUpdate, PaneFrame, PaneInfo, Tab, TaskAction,
    TaskSummary, ThemeInfo, WorktreeInfo, set_feed_slowdown, worktree_blockers,
};
pub use diff::{
    DiffFile, DiffHunk, DiffLayer, DiffLine, DiffLineKind, DiffNote, DiffSnapshot, FileDiff,
    NoteKind, NoteRange, NoteSelection, NoteSide, NoteState,
};
pub use discovery::{
    HOME_ENV, PILUS_HOME_ENV, SessionInfo, active_session, api_socket_path_for,
    client_socket_path_for, client_socket_path_under, config_dir, current_uid, describe,
    generate_session_name, is_provisioning_cache, list_session_infos, list_sessions, luvus_binary,
    luvus_command, pilus_config_dir, pilus_workspaces_dir, session_dir_for, split_client_socket,
    validate_session,
};
pub use download::{archive_name, download_path};
pub use entries::{create_entry, delete_entry, name_problem, parent_of, rename_entry};
pub use error::{Error, Result};
pub use files::{FileChange, FileChanges, FileRow, FileTree, OpenTarget, visible_rows};
pub use git::{
    RemoteRepos, common_git_dirs_remote, discard_worktree_changes, expand_local, expand_remote,
    init_repository, remote_home, worktree_is_dirty,
};
pub use git_status::{CommitRef, CommitRefKind, GitCommit, GitStatus};
pub use github::{
    Checks, Comment, CreatedPull, GhRepo, GhState, Issue, IssueDetail, Label, NewPull, Pull,
    PullDetail, PullDraft, RepoInfo, StateFilter, unix_seconds,
};
pub use github_account::{AccountState, GhAccount, ItemSummary, Listing, RepoSummary, RunSummary};
pub use machines::{
    MachineInfo, connect_machine, find_installed_binary, is_behind, list_machines,
    refresh_machines, restart_remote, stop_remote,
};
pub use mission::{
    AutomationDetail, AutomationHealth, AutomationRow, AutomationRun, MissionRow, MissionScope,
    MissionSnapshot, MissionUsage,
};
pub use pilus_protocol as protocol;
pub use ports::{ListeningPort, PortForward, free_local_port, kill_process, list_ports};
pub use read::{MAX_TEXT_BYTES, TextFile, Version, read_text};
pub use relay::RelaySettings;
pub use stage::{Discard, StageAction, discard, set_staged};
pub use surface::{Surface, SurfaceChange};
pub use tasks::{NewTask, Task, TaskStatus, split_list};
pub use transport::SshTarget;
pub use uhp::{Capabilities, Event, EventStream, Frame, Observer, Terminal, UhpClient};
pub use upload::upload_path;
pub use write::write_text;

use std::path::PathBuf;

/// Where `pilus` should look for a Luvus daemon.
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
    /// Honors `$LUVUS_SESSION` so `pilus` launched from inside a pane attaches to
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
///
/// When nothing listens at the target socket, the daemon isn't running, so
/// it is started first ([`ensure_server`]) and the attach is retried until
/// it stops initializing. Any other failure is returned as-is.
pub fn connect(target: &ConnectTarget, cols: u16, rows: u16) -> Result<Connection> {
    match Connection::open(target, cols, rows) {
        Err(Error::NotFound { .. }) => {}
        other => return other,
    }
    ensure_server(target)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match Connection::open(target, cols, rows) {
            Err(Error::NotFound { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => return other,
        }
    }
}
