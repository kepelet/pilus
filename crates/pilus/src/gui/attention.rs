//! Spots the moment an agent starts waiting on the user, or finishes.
//!
//! Every endpoint's agent feed hands the view a fresh `agent.list` snapshot
//! each poll. [`Watch`] folds those snapshots into transitions: an agent that
//! was not `blocked` and now is, one that stops being `blocked`, or one that
//! goes from `working` to `done`. The consumer drains the resulting [`Event`]s
//! and decides how to surface them (a native notification behind another app,
//! a toast in front of it, or, with no window open, the menu bar helper's
//! notification). Nothing here
//! touches GPUI, so the transition rules are unit-testable on their own.

use std::collections::{HashMap, HashSet};

use pilus_client::Agent;

use super::orchestration::agent_name;

/// An agent that just started waiting on the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Blocked {
    /// Stable per agent pane, so a repeat for the same pane replaces the
    /// earlier notification instead of stacking.
    pub(super) tag: String,
    /// Where a click on the banner should go: the owning endpoint's key and
    /// the agent's pane id.
    pub(super) endpoint: String,
    pub(super) pane: String,
    pub(super) agent: String,
    /// What the banner is headed with; see [`banner_title`].
    pub(super) title: String,
}

/// An agent that just finished its turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Finished {
    /// Stable per agent pane, like [`Blocked::tag`].
    pub(super) tag: String,
    pub(super) endpoint: String,
    pub(super) pane: String,
    pub(super) agent: String,
    /// What the banner is headed with; see [`banner_title`].
    pub(super) title: String,
}

/// A transition worth reacting to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Event {
    Blocked(Blocked),
    Finished(Finished),
    /// The agent no longer waits or reports done, so its notification is
    /// stale.
    Cleared(String),
}

/// The banner's title: the agent's own session title, else which agent on
/// which workspace so the banner still says where to look.
fn banner_title(agent: &Agent) -> String {
    if let Some(title) = agent.session_title.as_deref().map(str::trim)
        && !title.is_empty()
    {
        return title.to_string();
    }
    match agent.workspace_name.as_str() {
        "" => agent_name(agent),
        workspace => format!("{} on {workspace}", agent_name(agent)),
    }
}

fn blocked_tag(endpoint: &str, pane: &str) -> String {
    format!("pilus-blocked-{endpoint}-{pane}")
}

fn finished_tag(endpoint: &str, pane: &str) -> String {
    format!("pilus-done-{endpoint}-{pane}")
}

/// Last seen status per pane, per endpoint.
#[derive(Default)]
pub(super) struct Watch {
    seen: HashMap<String, HashMap<String, String>>,
    /// Endpoints that have reported once. An agent already blocked when pilus
    /// starts, or when a machine first connects, is old news, not an alert.
    seeded: HashSet<String>,
    events: Vec<Event>,
}

impl Watch {
    /// Fold one endpoint's agent list in, queuing any transitions.
    pub(super) fn observe(&mut self, endpoint: &str, agents: &[Agent]) {
        let seeded = !self.seeded.insert(endpoint.to_string());
        let previous = self.seen.remove(endpoint).unwrap_or_default();
        let mut current = HashMap::with_capacity(agents.len());
        for agent in agents {
            let was = previous.get(&agent.pane).map(String::as_str);
            let is = agent.status.as_str();
            if seeded && is == "blocked" && was != Some("blocked") {
                self.events.push(Event::Blocked(Blocked {
                    tag: blocked_tag(endpoint, &agent.pane),
                    endpoint: endpoint.to_string(),
                    pane: agent.pane.clone(),
                    agent: agent_name(agent),
                    title: banner_title(agent),
                }));
            } else if was == Some("blocked") && is != "blocked" {
                self.events
                    .push(Event::Cleared(blocked_tag(endpoint, &agent.pane)));
            }
            // Only a turn that was running counts as finishing: an agent that
            // first shows up already `done` is history, not news.
            if seeded && is == "done" && was == Some("working") {
                self.events.push(Event::Finished(Finished {
                    tag: finished_tag(endpoint, &agent.pane),
                    endpoint: endpoint.to_string(),
                    pane: agent.pane.clone(),
                    agent: agent_name(agent),
                    title: banner_title(agent),
                }));
            } else if was == Some("done") && is != "done" {
                self.events
                    .push(Event::Cleared(finished_tag(endpoint, &agent.pane)));
            }
            current.insert(agent.pane.clone(), agent.status.clone());
        }
        // A pane that vanished while blocked (the agent exited) leaves its
        // banner behind unless it is cleared too.
        for (pane, status) in previous {
            if current.contains_key(&pane) {
                continue;
            }
            match status.as_str() {
                "blocked" => self
                    .events
                    .push(Event::Cleared(blocked_tag(endpoint, &pane))),
                "done" => self
                    .events
                    .push(Event::Cleared(finished_tag(endpoint, &pane))),
                _ => {}
            }
        }
        self.seen.insert(endpoint.to_string(), current);
    }

    /// Forget every endpoint, as when the window moves to another session.
    ///
    /// Pane ids restart in every daemon, so the old statuses would answer for
    /// the new panes. Banners still up for the old panes are cleared, and the
    /// next snapshot seeds again instead of alerting on what is already blocked.
    pub(super) fn forget_all(&mut self) {
        self.seeded.clear();
        for (endpoint, panes) in std::mem::take(&mut self.seen) {
            for (pane, status) in panes {
                match status.as_str() {
                    "blocked" => self
                        .events
                        .push(Event::Cleared(blocked_tag(&endpoint, &pane))),
                    "done" => self
                        .events
                        .push(Event::Cleared(finished_tag(&endpoint, &pane))),
                    _ => {}
                }
            }
        }
    }

    /// Take the transitions queued since the last call.
    pub(super) fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, status: &str) -> Agent {
        Agent {
            pane: pane.to_string(),
            agent: "claude".to_string(),
            status: status.to_string(),
            workspace_name: "pilus".to_string(),
            ..Agent::default()
        }
    }

    #[test]
    fn a_banner_is_titled_by_the_agents_session_else_by_agent_and_workspace() {
        let mut agent = agent("1", "blocked");
        assert_eq!(banner_title(&agent), "claude on pilus");
        agent.session_title = Some("  fix the login bug ".to_string());
        assert_eq!(banner_title(&agent), "fix the login bug");
        agent.session_title = Some(String::new());
        agent.name = "reviewer".to_string();
        assert_eq!(banner_title(&agent), "reviewer on pilus");
        agent.workspace_name.clear();
        assert_eq!(banner_title(&agent), "reviewer");
    }

    #[test]
    fn first_snapshot_seeds_without_alerting() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "blocked")]);
        assert!(watch.drain().is_empty());
    }

    #[test]
    fn working_to_blocked_alerts_once() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        watch.observe("local", &[agent("1", "blocked")]);
        let events = watch.drain();
        assert_eq!(
            events,
            vec![Event::Blocked(Blocked {
                tag: "pilus-blocked-local-1".to_string(),
                endpoint: "local".to_string(),
                pane: "1".to_string(),
                agent: "claude".to_string(),
                title: "claude on pilus".to_string(),
            })]
        );
        watch.observe("local", &[agent("1", "blocked")]);
        assert!(
            watch.drain().is_empty(),
            "a steady state is not a transition"
        );
    }

    #[test]
    fn unblocking_clears_the_banner() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        watch.observe("local", &[agent("1", "blocked")]);
        watch.drain();
        watch.observe("local", &[agent("1", "working")]);
        assert_eq!(
            watch.drain(),
            vec![Event::Cleared("pilus-blocked-local-1".to_string())]
        );
    }

    #[test]
    fn a_blocked_agent_that_exits_clears_the_banner() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        watch.observe("local", &[agent("1", "blocked")]);
        watch.drain();
        watch.observe("local", &[]);
        assert_eq!(
            watch.drain(),
            vec![Event::Cleared("pilus-blocked-local-1".to_string())]
        );
    }

    #[test]
    fn forgetting_every_endpoint_clears_its_banners_and_seeds_again() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        watch.observe("local", &[agent("1", "blocked")]);
        watch.drain();
        watch.forget_all();
        assert_eq!(
            watch.drain(),
            vec![Event::Cleared("pilus-blocked-local-1".to_string())]
        );
        // Another daemon's pane 1 is already blocked: old news, not an alert.
        watch.observe("local", &[agent("1", "blocked")]);
        assert!(watch.drain().is_empty());
    }

    #[test]
    fn endpoints_are_tracked_independently() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        // A machine's first snapshot seeds even though `local` already has.
        watch.observe("box", &[agent("1", "blocked")]);
        assert!(watch.drain().is_empty());
        watch.observe("local", &[agent("1", "blocked")]);
        assert_eq!(watch.drain().len(), 1);
    }

    #[test]
    fn working_to_done_notifies_once_and_a_new_turn_clears_it() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "working")]);
        watch.observe("local", &[agent("1", "done")]);
        assert_eq!(
            watch.drain(),
            vec![Event::Finished(Finished {
                tag: "pilus-done-local-1".to_string(),
                endpoint: "local".to_string(),
                pane: "1".to_string(),
                agent: "claude".to_string(),
                title: "claude on pilus".to_string(),
            })]
        );
        watch.observe("local", &[agent("1", "done")]);
        assert!(watch.drain().is_empty());
        watch.observe("local", &[agent("1", "working")]);
        assert_eq!(
            watch.drain(),
            vec![Event::Cleared("pilus-done-local-1".to_string())]
        );
    }

    #[test]
    fn an_agent_first_seen_done_or_idle_is_not_news() {
        let mut watch = Watch::default();
        watch.observe("local", &[agent("1", "idle")]);
        watch.observe("local", &[agent("1", "done"), agent("2", "done")]);
        assert!(
            watch
                .drain()
                .iter()
                .all(|event| !matches!(event, Event::Finished(_)))
        );
    }
}
