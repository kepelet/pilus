//! Which lifecycle action a Luvus session's row in the session menu offers,
//! and where the window goes when the session it is attached to stops.
//!
//! The rules are separate from the menu so they can be tested without a
//! window. They exist because a local daemon that drops is restarted by the
//! reconnect loop: stopping the session the window is attached to would
//! bring it straight back, so the window must leave it first.

use pilus_client::SessionInfo;

/// The session every state root has, which the daemon refuses to delete.
const DEFAULT: &str = "default";

/// A lifecycle action a session row can offer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SessionVerb {
    /// Stop the daemon and its panes, keeping the session's state.
    Stop,
    /// Remove a stopped session's state.
    Delete,
}

impl SessionVerb {
    /// The past tense, for a toast.
    pub(super) fn done(self) -> &'static str {
        match self {
            Self::Stop => "stopped",
            Self::Delete => "deleted",
        }
    }

    /// The infinitive, for an error toast.
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Delete => "delete",
        }
    }
}

/// The session to attach to when `leaving` stops: the default one if it is
/// running, otherwise the first other running session. `None` means nothing
/// else is up, so stopping would only be undone by the reconnect.
pub(super) fn fallback(sessions: &[SessionInfo], leaving: &str) -> Option<String> {
    let mut running = sessions
        .iter()
        .filter(|session| session.running && session.name != leaving);
    let first = running.next()?;
    // `list_session_infos` puts the default first, so it is already preferred.
    Some(first.name.clone())
}

/// The action `session`'s row offers, if any. A running session can stop
/// unless it is the one the window is attached to and has nowhere else to go;
/// a stopped one can be deleted unless it is the default.
pub(super) fn offered(
    session: &SessionInfo,
    current: &str,
    sessions: &[SessionInfo],
) -> Option<SessionVerb> {
    if session.running {
        let movable = session.name != current || fallback(sessions, current).is_some();
        return movable.then_some(SessionVerb::Stop);
    }
    (session.name != DEFAULT).then_some(SessionVerb::Delete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, running: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            running,
        }
    }

    #[test]
    fn leaving_prefers_the_default_session() {
        let all = [
            session("default", true),
            session("alpha", true),
            session("beta", true),
        ];
        assert_eq!(fallback(&all, "beta").as_deref(), Some("default"));
        assert_eq!(fallback(&all, "default").as_deref(), Some("alpha"));
    }

    #[test]
    fn leaving_skips_stopped_sessions_and_itself() {
        let all = [
            session("default", false),
            session("alpha", true),
            session("beta", false),
        ];
        assert_eq!(fallback(&all, "alpha"), None);
        assert_eq!(fallback(&all, "beta").as_deref(), Some("alpha"));
    }

    #[test]
    fn a_running_session_stops_unless_it_is_the_last_one_attached() {
        let only = [session("default", true), session("alpha", false)];
        assert_eq!(offered(&only[0], "default", &only), None);
        // The same session is stoppable from elsewhere.
        assert_eq!(offered(&only[0], "alpha", &only), Some(SessionVerb::Stop),);
        let two = [session("default", true), session("alpha", true)];
        assert_eq!(offered(&two[1], "alpha", &two), Some(SessionVerb::Stop));
    }

    #[test]
    fn a_stopped_session_deletes_except_the_default() {
        let all = [session("default", false), session("alpha", false)];
        assert_eq!(offered(&all[0], "alpha", &all), None);
        assert_eq!(offered(&all[1], "default", &all), Some(SessionVerb::Delete));
    }
}
