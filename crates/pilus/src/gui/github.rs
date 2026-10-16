//! GitHub in the sidebar: a Repositories section above WORKSPACES, for the
//! account `gh` is signed in as on this Mac.
//!
//! The section shows three repositories: the ones pinned in Settings first, in
//! the order they were pinned, then the most recently pushed to fill what is
//! left. Four pins make four pins and the latest repository. A row opens that
//! repository's page; "All Repositories" in the header's right-click menu opens
//! the full list with stars and what is open in each (`github_page`).
//!
//! The account is the host's, so this never goes through a daemon or an SSH
//! profile: a remote machine's workspaces sit beside the same section.
//! `pilus_client::GhAccount` owns every request and its cache (fresh lists
//! are free, stale ones revalidate, a rate limit is waited out); this module
//! owns what the window does with the answers. [`GithubFeed`] is the state and
//! holds no GPUI, so its transitions are unit-tested; the `impl PilusView`
//! below drives it from the tick loop and paints it.
//!
//! Nothing here blocks the GUI thread. A probe or a read runs on its own
//! thread and answers through the feed's channel, and the tick folds the
//! answer in. Saved lists paint at once on launch, before `gh` has answered.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use gpui::{Context, Div, MouseButton, MouseDownEvent, div, prelude::*, px};
use pilus_client::{AccountState, GhAccount, GhState, Listing, RepoSummary, unix_seconds};

use super::github_page::View;
use super::sidebar_menu::Item as SidebarItem;
use super::style::RadiusExt;
use super::tasks::{age_label, unix_now};
use super::toast::ToastKind;
use super::{
    PilusView, SIDEBAR_BAND_HEIGHT, SIDEBAR_GLYPH, SIDEBAR_GLYPH_GAP, SIDEBAR_ROW_HEIGHT,
    SIDEBAR_ROW_PAD, SIDEBAR_SECTION_GAP, SectionHeader, icon, row_tooltip, style,
};

/// How often a running feed asks its cache whether anything is due. The cache
/// owns freshness (five minutes), so this only has to be shorter than that; a
/// check that finds a fresh list makes no request.
const RECHECK: Duration = Duration::from_secs(120);
/// Side of the up and down controls on a pinned row in Settings.
const MOVE_BUTTON: f32 = 24.0;
/// Repositories the sidebar shows: pins first, then the most recent.
pub(super) const SIDEBAR_REPOS: usize = 3;

/// Whether `gh` is usable, as far as the feed has found out.
#[derive(Debug, Default, Eq, PartialEq)]
enum Reach {
    /// GitHub is off, or has not started.
    #[default]
    Idle,
    /// A probe is out. Saved lists may already be showing.
    Checking,
    Ready {
        login: String,
    },
    /// `gh` can't be used; `missing` hides the section instead of nagging.
    Down {
        missing: bool,
        advice: String,
    },
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) enum Load {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

/// One list's rows and how current they are.
pub(super) struct Slot<T> {
    pub(super) items: Vec<T>,
    /// Unix seconds GitHub last confirmed the rows. `None` until a list
    /// arrives, which is what tells "nothing yet" from "nothing open".
    pub(super) fetched_at: Option<u64>,
    /// Why the rows are older than they should be, when they are.
    note: Option<String>,
    pub(super) load: Load,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            fetched_at: None,
            note: None,
            load: Load::Idle,
        }
    }
}

impl<T> Slot<T> {
    /// Take a finished read. Returns the error when it failed; the rows from
    /// before stay, so a failure never empties a list.
    pub(super) fn apply(&mut self, result: Result<Listing<T>, String>) -> Option<String> {
        match result {
            Ok(listing) => {
                self.items = listing.items;
                self.fetched_at = Some(listing.fetched_at);
                self.note = listing.note;
                self.load = Load::Idle;
                None
            }
            Err(error) => {
                self.load = Load::Failed(error.clone());
                Some(error)
            }
        }
    }

    pub(super) fn saved(&mut self, listing: Option<Listing<T>>) {
        if let Some(listing) = listing {
            self.items = listing.items;
            self.fetched_at = Some(listing.fetched_at);
        }
    }

    /// The line that explains why the rows may be old, if any.
    pub(super) fn caveat(&self) -> Option<&str> {
        match &self.load {
            Load::Failed(error) => Some(error),
            _ => self.note.as_deref(),
        }
    }

    pub(super) fn loading(&self) -> bool {
        self.load == Load::Loading
    }

    /// Nothing has been asked for, or has come back, yet.
    pub(super) fn untouched(&self) -> bool {
        self.load == Load::Idle && self.fetched_at.is_none()
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// A finished request, tagged with what it was for.
enum Msg {
    Probed(AccountState),
    Repos(Result<Listing<RepoSummary>, String>, bool),
    /// A pinned repository the recent list does not reach.
    Extra(String, Result<RepoSummary, String>),
}

/// What folding a message in changed.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Folded {
    pub changed: bool,
    /// The probe found the account; its list should be read now.
    pub ready: bool,
    /// A read the user asked for failed, so say so.
    pub error: Option<String>,
}

pub(super) struct GithubFeed {
    account: Arc<GhAccount>,
    reach: Reach,
    /// The account the rows belong to, from a probe or the saved cache, so a
    /// different login never shows the last one's list.
    login: Option<String>,
    checked_at: Instant,
    /// The account's repositories, most recently pushed first.
    pub(super) repos: Slot<RepoSummary>,
    /// Pinned repositories the list above does not reach, by lowercase name.
    extra: HashMap<String, RepoSummary>,
    /// Names already asked for, so a failing read isn't repeated every tick.
    asked: HashSet<String>,
    collapsed: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl GithubFeed {
    pub(super) fn new(root: &std::path::Path) -> Self {
        let (tx, rx) = channel();
        Self {
            account: Arc::new(GhAccount::new(root)),
            reach: Reach::Idle,
            login: None,
            checked_at: Instant::now(),
            repos: Slot::default(),
            extra: HashMap::new(),
            asked: HashSet::new(),
            collapsed: false,
            tx,
            rx,
        }
    }

    pub(super) fn account(&self) -> Arc<GhAccount> {
        Arc::clone(&self.account)
    }

    pub(super) fn login(&self) -> Option<&str> {
        self.login.as_deref()
    }

    fn running(&self) -> bool {
        self.reach != Reach::Idle
    }

    /// The saved list for the last login, so the sidebar paints before `gh`
    /// has said anything.
    fn show_saved(&mut self) {
        let Some(login) = self.account.saved_login() else {
            return;
        };
        self.repos.saved(self.account.saved_repos(&login));
        self.login = Some(login);
    }

    fn drop_rows(&mut self) {
        self.repos.clear();
        self.extra.clear();
        self.asked.clear();
    }

    /// Fold one finished request in.
    fn fold(&mut self, msg: Msg) -> Folded {
        let mut folded = Folded {
            changed: true,
            ..Folded::default()
        };
        match msg {
            Msg::Probed(AccountState::Ready { login }) => {
                if self.login.as_deref() != Some(login.as_str()) {
                    self.drop_rows();
                }
                self.login = Some(login.clone());
                self.reach = Reach::Ready { login };
                folded.ready = true;
            }
            Msg::Probed(AccountState::Unavailable(state)) => {
                self.reach = Reach::Down {
                    missing: matches!(state, GhState::Missing),
                    advice: state.advice().unwrap_or_default(),
                };
            }
            Msg::Repos(result, forced) => {
                folded.error = self.repos.apply(result).filter(|_| forced);
            }
            Msg::Extra(name, result) => {
                // A failure leaves the name asked, so it isn't retried until
                // the next recheck clears it.
                if let Ok(repo) = result {
                    self.extra.insert(name.to_lowercase(), repo);
                }
            }
        }
        folded
    }

    /// `name` (`owner/name`, any case) from the recent list, else from the
    /// pinned ones fetched by name.
    pub(super) fn find(&self, name: &str) -> Option<&RepoSummary> {
        self.repos
            .items
            .iter()
            .find(|repo| repo.name.eq_ignore_ascii_case(name))
            .or_else(|| self.extra.get(&name.to_lowercase()))
    }

    /// Pinned names nothing has been asked about and the list does not hold.
    fn missing_pins(&self, pinned: &[String]) -> Vec<String> {
        pinned
            .iter()
            .filter(|name| self.find(name).is_none() && !self.asked.contains(&name.to_lowercase()))
            .cloned()
            .collect()
    }

    /// The repositories the sidebar shows: every pin that has loaded, in the
    /// order pinned, then the most recently pushed to up to `limit`. Archived
    /// repositories are not "recent"; they only show when pinned.
    pub(super) fn sidebar_repos(&self, pinned: &[String], limit: usize) -> Vec<&RepoSummary> {
        let mut shown: Vec<&RepoSummary> =
            pinned.iter().filter_map(|name| self.find(name)).collect();
        for repo in &self.repos.items {
            if shown.len() >= limit {
                break;
            }
            if !repo.archived && !shown.iter().any(|known| known.name == repo.name) {
                shown.push(repo);
            }
        }
        shown
    }

    /// What the sidebar should draw for GitHub right now.
    fn showing(&self) -> Showing<'_> {
        match &self.reach {
            Reach::Idle => Showing::Nothing,
            Reach::Down { missing: true, .. } => Showing::Nothing,
            Reach::Down { advice, .. } => Showing::Prompt(advice),
            // While the probe is out, only saved rows are worth drawing; with
            // none, a skeleton would flash for someone who has no `gh`.
            Reach::Checking if self.login.is_none() => Showing::Nothing,
            Reach::Checking | Reach::Ready { .. } => Showing::List,
        }
    }
}

enum Showing<'a> {
    Nothing,
    Prompt(&'a str),
    List,
}

/// How long ago `timestamp` was, in the sidebar's compact words.
pub(super) fn age(timestamp: &str) -> Option<String> {
    Some(age_label(unix_now(), unix_seconds(timestamp)?))
}

/// The name a row shows: the account's own repositories drop the owner they
/// all share.
pub(super) fn display_name<'a>(repo: &'a RepoSummary, login: &str) -> &'a str {
    match repo.name.split_once('/') {
        Some((owner, name)) if owner.eq_ignore_ascii_case(login) => name,
        _ => &repo.name,
    }
}

/// A repository's one-line facts, for tooltips and the list.
pub(super) fn repo_facts(repo: &RepoSummary) -> String {
    let mut facts = Vec::new();
    if repo.private {
        facts.push("Private".to_string());
    }
    if repo.fork {
        facts.push("Fork".to_string());
    }
    if repo.archived {
        facts.push("Archived".to_string());
    }
    if !repo.language.is_empty() {
        facts.push(repo.language.clone());
    }
    if let Some(pushed) = age(&repo.pushed_at) {
        facts.push(format!("pushed {pushed} ago"));
    }
    facts.join(" · ")
}

impl PilusView {
    /// Drive the feed: start it, fold answers, and ask again when due.
    /// Returns whether anything changed.
    pub(super) fn github_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.github.rx.try_recv() {
            let folded = self.github.fold(msg);
            changed |= folded.changed;
            if folded.ready {
                self.github_refresh(false);
            }
            if let Some(error) = folded.error {
                self.toast(ToastKind::Error, format!("GitHub: {error}"));
            }
        }

        if !self.settings.github_shows_repositories() {
            if self.github.running() {
                self.github.reach = Reach::Idle;
                changed = true;
            }
            return changed;
        }
        if !self.github.running() {
            self.github.show_saved();
            self.github_probe();
            return true;
        }

        if self.github.checked_at.elapsed() >= RECHECK {
            match self.github.reach {
                Reach::Ready { .. } => {
                    self.github.checked_at = Instant::now();
                    self.github.asked.clear();
                    self.github_refresh(false);
                }
                // Signed out last time; the user may have signed in since.
                Reach::Down { .. } => self.github_probe(),
                _ => {}
            }
        }
        if matches!(self.github.reach, Reach::Ready { .. }) {
            if self.github.repos.untouched() {
                self.github_refresh(false);
            }
            self.github_fetch_pins();
        }
        changed
    }

    fn github_probe(&mut self) {
        self.github.reach = Reach::Checking;
        self.github.checked_at = Instant::now();
        let account = self.github.account();
        let tx = self.github.tx.clone();
        std::thread::spawn(move || {
            tx.send(Msg::Probed(account.probe())).ok();
        });
    }

    /// Read the repository list. `force` is a person asking, so it skips the
    /// cache's lifetime but not its rate-limit back-off.
    pub(super) fn github_refresh(&mut self, force: bool) {
        let Reach::Ready { login } = &self.github.reach else {
            return;
        };
        if self.github.repos.loading() {
            return;
        }
        let login = login.clone();
        self.github.repos.load = Load::Loading;
        let account = self.github.account();
        let tx = self.github.tx.clone();
        std::thread::spawn(move || {
            tx.send(Msg::Repos(account.repos(&login, force), force))
                .ok();
        });
    }

    /// Fetch the pinned repositories the recent list does not hold, once the
    /// list has said which those are.
    fn github_fetch_pins(&mut self) {
        if self.github.repos.fetched_at.is_none() {
            return;
        }
        for name in self.github.missing_pins(&self.settings.github_pinned) {
            self.github.asked.insert(name.to_lowercase());
            let account = self.github.account();
            let tx = self.github.tx.clone();
            std::thread::spawn(move || {
                let result = account.repo_summary(&name, false);
                tx.send(Msg::Extra(name, result)).ok();
            });
        }
    }

    /// The Repositories section, ready to sit above WORKSPACES.
    pub(super) fn github_sidebar(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.settings.github_shows_repositories() {
            return None;
        }
        match self.github.showing() {
            Showing::Nothing => None,
            Showing::Prompt(advice) => Some(self.github_prompt(advice, cx)),
            Showing::List => Some(self.github_repositories(cx)),
        }
    }

    fn github_header(&self, label: &'static str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(SIDEBAR_SECTION_GAP))
            .child(self.section_header(
                SectionHeader {
                    id: "pilus-github-toggle",
                    label,
                    collapsed: self.github.collapsed,
                    toggle: Box::new(|this, cx| {
                        this.github.collapsed = !this.github.collapsed;
                        cx.notify();
                    }),
                    menu: true,
                },
                cx,
            ))
    }

    /// `gh` is installed but can't be used (not signed in): one section that
    /// says what to do.
    fn github_prompt(&self, advice: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let section = self.github_header("GITHUB", cx);
        if self.github.collapsed {
            return section;
        }
        // The advice is the point, so it wraps instead of truncating.
        section.child(style::enter(
            "pilus-github-prompt",
            div()
                .px(px(SIDEBAR_ROW_PAD))
                .py_1()
                .text_size(px(self.footer_font()))
                .text_color(theme.muted)
                .child(advice.to_string()),
        ))
    }

    fn github_repositories(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let section = self.github_header("REPOSITORIES", cx);
        if self.github.collapsed {
            return section;
        }
        let login = self.github.login().unwrap_or_default();
        let pinned = &self.settings.github_pinned;
        let shown = self.github.sidebar_repos(pinned, SIDEBAR_REPOS);

        let mut children = div().flex().flex_col().gap_1();
        if shown.is_empty() {
            let slot = &self.github.repos;
            children = children.child(match (slot.fetched_at, slot.caveat()) {
                (None, None) => {
                    self.sidebar_skeleton("pilus-github-skeleton", &[0.5, 0.62, 0.42, 0.56])
                }
                (None, Some(error)) => self.sidebar_note(error.to_string(), theme.muted),
                (Some(_), _) => self.sidebar_note("No repositories yet".to_string(), theme.muted),
            });
            return section.child(style::enter("pilus-github-rows", children));
        }

        for repo in shown {
            children = children.child(self.github_row(repo, login, cx));
        }
        if let Some(caveat) = self.github.repos.caveat() {
            children = children.child(self.sidebar_note(caveat.to_string(), theme.muted));
        }
        section.child(style::enter("pilus-github-rows", children))
    }

    /// Run one row of the section header's menu.
    pub(super) fn github_menu_item(&mut self, item: SidebarItem, cx: &mut Context<Self>) {
        match item {
            SidebarItem::GithubAll => self.github_open(View::List, true, cx),
            SidebarItem::GithubRefresh => {
                self.github.asked.clear();
                self.github_refresh(true);
                self.toast(ToastKind::Info, "Refreshing GitHub…");
            }
            SidebarItem::GithubHideSection => {
                self.settings.github_repositories = false;
                self.save_settings();
            }
            SidebarItem::GithubDisable => {
                self.settings.github = false;
                self.save_settings();
            }
            _ => {}
        }
        cx.notify();
    }

    /// A muted, truncated line.
    pub(super) fn sidebar_note(&self, text: String, color: gpui::Rgba) -> Div {
        div()
            .flex()
            .items_center()
            .h(px(SIDEBAR_BAND_HEIGHT))
            .pl(px(SIDEBAR_ROW_PAD))
            .pr(px(SIDEBAR_ROW_PAD))
            .min_w_0()
            .overflow_hidden()
            .text_size(px(self.footer_font()))
            .text_color(color)
            .child(div().min_w_0().truncate().child(text))
    }

    fn github_row(
        &self,
        repo: &RepoSummary,
        login: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let mut lines = Vec::new();
        if !repo.description.is_empty() {
            lines.push((repo.description.clone(), theme.muted));
        }
        lines.push((
            format!(
                "★ {} · {} open issues · {} open pull requests",
                repo.stars, repo.open_issues, repo.open_pulls
            ),
            theme.muted,
        ));
        let facts = repo_facts(repo);
        if !facts.is_empty() {
            lines.push((facts, theme.muted));
        }
        let tip = row_tooltip(repo.name.clone(), theme.panel_fg, lines, theme);

        let name = repo.name.clone();
        let small = self.footer_font();
        let stat = |glyph: &'static [u8], value: u64| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(3.0))
                .text_size(px(small))
                .text_color(theme.muted)
                .child(icon(glyph, small, theme.muted))
                .child(value.to_string())
        };
        div()
            .id(format!("pilus-gh-repo-{}", repo.name))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(SIDEBAR_ROW_PAD))
            .pr(px(SIDEBAR_ROW_PAD))
            .radius_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            // The mark sits where an agent's status dot does, but square.
            .child(
                div()
                    .w(px(SIDEBAR_GLYPH))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().w(px(8.0)).h(px(8.0)).bg(theme.muted)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.panel_fg)
                    .child(display_name(repo, login).to_string()),
            )
            // Stars and open pull requests ride the name's line; the name
            // gives way first.
            .child(stat(
                include_bytes!("../../assets/icons/star.svg"),
                repo.stars,
            ))
            .child(stat(
                include_bytes!("../../assets/icons/git-pull-request.svg"),
                repo.open_pulls,
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.github_open(View::repo(&name), true, cx);
                }),
            )
    }

    /// The GITHUB card of the Settings page: the switches, the pins, and what
    /// the app has found out about `gh`.
    pub(super) fn github_settings_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let master = self.settings.github;
        let status = match &self.github.reach {
            Reach::Ready { login } => format!("Signed in to GitHub as {login}."),
            Reach::Down { missing: true, .. } => {
                "The GitHub CLI (gh) isn't installed on this Mac. Install it from cli.github.com, then run `gh auth login`."
                    .to_string()
            }
            Reach::Down { advice, .. } => advice.clone(),
            Reach::Checking => "Checking the GitHub CLI…".to_string(),
            Reach::Idle if master => String::new(),
            Reach::Idle => "GitHub is off. Nothing is requested from it.".to_string(),
        };
        let row = |label: &'static str, hint: String, toggle: gpui::Stateful<Div>, dim: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .when(dim, |row| row.opacity(style::DISABLED_OPACITY))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(self.chrome_font()))
                                .text_color(theme.panel_fg)
                                .child(label),
                        )
                        .when(!hint.is_empty(), |column| {
                            column.child(
                                div()
                                    .text_size(px(self.footer_font()))
                                    .text_color(theme.muted)
                                    .child(hint),
                            )
                        }),
                )
                .child(toggle)
        };

        let mut block = div().flex().flex_col().gap_3().child(row(
            "Show GitHub in the sidebar",
            "Reads your repositories through the GitHub CLI on this Mac. Results are cached for five minutes, revalidated cheaply after that, and never requested while this is off."
                .to_string(),
            style::toggle("pilus-github-toggle-master", master, theme).on_click(cx.listener(
                |this, _, _, cx| {
                    this.settings.github = !this.settings.github;
                    this.save_settings();
                    cx.notify();
                },
            )),
            false,
        ));
        if !status.is_empty() {
            block = block.child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child(status),
            );
        }
        block
            .child(row(
                "Repositories",
                format!(
                    "The {SIDEBAR_REPOS} repositories shown above Workspaces: your pins first, then the most recently pushed to."
                ),
                style::toggle(
                    "pilus-github-toggle-repos",
                    self.settings.github_repositories,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings.github_repositories = !this.settings.github_repositories;
                    this.save_settings();
                    cx.notify();
                })),
                !master,
            ))
            .child(self.github_pins_block(cx))
    }

    /// The pinned repositories, each with an Unpin, and a few recent ones to
    /// pin. A repository's own page pins too.
    fn github_pins_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let pinned = &self.settings.github_pinned;
        let line = |id: String,
                    name: String,
                    label: &'static str,
                    on: bool,
                    lead: Vec<gpui::AnyElement>| {
            let target = name.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(self.chrome_font()))
                        .text_color(theme.panel_fg)
                        .child(name),
                )
                .children(lead)
                .child(
                    style::small_button(
                        id,
                        label,
                        style::ButtonKind::Secondary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.set_pinned(&target, on);
                        this.save_settings();
                        cx.notify();
                    })),
                )
        };

        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .child(style::section_label(theme, "PINNED", fonts.label));
        if pinned.is_empty() {
            block = block.child(
                div()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("Nothing pinned. Pin a repository to keep it in the sidebar."),
            );
        }
        // The sidebar lists pins in this order, so each row can move one
        // place; an end keeps a blank slot so the Unpin buttons stay aligned.
        let last = pinned.len().saturating_sub(1);
        for (position, name) in pinned.iter().enumerate() {
            let mover = |earlier: bool, glyph: &'static [u8], tip: &'static str, can: bool| {
                let slot = div().w(px(MOVE_BUTTON)).h(px(MOVE_BUTTON)).flex_shrink_0();
                if !can {
                    return slot.into_any_element();
                }
                let target = name.clone();
                style::icon_button(
                    format!("pilus-github-move-{earlier}-{name}"),
                    glyph,
                    style::ButtonKind::Ghost,
                    theme,
                    MOVE_BUTTON,
                )
                .tooltip(row_tooltip(tip.to_string(), theme.panel_fg, Vec::new(), theme))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings.move_pinned(&target, earlier);
                    this.save_settings();
                    cx.notify();
                }))
                .into_any_element()
            };
            block = block.child(
                line(
                    format!("pilus-github-unpin-{name}"),
                    name.clone(),
                    "Unpin",
                    false,
                    vec![
                        mover(
                            true,
                            include_bytes!("../../assets/icons/arrow-up.svg"),
                            "Move up",
                            position > 0,
                        ),
                        mover(
                            false,
                            include_bytes!("../../assets/icons/arrow-down.svg"),
                            "Move down",
                            position < last,
                        ),
                    ],
                ),
            );
        }
        let suggestions: Vec<&RepoSummary> = self
            .github
            .repos
            .items
            .iter()
            .filter(|repo| !self.settings.is_pinned(&repo.name))
            .take(8)
            .collect();
        if !suggestions.is_empty() {
            block = block.child(div().pt_2().child(style::section_label(
                theme,
                "RECENT",
                fonts.label,
            )));
            for repo in suggestions {
                block = block.child(line(
                    format!("pilus-github-pin-{}", repo.name),
                    repo.name.clone(),
                    "Pin",
                    true,
                    Vec::new(),
                ));
            }
        }
        block
    }
}

#[cfg(feature = "visual-test")]
impl GithubFeed {
    /// Rows for the visual captures, so a shot never reaches GitHub.
    pub(super) fn fill_sample(&mut self) {
        let repo = |name: &str,
                    private: bool,
                    language: &str,
                    about: &str,
                    stars: u64,
                    issues: u64,
                    pulls: u64| RepoSummary {
            name: name.to_string(),
            description: about.to_string(),
            private,
            fork: false,
            archived: false,
            language: language.to_string(),
            stars,
            open_issues: issues,
            open_pulls: pulls,
            pushed_at: "2026-10-15T09:00:00Z".to_string(),
            url: format!("https://github.com/{name}"),
            default_branch: "main".to_string(),
        };
        self.login = Some("rizaldy".to_string());
        self.reach = Reach::Ready {
            login: "rizaldy".to_string(),
        };
        self.repos.items = vec![
            repo(
                "rizaldy/pilus",
                false,
                "Rust",
                "Native client for a Luvus daemon",
                412,
                9,
                3,
            ),
            repo(
                "rizaldy/dotfiles",
                true,
                "Shell",
                "Private configuration",
                0,
                0,
                1,
            ),
            repo(
                "kepelet/luvus",
                false,
                "Rust",
                "A terminal workspace daemon",
                1280,
                44,
                12,
            ),
            repo(
                "kepelet/site",
                false,
                "TypeScript",
                "The marketing site",
                7,
                2,
                0,
            ),
            repo("kepelet/docs", false, "MDX", "Documentation", 31, 6, 1),
            repo("rizaldy/notes", true, "Markdown", "", 0, 0, 0),
            repo(
                "kepelet/infra",
                true,
                "HCL",
                "Infrastructure as code",
                2,
                11,
                4,
            ),
        ];
        self.repos.fetched_at = Some(unix_now());
    }

    pub(super) fn fill_down(&mut self, missing: bool) {
        self.reach = Reach::Down {
            missing,
            advice: GhState::SignedOut("You are not logged into any GitHub hosts.".to_string())
                .advice()
                .unwrap_or_default(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> GithubFeed {
        let dir = std::env::temp_dir().join(format!("pilus-feed-{}", std::process::id()));
        GithubFeed::new(&dir)
    }

    fn repo(name: &str) -> RepoSummary {
        RepoSummary {
            name: name.to_string(),
            description: String::new(),
            private: false,
            fork: false,
            archived: false,
            language: String::new(),
            stars: 0,
            open_issues: 0,
            open_pulls: 0,
            pushed_at: String::new(),
            url: String::new(),
            default_branch: "main".to_string(),
        }
    }

    fn listing(names: &[&str]) -> Result<Listing<RepoSummary>, String> {
        Ok(Listing {
            items: names.iter().map(|name| repo(name)).collect(),
            fetched_at: 100,
            note: None,
        })
    }

    fn names<'a>(shown: &[&'a RepoSummary]) -> Vec<&'a str> {
        shown.iter().map(|repo| repo.name.as_str()).collect()
    }

    fn pins(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn with_no_pins_the_sidebar_shows_the_five_most_recent() {
        let mut feed = feed();
        feed.fold(Msg::Repos(
            listing(&["a/1", "a/2", "a/3", "a/4", "a/5", "a/6"]),
            false,
        ));
        assert_eq!(
            names(&feed.sidebar_repos(&[], 5)),
            ["a/1", "a/2", "a/3", "a/4", "a/5"]
        );
    }

    #[test]
    fn four_pins_make_four_pins_and_the_most_recent_one() {
        let mut feed = feed();
        feed.fold(Msg::Repos(
            listing(&["a/1", "a/2", "a/3", "a/4", "a/5", "a/6"]),
            false,
        ));
        let pinned = pins(&["a/6", "a/4", "a/3", "A/5"]);
        assert_eq!(
            names(&feed.sidebar_repos(&pinned, 5)),
            ["a/6", "a/4", "a/3", "a/5", "a/1"],
            "pins in pinned order (any case), then the latest not pinned"
        );
    }

    #[test]
    fn five_or_more_pins_crowd_out_the_recent_ones() {
        let mut feed = feed();
        feed.fold(Msg::Repos(
            listing(&["a/1", "a/2", "a/3", "a/4", "a/5", "a/6"]),
            false,
        ));
        let pinned = pins(&["a/6", "a/5", "a/4", "a/3", "a/2", "a/1"]);
        assert_eq!(feed.sidebar_repos(&pinned, 5).len(), 6, "every pin shows");
    }

    #[test]
    fn a_pin_outside_the_list_shows_once_it_is_fetched() {
        let mut feed = feed();
        feed.fold(Msg::Repos(listing(&["a/1", "a/2"]), false));
        let pinned = pins(&["z/old"]);
        assert_eq!(feed.missing_pins(&pinned), ["z/old"]);
        assert_eq!(names(&feed.sidebar_repos(&pinned, 5)), ["a/1", "a/2"]);

        feed.asked.insert("z/old".to_string());
        assert!(feed.missing_pins(&pinned).is_empty(), "asked once");
        feed.fold(Msg::Extra("z/old".to_string(), Ok(repo("z/old"))));
        assert_eq!(
            names(&feed.sidebar_repos(&pinned, 5)),
            ["z/old", "a/1", "a/2"]
        );
        assert!(feed.find("Z/OLD").is_some());
    }

    #[test]
    fn archived_repositories_are_not_recent_but_can_be_pinned() {
        let mut feed = feed();
        feed.fold(Msg::Repos(listing(&["a/1", "a/2"]), false));
        feed.repos.items[0].archived = true;
        assert_eq!(names(&feed.sidebar_repos(&[], 5)), ["a/2"]);
        assert_eq!(
            names(&feed.sidebar_repos(&pins(&["a/1"]), 5)),
            ["a/1", "a/2"]
        );
    }

    #[test]
    fn a_missing_gh_draws_nothing_and_a_signed_out_one_draws_a_prompt() {
        let mut feed = feed();
        assert!(matches!(feed.showing(), Showing::Nothing));

        feed.reach = Reach::Checking;
        assert!(
            matches!(feed.showing(), Showing::Nothing),
            "no saved login, so no skeleton while probing"
        );

        feed.fold(Msg::Probed(AccountState::Unavailable(GhState::Missing)));
        assert!(matches!(feed.showing(), Showing::Nothing));

        feed.fold(Msg::Probed(AccountState::Unavailable(GhState::SignedOut(
            String::new(),
        ))));
        assert!(matches!(feed.showing(), Showing::Prompt(text) if text.contains("gh auth login")));
    }

    #[test]
    fn saved_rows_show_while_the_probe_is_out_and_a_new_login_drops_them() {
        let mut feed = feed();
        feed.login = Some("me".to_string());
        feed.reach = Reach::Checking;
        feed.repos.items = vec![repo("me/app")];
        assert!(matches!(feed.showing(), Showing::List));

        let same = feed.fold(Msg::Probed(AccountState::Ready {
            login: "me".to_string(),
        }));
        assert!(same.ready);
        assert_eq!(feed.repos.items.len(), 1, "same account keeps its rows");

        feed.fold(Msg::Probed(AccountState::Ready {
            login: "other".to_string(),
        }));
        assert!(
            feed.repos.items.is_empty(),
            "another account never sees them"
        );
    }

    #[test]
    fn a_failed_read_keeps_the_rows_and_only_a_forced_one_reports() {
        let mut feed = feed();
        feed.fold(Msg::Repos(listing(&["a/1"]), false));
        assert_eq!(feed.repos.fetched_at, Some(100));

        let quiet = feed.fold(Msg::Repos(Err("rate limited".into()), false));
        assert_eq!(quiet.error, None, "a background failure doesn't toast");
        assert_eq!(feed.repos.items.len(), 1);
        assert_eq!(feed.repos.caveat(), Some("rate limited"));

        let loud = feed.fold(Msg::Repos(Err("rate limited".into()), true));
        assert_eq!(loud.error.as_deref(), Some("rate limited"));

        feed.fold(Msg::Repos(listing(&["a/1"]), false));
        assert_eq!(feed.repos.caveat(), None, "a good read clears the caveat");
    }

    #[test]
    fn a_list_is_untouched_until_something_arrives_or_fails() {
        let mut feed = feed();
        assert!(feed.repos.untouched());
        feed.repos.load = Load::Loading;
        assert!(feed.repos.loading());
        assert!(!feed.repos.untouched());
        feed.fold(Msg::Repos(Err("x".into()), false));
        assert!(!feed.repos.untouched(), "no retry storm");
    }

    #[test]
    fn a_row_drops_the_accounts_own_owner() {
        let own = repo("me/app");
        assert_eq!(display_name(&own, "Me"), "app");
        assert_eq!(display_name(&own, "someone"), "me/app");
    }
}
