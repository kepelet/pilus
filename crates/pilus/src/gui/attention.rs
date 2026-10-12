//! Spots the moment an agent starts waiting on the user.
//!
//! Every endpoint's agent feed hands the view a fresh `agent.list` snapshot
//! each poll. [`Watch`] folds those snapshots into transitions: an agent that
//! was not `blocked` and now is, or one that stops being `blocked`. The view
//! drains the resulting [`Event`]s and decides how to surface them (a native
//! notification behind another app, a toast in front of it). Nothing here
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
    pub(super) agent: String,
    pub(super) workspace: String,
}

/// A transition worth reacting to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Event {
    Blocked(Blocked),
    /// The agent no longer waits, so its notification is stale.
    Cleared(String),
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
            let tag = format!("pilus-blocked-{endpoint}-{}", agent.pane);
            let was_blocked = previous.get(&agent.pane).is_some_and(|s| s == "blocked");
            let is_blocked = agent.status == "blocked";
            if seeded && is_blocked && !was_blocked {
                self.events.push(Event::Blocked(Blocked {
                    tag,
                    agent: agent_name(agent),
                    workspace: agent.workspace_name.clone(),
                }));
            } else if was_blocked && !is_blocked {
                self.events.push(Event::Cleared(tag));
            }
            current.insert(agent.pane.clone(), agent.status.clone());
        }
        // A pane that vanished while blocked (the agent exited) leaves its
        // banner behind unless it is cleared too.
        for (pane, status) in previous {
            if status == "blocked" && !current.contains_key(&pane) {
                self.events
                    .push(Event::Cleared(format!("pilus-blocked-{endpoint}-{pane}")));
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
                if status == "blocked" {
                    self.events
                        .push(Event::Cleared(format!("pilus-blocked-{endpoint}-{pane}")));
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
                agent: "claude".to_string(),
                workspace: "pilus".to_string(),
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
}
