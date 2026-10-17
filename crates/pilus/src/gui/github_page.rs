//! The native pages behind the sidebar's Repositories section, drawn in the
//! terminal's card: the list of every repository, one repository with its
//! Issues, Pull Requests and Actions tabs, and one issue or pull request.
//!
//! The pages form a trail. A sidebar row starts a fresh one; going deeper
//! (list → repository → issue) pushes, and Back pops, closing the page when
//! the trail runs out. A repository's header comes from the same summary the
//! list shows (stars and what is open), so it needs no request of its own; its
//! three tabs each read one list through `pilus_client::GhAccount`, which
//! caches them, so flipping tabs and coming back costs nothing. Only the tab
//! on screen is read.
//!
//! Every read runs off the GUI thread; an answer for a repository the page
//! has since left is dropped. Descriptions and comments are Markdown and go
//! through the renderer the Files tab uses, parsed once when the detail lands.

use std::sync::mpsc::{Receiver, Sender, channel};

use gpui::{ClipboardItem, Context, Div, FontWeight, div, prelude::*, px};
use pilus_client::{
    Comment, IssueDetail, ItemSummary, Listing, PullDetail, RepoSummary, RunSummary, unix_seconds,
};

use super::github::{Load, Slot, display_name, repo_facts};
use super::markdown::{self, Document};
use super::md_select::Leaves;
use super::style::{self, ButtonKind, FillExt};
use super::tasks::{age_label, unix_now};
use super::toast::ToastKind;
use super::{PilusView, SIDEBAR_GLYPH, icon};

/// The tabs of a repository's page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RepoTab {
    Issues,
    Pulls,
    Actions,
}

impl RepoTab {
    const ALL: [RepoTab; 3] = [RepoTab::Issues, RepoTab::Pulls, RepoTab::Actions];

    fn label(self) -> &'static str {
        match self {
            Self::Issues => "Issues",
            Self::Pulls => "Pull Requests",
            Self::Actions => "Actions",
        }
    }
}

/// An issue or pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Subject {
    Pull { repo: String, number: u64 },
    Issue { repo: String, number: u64 },
}

impl Subject {
    fn repo(&self) -> &str {
        match self {
            Self::Pull { repo, .. } | Self::Issue { repo, .. } => repo,
        }
    }

    /// Where the item lives on github.com, which opens before it has loaded.
    fn url(&self) -> String {
        match self {
            Self::Pull { repo, number } => format!("https://github.com/{repo}/pull/{number}"),
            Self::Issue { repo, number } => format!("https://github.com/{repo}/issues/{number}"),
        }
    }

    fn number(&self) -> u64 {
        match self {
            Self::Pull { number, .. } | Self::Issue { number, .. } => *number,
        }
    }
}

/// One page of the trail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum View {
    /// Every repository, with its stars and what is open.
    List,
    Repo {
        slug: String,
        tab: RepoTab,
    },
    Item(Subject),
}

impl View {
    /// A repository's page, on its first tab.
    pub(super) fn repo(slug: &str) -> Self {
        Self::Repo {
            slug: slug.to_string(),
            tab: RepoTab::Issues,
        }
    }
}

enum Detail {
    Pull(Box<PullDetail>),
    Issue(Box<IssueDetail>),
}

enum Body {
    Loading,
    Failed(String),
    Ready(Detail),
}

/// The open repository's three lists.
struct Inside {
    slug: String,
    issues: Slot<ItemSummary>,
    pulls: Slot<ItemSummary>,
    runs: Slot<RunSummary>,
}

impl Inside {
    fn new(slug: &str) -> Self {
        Self {
            slug: slug.to_string(),
            issues: Slot::default(),
            pulls: Slot::default(),
            runs: Slot::default(),
        }
    }
}

enum Answer {
    Item(Subject, Result<Detail, String>),
    Issues(String, Result<Listing<ItemSummary>, String>, bool),
    Pulls(String, Result<Listing<ItemSummary>, String>, bool),
    Runs(String, Result<Listing<RunSummary>, String>, bool),
}

pub(super) struct GithubPage {
    /// Where the page has been; the last is on screen, empty is closed.
    trail: Vec<View>,
    inside: Inside,
    item: Body,
    /// The Markdown of the open item, parsed once, in the order it draws:
    /// the description first, then each comment.
    docs: Vec<Document>,
    tx: Sender<Answer>,
    rx: Receiver<Answer>,
}

impl Default for GithubPage {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            trail: Vec::new(),
            inside: Inside::new(""),
            item: Body::Loading,
            docs: Vec::new(),
            tx,
            rx,
        }
    }
}

impl GithubPage {
    pub(super) fn is_open(&self) -> bool {
        !self.trail.is_empty()
    }

    fn current(&self) -> Option<&View> {
        self.trail.last()
    }

    pub(super) fn close(&mut self) {
        self.trail.clear();
        self.docs.clear();
        self.item = Body::Loading;
    }

    /// Go to `view`: replacing the trail when `fresh` (a sidebar click),
    /// else on top of it.
    fn go(&mut self, view: View, fresh: bool) {
        if fresh {
            self.trail.clear();
        }
        self.trail.push(view);
    }

    /// Leave the page on screen. Returns whether another one is under it.
    fn back(&mut self) -> bool {
        self.trail.pop();
        !self.trail.is_empty()
    }

    fn set_tab(&mut self, tab: RepoTab) {
        if let Some(View::Repo { tab: current, .. }) = self.trail.last_mut() {
            *current = tab;
        }
    }

    /// Point the lists at `slug`, starting from what is saved when it is a
    /// different repository than the last one shown.
    fn focus(&mut self, slug: &str, account: &pilus_client::GhAccount) {
        if self.inside.slug.eq_ignore_ascii_case(slug) {
            return;
        }
        self.inside = Inside::new(slug);
        self.inside.issues.saved(account.saved_issues(slug));
        self.inside.pulls.saved(account.saved_pulls(slug));
        self.inside.runs.saved(account.saved_runs(slug));
    }

    fn begin_item(&mut self) {
        self.item = Body::Loading;
        self.docs.clear();
    }

    /// Take a finished read. Returns whether the screen changed and, when a
    /// read the user asked for failed, what to tell them.
    fn receive(&mut self, answer: Answer) -> (bool, Option<String>) {
        let same = |slug: &str, inside: &Inside| inside.slug.eq_ignore_ascii_case(slug);
        match answer {
            Answer::Item(subject, result) => {
                if self.current() != Some(&View::Item(subject)) {
                    return (false, None);
                }
                match result {
                    Ok(detail) => {
                        self.docs = markdown_of(&detail)
                            .into_iter()
                            .map(|source| Document::parse(&source))
                            .collect();
                        self.item = Body::Ready(detail);
                    }
                    Err(error) => {
                        self.docs.clear();
                        self.item = Body::Failed(error);
                    }
                }
                (true, None)
            }
            Answer::Issues(slug, result, forced) if same(&slug, &self.inside) => {
                (true, self.inside.issues.apply(result).filter(|_| forced))
            }
            Answer::Pulls(slug, result, forced) if same(&slug, &self.inside) => {
                (true, self.inside.pulls.apply(result).filter(|_| forced))
            }
            Answer::Runs(slug, result, forced) if same(&slug, &self.inside) => {
                (true, self.inside.runs.apply(result).filter(|_| forced))
            }
            _ => (false, None),
        }
    }
}

/// Every Markdown source an item draws, in drawing order.
fn markdown_of(detail: &Detail) -> Vec<String> {
    fn thread(body: &str, comments: &[Comment]) -> Vec<String> {
        std::iter::once(body.to_string())
            .chain(comments.iter().map(|comment| comment.body.clone()))
            .collect()
    }
    match detail {
        Detail::Pull(pull) => thread(&pull.body, &pull.comments),
        Detail::Issue(issue) => thread(&issue.issue.body, &issue.comments),
    }
}

/// `3d ago`, or nothing for a timestamp that doesn't parse.
fn ago(timestamp: &str) -> Option<String> {
    let then = unix_seconds(timestamp)?;
    Some(match age_label(unix_now(), then).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    })
}

fn count_of(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// How a workflow run stands, for its glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunState {
    Passed,
    Failed,
    Running,
    Queued,
    /// Cancelled, skipped, neutral, stale.
    Stopped,
}

fn run_state(run: &RunSummary) -> RunState {
    match (run.status.as_str(), run.conclusion.as_str()) {
        ("in_progress", _) => RunState::Running,
        ("queued" | "waiting" | "pending" | "requested", _) => RunState::Queued,
        (_, "success") => RunState::Passed,
        (_, "failure" | "timed_out" | "startup_failure" | "action_required") => RunState::Failed,
        _ => RunState::Stopped,
    }
}

/// An issue or pull request's second line.
fn item_sub(item: &ItemSummary) -> String {
    let mut parts = vec![format!("#{}", item.number), format!("by {}", item.author)];
    if let Some(when) = ago(&item.updated_at) {
        parts.push(format!("updated {when}"));
    }
    if item.comments > 0 {
        parts.push(count_of(item.comments, "comment", "comments"));
    }
    if !item.branch.is_empty() {
        parts.push(item.branch.clone());
    }
    parts.join(" · ")
}

fn run_sub(run: &RunSummary) -> String {
    let mut parts = vec![format!("{} #{}", run.workflow, run.number)];
    if !run.branch.is_empty() {
        parts.push(run.branch.clone());
    }
    if !run.event.is_empty() {
        parts.push(run.event.replace('_', " "));
    }
    if !run.actor.is_empty() {
        parts.push(run.actor.clone());
    }
    if let Some(when) = ago(&run.updated_at) {
        parts.push(when);
    }
    parts.join(" · ")
}

impl PilusView {
    /// Open `view`: replacing the trail for a sidebar click (`fresh`), else
    /// going one deeper.
    pub(super) fn github_open(&mut self, view: View, fresh: bool, cx: &mut Context<Self>) {
        if fresh {
            self.leave_settings_page();
        }
        self.github_page.go(view, fresh);
        self.github_enter();
        cx.notify();
    }

    /// Back one page, or close when it was the first.
    fn github_back(&mut self, cx: &mut Context<Self>) {
        if self.github_page.back() {
            self.github_enter();
        }
        cx.notify();
    }

    /// Read what the page on screen needs. Cheap when it is already held:
    /// the account's cache answers a fresh list without a request.
    fn github_enter(&mut self) {
        match self.github_page.current().cloned() {
            Some(View::List) => self.github_refresh(false),
            Some(View::Repo { slug, tab }) => {
                let account = self.github.account();
                self.github_page.focus(&slug, &account);
                self.github_load_tab(&slug, tab, false);
            }
            Some(View::Item(subject)) => {
                self.github_page.begin_item();
                self.github_load_item(subject, false);
            }
            None => {}
        }
    }

    fn github_load_tab(&mut self, slug: &str, tab: RepoTab, force: bool) {
        let load = match tab {
            RepoTab::Issues => &mut self.github_page.inside.issues.load,
            RepoTab::Pulls => &mut self.github_page.inside.pulls.load,
            RepoTab::Actions => &mut self.github_page.inside.runs.load,
        };
        if *load == Load::Loading {
            return;
        }
        *load = Load::Loading;
        let account = self.github.account();
        let tx = self.github_page.tx.clone();
        let slug = slug.to_string();
        std::thread::spawn(move || {
            let answer = match tab {
                RepoTab::Issues => {
                    Answer::Issues(slug.clone(), account.repo_issues(&slug, force), force)
                }
                RepoTab::Pulls => {
                    Answer::Pulls(slug.clone(), account.repo_pulls(&slug, force), force)
                }
                RepoTab::Actions => {
                    Answer::Runs(slug.clone(), account.repo_runs(&slug, force), force)
                }
            };
            tx.send(answer).ok();
        });
    }

    fn github_load_item(&mut self, subject: Subject, force: bool) {
        let account = self.github.account();
        let tx = self.github_page.tx.clone();
        std::thread::spawn(move || {
            let result = match &subject {
                Subject::Pull { repo, number } => account
                    .pull_detail(repo, *number, force)
                    .map(|detail| Detail::Pull(Box::new(detail))),
                Subject::Issue { repo, number } => account
                    .issue_detail(repo, *number, force)
                    .map(|detail| Detail::Issue(Box::new(detail))),
            };
            tx.send(Answer::Item(subject, result)).ok();
        });
    }

    /// Ask GitHub again for the page on screen, past the cache's lifetime.
    fn github_refresh_page(&mut self, cx: &mut Context<Self>) {
        match self.github_page.current().cloned() {
            Some(View::List) => self.github_refresh(true),
            Some(View::Repo { slug, tab }) => {
                self.github_refresh(true);
                self.github_load_tab(&slug, tab, true);
            }
            Some(View::Item(subject)) => {
                self.github_page.begin_item();
                self.github_load_item(subject, true);
            }
            None => {}
        }
        cx.notify();
    }

    fn github_pick_tab(&mut self, tab: RepoTab, cx: &mut Context<Self>) {
        self.github_page.set_tab(tab);
        if let Some(View::Repo { slug, .. }) = self.github_page.current().cloned() {
            self.github_load_tab(&slug, tab, false);
        }
        cx.notify();
    }

    /// Fold finished reads in. Returns whether anything changed.
    pub(super) fn github_page_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(answer) = self.github_page.rx.try_recv() {
            let (moved, error) = self.github_page.receive(answer);
            changed |= moved;
            if let Some(error) = error {
                self.toast(ToastKind::Error, format!("GitHub: {error}"));
            }
        }
        changed
    }

    /// The page on screen, in place of the terminal card.
    pub(super) fn github_page(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(view) = self.github_page.current().cloned() else {
            return div();
        };

        let (url, pin) = match &view {
            View::List => (None, None),
            View::Repo { slug, .. } => (
                Some(
                    self.github
                        .find(slug)
                        .map(|repo| repo.url.clone())
                        .filter(|url| !url.is_empty())
                        .unwrap_or_else(|| format!("https://github.com/{slug}")),
                ),
                Some(slug.clone()),
            ),
            View::Item(subject) => (Some(subject.url()), None),
        };

        let mut actions = div().flex().flex_row().gap_2();
        if let Some(slug) = pin {
            let pinned = self.settings.is_pinned(&slug);
            actions = actions.child(
                style::small_button(
                    "pilus-github-pin",
                    if pinned { "Unpin" } else { "Pin" },
                    ButtonKind::Secondary,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let on = !this.settings.is_pinned(&slug);
                    this.settings.set_pinned(&slug, on);
                    this.save_settings();
                    cx.notify();
                })),
            );
        }
        actions = actions.child(
            style::small_button(
                "pilus-github-refresh",
                "Refresh",
                ButtonKind::Secondary,
                theme,
                fonts.label,
            )
            .on_click(cx.listener(|this, _, _, cx| this.github_refresh_page(cx))),
        );
        if let Some(url) = url {
            let copy = url.clone();
            actions = actions
                .child(
                    style::small_button(
                        "pilus-github-copy",
                        "Copy Link",
                        ButtonKind::Secondary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        this.toast(ToastKind::Info, "Copied link");
                    })),
                )
                .child(
                    style::small_button(
                        "pilus-github-open",
                        "Open on GitHub",
                        ButtonKind::Primary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                );
        }

        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .px_4()
            .py_3()
            .child(
                style::small_button(
                    "pilus-github-back",
                    "‹ Back",
                    ButtonKind::Secondary,
                    theme,
                    fonts.label,
                )
                .text_color(theme.muted)
                .on_click(cx.listener(|this, _, _, cx| this.github_back(cx))),
            )
            .child(actions);

        let content = match &view {
            View::List => self.github_list_view(cx),
            View::Repo { slug, tab } => self.github_repo_view(slug, *tab, cx),
            View::Item(subject) => self.github_item_view(subject, cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(bar)
            .child(
                div()
                    .id("pilus-github-page-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(content),
            )
    }

    /// The page's column: never wider than a comfortable measure.
    fn github_column(&self) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .max_w(px(style::scaled(&self.shared_settings, 960.0)))
            .pb_6()
    }

    fn github_title(&self, title: &str, subtitle: &str) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .child(
                div()
                    .text_size(px(fonts.headline))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.fg)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(subtitle.to_string()),
            )
    }

    /// A wrapped row of small tags.
    fn github_chips(&self, chips: Vec<(String, Option<gpui::Rgba>)>) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut row = div().flex().flex_row().flex_wrap().gap_2().px_4();
        for (text, tone) in chips {
            let mut chip = style::chip(self.theme, text, fonts.label);
            if let Some(tone) = tone {
                chip = chip.text_color(tone);
            }
            row = row.child(chip);
        }
        row
    }

    fn github_line(&self, text: String) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .px_4()
            .text_size(px(fonts.body))
            .text_color(self.theme.panel_fg)
            .child(text)
    }

    /// A status in the middle of a page: a glyph slot, a headline and a
    /// sentence.
    fn github_message(&self, glyph: Div, headline: &str, detail: String) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        style::empty_state(self.theme, fonts, glyph, headline.to_string(), detail).py(px(48.0))
    }

    fn github_spinner(&self) -> Div {
        div().child(style::spinner(
            "pilus-github-page-spinner",
            SIDEBAR_GLYPH * 1.5,
            self.theme.muted,
        ))
    }

    fn github_caveat(&self, text: &str) -> Div {
        div()
            .px_4()
            .text_size(px(style::Fonts::resolve(&self.shared_settings).secondary))
            .text_color(self.theme.muted)
            .child(text.to_string())
    }

    /// A framed list of rows, each under a hairline but the first.
    fn github_rows(&self, rows: Vec<gpui::AnyElement>) -> Div {
        let theme = self.theme;
        let mut card = style::section_card(theme).py_0().overflow_hidden();
        for (index, row) in rows.into_iter().enumerate() {
            card = card.child(
                div()
                    .when(index > 0, |wrap| {
                        wrap.border_t_1().border_color(theme.border)
                    })
                    .child(row),
            );
        }
        card
    }

    // ---- the list of every repository ---------------------------------

    fn github_list_view(&self, cx: &mut Context<Self>) -> Div {
        let slot = &self.github.repos;
        let login = self.github.login().unwrap_or_default().to_string();
        let subtitle = match slot.fetched_at {
            Some(_) => format!(
                "{} · most recently pushed first",
                count_of(slot.items.len() as u64, "repository", "repositories")
            ),
            None => "Asking GitHub…".to_string(),
        };
        let mut column = self
            .github_column()
            .child(self.github_title("Repositories", &subtitle));
        if let Some(caveat) = slot.caveat() {
            column = column.child(self.github_caveat(caveat));
        }
        if slot.items.is_empty() {
            return column.child(match slot.fetched_at {
                None => self.github_message(
                    self.github_spinner(),
                    "Loading…",
                    "Asking GitHub.".to_string(),
                ),
                Some(_) => self.github_message(
                    div(),
                    "No repositories",
                    "GitHub lists none for this account.".to_string(),
                ),
            });
        }
        let rows = slot
            .items
            .iter()
            .map(|repo| self.github_repo_entry(repo, &login, cx).into_any_element())
            .collect();
        column.child(self.github_rows(rows))
    }

    /// One repository in the list: name and facts on the left, what is open on
    /// the right, and a pin.
    fn github_repo_entry(
        &self,
        repo: &RepoSummary,
        login: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let pinned = self.settings.is_pinned(&repo.name);
        let slug = repo.name.clone();
        let pin_slug = repo.name.clone();

        let mut name = div().flex().flex_row().items_center().gap_2().child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(fonts.body))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.panel_fg)
                .child(display_name(repo, login).to_string()),
        );
        for (flag, label) in [
            (repo.private, "Private"),
            (repo.fork, "Fork"),
            (repo.archived, "Archived"),
        ] {
            if flag {
                name = name.child(style::chip(theme, label, fonts.label));
            }
        }
        let about = if repo.description.is_empty() {
            repo_facts(repo)
        } else {
            repo.description.clone()
        };

        let stat = |glyph: &'static [u8], value: u64| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap_1()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(icon(glyph, fonts.secondary, theme.muted))
                .child(value.to_string())
        };
        // Stars, open issues and open pull requests ride the name's line.
        let name = name
            .child(div().w(px(4.0)))
            .child(stat(
                include_bytes!("../../assets/icons/star.svg"),
                repo.stars,
            ))
            .child(stat(
                include_bytes!("../../assets/icons/circle-dot.svg"),
                repo.open_issues,
            ))
            .child(stat(
                include_bytes!("../../assets/icons/git-pull-request.svg"),
                repo.open_pulls,
            ));

        div()
            .id(format!("pilus-github-list-{}", repo.name))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .cursor_pointer()
            .hover_fill(theme.panel, style::RADIUS_MD)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(name)
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(about),
                    ),
            )
            .child(
                style::icon_button(
                    format!("pilus-github-list-pin-{}", repo.name),
                    include_bytes!("../../assets/icons/pin.svg"),
                    if pinned {
                        ButtonKind::Secondary
                    } else {
                        ButtonKind::Ghost
                    },
                    theme,
                    style::scaled(&self.shared_settings, 28.0),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    // The pin sits inside the row; pinning must not open it.
                    cx.stop_propagation();
                    let on = !this.settings.is_pinned(&pin_slug);
                    this.settings.set_pinned(&pin_slug, on);
                    this.save_settings();
                    cx.notify();
                })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.github_open(View::repo(&slug), false, cx);
            }))
    }

    // ---- one repository ------------------------------------------------

    fn github_repo_view(&self, slug: &str, tab: RepoTab, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let summary = self.github.find(slug);

        let mut column = self.github_column();
        match summary {
            Some(repo) => {
                let mut chips = vec![(
                    if repo.private { "Private" } else { "Public" }.to_string(),
                    None,
                )];
                if repo.fork {
                    chips.push(("Fork".to_string(), None));
                }
                if repo.archived {
                    chips.push(("Archived".to_string(), Some(theme.alert)));
                }
                let mut facts = Vec::new();
                if !repo.language.is_empty() {
                    facts.push(repo.language.clone());
                }
                facts.push(format!("★ {}", repo.stars));
                if let Some(when) = ago(&repo.pushed_at) {
                    facts.push(format!("pushed {when}"));
                }
                if !repo.default_branch.is_empty() {
                    facts.push(format!("default branch {}", repo.default_branch));
                }
                column = column
                    .child(self.github_title(&repo.name, &repo.url))
                    .child(self.github_chips(chips));
                if !repo.description.is_empty() {
                    column = column.child(self.github_line(repo.description.clone()));
                }
                column = column.child(self.github_line(facts.join(" · ")));
            }
            None => column = column.child(self.github_title(slug, "")),
        }

        let inside = &self.github_page.inside;
        let counts = summary.map(|repo| (repo.open_issues, repo.open_pulls));
        let mut tabs = div().flex().flex_row().gap_1().px_3();
        for each in RepoTab::ALL {
            let count = match (each, counts) {
                (RepoTab::Issues, Some((issues, _))) => Some(issues),
                (RepoTab::Pulls, Some((_, pulls))) => Some(pulls),
                _ => None,
            };
            let label = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(each.label())
                .children(count.map(|count| {
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(count.to_string())
                }));
            tabs = tabs.child(
                style::tab(
                    format!("pilus-github-tab-{}", each.label()),
                    label,
                    each == tab,
                    theme,
                    fonts.body,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.github_pick_tab(each, cx))),
            );
        }
        column = column.child(tabs);

        column.child(match tab {
            RepoTab::Issues => self.github_items(&inside.issues, false, "No open issues", cx),
            RepoTab::Pulls => self.github_items(&inside.pulls, true, "No open pull requests", cx),
            RepoTab::Actions => self.github_runs(&inside.runs, cx),
        })
    }

    /// What a tab shows while it has no rows: a spinner on the first read, the
    /// error if it failed, or the empty sentence.
    fn github_hollow<T>(&self, slot: &Slot<T>, empty: &str) -> Div {
        match (slot.fetched_at, slot.caveat()) {
            (None, Some(error)) => {
                self.github_message(div(), "Couldn't load this", error.to_string())
            }
            (None, None) => self.github_message(self.github_spinner(), "Loading…", String::new()),
            (Some(_), _) => self.github_message(div(), empty, String::new()),
        }
    }

    fn github_items(
        &self,
        slot: &Slot<ItemSummary>,
        pulls: bool,
        empty: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        if slot.items.is_empty() {
            return self.github_hollow(slot, empty);
        }
        let rows = slot
            .items
            .iter()
            .map(|item| self.github_item_entry(item, pulls, cx).into_any_element())
            .collect();
        let mut list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.github_rows(rows));
        if let Some(caveat) = slot.caveat() {
            list = list.child(self.github_caveat(caveat));
        }
        list
    }

    fn github_item_entry(
        &self,
        item: &ItemSummary,
        pulls: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let (glyph, tone): (&'static [u8], _) = match (pulls, item.draft) {
            (true, true) => (
                include_bytes!("../../assets/icons/git-pull-request-draft.svg"),
                theme.muted,
            ),
            (true, false) => (
                include_bytes!("../../assets/icons/git-pull-request.svg"),
                theme.ok,
            ),
            (false, _) => (
                include_bytes!("../../assets/icons/circle-dot.svg"),
                theme.ok,
            ),
        };
        let subject = if pulls {
            Subject::Pull {
                repo: item.repo.clone(),
                number: item.number,
            }
        } else {
            Subject::Issue {
                repo: item.repo.clone(),
                number: item.number,
            }
        };
        let mut head = div().flex().flex_row().items_center().gap_2().child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(fonts.body))
                .text_color(theme.panel_fg)
                .child(item.title.clone()),
        );
        if item.draft {
            head = head.child(style::chip(theme, "Draft", fonts.label));
        }
        self.github_entry(
            format!(
                "pilus-github-{}-{}",
                if pulls { "pull" } else { "issue" },
                item.number
            ),
            icon(glyph, SIDEBAR_GLYPH, tone).into_any_element(),
            head,
            item_sub(item),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.github_open(View::Item(subject.clone()), false, cx);
        }))
    }

    fn github_runs(&self, slot: &Slot<RunSummary>, cx: &mut Context<Self>) -> Div {
        if slot.items.is_empty() {
            return self.github_hollow(slot, "No workflow runs");
        }
        let rows = slot
            .items
            .iter()
            .map(|run| self.github_run_entry(run, cx).into_any_element())
            .collect();
        let mut list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.github_rows(rows));
        if let Some(caveat) = slot.caveat() {
            list = list.child(self.github_caveat(caveat));
        }
        list
    }

    fn github_run_entry(&self, run: &RunSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let glyph = match run_state(run) {
            RunState::Passed => icon(
                include_bytes!("../../assets/icons/circle-check.svg"),
                SIDEBAR_GLYPH,
                theme.ok,
            )
            .into_any_element(),
            RunState::Failed => icon(
                include_bytes!("../../assets/icons/circle-alert.svg"),
                SIDEBAR_GLYPH,
                theme.alert,
            )
            .into_any_element(),
            RunState::Running => style::spinner(
                format!("pilus-github-run-{}", run.id),
                SIDEBAR_GLYPH,
                theme.muted,
            )
            .into_any_element(),
            RunState::Queued => icon(
                include_bytes!("../../assets/icons/clock.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            )
            .into_any_element(),
            RunState::Stopped => icon(
                include_bytes!("../../assets/icons/circle.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            )
            .into_any_element(),
        };
        let title = if run.title.is_empty() {
            run.workflow.clone()
        } else {
            run.title.clone()
        };
        let url = run.url.clone();
        self.github_entry(
            format!("pilus-github-run-row-{}", run.id),
            glyph,
            div()
                .min_w_0()
                .truncate()
                .text_size(px(fonts.body))
                .text_color(theme.panel_fg)
                .child(title),
            run_sub(run),
        )
        // A run has no page of its own here; GitHub shows its logs.
        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
    }

    /// One row of a list inside a repository: a leading glyph, a head line and
    /// a muted second line.
    fn github_entry(
        &self,
        id: String,
        glyph: gpui::AnyElement,
        head: impl IntoElement,
        sub: String,
    ) -> gpui::Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .px_3()
            .py_2()
            .cursor_pointer()
            .hover_fill(theme.panel, style::RADIUS_MD)
            .child(
                div()
                    .w(px(SIDEBAR_GLYPH))
                    .h(px(fonts.body * 1.4))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(head)
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(sub),
                    ),
            )
    }

    // ---- one issue or pull request -------------------------------------

    fn github_item_view(&self, subject: &Subject, cx: &mut Context<Self>) -> Div {
        match &self.github_page.item {
            Body::Loading => self.github_message(
                self.github_spinner(),
                "Loading…",
                format!("{}#{}", subject.repo(), subject.number()),
            ),
            Body::Failed(error) => {
                let retry = subject.clone();
                self.github_message(div(), "Couldn't load this", error.clone())
                    .child(
                        style::button(
                            "pilus-github-retry",
                            "Try Again",
                            ButtonKind::Secondary,
                            self.theme,
                            style::Fonts::resolve(&self.shared_settings).body,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.github_page.begin_item();
                            this.github_load_item(retry.clone(), true);
                            cx.notify();
                        })),
                    )
            }
            Body::Ready(Detail::Pull(pull)) => self.github_pull_view(subject.repo(), pull),
            Body::Ready(Detail::Issue(issue)) => self.github_issue_view(subject.repo(), issue),
        }
    }

    /// One parsed Markdown source, as a column of blocks.
    fn github_markdown(&self, at: usize, empty: &str) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut column = div().flex().flex_col().px_3();
        let blocks = self
            .github_page
            .docs
            .get(at)
            .map(|doc| doc.blocks())
            .unwrap_or_default();
        if blocks.is_empty() {
            return column.child(
                div()
                    .text_size(px(fonts.body))
                    .text_color(theme.muted)
                    .child(empty.to_string()),
            );
        }
        let unit = fonts.body / 13.0;
        for (ix, block) in blocks.iter().enumerate() {
            let (before, after) = block.spacing();
            let leaves = Leaves::inert(ix);
            column = column.child(
                div()
                    .w_full()
                    .pt(px(before * unit))
                    .pb(px(after * unit))
                    .child(markdown::render_block(
                        block,
                        theme,
                        fonts,
                        &self.shared_settings,
                        &leaves,
                    )),
            );
        }
        column
    }

    /// The conversation: each comment under its author and age.
    fn github_comments(&self, comments: &[Comment]) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut column = div().flex().flex_col();
        for (index, comment) in comments.iter().enumerate() {
            let mut head = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .pb_1()
                .text_size(px(fonts.secondary))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.panel_fg)
                        .child(comment.author.clone()),
                );
            if let Some(when) = ago(&comment.created_at) {
                head = head.child(div().text_color(theme.muted).child(when));
            }
            let mut item = div().flex().flex_col().py_2();
            if index > 0 {
                item = item.border_t_1().border_color(theme.border).pt_3();
            }
            column = column.child(
                item.child(head)
                    // Doc 0 is the description; the comments follow it.
                    .child(self.github_markdown(index + 1, "")),
            );
        }
        column
    }

    fn github_card(&self, title: &'static str, body: Div) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        // The tab rises above the card, so the card needs room above it.
        style::section_card_tab(self.theme, title, fonts.label)
            .mt(px(fonts.label + 14.0 + 12.0))
            .child(body)
    }

    fn github_labels(&self, labels: &[pilus_client::Label]) -> Option<Div> {
        (!labels.is_empty()).then(|| {
            self.github_chips(
                labels
                    .iter()
                    .map(|label| (label.name.clone(), None))
                    .collect(),
            )
        })
    }

    fn github_pull_view(&self, repo: &str, detail: &PullDetail) -> Div {
        let theme = self.theme;
        let pull = &detail.pull;
        let state_tone = match pull.state.as_str() {
            "OPEN" => Some(theme.ok),
            "CLOSED" => Some(theme.alert),
            _ => None,
        };
        let mut chips = vec![(
            if pull.draft {
                "Draft".to_string()
            } else {
                title_case(&pull.state)
            },
            if pull.draft { None } else { state_tone },
        )];
        match pull.review.as_str() {
            "APPROVED" => chips.push(("Approved".to_string(), Some(theme.ok))),
            "CHANGES_REQUESTED" => chips.push(("Changes requested".to_string(), Some(theme.alert))),
            "REVIEW_REQUIRED" => chips.push(("Review required".to_string(), None)),
            _ => {}
        }
        if detail.mergeable == "CONFLICTING" {
            chips.push(("Merge conflicts".to_string(), Some(theme.alert)));
        }

        let mut meta = format!(
            "{} wants to merge {} into {}",
            pull.author, pull.head, pull.base
        );
        if let Some(when) = ago(&pull.updated_at) {
            meta.push_str(&format!(" · updated {when}"));
        }
        let stats = format!(
            "+{} −{} across {} · {}",
            detail.additions,
            detail.deletions,
            count_of(detail.changed_files, "file", "files"),
            checks_words(detail.checks),
        );

        let mut column = self
            .github_column()
            .child(self.github_title(&pull.title, &format!("{repo}#{}", pull.number)))
            .child(self.github_chips(chips))
            .child(self.github_line(meta))
            .child(self.github_line(stats));
        if let Some(labels) = self.github_labels(&pull.labels) {
            column = column.child(labels);
        }
        column = column.child(self.github_card(
            "DESCRIPTION",
            self.github_markdown(0, "No description provided."),
        ));
        if !detail.comments.is_empty() {
            column =
                column.child(self.github_card("COMMENTS", self.github_comments(&detail.comments)));
        }
        column
    }

    fn github_issue_view(&self, repo: &str, detail: &IssueDetail) -> Div {
        let theme = self.theme;
        let issue = &detail.issue;
        let tone = match issue.state.as_str() {
            "OPEN" => Some(theme.ok),
            "CLOSED" => Some(theme.alert),
            _ => None,
        };
        let mut meta = format!("{} opened this", issue.author);
        if let Some(when) = ago(&detail.created_at) {
            meta.push_str(&format!(" {when}"));
        }
        meta.push_str(&format!(
            " · {}",
            count_of(detail.comments.len() as u64, "comment", "comments")
        ));

        let mut column = self
            .github_column()
            .child(self.github_title(&issue.title, &format!("{repo}#{}", issue.number)))
            .child(self.github_chips(vec![(title_case(&issue.state), tone)]))
            .child(self.github_line(meta));
        if !detail.assignees.is_empty() {
            column = column
                .child(self.github_line(format!("Assigned to {}", detail.assignees.join(", "))));
        }
        if let Some(labels) = self.github_labels(&issue.labels) {
            column = column.child(labels);
        }
        column = column.child(self.github_card(
            "DESCRIPTION",
            self.github_markdown(0, "No description provided."),
        ));
        if !detail.comments.is_empty() {
            column =
                column.child(self.github_card("COMMENTS", self.github_comments(&detail.comments)));
        }
        column
    }
}

/// `OPEN` as `Open`.
fn title_case(state: &str) -> String {
    let lower = state.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How a pull request's checks stand, in one phrase.
fn checks_words(checks: pilus_client::Checks) -> String {
    if checks.passed + checks.failed + checks.pending == 0 {
        return "no checks".to_string();
    }
    let mut parts = Vec::new();
    if checks.failed > 0 {
        parts.push(format!("{} failing", checks.failed));
    }
    if checks.pending > 0 {
        parts.push(format!("{} pending", checks.pending));
    }
    if checks.passed > 0 {
        parts.push(format!("{} passing", checks.passed));
    }
    parts.join(", ")
}

/// Pages for the visual captures, so a shot never reaches GitHub.
#[cfg(feature = "visual-test")]
impl GithubPage {
    pub(super) fn open_sample(&mut self, which: &str) {
        let item =
            |number: u64, title: &str, draft: bool, branch: &str, comments: u64| ItemSummary {
                repo: "kepelet/luvus".to_string(),
                number,
                title: title.to_string(),
                author: "octocat".to_string(),
                draft,
                branch: branch.to_string(),
                updated_at: "2026-10-15T20:00:00Z".to_string(),
                comments,
                url: String::new(),
            };
        let run = |id: u64, title: &str, status: &str, conclusion: &str| RunSummary {
            id,
            workflow: "CI".to_string(),
            title: title.to_string(),
            number: id,
            status: status.to_string(),
            conclusion: conclusion.to_string(),
            branch: "main".to_string(),
            event: "pull_request".to_string(),
            actor: "octocat".to_string(),
            updated_at: "2026-10-16T05:00:00Z".to_string(),
            url: String::new(),
        };
        fn listing<T>(items: Vec<T>) -> Listing<T> {
            Listing {
                items,
                fetched_at: unix_now(),
                note: None,
            }
        }
        let tab = match which {
            "pulls" => Some(RepoTab::Pulls),
            "actions" => Some(RepoTab::Actions),
            "issues" => Some(RepoTab::Issues),
            _ => None,
        };
        match (which, tab) {
            ("list", _) => self.go(View::List, true),
            (_, Some(tab)) => {
                self.go(
                    View::Repo {
                        slug: "kepelet/luvus".to_string(),
                        tab,
                    },
                    true,
                );
                self.inside = Inside::new("kepelet/luvus");
                self.inside.issues.apply(Ok(listing(vec![
                    item(91, "Agents tab flickers on reconnect", false, "", 3),
                    item(
                        88,
                        "Pane titles ignore the Interface size setting",
                        false,
                        "",
                        0,
                    ),
                    item(80, "Support a custom shell per workspace", false, "", 12),
                ])));
                self.inside.pulls.apply(Ok(listing(vec![
                    item(
                        87,
                        "Fix a panic when a pane closes mid-resize",
                        false,
                        "fix-resize-panic",
                        0,
                    ),
                    item(
                        86,
                        "Refactor the sidebar menu into its own module",
                        true,
                        "menu-module",
                        0,
                    ),
                ])));
                self.inside.runs.apply(Ok(listing(vec![
                    run(
                        412,
                        "Fix a panic when a pane closes mid-resize",
                        "completed",
                        "failure",
                    ),
                    run(411, "Bump bincode to 2.0.1", "in_progress", ""),
                    run(410, "Update the download page", "completed", "success"),
                    run(409, "Nightly", "queued", ""),
                    run(408, "Revert the theme change", "completed", "cancelled"),
                ])));
            }
            _ => self.sample_item(which),
        }
    }

    fn sample_item(&mut self, which: &str) {
        use pilus_client::{Checks, Label, Pull};
        let comment = |author: &str, body: &str| Comment {
            author: author.to_string(),
            body: body.to_string(),
            created_at: "2026-10-15T20:00:00Z".to_string(),
            url: String::new(),
        };
        let (subject, detail) = if which == "issue" {
            (
                Subject::Issue {
                    repo: "kepelet/pilus".to_string(),
                    number: 212,
                },
                Detail::Issue(Box::new(IssueDetail {
                    issue: pilus_client::Issue {
                        number: 212,
                        title: "Sidebar rows ignore the Interface size setting".to_string(),
                        state: "OPEN".to_string(),
                        author: "octocat".to_string(),
                        labels: vec![Label {
                            name: "bug".to_string(),
                            color: "d73a4a".to_string(),
                        }],
                        updated_at: "2026-10-15T20:00:00Z".to_string(),
                        url: String::new(),
                        body: "When the title wraps, the second line keeps the default size.\n\n1. Set Interface size to 16\n2. Open a long-named workspace".to_string(),
                    },
                    assignees: vec!["rizaldy".to_string()],
                    created_at: "2026-10-12T20:00:00Z".to_string(),
                    comments: vec![comment("rizaldy", "Reproduced. `section_font` is fixed at 11.")],
                })),
            )
        } else {
            (
                Subject::Pull {
                    repo: "kepelet/pilus".to_string(),
                    number: 214,
                },
                Detail::Pull(Box::new(PullDetail {
                    pull: Pull {
                        number: 214,
                        title: "Show your repositories above WORKSPACES".to_string(),
                        state: "OPEN".to_string(),
                        draft: false,
                        author: "rizaldy".to_string(),
                        head: "github-repos".to_string(),
                        base: "main".to_string(),
                        review: "REVIEW_REQUIRED".to_string(),
                        labels: vec![Label {
                            name: "enhancement".to_string(),
                            color: "a2eeef".to_string(),
                        }],
                        updated_at: "2026-10-16T05:00:00Z".to_string(),
                        url: String::new(),
                    },
                    body: "## Summary\n\nShows five repositories from `gh`, pins first.\n\n- cached for five minutes\n- revalidated by `ETag`".to_string(),
                    comments: vec![comment("octocat", "Looks good.")],
                    additions: 1480,
                    deletions: 12,
                    changed_files: 14,
                    mergeable: "MERGEABLE".to_string(),
                    checks: Checks {
                        passed: 7,
                        failed: 0,
                        pending: 1,
                    },
                })),
            )
        };
        self.go(
            View::Repo {
                slug: subject.repo().to_string(),
                tab: RepoTab::Pulls,
            },
            true,
        );
        self.go(View::Item(subject.clone()), false);
        self.receive(Answer::Item(subject, Ok(detail)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(number: u64) -> ItemSummary {
        ItemSummary {
            repo: "o/r".into(),
            number,
            title: format!("T{number}"),
            author: "u".into(),
            draft: false,
            branch: String::new(),
            updated_at: "not a time".into(),
            comments: 0,
            url: String::new(),
        }
    }

    fn run(status: &str, conclusion: &str) -> RunSummary {
        RunSummary {
            id: 1,
            workflow: "CI".into(),
            title: String::new(),
            number: 7,
            status: status.into(),
            conclusion: conclusion.into(),
            branch: "main".into(),
            event: "pull_request".into(),
            actor: "u".into(),
            updated_at: String::new(),
            url: String::new(),
        }
    }

    fn listing<T>(items: Vec<T>) -> Result<Listing<T>, String> {
        Ok(Listing {
            items,
            fetched_at: 1,
            note: None,
        })
    }

    #[test]
    fn the_trail_goes_deeper_and_back_out_to_closed() {
        let mut page = GithubPage::default();
        assert!(!page.is_open());
        page.go(View::List, true);
        page.go(View::repo("o/r"), false);
        page.go(
            View::Item(Subject::Issue {
                repo: "o/r".into(),
                number: 3,
            }),
            false,
        );
        assert!(page.back(), "back to the repository");
        assert!(matches!(page.current(), Some(View::Repo { .. })));
        assert!(page.back(), "back to the list");
        assert_eq!(page.current(), Some(&View::List));
        assert!(!page.back(), "the first page closes");
        assert!(!page.is_open());

        // A sidebar click starts over, whatever came before.
        page.go(View::List, true);
        page.go(View::repo("o/r"), false);
        page.go(View::repo("o/other"), true);
        assert_eq!(page.trail.len(), 1);
    }

    #[test]
    fn picking_a_tab_changes_only_the_repository_page() {
        let mut page = GithubPage::default();
        page.go(View::repo("o/r"), true);
        page.set_tab(RepoTab::Actions);
        assert_eq!(
            page.current(),
            Some(&View::Repo {
                slug: "o/r".into(),
                tab: RepoTab::Actions
            })
        );
        page.go(View::List, false);
        page.set_tab(RepoTab::Pulls);
        assert_eq!(page.current(), Some(&View::List));
    }

    #[test]
    fn an_answer_for_a_repository_the_page_left_is_dropped() {
        let mut page = GithubPage::default();
        page.go(View::repo("o/a"), true);
        page.inside = Inside::new("o/a");

        let (moved, _) = page.receive(Answer::Issues(
            "o/b".into(),
            listing(vec![summary(1)]),
            false,
        ));
        assert!(!moved);
        assert!(page.inside.issues.items.is_empty());

        let (moved, _) = page.receive(Answer::Issues(
            "O/A".into(),
            listing(vec![summary(1)]),
            false,
        ));
        assert!(moved, "names compare without case");
        assert_eq!(page.inside.issues.items.len(), 1);
    }

    #[test]
    fn only_a_forced_read_reports_its_failure() {
        let mut page = GithubPage::default();
        page.inside = Inside::new("o/a");
        let (_, quiet) = page.receive(Answer::Runs("o/a".into(), Err("boom".into()), false));
        assert_eq!(quiet, None);
        assert_eq!(page.inside.runs.caveat(), Some("boom"));
        let (_, loud) = page.receive(Answer::Runs("o/a".into(), Err("boom".into()), true));
        assert_eq!(loud.as_deref(), Some("boom"));
    }

    #[test]
    fn an_item_answer_only_lands_on_the_item_on_screen() {
        let subject = Subject::Issue {
            repo: "o/r".into(),
            number: 1,
        };
        let other = Subject::Issue {
            repo: "o/r".into(),
            number: 2,
        };
        let detail = |body: &str| {
            Detail::Issue(Box::new(IssueDetail {
                issue: pilus_client::Issue {
                    number: 1,
                    title: "t".into(),
                    state: "OPEN".into(),
                    author: "a".into(),
                    labels: vec![],
                    updated_at: String::new(),
                    url: String::new(),
                    body: body.into(),
                },
                assignees: vec![],
                created_at: String::new(),
                comments: vec![Comment {
                    author: "b".into(),
                    body: "first".into(),
                    created_at: String::new(),
                    url: String::new(),
                }],
            }))
        };
        let mut page = GithubPage::default();
        page.go(View::Item(subject.clone()), true);

        assert!(!page.receive(Answer::Item(other, Ok(detail("late")))).0);
        assert!(matches!(page.item, Body::Loading));

        assert!(
            page.receive(Answer::Item(subject, Ok(detail("# Heading\n\ntext"))))
                .0
        );
        assert_eq!(page.docs.len(), 2, "description plus one comment");
        assert_eq!(page.docs[0].blocks().len(), 2);
        assert_eq!(page.docs[1].blocks().len(), 1);
    }

    #[test]
    fn a_run_reads_as_what_it_did() {
        assert_eq!(run_state(&run("completed", "success")), RunState::Passed);
        assert_eq!(run_state(&run("completed", "failure")), RunState::Failed);
        assert_eq!(run_state(&run("completed", "timed_out")), RunState::Failed);
        assert_eq!(run_state(&run("in_progress", "")), RunState::Running);
        assert_eq!(run_state(&run("queued", "")), RunState::Queued);
        assert_eq!(run_state(&run("completed", "cancelled")), RunState::Stopped);
        assert_eq!(run_state(&run("completed", "skipped")), RunState::Stopped);
    }

    #[test]
    fn rows_say_who_and_what_without_inventing_an_age() {
        let mut item = summary(12);
        item.comments = 2;
        item.branch = "feature".into();
        assert_eq!(
            item_sub(&item),
            "#12 · by u · 2 comments · feature",
            "an unreadable time is left out, not guessed"
        );
        assert_eq!(
            run_sub(&run("completed", "success")),
            "CI #7 · main · pull request · u"
        );
    }

    #[test]
    fn check_summaries_lead_with_what_needs_attention() {
        use pilus_client::Checks;
        assert_eq!(checks_words(Checks::default()), "no checks");
        assert_eq!(
            checks_words(Checks {
                passed: 5,
                failed: 1,
                pending: 2
            }),
            "1 failing, 2 pending, 5 passing"
        );
        assert_eq!(title_case("OPEN"), "Open");
        assert_eq!(count_of(1, "file", "files"), "1 file");
    }

    #[test]
    fn subjects_know_their_github_address() {
        assert_eq!(
            Subject::Pull {
                repo: "o/r".into(),
                number: 4
            }
            .url(),
            "https://github.com/o/r/pull/4"
        );
        assert_eq!(
            Subject::Issue {
                repo: "o/r".into(),
                number: 9
            }
            .url(),
            "https://github.com/o/r/issues/9"
        );
    }
}
