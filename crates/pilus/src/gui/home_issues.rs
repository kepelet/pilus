//! The Home composer's Issue picker: the open GitHub issues of the workspace
//! the composer targets, offered as something to attach to the prompt.
//!
//! The daemon has no UHP call for GitHub, so the list comes from the GitHub
//! CLI (`pilus_client::GhRepo`): the workspace's `origin` is read in its
//! directory, on this Mac or over the machine's saved SSH profile, and the
//! issues are listed with this Mac's own `gh` login. The read runs off the GUI thread and
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
    /// When the last *good* read landed. A failure leaves it alone, so the
    /// next opening of the picker tries again at once.
    read_at: Option<Instant>,
    /// The issue the person committed, by number: the picker's highlight is a
    /// row index, which moves when the list is re-read.
    attached: Option<u64>,
    /// The picker is open and has not been committed or cancelled yet.
    editing: bool,
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
            attached: None,
            editing: false,
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

    /// The picker row of issue `number`, in the list `options` builds.
    fn row_of(&self, number: u64) -> Option<usize> {
        match &self.load {
            Load::Ready(issues) => issues
                .iter()
                .position(|issue| issue.number == number)
                .map(|index| index + 1),
            _ => None,
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
            match result {
                Ok(issues) => {
                    self.home_issues.load = Load::Ready(issues);
                    self.home_issues.read_at = Some(Instant::now());
                }
                // A re-read that fails keeps the list it had: the person's
                // choice should not vanish with a dropped connection.
                Err(error) => {
                    if !matches!(self.home_issues.load, Load::Ready(_)) {
                        self.home_issues.load = Load::Failed(error);
                    }
                }
            }
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
        // A workspace is chosen but its folder is momentarily unknown, as when
        // the catalog is being refreshed: leave what was read alone rather than
        // throwing it, and the person's choice, away.
        if spot.is_none() && self.home.workspace_index().is_some() {
            return;
        }
        if self.home_issues.spot != spot {
            // A fresh channel too: a read still in flight for the old
            // workspace must not land on the new one.
            self.home_issues = HomeIssues {
                spot,
                ..HomeIssues::default()
            };
            self.home.choice_mut(HomeField::Issue).choose(0);
        }
        let open = self.home.dropdown() == Some(HomeField::Issue);
        if open {
            self.home_issues.editing = true;
        } else if self.home_issues.editing {
            // The picker closed without being cancelled: Enter, a click or Tab
            // took what the highlight was on.
            self.home_issues.editing = false;
            let cursor = self.home.choice(HomeField::Issue).cursor();
            self.home_issues.attached = self.home_issues.picked(cursor).map(|issue| issue.number);
        }
        let options = self.home_issues.options(self.home_issues.spot.is_some());
        self.home.choice_mut(HomeField::Issue).set(options);
        if !open {
            self.home_issues_point_at_attached();
        }
    }

    /// Put the picker's highlight on the committed issue, wherever the list
    /// now has it. One that is gone from a good list is dropped; a list that
    /// is loading or failed says nothing about it, so it is kept.
    fn home_issues_point_at_attached(&mut self) {
        let Some(number) = self.home_issues.attached else {
            self.home.choice_mut(HomeField::Issue).choose(0);
            return;
        };
        match self.home_issues.row_of(number) {
            Some(row) => self.home.choice_mut(HomeField::Issue).choose(row),
            None if matches!(self.home_issues.load, Load::Ready(_)) => {
                self.home_issues.attached = None;
                self.home.choice_mut(HomeField::Issue).choose(0);
            }
            None => {}
        }
    }

    /// Escape or a press outside the picker: the highlight goes back to the
    /// issue that was attached when it opened.
    pub(super) fn home_issues_cancel(&mut self) {
        if self.home_issues.editing {
            self.home_issues.editing = false;
            self.home_issues_point_at_attached();
        }
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
            // The checkout only says which repository it is. Listing goes
            // through this Mac's own `gh` login, so a workspace on a machine
            // where `gh` is not signed in reads like any other.
            let result = repo
                .origin_slug()
                .and_then(|slug| GhRepo::for_slug(slug).issues(StateFilter::Open, LIST_LIMIT));
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
        self.home_issues.attached = None;
        self.home.choice_mut(HomeField::Issue).choose(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            state: "OPEN".to_string(),
            author: String::new(),
            labels: Vec::new(),
            updated_at: String::new(),
            url: String::new(),
            body: String::new(),
        }
    }

    fn ready(numbers: &[u64]) -> HomeIssues {
        HomeIssues {
            load: Load::Ready(numbers.iter().copied().map(issue).collect()),
            ..HomeIssues::default()
        }
    }

    #[test]
    fn an_issue_keeps_its_row_number_not_its_place_when_the_list_changes() {
        let mut issues = ready(&[7, 5, 3]);
        assert_eq!(issues.row_of(5), Some(2));
        // A newer issue lands above it: the same issue, one row lower.
        issues.load = Load::Ready([9, 7, 5, 3].into_iter().map(issue).collect());
        assert_eq!(issues.row_of(5), Some(3));
        assert_eq!(issues.row_of(4), None);
    }

    #[test]
    fn row_zero_is_no_issue_and_a_list_that_is_not_ready_has_no_rows() {
        let issues = ready(&[7]);
        assert!(issues.picked(0).is_none());
        assert_eq!(issues.picked(1).map(|issue| issue.number), Some(7));
        assert_eq!(HomeIssues::default().row_of(7), None);
    }
}
