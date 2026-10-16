//! The Home composer's Issue picker: the open GitHub issues of the workspace
//! the composer targets, offered as something to attach to the prompt.
//!
//! The daemon has no UHP call for GitHub, so the list comes from the GitHub
//! CLI (`pilus_client::GhRepo`) in the workspace's directory, on this Mac or
//! over the machine's saved SSH profile. The read runs off the GUI thread and
//! answers through this module's channel; an answer for a workspace the
//! composer no longer targets is dropped.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::Context;
use pilus_client::{GhRepo, Issue, StateFilter};

use super::home::HomeField;
use super::{EndpointId, PilusView};

/// Issues a read asks for.
const LIST_LIMIT: u32 = 50;
/// How long a read of the open issues is trusted while the picker reopens.
const TTL: Duration = Duration::from_secs(60);
/// Longest an attached issue's title runs on the picker chip.
const CHIP_TITLE: usize = 26;

/// The workspace a read was made for: where it lives and which directory.
type Spot = (EndpointId, String);

/// A finished read, tagged with the workspace it was made for.
struct Read {
    spot: Spot,
    result: Result<Vec<Issue>, String>,
}

/// The issues the Home composer can attach, for the workspace it targets.
///
/// Read when the Issue picker opens, not before: most launches attach nothing,
/// and each read is a GitHub round trip.
pub(super) struct HomeIssues {
    spot: Option<Spot>,
    load: Load,
    read_at: Option<Instant>,
    tx: Sender<Read>,
    rx: Receiver<Read>,
}

#[derive(Default)]
enum Load {
    #[default]
    Idle,
    Loading,
    Ready(Vec<Issue>),
    Failed(String),
}

impl Default for HomeIssues {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            spot: None,
            load: Load::Idle,
            read_at: None,
            tx,
            rx,
        }
    }
}

impl HomeIssues {
    /// What the Issue picker lists. Index 0 is "no issue" whenever there are
    /// issues to pick, so the picker's cursor maps straight onto the list.
    fn options(&self, targeted: bool) -> Vec<String> {
        if !targeted {
            return vec!["Pick a workspace to attach its issues".to_string()];
        }
        match &self.load {
            Load::Idle => vec!["No issue".to_string()],
            Load::Loading => vec!["Loading issues…".to_string()],
            Load::Failed(error) => vec![error.clone()],
            Load::Ready(issues) if issues.is_empty() => vec!["No open issues".to_string()],
            Load::Ready(issues) => std::iter::once("No issue".to_string())
                .chain(
                    issues
                        .iter()
                        .map(|issue| format!("#{} {}", issue.number, issue.title)),
                )
                .collect(),
        }
    }

    /// The issue the picker's `cursor` names, if it names one.
    fn picked(&self, cursor: usize) -> Option<&Issue> {
        match &self.load {
            Load::Ready(issues) => issues.get(cursor.checked_sub(1)?),
            _ => None,
        }
    }
}

impl PilusView {
    /// Fold finished reads in. Returns whether anything changed.
    pub(super) fn home_issues_tick(&mut self, _cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(Read { spot, result }) = self.home_issues.rx.try_recv() {
            if self.home_issues.spot.as_ref() != Some(&spot) {
                continue;
            }
            self.home_issues.load = match result {
                Ok(issues) => Load::Ready(issues),
                Err(error) => Load::Failed(error),
            };
            self.home_issues.read_at = Some(Instant::now());
            changed = true;
        }
        changed
    }

    /// How to run `gh` for `spot`: here, or through the machine's SSH profile.
    fn github_repo(&self, spot: &Spot) -> Result<GhRepo, String> {
        let target = match &spot.0 {
            EndpointId::Local => None,
            EndpointId::Machine(id) => Some(
                self.machines
                    .iter()
                    .find(|machine| &machine.id == id)
                    .and_then(|machine| machine.ssh_target())
                    .ok_or("GitHub needs this machine's SSH profile to reach it")?,
            ),
        };
        Ok(GhRepo::new(target, spot.1.clone()))
    }

    /// Point the Issue picker at the workspace the composer targets and list
    /// what it holds. Cheap enough to run every frame the composer is open.
    pub(super) fn home_issues_sync(&mut self) {
        let spot = self
            .home_workspace()
            .map(|(endpoint, workspace)| (endpoint, workspace.cwd.trim().to_string()))
            .filter(|(_, cwd)| !cwd.is_empty());
        if self.home_issues.spot != spot {
            // A fresh channel too: a read still in flight for the old
            // workspace must not land on the new one.
            self.home_issues = HomeIssues {
                spot,
                ..HomeIssues::default()
            };
            self.home.choice_mut(HomeField::Issue).choose(0);
        }
        let options = self.home_issues.options(self.home_issues.spot.is_some());
        self.home.choice_mut(HomeField::Issue).set(options);
    }

    /// Read the target workspace's open issues, unless a recent read is held.
    pub(super) fn home_issues_load(&mut self) {
        let Some(spot) = self.home_issues.spot.clone() else {
            return;
        };
        let fresh = self
            .home_issues
            .read_at
            .is_some_and(|at| at.elapsed() < TTL);
        if fresh || matches!(self.home_issues.load, Load::Loading) {
            return;
        }
        let repo = match self.github_repo(&spot) {
            Ok(repo) => repo,
            Err(error) => {
                self.home_issues.load = Load::Failed(error);
                return;
            }
        };
        // Keep the list on screen while a re-read runs; only a first read
        // shows Loading.
        if !matches!(self.home_issues.load, Load::Ready(_)) {
            self.home_issues.load = Load::Loading;
        }
        let tx = self.home_issues.tx.clone();
        std::thread::spawn(move || {
            let result = repo.issues(StateFilter::Open, LIST_LIMIT);
            tx.send(Read { spot, result }).ok();
        });
    }

    /// The issue the composer will send with the prompt.
    pub(super) fn home_attached_issue(&self) -> Option<Issue> {
        let cursor = self.home.choice(HomeField::Issue).cursor();
        self.home_issues.picked(cursor).cloned()
    }

    /// What the Issue chip says: the attached issue, or the bare field name.
    pub(super) fn home_issue_chip_label(&self) -> String {
        match self.home_attached_issue() {
            None => "Issue".to_string(),
            Some(issue) => {
                let title: String = issue.title.chars().take(CHIP_TITLE).collect();
                let ellipsis = if issue.title.chars().count() > CHIP_TITLE {
                    "…"
                } else {
                    ""
                };
                format!("#{} {title}{ellipsis}", issue.number)
            }
        }
    }

    /// Drop the attachment, as after a launch that used it.
    pub(super) fn home_issue_clear(&mut self) {
        self.home.choice_mut(HomeField::Issue).choose(0);
    }
}
