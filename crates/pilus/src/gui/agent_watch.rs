//! The agent list for the menu bar while no window is open.
//!
//! A window folds every connected endpoint's `agent.list` into the menu bar
//! itself. Once the last window closes that data would stop, so the app keeps
//! a [`Watch`]: one agents-only feed for the local daemon and one per enabled
//! machine that has a verified remote binary, the same hidden-endpoint feeds
//! a window runs. It exists only while no window does, and it owns no
//! rendering; [`Watch::rows`] hands the menu bar the same rows a window would.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use pilus_client::{Agent, AgentFeed, MachineInfo, list_machines};

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
    fn drain(&mut self) {
        while let Some(update) = self.feed.try_recv() {
            self.agents = update.agents;
        }
    }
}

/// The windowless agent feeds.
pub struct Watch {
    local: Source,
    machines: Vec<Source>,
    /// The machine catalog, read off-thread because it shells out to `luvus`.
    catalog: Receiver<Vec<MachineInfo>>,
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
        std::iter::once(&mut self.local)
            .chain(self.machines.iter_mut())
            .flat_map(|source| {
                source.drain();
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
}
