//! The agent list for the menu bar while no window is open.
//!
//! A window folds every connected endpoint's `agent.list` into the menu bar
//! itself. Once the last window closes that data would stop, so the app keeps
//! a [`Watch`]: one agents-only feed for the local daemon and one per enabled
//! machine that has a verified remote binary, the same hidden-endpoint feeds
//! a window runs. It exists only while no window does, and it owns no
//! rendering; [`Watch::rows`] hands the menu bar the same rows a window would.
//!
//! The same snapshots also feed an [`attention::Watch`], so an agent that
//! blocks or finishes while no window is open still reaches the user:
//! [`Watch::alerts`] hands the caller those transitions to post as native
//! notifications. The daemon's own sound cue travels over the render
//! connection, which the helper does not open, so these notifications are how
//! a quit pilus stays audible.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use pilus_client::{Agent, AgentFeed, MachineInfo, list_machines};

use super::attention;
use super::menubar::Row;
use super::theme::Theme;
use super::{agent_menu_row, shorten_home};

/// One endpoint's feed and its latest agents.
struct Source {
    key: String,
    group: String,
    feed: AgentFeed,
    agents: Vec<Agent>,
}

impl Source {
    /// Take the feed's pending updates; whether the list changed hands.
    fn drain(&mut self) -> bool {
        let mut fresh = false;
        while let Some(update) = self.feed.try_recv() {
            self.agents = update.agents;
            fresh = true;
        }
        fresh
    }
}

/// The windowless agent feeds.
pub struct Watch {
    local: Source,
    machines: Vec<Source>,
    /// The machine catalog, read off-thread because it shells out to `luvus`.
    catalog: Receiver<Vec<MachineInfo>>,
    attention: attention::Watch,
    /// Tags of the notifications still standing: blocked or finished agents
    /// the user has not come back to. Its size is the app icon's badge.
    open: HashSet<String>,
}

impl Watch {
    /// Watch the local daemon at `socket`, then every enabled machine under
    /// `session_root` once the catalog has loaded.
    pub fn new(socket: PathBuf, session_root: PathBuf, session: Option<String>) -> Self {
        let (tx, catalog) = channel();
        std::thread::spawn(move || {
            tx.send(list_machines(&session_root, session.as_deref()))
                .ok();
        });
        Self {
            local: Source {
                key: "local".to_string(),
                group: "Local".to_string(),
                feed: AgentFeed::spawn_agents(socket),
                agents: Vec::new(),
            },
            machines: Vec::new(),
            catalog,
            attention: attention::Watch::default(),
            open: HashSet::new(),
        }
    }

    /// Fold pending feed updates into menu rows: local first, then machines.
    pub fn rows(&mut self, theme: Theme) -> Vec<Row> {
        if let Ok(machines) = self.catalog.try_recv() {
            self.machines = machines
                .iter()
                .filter(|machine| machine.id != "local" && machine.enabled)
                .filter_map(|machine| {
                    Some(Source {
                        key: machine.id.clone(),
                        group: machine.display().to_string(),
                        feed: AgentFeed::spawn_ssh_agents(machine.ssh_target()?),
                        agents: Vec::new(),
                    })
                })
                .collect();
        }
        let attention = &mut self.attention;
        std::iter::once(&mut self.local)
            .chain(self.machines.iter_mut())
            .flat_map(|source| {
                if source.drain() {
                    attention.observe(&source.key, &source.agents);
                }
                let local = source.key == "local";
                source
                    .agents
                    .iter()
                    .map(|agent| {
                        // A remote cwd is absolute on that machine, and this
                        // machine's home says nothing about it.
                        let path = if local {
                            shorten_home(&agent.cwd)
                        } else {
                            agent.cwd.clone()
                        };
                        agent_menu_row(&source.key, &source.group, agent, path, theme)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The display name of the endpoint `key`, once its machine is known.
    pub(super) fn group_of(&self, key: &str) -> Option<String> {
        std::iter::once(&self.local)
            .chain(self.machines.iter())
            .find(|source| source.key == key)
            .map(|source| source.group.clone())
    }

    /// Take the blocked and finished transitions seen since the last call.
    pub(super) fn alerts(&mut self) -> Vec<attention::Event> {
        let events = self.attention.drain();
        for event in &events {
            match event {
                attention::Event::Blocked(blocked) => {
                    self.open.insert(blocked.tag.clone());
                }
                attention::Event::Finished(finished) => {
                    self.open.insert(finished.tag.clone());
                }
                attention::Event::Cleared(tag) => {
                    self.open.remove(tag);
                }
            }
        }
        events
    }

    /// How many notifications are still standing.
    pub(super) fn open_alerts(&self) -> u32 {
        u32::try_from(self.open.len()).unwrap_or(u32::MAX)
    }
}
