//! The agents the index reads and the rows it keeps.

/// A coding agent whose transcripts the index reads.
///
/// Both store one JSONL file per session. The directory is the working
/// directory with its separators flattened, so the file, not the directory
/// name, is the source of the real `cwd`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Agent {
    Claude,
    Pi,
}

impl Agent {
    /// Every agent the index reads, in the order the filter lists them.
    pub const ALL: [Self; 2] = [Self::Pi, Self::Claude];

    /// The kind id the daemon uses for the same agent (`agent.sessions`).
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Pi => "pi",
        }
    }

    /// The label drawn in the window.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Pi => "Pi",
        }
    }

    /// The agent for a kind id, or `None` for one the index does not read.
    #[must_use]
    pub fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.kind() == kind)
    }

    /// Where the agent keeps its sessions, relative to `$HOME`.
    pub(crate) const fn root(self) -> &'static str {
        match self {
            Self::Claude => ".claude/projects",
            Self::Pi => ".pi/agent/sessions",
        }
    }

    /// The agent a transcript path belongs to, judged by its directory so a
    /// remote listing needs no home directory to classify its lines.
    pub(crate) fn from_path(path: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|agent| path.contains(&format!("/{}/", agent.root())))
    }
}

/// One indexed session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// The machine it lives on: `local`, or a saved profile's id.
    pub machine: String,
    pub agent: Agent,
    /// The agent's own id, which `agent.resume` takes.
    pub session_id: String,
    /// The directory the session was recorded in; empty when unknown.
    pub cwd: String,
    /// The name the agent or the person gave it, else the first prompt.
    pub title: String,
    /// The git branch it ran on, when the agent records one.
    pub branch: String,
    /// Unix seconds of the first prompt.
    pub started: i64,
    /// Unix seconds of the last write to the transcript.
    pub updated: i64,
    /// The transcript file, which is also the row's identity on its machine.
    pub path: String,
    pub size: u64,
}
