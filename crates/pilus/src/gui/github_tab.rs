//! The native GitHub tab: the active workspace's issues and pull requests,
//! with comments and a way to submit the current branch.
//!
//! The daemon has a GitHub screen of its own but no UHP call for it, so the
//! tab asks the GitHub CLI itself (`pilus_client::GhRepo`) in the workspace's
//! directory, on this Mac or over the machine's saved SSH profile. Every call
//! runs off the GUI thread and reports through this panel's channel. A call
//! carries the workspace it was made for, so an answer that arrives after the
//! user moved on is dropped instead of painted over the new workspace.
//!
//! The tab polls slowly (it is network-bound and a list changes on human
//! time) and re-reads right away after a write. What `gh` needs is told to the
//! user as a sentence: not installed, not signed in, or no GitHub remote.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, MouseButton, SharedString, Stateful, div,
    prelude::*, px,
};
use pilus_client::{
    CreatedPull, GhRepo, GhState, Issue, IssueDetail, NewPull, Pull, PullDetail, PullDraft,
    StateFilter, unix_seconds,
};

use super::home::HomeField;
use super::markdown;
use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::tasks::{age_label, unix_now};
use super::text_input::TextInput;
use super::toast::ToastKind;
use super::{EndpointId, PilusView, icon};

/// How often the open tab re-reads its list. GitHub is a network call away
/// and the lists move slowly; the refresh button asks sooner.
const POLL: Duration = Duration::from_secs(45);
/// Items a list asks for.
const LIST_LIMIT: u32 = 50;

/// The workspace a read was made for: where it lives and which directory.
pub(super) type Spot = (EndpointId, String);

/// What a background call reports back to the tab.
pub(super) enum GithubMsg {
    Snapshot {
        spot: Spot,
        /// `None` when the call did not probe again.
        state: Option<GhState>,
        section: Section,
        filter: StateFilter,
        listing: Option<Listing>,
    },
    Issue {
        spot: Spot,
        number: u64,
        result: Result<IssueDetail, String>,
    },
    Pull {
        spot: Spot,
        number: u64,
        result: Result<PullDetail, String>,
    },
    Draft {
        spot: Spot,
        result: Result<PullDraft, String>,
    },
    Commented {
        spot: Spot,
        section: Section,
        number: u64,
        result: Result<String, String>,
    },
    Created {
        spot: Spot,
        result: Result<CreatedPull, String>,
    },
    /// The open issues the Home composer offers to attach.
    HomeIssues {
        spot: Spot,
        result: Result<Vec<Issue>, String>,
    },
}

/// One list read.
pub(super) enum Listing {
    Issues(Result<Vec<Issue>, String>),
    Pulls(Result<Vec<Pull>, String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Section {
    Issues,
    Pulls,
}

/// Which screen of the tab is showing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    List,
    Issue(u64),
    Pull(u64),
    NewPull,
}

/// A read that is in flight, failed, or done.
enum Load<T> {
    Loading,
    Failed(String),
    Ready(T),
}

impl<T> Load<T> {
    fn from(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ready(value),
            Err(error) => Self::Failed(error),
        }
    }
}

/// Which text field holds the keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GithubFocus {
    Comment,
    Title,
    Body,
}

/// The tab's text fields, one entity each so every caret and selection is its
/// own.
pub(super) struct GithubInputs {
    comment: Entity<TextInput>,
    title: Entity<TextInput>,
    body: Entity<TextInput>,
}

impl GithubInputs {
    pub(super) fn new(
        font: gpui::Font,
        size: f32,
        theme: super::theme::Theme,
        cx: &mut Context<PilusView>,
    ) -> Self {
        let mut field = |placeholder: &'static str, multiline: bool, submits: bool| {
            let font = font.clone();
            cx.new(move |cx| {
                let input = TextInput::new(
                    String::new(),
                    placeholder,
                    font,
                    px(size),
                    multiline,
                    theme,
                    cx,
                );
                if submits {
                    input.submit_on_enter()
                } else {
                    input
                }
            })
        };
        Self {
            comment: field("Leave a comment…", true, true),
            title: field("Title", false, true),
            body: field("Describe the change (optional)", true, false),
        }
    }

    fn entity(&self, focus: GithubFocus) -> &Entity<TextInput> {
        match focus {
            GithubFocus::Comment => &self.comment,
            GithubFocus::Title => &self.title,
            GithubFocus::Body => &self.body,
        }
    }

    pub(super) fn focus_handle(&self, focus: GithubFocus, cx: &gpui::App) -> gpui::FocusHandle {
        gpui::Focusable::focus_handle(self.entity(focus), cx)
    }
}

/// The issues the Home composer can attach, for the workspace it targets.
///
/// Read when the Issue picker opens, not before: most launches attach nothing,
/// and each read is a GitHub round trip.
#[derive(Default)]
pub(super) struct HomeIssues {
    spot: Option<Spot>,
    load: HomeLoad,
    read_at: Option<Instant>,
}

#[derive(Default)]
enum HomeLoad {
    #[default]
    Idle,
    Loading,
    Ready(Vec<Issue>),
    Failed(String),
}

/// How long a read of the open issues is trusted while the picker reopens.
const HOME_ISSUES_TTL: Duration = Duration::from_secs(60);
/// Longest an attached issue's title runs on the picker chip.
const CHIP_TITLE: usize = 26;

impl HomeIssues {
    /// What the Issue picker lists. Index 0 is "no issue" whenever there are
    /// issues to pick, so the picker's cursor maps straight onto the list.
    fn options(&self, targeted: bool) -> Vec<String> {
        if !targeted {
            return vec!["Pick a workspace to attach its issues".to_string()];
        }
        match &self.load {
            HomeLoad::Idle => vec!["No issue".to_string()],
            HomeLoad::Loading => vec!["Loading issues…".to_string()],
            HomeLoad::Failed(error) => vec![error.clone()],
            HomeLoad::Ready(issues) if issues.is_empty() => vec!["No open issues".to_string()],
            HomeLoad::Ready(issues) => std::iter::once("No issue".to_string())
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
            HomeLoad::Ready(issues) => issues.get(cursor.checked_sub(1)?),
            _ => None,
        }
    }
}

/// The tab's own state.
pub(super) struct GithubPanel {
    spot: Option<Spot>,
    /// `None` until the first probe of this workspace lands.
    state: Option<GhState>,
    section: Section,
    issue_filter: StateFilter,
    pull_filter: StateFilter,
    issues: Option<Result<Vec<Issue>, String>>,
    pulls: Option<Result<Vec<Pull>, String>>,
    page: Page,
    issue_detail: Option<(u64, Load<IssueDetail>)>,
    pull_detail: Option<(u64, Load<PullDetail>)>,
    draft: Option<Load<PullDraft>>,
    /// The new pull request opens as a draft.
    as_draft: bool,
    focus: Option<GithubFocus>,
    /// A comment is being posted.
    commenting: bool,
    /// A pull request is being created.
    creating: bool,
    polling: bool,
    last_poll: Option<Instant>,
    /// The next read probes `gh` again (a refresh the user asked for).
    reprobe: bool,
    /// Shots hold the panel on sample data instead of polling.
    #[cfg(feature = "visual-test")]
    fixture: bool,
    tx: Sender<GithubMsg>,
    rx: Receiver<GithubMsg>,
}

impl GithubPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            spot: None,
            state: None,
            section: Section::Issues,
            issue_filter: StateFilter::Open,
            pull_filter: StateFilter::Open,
            issues: None,
            pulls: None,
            page: Page::List,
            issue_detail: None,
            pull_detail: None,
            draft: None,
            as_draft: false,
            focus: None,
            commenting: false,
            creating: false,
            polling: false,
            last_poll: None,
            reprobe: false,
            #[cfg(feature = "visual-test")]
            fixture: false,
            tx,
            rx,
        }
    }

    /// Ask for a fresh read the next time the tab polls.
    pub(super) fn mark_stale(&mut self) {
        self.last_poll = None;
    }

    /// Whether a text field holds the keyboard.
    pub(super) fn composing(&self) -> bool {
        self.focus.is_some()
    }

    pub(super) fn focus(&self) -> Option<GithubFocus> {
        self.focus
    }

    /// Forget what was read for the previous workspace.
    fn reset(&mut self) {
        self.state = None;
        self.issues = None;
        self.pulls = None;
        self.page = Page::List;
        self.issue_detail = None;
        self.pull_detail = None;
        self.draft = None;
        self.focus = None;
        self.polling = false;
        self.last_poll = None;
        self.reprobe = false;
    }

    fn filter(&self) -> StateFilter {
        match self.section {
            Section::Issues => self.issue_filter,
            Section::Pulls => self.pull_filter,
        }
    }
}

impl PilusView {
    /// Fold finished calls in and poll the open tab. Returns whether anything
    /// changed.
    pub(super) fn github_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(message) = self.github_panel.rx.try_recv() {
            changed |= self.github_apply(message, cx);
        }
        #[cfg(feature = "visual-test")]
        if self.github_panel.fixture {
            return changed;
        }
        if !self.github_visible() {
            return changed;
        }
        let spot = self.github_spot();
        if self.github_panel.spot != spot {
            self.github_panel.reset();
            self.github_panel.spot = spot;
            changed = true;
        }
        let panel = &self.github_panel;
        if panel.spot.is_none()
            || panel.polling
            || panel.last_poll.is_some_and(|at| at.elapsed() < POLL)
        {
            return changed;
        }
        self.github_fetch_list();
        changed
    }

    fn github_apply(&mut self, message: GithubMsg, cx: &mut Context<Self>) -> bool {
        let current = self.github_panel.spot.clone();
        let stale = |spot: &Spot| current.as_ref() != Some(spot);
        match message {
            GithubMsg::Snapshot {
                spot,
                state,
                section,
                filter,
                listing,
            } => {
                if stale(&spot) {
                    return false;
                }
                let panel = &mut self.github_panel;
                panel.polling = false;
                if let Some(state) = state {
                    panel.state = Some(state);
                }
                // A list read for a section or filter the user has since left
                // would flash the wrong rows.
                if panel.section == section && panel.filter() == filter {
                    match listing {
                        Some(Listing::Issues(result)) => panel.issues = Some(result),
                        Some(Listing::Pulls(result)) => panel.pulls = Some(result),
                        None => {}
                    }
                }
                true
            }
            GithubMsg::Issue {
                spot,
                number,
                result,
            } => {
                if stale(&spot) || self.github_panel.page != Page::Issue(number) {
                    return false;
                }
                self.github_panel.issue_detail = Some((number, Load::from(result)));
                true
            }
            GithubMsg::Pull {
                spot,
                number,
                result,
            } => {
                if stale(&spot) || self.github_panel.page != Page::Pull(number) {
                    return false;
                }
                self.github_panel.pull_detail = Some((number, Load::from(result)));
                true
            }
            GithubMsg::Draft { spot, result } => {
                if stale(&spot) || self.github_panel.page != Page::NewPull {
                    return false;
                }
                if let Ok(draft) = &result {
                    let title = draft.title.clone();
                    self.github_inputs
                        .title
                        .update(cx, |input, cx| input.set_value(title, cx));
                }
                self.github_panel.draft = Some(Load::from(result));
                true
            }
            GithubMsg::HomeIssues { spot, result } => {
                if self.home_issues.spot.as_ref() != Some(&spot) {
                    return false;
                }
                self.home_issues.load = match result {
                    Ok(issues) => HomeLoad::Ready(issues),
                    Err(error) => HomeLoad::Failed(error),
                };
                self.home_issues.read_at = Some(Instant::now());
                true
            }
            GithubMsg::Commented {
                spot,
                section,
                number,
                result,
            } => {
                if stale(&spot) {
                    return false;
                }
                self.github_panel.commenting = false;
                match result {
                    Ok(_) => {
                        self.github_inputs
                            .comment
                            .update(cx, |input, cx| input.clear(cx));
                        self.toast(ToastKind::Success, format!("Commented on #{number}"));
                        match section {
                            Section::Issues => self.github_load_issue(number),
                            Section::Pulls => self.github_load_pull(number),
                        }
                    }
                    Err(error) => self.toast(ToastKind::Error, error),
                }
                true
            }
            GithubMsg::Created { spot, result } => {
                if stale(&spot) {
                    return false;
                }
                self.github_panel.creating = false;
                match result {
                    Ok(created) => {
                        let label = created.number.map_or("Pull request".to_string(), |number| {
                            format!("Pull request #{number}")
                        });
                        self.toast(ToastKind::Success, format!("{label} opened"));
                        self.github_inputs
                            .title
                            .update(cx, |input, cx| input.clear(cx));
                        self.github_inputs
                            .body
                            .update(cx, |input, cx| input.clear(cx));
                        self.github_close_composer();
                        self.github_panel.section = Section::Pulls;
                        self.github_panel.draft = None;
                        match created.number {
                            Some(number) => self.github_open_pull(number),
                            None => self.github_panel.page = Page::List,
                        }
                        self.github_panel.pulls = None;
                        self.github_panel.mark_stale();
                    }
                    Err(error) => self.toast(ToastKind::Error, error),
                }
                true
            }
        }
    }

    /// The workspace the tab follows: the active one, in the active endpoint.
    fn github_spot(&self) -> Option<Spot> {
        let workspace = self
            .endpoint_workspaces(&self.active)
            .iter()
            .find(|workspace| workspace.active)?;
        let cwd = workspace.cwd.trim();
        (!cwd.is_empty()).then(|| (self.active.clone(), cwd.to_string()))
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

    /// Read the open list, probing `gh` first when it is not known to work.
    fn github_fetch_list(&mut self) {
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        let repo = match self.github_repo(&spot) {
            Ok(repo) => repo,
            Err(error) => {
                self.github_panel.state = Some(GhState::Failed(error));
                self.github_panel.last_poll = Some(Instant::now());
                return;
            }
        };
        let panel = &mut self.github_panel;
        panel.polling = true;
        panel.last_poll = Some(Instant::now());
        let probe = panel.reprobe || !matches!(panel.state, Some(GhState::Ready(_)));
        panel.reprobe = false;
        let (section, filter) = (panel.section, panel.filter());
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            let state = probe.then(|| repo.probe());
            let usable = match &state {
                Some(state) => matches!(state, GhState::Ready(_)),
                None => true,
            };
            let listing = usable.then(|| match section {
                Section::Issues => Listing::Issues(repo.issues(filter, LIST_LIMIT)),
                Section::Pulls => Listing::Pulls(repo.pulls(filter, LIST_LIMIT)),
            });
            tx.send(GithubMsg::Snapshot {
                spot,
                state,
                section,
                filter,
                listing,
            })
            .ok();
        });
    }

    /// Re-read now, including whether `gh` works.
    fn github_refresh(&mut self, cx: &mut Context<Self>) {
        self.github_panel.reprobe = true;
        self.github_panel.mark_stale();
        match self.github_panel.page {
            Page::Issue(number) => self.github_load_issue(number),
            Page::Pull(number) => self.github_load_pull(number),
            Page::NewPull => self.github_load_draft(),
            Page::List => {}
        }
        cx.notify();
    }

    fn github_set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.github_panel.section != section {
            self.github_panel.section = section;
            self.github_panel.mark_stale();
        }
        cx.notify();
    }

    fn github_set_filter(&mut self, filter: StateFilter, cx: &mut Context<Self>) {
        let panel = &mut self.github_panel;
        let slot = match panel.section {
            Section::Issues => &mut panel.issue_filter,
            Section::Pulls => &mut panel.pull_filter,
        };
        if *slot != filter {
            *slot = filter;
            match panel.section {
                Section::Issues => panel.issues = None,
                Section::Pulls => panel.pulls = None,
            }
            panel.mark_stale();
        }
        cx.notify();
    }

    fn github_open_issue(&mut self, number: u64) {
        self.github_panel.page = Page::Issue(number);
        self.github_load_issue(number);
    }

    fn github_open_pull(&mut self, number: u64) {
        self.github_panel.page = Page::Pull(number);
        self.github_load_pull(number);
    }

    fn github_load_issue(&mut self, number: u64) {
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        // A re-read keeps what is on screen; only a first read shows Loading.
        let keep = matches!(&self.github_panel.issue_detail, Some((open, Load::Ready(_))) if *open == number);
        if !keep {
            self.github_panel.issue_detail = Some((number, Load::Loading));
        }
        let repo = self.github_repo(&spot);
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = repo.and_then(|repo| repo.issue(number));
            tx.send(GithubMsg::Issue {
                spot,
                number,
                result,
            })
            .ok();
        });
    }

    fn github_load_pull(&mut self, number: u64) {
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        let keep = matches!(&self.github_panel.pull_detail, Some((open, Load::Ready(_))) if *open == number);
        if !keep {
            self.github_panel.pull_detail = Some((number, Load::Loading));
        }
        let repo = self.github_repo(&spot);
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = repo.and_then(|repo| repo.pull(number));
            tx.send(GithubMsg::Pull {
                spot,
                number,
                result,
            })
            .ok();
        });
    }

    fn github_load_draft(&mut self) {
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        self.github_panel.draft = Some(Load::Loading);
        let repo = self.github_repo(&spot);
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = repo.and_then(|repo| repo.pull_draft());
            tx.send(GithubMsg::Draft { spot, result }).ok();
        });
    }

    fn github_back(&mut self, cx: &mut Context<Self>) {
        self.github_close_composer();
        self.github_panel.page = Page::List;
        cx.notify();
    }

    fn github_open_new_pull(&mut self, cx: &mut Context<Self>) {
        self.github_panel.page = Page::NewPull;
        self.github_panel.as_draft = false;
        self.github_load_draft();
        self.github_set_focus(Some(GithubFocus::Title));
        cx.notify();
    }

    /// Give a field the keyboard, or take it back for the terminal.
    fn github_set_focus(&mut self, focus: Option<GithubFocus>) {
        self.github_panel.focus = focus;
        self.right.focused = focus.is_some();
    }

    fn github_close_composer(&mut self) {
        self.github_set_focus(None);
    }

    /// Post the comment field to the open issue or pull request.
    fn github_post_comment(&mut self, cx: &mut Context<Self>) {
        if self.github_panel.commenting {
            return;
        }
        let (section, number) = match self.github_panel.page {
            Page::Issue(number) => (Section::Issues, number),
            Page::Pull(number) => (Section::Pulls, number),
            _ => return,
        };
        let body = self.github_inputs.comment.read(cx).value();
        if body.trim().is_empty() {
            return;
        }
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        let repo = match self.github_repo(&spot) {
            Ok(repo) => repo,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                return;
            }
        };
        self.github_panel.commenting = true;
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = match section {
                Section::Issues => repo.comment_issue(number, &body),
                Section::Pulls => repo.comment_pull(number, &body),
            };
            tx.send(GithubMsg::Commented {
                spot,
                section,
                number,
                result,
            })
            .ok();
        });
        cx.notify();
    }

    /// Submit the form as a pull request from the checkout's branch.
    fn github_create_pull(&mut self, cx: &mut Context<Self>) {
        if self.github_panel.creating {
            return;
        }
        let Some(Load::Ready(draft)) = &self.github_panel.draft else {
            return;
        };
        if draft.existing.is_some() {
            return;
        }
        let title = self.github_inputs.title.read(cx).value();
        if title.trim().is_empty() {
            self.toast(ToastKind::Error, "A pull request needs a title");
            return;
        }
        let new = NewPull {
            title,
            body: self.github_inputs.body.read(cx).value(),
            base: draft.base.clone(),
            draft: self.github_panel.as_draft,
        };
        let Some(spot) = self.github_panel.spot.clone() else {
            return;
        };
        let repo = match self.github_repo(&spot) {
            Ok(repo) => repo,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                return;
            }
        };
        self.github_panel.creating = true;
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = repo.create_pull(&new);
            tx.send(GithubMsg::Created { spot, result }).ok();
        });
        cx.notify();
    }

    /// Keys while a field is open: Escape closes it, Enter sends a comment or
    /// moves from the title to the body, and Cmd+Enter submits the form.
    pub(super) fn github_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        match (self.github_panel.focus, key) {
            (None, _) => {}
            (Some(_), "escape") => self.github_close_composer(),
            (Some(GithubFocus::Comment), "enter") => self.github_post_comment(cx),
            (Some(GithubFocus::Title), "enter") => {
                self.github_set_focus(Some(GithubFocus::Body));
            }
            (Some(GithubFocus::Title), "tab") => self.github_set_focus(Some(GithubFocus::Body)),
            (Some(GithubFocus::Body), "tab") => self.github_set_focus(Some(GithubFocus::Title)),
            (Some(GithubFocus::Body), "enter") if mods.platform => self.github_create_pull(cx),
            _ => {}
        }
        cx.notify();
    }

    /// The open page's URL, for the browser button.
    fn github_page_url(&self) -> Option<String> {
        match self.github_panel.page {
            Page::Issue(_) => match &self.github_panel.issue_detail {
                Some((_, Load::Ready(detail))) => Some(detail.issue.url.clone()),
                _ => None,
            },
            Page::Pull(_) => match &self.github_panel.pull_detail {
                Some((_, Load::Ready(detail))) => Some(detail.pull.url.clone()),
                _ => None,
            },
            _ => self
                .github_panel
                .state
                .as_ref()
                .and_then(GhState::repo)
                .map(|repo| repo.url.clone()),
        }
    }
}

// Rendering.
impl PilusView {
    pub(super) fn github_page(&mut self, cx: &mut Context<Self>) -> Div {
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.github_header(cx));
        if self.github_panel.spot.is_none() {
            return page.child(self.github_message("Open a workspace to see its GitHub issues."));
        }
        match &self.github_panel.state {
            None => return page.child(self.github_message("Checking GitHub…")),
            Some(state) => {
                if let Some(advice) = state.advice() {
                    return page.child(self.github_message(advice));
                }
            }
        }
        match self.github_panel.page {
            Page::List => page.child(self.github_list(cx)),
            Page::Issue(_) => page.child(self.github_issue_page(cx)),
            Page::Pull(_) => page.child(self.github_pull_page(cx)),
            Page::NewPull => page.child(self.github_new_pull_page(cx)),
        }
    }

    /// A centered sentence that wraps inside the column. `diff_message` does
    /// not, and the setup advice is long.
    fn github_message(&self, text: impl Into<SharedString>) -> Div {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .px_6()
            .min_w(px(0.0))
            .child(
                div()
                    .max_w_full()
                    .text_center()
                    .text_size(px(style::Fonts::resolve(&self.shared_settings).body))
                    .text_color(self.theme.muted)
                    .child(text.into()),
            )
    }

    /// One row: the repository, then the browser and refresh buttons.
    fn github_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let in_detail = self.github_panel.page != Page::List;
        let name = self
            .github_panel
            .state
            .as_ref()
            .and_then(GhState::repo)
            .map_or("GitHub".to_string(), |repo| repo.name.clone());
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border);
        if in_detail {
            row = row.child(
                self.diff_icon_button(
                    "pilus-github-back",
                    include_bytes!("../../assets/icons/chevron-left.svg"),
                    "Back to the list".to_string(),
                    false,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.github_back(cx))),
            );
        }
        row = row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(name),
        );
        if let Some(url) = self.github_page_url() {
            row = row.child(
                self.diff_icon_button(
                    "pilus-github-open",
                    include_bytes!("../../assets/icons/external-link.svg"),
                    "Open on GitHub".to_string(),
                    false,
                    false,
                )
                .on_click(move |_, _, cx| cx.open_url(&url)),
            );
        }
        row.child(
            self.diff_icon_button(
                "pilus-github-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.github_refresh(cx))),
        )
    }

    /// The section switch and state filter above a list.
    fn github_list_controls(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let section = self.github_panel.section;
        let filter = self.github_panel.filter();
        let segment = |id: &'static str, label: &'static str, on: bool| {
            style::small_button(
                id,
                label,
                if on {
                    ButtonKind::Secondary
                } else {
                    ButtonKind::Ghost
                },
                theme,
                fonts.secondary,
            )
        };
        let filters: &[(&'static str, &'static str, StateFilter)] = match section {
            Section::Issues => &[
                ("pilus-github-open-filter", "Open", StateFilter::Open),
                ("pilus-github-closed-filter", "Closed", StateFilter::Closed),
            ],
            Section::Pulls => &[
                ("pilus-github-open-filter", "Open", StateFilter::Open),
                ("pilus-github-merged-filter", "Merged", StateFilter::Merged),
                ("pilus-github-closed-filter", "Closed", StateFilter::Closed),
            ],
        };
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .child(
                segment("pilus-github-issues", "Issues", section == Section::Issues).on_click(
                    cx.listener(|this, _, _, cx| this.github_set_section(Section::Issues, cx)),
                ),
            )
            .child(
                segment(
                    "pilus-github-pulls",
                    "Pull requests",
                    section == Section::Pulls,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.github_set_section(Section::Pulls, cx)),
                ),
            )
            .child(div().flex_1());
        for &(id, label, value) in filters {
            row = row
                .child(segment(id, label, filter == value).on_click(
                    cx.listener(move |this, _, _, cx| this.github_set_filter(value, cx)),
                ));
        }
        row
    }

    fn github_list(&self, cx: &mut Context<Self>) -> Div {
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(self.github_list_controls(cx));
        let body = match self.github_panel.section {
            Section::Issues => match &self.github_panel.issues {
                None => return column.child(self.github_message("Loading issues…")),
                Some(Err(error)) => return column.child(self.github_message(error.clone())),
                Some(Ok(issues)) if issues.is_empty() => {
                    let enabled = self
                        .github_panel
                        .state
                        .as_ref()
                        .and_then(GhState::repo)
                        .is_none_or(|repo| repo.issues_enabled);
                    return column.child(self.github_message(if enabled {
                        "No issues here."
                    } else {
                        "Issues are turned off for this repository."
                    }));
                }
                Some(Ok(issues)) => {
                    let mut rows = div().flex().flex_col().p_2();
                    for issue in issues {
                        rows = rows.child(self.github_issue_row(issue, cx));
                    }
                    rows
                }
            },
            Section::Pulls => match &self.github_panel.pulls {
                None => return column.child(self.github_message("Loading pull requests…")),
                Some(Err(error)) => return column.child(self.github_message(error.clone())),
                Some(Ok(pulls)) if pulls.is_empty() => {
                    column = column.child(self.github_message("No pull requests here."));
                    return column.child(self.github_new_pull_bar(cx));
                }
                Some(Ok(pulls)) => {
                    let mut rows = div().flex().flex_col().p_2();
                    for pull in pulls {
                        rows = rows.child(self.github_pull_row(pull, cx));
                    }
                    rows
                }
            },
        };
        column = column.child(
            div()
                .id("pilus-github-list")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .child(body),
        );
        if self.github_panel.section == Section::Pulls {
            column = column.child(self.github_new_pull_bar(cx));
        }
        column
    }

    /// The bar under the pull request list that starts a new one.
    fn github_new_pull_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .flex_shrink_0()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                style::small_button(
                    "pilus-github-new-pull",
                    "Submit this branch…",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.github_open_new_pull(cx))),
            )
    }

    fn github_issue_row(&self, issue: &Issue, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let open = issue.state == "OPEN";
        let glyph = if open {
            icon(
                include_bytes!("../../assets/icons/circle-dot.svg"),
                14.0,
                theme.ok,
            )
        } else {
            icon(
                include_bytes!("../../assets/icons/circle-check.svg"),
                14.0,
                theme.branch,
            )
        };
        let byline = format!(
            "#{} · {}{}",
            issue.number,
            issue.author,
            age_suffix(&issue.updated_at)
        );
        let number = issue.number;
        self.github_row(
            format!("pilus-github-issue-{number}"),
            glyph.into_any_element(),
            &issue.title,
            byline,
            &issue.labels,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.github_open_issue(number);
            cx.notify();
        }))
    }

    fn github_pull_row(&self, pull: &Pull, cx: &mut Context<Self>) -> Stateful<Div> {
        let glyph = self.github_pull_glyph(pull, 14.0);
        let mut byline = format!(
            "#{} · {} · {} → {}{}",
            pull.number,
            pull.author,
            pull.head,
            pull.base,
            age_suffix(&pull.updated_at)
        );
        if let Some(review) = review_label(&pull.review) {
            byline.push_str(" · ");
            byline.push_str(review);
        }
        let number = pull.number;
        self.github_row(
            format!("pilus-github-pull-{number}"),
            glyph,
            &pull.title,
            byline,
            &pull.labels,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.github_open_pull(number);
            cx.notify();
        }))
    }

    /// The state glyph of a pull request, tinted by where it stands.
    fn github_pull_glyph(&self, pull: &Pull, size: f32) -> AnyElement {
        let theme = self.theme;
        let (bytes, color): (&'static [u8], _) = match (pull.state.as_str(), pull.draft) {
            ("MERGED", _) => (
                include_bytes!("../../assets/icons/git-merge.svg"),
                theme.branch,
            ),
            ("CLOSED", _) => (
                include_bytes!("../../assets/icons/git-pull-request-closed.svg"),
                theme.alert,
            ),
            (_, true) => (
                include_bytes!("../../assets/icons/git-pull-request-draft.svg"),
                theme.muted,
            ),
            _ => (
                include_bytes!("../../assets/icons/git-pull-request.svg"),
                theme.ok,
            ),
        };
        icon(bytes, size, color).into_any_element()
    }

    /// A list row: glyph, a title that may wrap to two lines, and a byline with
    /// up to two label chips.
    fn github_row(
        &self,
        id: String,
        glyph: AnyElement,
        title: &str,
        byline: String,
        labels: &[pilus_client::Label],
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut meta = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w(px(0.0))
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(byline),
            );
        for label in labels.iter().take(2) {
            meta = meta.child(style::chip(theme, label.name.clone(), fonts.label));
        }
        div()
            .id(SharedString::from(id))
            .flex()
            .flex_row()
            .items_start()
            .gap_2()
            .px_2()
            .py_2()
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel, style::RADIUS_MD)
            .child(div().pt(px(2.0)).flex_shrink_0().child(glyph))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(fonts.body))
                            .text_color(theme.panel_fg)
                            .font_weight(FontWeight::MEDIUM)
                            .child(title.to_string()),
                    )
                    .child(meta),
            )
    }

    fn github_issue_page(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let detail = match &self.github_panel.issue_detail {
            Some((_, Load::Ready(detail))) => detail,
            Some((_, Load::Failed(error))) => return self.github_message(error.clone()),
            _ => return self.github_message("Loading issue…"),
        };
        let issue = &detail.issue;
        let open = issue.state == "OPEN";
        let glyph = if open {
            icon(
                include_bytes!("../../assets/icons/circle-dot.svg"),
                fonts.body,
                theme.ok,
            )
        } else {
            icon(
                include_bytes!("../../assets/icons/circle-check.svg"),
                fonts.body,
                theme.branch,
            )
        };
        let mut meta = format!(
            "#{} · opened by {}{}",
            issue.number,
            issue.author,
            age_suffix(&detail.created_at)
        );
        if !detail.assignees.is_empty() {
            meta.push_str(&format!(" · assigned to {}", detail.assignees.join(", ")));
        }
        let head = self.github_detail_head(
            glyph.into_any_element(),
            &issue.title,
            meta,
            &issue.labels,
            None,
        );
        self.github_thread(head, &issue.body, &detail.comments, cx)
    }

    fn github_pull_page(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let detail = match &self.github_panel.pull_detail {
            Some((_, Load::Ready(detail))) => detail,
            Some((_, Load::Failed(error))) => return self.github_message(error.clone()),
            _ => return self.github_message("Loading pull request…"),
        };
        let pull = &detail.pull;
        let mut meta = format!(
            "#{} · {} wants to merge {} into {}",
            pull.number, pull.author, pull.head, pull.base
        );
        if let Some(review) = review_label(&pull.review) {
            meta.push_str(" · ");
            meta.push_str(review);
        }
        let mut facts = format!(
            "+{} −{} in {} file{}",
            detail.additions,
            detail.deletions,
            detail.changed_files,
            if detail.changed_files == 1 { "" } else { "s" }
        );
        let checks = detail.checks;
        if checks.passed + checks.failed + checks.pending > 0 {
            facts.push_str(&format!(
                " · checks: {} passed, {} failed, {} pending",
                checks.passed, checks.failed, checks.pending
            ));
        }
        if detail.mergeable == "CONFLICTING" {
            facts.push_str(" · has conflicts");
        }
        let fact_color = if checks.failed > 0 || detail.mergeable == "CONFLICTING" {
            theme.alert
        } else {
            theme.muted
        };
        let head = self.github_detail_head(
            self.github_pull_glyph(pull, fonts.body),
            &pull.title,
            meta,
            &pull.labels,
            Some((facts, fact_color)),
        );
        self.github_thread(head, &detail.body, &detail.comments, cx)
    }

    /// The title block of an issue or pull request.
    fn github_detail_head(
        &self,
        glyph: AnyElement,
        title: &str,
        meta: String,
        labels: &[pilus_client::Label],
        fact: Option<(String, gpui::Rgba)>,
    ) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut chips = div().flex().flex_row().flex_wrap().gap_1();
        for label in labels {
            chips = chips.child(style::chip(theme, label.name.clone(), fonts.label));
        }
        let mut column = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .child(div().pt(px(2.0)).flex_shrink_0().child(glyph))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(fonts.body))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.panel_fg)
                            .child(title.to_string()),
                    ),
            )
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(meta),
            );
        if let Some((text, color)) = fact {
            column = column.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(text),
            );
        }
        if !labels.is_empty() {
            column = column.child(chips);
        }
        column
    }

    /// An issue or pull request's page: the title block, the description, the
    /// comments, and the comment field pinned beneath.
    fn github_thread(
        &self,
        head: Div,
        body: &str,
        comments: &[pilus_client::Comment],
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let markdown = |text: &str| {
            if text.trim().is_empty() {
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("No description provided.")
                    .into_any_element()
            } else {
                markdown::render(text, theme, fonts, &self.shared_settings)
            }
        };
        let mut thread = div().flex().flex_col().gap_3().p_3().child(head).child(
            div()
                .p_3()
                .radius_md()
                .border_1()
                .border_color(theme.border)
                .child(markdown(body)),
        );
        for comment in comments {
            thread = thread.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .radius_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(format!(
                                "{} commented{}",
                                comment.author,
                                age_suffix(&comment.created_at)
                            )),
                    )
                    .child(markdown(&comment.body)),
            );
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .id("pilus-github-thread")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(thread),
            )
            .child(self.github_comment_box(cx))
    }

    fn github_comment_box(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let focused = self.github_panel.focus == Some(GithubFocus::Comment);
        self.github_inputs.comment.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let busy = self.github_panel.commenting;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_shrink_0()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                style::field(
                    "pilus-github-comment-field",
                    "Comment",
                    focused,
                    theme,
                    style::scaled(&self.shared_settings, style::CONTROL_HEIGHT * 2.0),
                    fonts.label,
                    self.github_inputs.comment.clone(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.github_set_focus(Some(GithubFocus::Comment));
                        cx.notify();
                    }),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child("Enter to send · Shift+Enter for a new line"),
                    )
                    .child(
                        style::small_button(
                            "pilus-github-comment-send",
                            if busy { "Sending…" } else { "Comment" },
                            ButtonKind::Primary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.github_post_comment(cx))),
                    ),
            )
    }

    fn github_new_pull_page(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let draft = match &self.github_panel.draft {
            Some(Load::Ready(draft)) => draft,
            Some(Load::Failed(error)) => return self.github_message(error.clone()),
            _ => return self.github_message("Looking at this branch…"),
        };
        if let Some(existing) = &draft.existing {
            let url = existing.url.clone();
            return div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .child(
                    div()
                        .text_size(px(fonts.body))
                        .text_color(theme.panel_fg)
                        .child(format!(
                            "{} already has an open pull request: #{} {}",
                            draft.branch, existing.number, existing.title
                        )),
                )
                .child(
                    style::small_button(
                        "pilus-github-existing-open",
                        "Open it",
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click({
                        let number = existing.number;
                        cx.listener(move |this, _, _, cx| {
                            this.github_close_composer();
                            this.github_open_pull(number);
                            cx.notify();
                        })
                    }),
                )
                .child(
                    div()
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .child(url),
                );
        }
        for input in [&self.github_inputs.title, &self.github_inputs.body] {
            input.update(cx, |input, _| {
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(fonts.body),
                );
            });
        }
        let focus = self.github_panel.focus;
        let field_height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT);
        let mut summary = vec![format!("{} → {}", draft.branch, draft.base)];
        if !draft.commits.is_empty() {
            summary.push(format!(
                "{} commit{}",
                draft.commits.len(),
                if draft.commits.len() == 1 { "" } else { "s" }
            ));
        }
        let mut commits = div().flex().flex_col().gap_1();
        for subject in draft.commits.iter().take(6) {
            commits = commits.child(
                div()
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("· {subject}")),
            );
        }
        if draft.commits.len() > 6 {
            commits = commits.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("· and {} more", draft.commits.len() - 6)),
            );
        }
        let busy = self.github_panel.creating;
        let as_draft = self.github_panel.as_draft;
        let mut form = div()
            .id("pilus-github-new-pull-form")
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(fonts.body))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.panel_fg)
                    .child("Submit pull request"),
            )
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(summary.join(" · ")),
            )
            .child(commits)
            .child(
                style::field(
                    "pilus-github-title-field",
                    "Title",
                    focus == Some(GithubFocus::Title),
                    theme,
                    field_height,
                    fonts.label,
                    self.github_inputs.title.clone(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.github_set_focus(Some(GithubFocus::Title));
                        cx.notify();
                    }),
                ),
            )
            .child(
                style::field(
                    "pilus-github-body-field",
                    "Description",
                    focus == Some(GithubFocus::Body),
                    theme,
                    field_height * 3.0,
                    fonts.label,
                    self.github_inputs.body.clone(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.github_set_focus(Some(GithubFocus::Body));
                        cx.notify();
                    }),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        style::toggle("pilus-github-draft-toggle", as_draft, theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.github_panel.as_draft = !this.github_panel.as_draft;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.panel_fg)
                            .child("Open as a draft"),
                    ),
            );
        if draft.needs_push {
            form = form.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!(
                        "This pushes {} to its remote first. Nothing else is pushed.",
                        draft.branch
                    )),
            );
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(form)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .flex_shrink_0()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        style::small_button(
                            "pilus-github-new-cancel",
                            "Cancel",
                            ButtonKind::Ghost,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.github_back(cx))),
                    )
                    .child(
                        style::small_button(
                            "pilus-github-new-submit",
                            if busy {
                                "Submitting…"
                            } else if draft.needs_push {
                                "Push and submit"
                            } else {
                                "Submit"
                            },
                            ButtonKind::Primary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.github_create_pull(cx))),
                    ),
            )
    }
}

/// ` · 3d` after a byline, or nothing when the timestamp is unreadable.
fn age_suffix(timestamp: &str) -> String {
    unix_seconds(timestamp).map_or(String::new(), |then| {
        format!(" · {}", age_label(unix_now(), then))
    })
}

// The Home composer's Issue picker.
impl PilusView {
    /// Point the Issue picker at the workspace the composer targets and list
    /// what it holds. Cheap enough to run every frame the composer is open.
    pub(super) fn home_issues_sync(&mut self) {
        let spot = self
            .home_workspace()
            .map(|(endpoint, workspace)| (endpoint, workspace.cwd.trim().to_string()))
            .filter(|(_, cwd)| !cwd.is_empty());
        if self.home_issues.spot != spot {
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
            .is_some_and(|at| at.elapsed() < HOME_ISSUES_TTL);
        if fresh || matches!(self.home_issues.load, HomeLoad::Loading) {
            return;
        }
        let repo = match self.github_repo(&spot) {
            Ok(repo) => repo,
            Err(error) => {
                self.home_issues.load = HomeLoad::Failed(error);
                return;
            }
        };
        // Keep the list on screen while a re-read runs; only a first read
        // shows Loading.
        if !matches!(self.home_issues.load, HomeLoad::Ready(_)) {
            self.home_issues.load = HomeLoad::Loading;
        }
        let tx = self.github_panel.tx.clone();
        std::thread::spawn(move || {
            let result = repo.issues(StateFilter::Open, LIST_LIMIT);
            tx.send(GithubMsg::HomeIssues { spot, result }).ok();
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

/// A pull request's review decision in words, or `None` when there is none.
fn review_label(decision: &str) -> Option<&'static str> {
    match decision {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes requested"),
        "REVIEW_REQUIRED" => Some("review required"),
        _ => None,
    }
}

/// Which screen of the tab a shot captures.
#[cfg(feature = "visual-test")]
#[derive(Clone, Copy)]
pub(super) enum Scene {
    Issues,
    Issue,
    Pulls,
    Pull,
    NewPull,
    SignedOut,
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// Put the tab on fixed sample data for a shot, with no `gh` involved.
    pub(super) fn github_show_sample(&mut self, scene: Scene, cx: &mut Context<Self>) {
        use super::right_sidebar::SidebarTab;
        use pilus_client::{Checks, Comment, Label, RepoInfo};

        let label = |name: &str, color: &str| Label {
            name: name.to_string(),
            color: color.to_string(),
        };
        let issue = |number: u64, title: &str, state: &str, who: &str, labels: Vec<Label>| {
            Issue {
            number,
            title: title.to_string(),
            state: state.to_string(),
            author: who.to_string(),
            labels,
            updated_at: "2026-10-09T08:00:00Z".to_string(),
            url: format!("https://github.com/kepelet/pilus/issues/{number}"),
            body: "The right sidebar loses its scroll position when a tab is reselected.\n\n**Steps**\n\n1. Open the Files tab and scroll down\n2. Switch to Changes and back\n\nExpected: the tree stays where it was.".to_string(),
        }
        };
        let pull = |number: u64, title: &str, state: &str, draft: bool, review: &str| Pull {
            number,
            title: title.to_string(),
            state: state.to_string(),
            draft,
            author: "rizaldy".to_string(),
            head: format!("feat/change-{number}"),
            base: "main".to_string(),
            review: review.to_string(),
            labels: Vec::new(),
            updated_at: "2026-10-10T08:00:00Z".to_string(),
            url: format!("https://github.com/kepelet/pilus/pull/{number}"),
        };
        let comment = |who: &str, body: &str| Comment {
            author: who.to_string(),
            body: body.to_string(),
            created_at: "2026-10-09T12:00:00Z".to_string(),
            url: String::new(),
        };

        self.right.visible = true;
        self.right.select_tab(SidebarTab::Github);
        let spot = self
            .github_spot()
            .unwrap_or((self.active.clone(), "~/pilus".to_string()));
        let panel = &mut self.github_panel;
        panel.fixture = true;
        panel.spot = Some(spot);
        panel.state = Some(match scene {
            Scene::SignedOut => GhState::SignedOut(String::new()),
            _ => GhState::Ready(RepoInfo {
                name: "kepelet/pilus".to_string(),
                url: "https://github.com/kepelet/pilus".to_string(),
                default_branch: "main".to_string(),
                can_write: true,
                issues_enabled: true,
            }),
        });
        panel.issues = Some(Ok(vec![
            issue(
                41,
                "Sidebar scroll resets when a tab is reselected",
                "OPEN",
                "sam",
                vec![label("bug", "D6393F"), label("ui", "1D76DB")],
            ),
            issue(
                38,
                "Show unread comment counts on the Files tab",
                "OPEN",
                "kai",
                vec![label("enhancement", "A2EEEF")],
            ),
            issue(
                35,
                "Ports tab: forward fails when ssh uses ProxyJump",
                "OPEN",
                "rizaldy",
                Vec::new(),
            ),
        ]));
        panel.pulls = Some(Ok(vec![
            pull(44, "Add the GitHub tab", "OPEN", false, "REVIEW_REQUIRED"),
            pull(
                43,
                "Keep the sidebar scroll position per tab",
                "OPEN",
                true,
                "",
            ),
            pull(40, "Ports: end forwards on quit", "OPEN", false, "APPROVED"),
        ]));
        panel.section = match scene {
            Scene::Pulls | Scene::Pull | Scene::NewPull => Section::Pulls,
            _ => Section::Issues,
        };
        panel.page = match scene {
            Scene::Issue => Page::Issue(41),
            Scene::Pull => Page::Pull(44),
            Scene::NewPull => Page::NewPull,
            _ => Page::List,
        };
        panel.issue_detail = Some((
            41,
            Load::Ready(IssueDetail {
                issue: issue(
                    41,
                    "Sidebar scroll resets when a tab is reselected",
                    "OPEN",
                    "sam",
                    vec![label("bug", "D6393F")],
                ),
                assignees: vec!["rizaldy".to_string()],
                created_at: "2026-10-07T08:00:00Z".to_string(),
                comments: vec![
                    comment(
                        "kai",
                        "I can reproduce this on 0.1.3 too. It only happens with a document open.",
                    ),
                    comment(
                        "rizaldy",
                        "Thanks. Reading the saved offset from `Place` should fix it:\n\n```rust\nplace.scroll = list.offset();\n```",
                    ),
                ],
            }),
        ));
        panel.pull_detail = Some((
            44,
            Load::Ready(PullDetail {
                pull: pull(44, "Add the GitHub tab", "OPEN", false, "REVIEW_REQUIRED"),
                body: "Lists a workspace's issues and pull requests through `gh`, with comments and a way to submit the branch.".to_string(),
                comments: vec![comment("kai", "Looks good. Can the list poll slower?")],
                additions: 1240,
                deletions: 36,
                changed_files: 14,
                mergeable: "MERGEABLE".to_string(),
                checks: Checks { passed: 7, failed: 1, pending: 0 },
            }),
        ));
        panel.draft = Some(Load::Ready(PullDraft {
            branch: "feat/github-tab".to_string(),
            base: "main".to_string(),
            title: "Add the GitHub tab".to_string(),
            commits: vec![
                "Wire the tab into the right sidebar".to_string(),
                "Add gh client".to_string(),
            ],
            needs_push: true,
            existing: None,
        }));
        if matches!(scene, Scene::NewPull) {
            self.github_inputs.title.update(cx, |input, cx| {
                input.set_value("Add the GitHub tab".to_string(), cx)
            });
        }
        cx.notify();
    }

    pub(super) fn github_clear_sample(&mut self, cx: &mut Context<Self>) {
        self.github_panel = GithubPanel::new();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_decisions_read_as_words() {
        assert_eq!(review_label("APPROVED"), Some("approved"));
        assert_eq!(review_label("CHANGES_REQUESTED"), Some("changes requested"));
        assert_eq!(review_label(""), None);
    }

    #[test]
    fn an_unreadable_timestamp_shows_no_age() {
        assert_eq!(age_suffix("yesterday"), "");
        assert!(age_suffix("2020-01-01T00:00:00Z").starts_with(" · "));
    }

    #[test]
    fn a_read_error_becomes_a_failed_load() {
        assert!(matches!(Load::<u8>::from(Err("x".into())), Load::Failed(_)));
        assert!(matches!(Load::from(Ok(1u8)), Load::Ready(1)));
    }

    #[test]
    fn the_filter_follows_the_section() {
        let mut panel = GithubPanel::new();
        panel.pull_filter = StateFilter::Merged;
        assert_eq!(panel.filter(), StateFilter::Open);
        panel.section = Section::Pulls;
        assert_eq!(panel.filter(), StateFilter::Merged);
    }

    #[test]
    fn moving_to_another_workspace_drops_what_was_read() {
        let mut panel = GithubPanel::new();
        panel.issues = Some(Ok(Vec::new()));
        panel.page = Page::Issue(3);
        panel.focus = Some(GithubFocus::Comment);
        panel.reset();
        assert!(panel.issues.is_none() && !panel.composing());
        assert_eq!(panel.page, Page::List);
    }
}
