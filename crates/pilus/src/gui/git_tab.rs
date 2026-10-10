//! The native Git tab: the active workspace's commits above, and below them
//! what the picked commit changed.
//!
//! The daemon runs Git for the history (`git.status` and `git.log` over UHP)
//! and answers for whichever workspace it has active, so the tab follows the
//! sidebar's selection rather than carrying a workspace of its own. It has no
//! call for what a commit changed, so a click asks Git itself through
//! `pilus_client::show_commit`, off the GUI thread, for the repository the
//! daemon reports. What is changed but not yet committed is the Diff tab's to
//! show. Both reads are bounded and read-only: the tab never writes to the
//! checkout.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Div, FontWeight, ListAlignment, ListState, MouseButton, MouseDownEvent,
    SharedString, Stateful, div, list, prelude::*, px, relative,
};
use pilus_client::{
    CommitDetail, CommitRefKind, DiffLayer, Error, GitCommit, GitStatus, SshTarget, show_commit,
};

use super::diff::{split_path, status_glyph};
use super::diff_model::{self as model, Src, SrcKind};
use super::highlight;
use super::style::{self, ButtonKind, FillExt};
use super::{EndpointId, PilusView, icon};

/// How often the open tab asks the daemon for a fresh answer.
const POLL: Duration = Duration::from_millis(2000);
/// Commits the first read asks for, and each "Show more" adds.
const PAGE: u32 = 50;
/// Branch and tag labels shown on one commit before the rest fold into `+n`.
const MAX_REFS: usize = 2;
/// Widest a label grows before its name is cut off.
const REF_MAX_WIDTH: f32 = 128.0;
/// Height the commit list starts at above the detail, before the user drags.
const DEFAULT_LIST_HEIGHT: f32 = 240.0;
/// The strip between the list and the detail that a drag grabs.
const DIVIDER_HEIGHT: f32 = 7.0;
/// Most of the tab the commit list may take before the detail is squeezed.
const MAX_LIST_SHARE: f32 = 0.7;

/// What a background read reports back to the tab.
pub(super) enum GitMsg {
    Snapshot {
        endpoint: EndpointId,
        result: Result<(GitStatus, Vec<GitCommit>), String>,
    },
    Commit {
        endpoint: EndpointId,
        sha: String,
        result: Result<CommitDetail, String>,
    },
}

/// A commit's detail pane, from the click to the laid-out patch.
enum Detail {
    Loading,
    Failed(String),
    Ready(Box<Opened>),
}

/// A loaded commit and the rows its patch lays out as.
struct Opened {
    detail: CommitDetail,
    rows: Vec<DetailRow>,
}

/// One row of the detail list. The patch is flattened so the list can
/// virtualize it, as the Diff's is.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DetailRow {
    /// The message and totals.
    Head,
    /// Index into [`CommitDetail::files`].
    File(usize),
    Hunk(String),
    Line(Src),
    /// A line of explanation: a binary file, a dropped patch.
    Note(String),
}

/// The tab's own state.
pub(super) struct GitPanel {
    endpoint: Option<EndpointId>,
    status: Option<GitStatus>,
    commits: Vec<GitCommit>,
    /// How many commits to ask for; it grows with "Show more".
    limit: u32,
    /// The commit whose changes the detail pane shows, by the hash the list
    /// shows.
    picked: Option<String>,
    detail: Option<Detail>,
    /// Files of the open commit folded down to their header, by index into
    /// [`CommitDetail::files`]. A new commit opens with every file expanded.
    collapsed: HashSet<usize>,
    /// The file whose chevron was just turned, so only it animates.
    toggled: Option<usize>,
    list: ListState,
    /// Height of the commit list above the detail, in logical pixels.
    list_h: f32,
    /// A divider drag in progress: pointer y and list height at its start.
    divider: Option<(f32, f32)>,
    error: Option<String>,
    polling: bool,
    last_poll: Option<Instant>,
    tx: Sender<GitMsg>,
    rx: Receiver<GitMsg>,
}

impl GitPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            endpoint: None,
            status: None,
            commits: Vec::new(),
            limit: PAGE,
            picked: None,
            detail: None,
            collapsed: HashSet::new(),
            toggled: None,
            list: ListState::new(0, ListAlignment::Top, px(600.0)),
            list_h: DEFAULT_LIST_HEIGHT,
            divider: None,
            error: None,
            polling: false,
            last_poll: None,
            tx,
            rx,
        }
    }

    /// Start the commit list at a saved height; anything too small to use (an
    /// unset 0) keeps the default.
    pub(super) fn set_list_height(&mut self, height: f32) {
        if height >= model::MIN_LIST_HEIGHT {
            self.list_h = height;
        }
    }

    /// Ask for a fresh read the next time the tab polls, such as when it
    /// comes back on screen.
    pub(super) fn mark_stale(&mut self) {
        self.last_poll = None;
    }

    /// Drop everything read from the daemon: another machine answers for
    /// other repositories.
    fn reset(&mut self) {
        self.status = None;
        self.commits.clear();
        self.limit = PAGE;
        self.picked = None;
        self.detail = None;
        self.collapsed.clear();
        self.toggled = None;
        self.error = None;
        self.last_poll = None;
    }

    /// Whether the last read filled the page, so older commits may exist.
    fn may_have_more(&self) -> bool {
        self.commits.len() >= self.limit as usize
    }

    /// Fold a finished read in; true when what is drawn changed.
    fn apply(&mut self, result: Result<(GitStatus, Vec<GitCommit>), String>) -> bool {
        match result {
            Ok((status, commits)) => {
                let mut changed = self.error.take().is_some()
                    || self.status.as_ref() != Some(&status)
                    || self.commits != commits;
                self.status = Some(status);
                self.commits = commits;
                // Another workspace's history has no such commit: drop the
                // patch rather than show one from elsewhere.
                if let Some(sha) = &self.picked
                    && !self.commits.iter().any(|commit| commit.sha == *sha)
                {
                    self.picked = None;
                    self.detail = None;
                    changed = true;
                }
                changed
            }
            Err(error) => {
                let changed = self.error.as_ref() != Some(&error);
                self.error = Some(error);
                changed
            }
        }
    }

    /// Show a commit's patch once it arrives; true when it was still wanted.
    fn open_commit(&mut self, sha: &str, result: Result<CommitDetail, String>) -> bool {
        if self.picked.as_deref() != Some(sha) {
            return false;
        }
        self.detail = Some(match result {
            Ok(detail) => {
                let rows = detail_rows(&detail, &self.collapsed);
                self.list.reset(rows.len());
                Detail::Ready(Box::new(Opened { detail, rows }))
            }
            Err(error) => Detail::Failed(error),
        });
        true
    }

    /// Fold a file down to its header, or open it again, and re-lay the rows
    /// out. The list keeps its scroll position by row, so a file turned
    /// over under the pointer does not move what is above it.
    fn toggle_file(&mut self, index: usize) {
        if !self.collapsed.remove(&index) {
            self.collapsed.insert(index);
        }
        self.toggled = Some(index);
        if let Some(Detail::Ready(opened)) = &mut self.detail {
            let rows = detail_rows(&opened.detail, &self.collapsed);
            self.list.splice(0..opened.rows.len(), rows.len());
            opened.rows = rows;
        }
    }
}

/// A commit laid out as list rows: the message, then each file's header,
/// hunks, and lines. A collapsed file keeps only its header.
fn detail_rows(detail: &CommitDetail, collapsed: &HashSet<usize>) -> Vec<DetailRow> {
    let mut rows = vec![DetailRow::Head];
    for (index, file) in detail.files.iter().enumerate() {
        rows.push(DetailRow::File(index));
        if collapsed.contains(&index) {
            continue;
        }
        if file.binary {
            rows.push(DetailRow::Note("Binary file".to_string()));
        } else if file.hunks.is_empty() {
            rows.push(DetailRow::Note("No textual changes".to_string()));
        }
        let mut colored = highlight::hunks(&file.path, &file.hunks).into_iter();
        for hunk in &file.hunks {
            rows.push(DetailRow::Hunk(hunk.header.clone()));
            let mut spans = colored.next().unwrap_or_default().into_iter();
            rows.extend(hunk.lines.iter().map(|line| {
                DetailRow::Line(Src {
                    kind: match line.kind {
                        pilus_client::DiffLineKind::Context => SrcKind::Context,
                        pilus_client::DiffLineKind::Addition => SrcKind::Addition,
                        pilus_client::DiffLineKind::Deletion => SrcKind::Deletion,
                    },
                    old_line: line.old_line,
                    new_line: line.new_line,
                    // Tabs would collapse to one cell in a proportional layout.
                    text: line.text.replace('\t', "    "),
                    spans: spans.next().unwrap_or_default(),
                })
            }));
        }
    }
    if detail.truncated {
        rows.push(DetailRow::Note(
            "This commit is larger than the tab reads; the rest is not shown".to_string(),
        ));
    }
    rows
}

impl PilusView {
    /// Poll the daemon and fold finished reads into the tab. Returns whether
    /// anything changed.
    pub(super) fn git_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.git_panel.rx.try_recv() {
            let current = self.git_panel.endpoint.clone();
            match message {
                GitMsg::Snapshot { endpoint, result } => {
                    self.git_panel.polling = false;
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    changed |= self.git_panel.apply(result);
                    changed |= self.git_open_newest();
                }
                GitMsg::Commit {
                    endpoint,
                    sha,
                    result,
                } => {
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    changed |= self.git_panel.open_commit(&sha, result);
                }
            }
        }
        if !self.git_visible() {
            return changed;
        }
        if self.git_panel.endpoint.as_ref() != Some(&self.active) {
            self.git_panel.reset();
            self.git_panel.endpoint = Some(self.active.clone());
            changed = true;
        }
        let panel = &mut self.git_panel;
        if panel.polling || panel.last_poll.is_some_and(|at| at.elapsed() < POLL) {
            return changed;
        }
        panel.polling = true;
        panel.last_poll = Some(Instant::now());
        let limit = panel.limit;
        let tx = panel.tx.clone();
        let endpoint = self.active.clone();
        let client = self.control_client_for_endpoint(&endpoint);
        std::thread::spawn(move || {
            let result = match client {
                None => Err("Could not reach the daemon".to_string()),
                Some(mut client) => (|| {
                    let status = client.git_status()?;
                    // A repository with no commits yet has no log to read; the
                    // tab still shows its working tree.
                    let commits = client.git_log(limit).unwrap_or_default();
                    Ok((status, commits))
                })()
                .map_err(|error: Error| error.to_string()),
            };
            tx.send(GitMsg::Snapshot { endpoint, result }).ok();
        });
        changed
    }

    fn git_refresh_now(&mut self, cx: &mut Context<Self>) {
        self.git_panel.mark_stale();
        cx.notify();
    }

    fn git_show_more(&mut self, cx: &mut Context<Self>) {
        self.git_panel.limit = self.git_panel.limit.saturating_add(PAGE);
        self.git_panel.mark_stale();
        cx.notify();
    }

    /// Show `sha`'s changes below the list; true when that was a change.
    fn git_select(&mut self, sha: String) -> bool {
        if self.git_panel.picked.as_ref() == Some(&sha) {
            return false;
        }
        self.git_panel.picked = Some(sha.clone());
        self.git_panel.collapsed.clear();
        self.git_panel.toggled = None;
        self.git_panel.detail = Some(Detail::Loading);
        self.git_load_commit(sha);
        true
    }

    /// With nothing picked, open the newest commit, so the pane below the
    /// list is never empty while there is history to show.
    fn git_open_newest(&mut self) -> bool {
        if self.git_panel.picked.is_some() {
            return false;
        }
        match self
            .git_panel
            .commits
            .first()
            .map(|commit| commit.sha.clone())
        {
            Some(sha) => self.git_select(sha),
            None => false,
        }
    }

    /// Read a commit's patch on a background thread. The repository is
    /// whichever the daemon reports for its active workspace, and Git runs on
    /// the active machine: here, or over its saved SSH target.
    fn git_load_commit(&mut self, sha: String) {
        let endpoint = self.active.clone();
        let tx = self.git_panel.tx.clone();
        let target: Result<Option<SshTarget>, String> = match &self.active_machine {
            None => Ok(None),
            Some(machine) => machine.ssh_target().map(Some).ok_or_else(|| {
                "Commit contents are only available on this Mac and on SSH machines".to_string()
            }),
        };
        let client = self.control_client_for_endpoint(&endpoint);
        std::thread::spawn(move || {
            let result = (|| {
                let target = target?;
                let mut client = client.ok_or("Could not reach the daemon")?;
                let repo = client.diff_list().map_err(|error| error.to_string())?.repo;
                if repo.is_empty() {
                    return Err("The daemon reported no repository".to_string());
                }
                show_commit(target.as_ref(), &repo, &sha)
            })();
            tx.send(GitMsg::Commit {
                endpoint,
                sha,
                result,
            })
            .ok();
        });
    }

    /// Follow a divider drag; called for every pointer move in the window.
    pub(super) fn git_divider_move(&mut self, y: f32, window_h: f32, cx: &mut Context<Self>) {
        let panel = &mut self.git_panel;
        let Some((start_y, start_height)) = panel.divider else {
            return;
        };
        let height = model::clamp_list_height(start_height + (y - start_y), window_h);
        if (height - panel.list_h).abs() >= 0.5 {
            panel.list_h = height;
            cx.notify();
        }
    }

    /// End a divider drag and keep where it landed.
    pub(super) fn git_divider_end(&mut self, cx: &mut Context<Self>) {
        if self.git_panel.divider.take().is_some() {
            self.settings.git_list_height = self.git_panel.list_h;
            self.settings.save(&self.pilus_root);
            cx.notify();
        }
    }
}

// Rendering.
impl PilusView {
    /// The tab: a summary row, the commit list, and what the picked commit
    /// changed.
    pub(super) fn git_page(&mut self, cx: &mut Context<Self>) -> Div {
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.git_header(cx));
        let panel = &self.git_panel;
        // Nothing to draw until the first read lands; a repository that does
        // not exist says why instead.
        if let (None, Some(error)) = (&panel.status, &panel.error) {
            return page.child(self.diff_message(error.clone()));
        }
        let detail = self.git_commit_view(cx);
        let footer = panel
            .error
            .as_ref()
            .filter(|_| panel.status.is_some())
            .map(|error| {
                div()
                    .px_3()
                    .h(px(style::scaled(&self.shared_settings, 28.0)))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(self.theme.border)
                    .text_size(px(style::Fonts::resolve(&self.shared_settings).secondary))
                    .text_color(self.theme.alert)
                    .truncate()
                    .child(error.clone())
            });
        page.child(self.git_history(cx))
            .child(self.git_divider(cx))
            .child(detail)
            .children(footer)
    }

    /// One row: the branch with how far it is from its upstream, and the
    /// refresh control.
    fn git_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let summary = self
            .git_panel
            .status
            .as_ref()
            .map(summary_line)
            .unwrap_or_default();
        let refresh = self
            .diff_icon_button(
                "pilus-git-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.git_refresh_now(cx)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(icon(
                include_bytes!("../../assets/icons/git-branch.svg"),
                fonts.body,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(summary),
            )
            .child(refresh)
    }

    /// The commit list, newest first, scrolling on its own.
    fn git_history(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.git_panel;
        let mut column = div().flex().flex_col().p_2();
        if panel.status.is_none() {
            column = column.children(self.git_skeleton("pilus-git-history-skeleton"));
        } else {
            if panel.commits.is_empty() {
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(style::scaled(&self.shared_settings, 56.0)))
                        .text_size(px(fonts.body))
                        .text_color(theme.muted)
                        .child("No commits yet"),
                );
            } else {
                column = column.child(self.git_group_label("Commits", panel.commits.len()));
                for commit in &panel.commits {
                    column = column.child(self.git_commit_row(commit, cx));
                }
            }
            if panel.may_have_more() {
                column = column.child(
                    div().flex().justify_center().py_2().child(
                        style::small_button(
                            "pilus-git-more",
                            "Show more",
                            ButtonKind::Ghost,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.git_show_more(cx))),
                    ),
                );
            }
        }
        div()
            .id("pilus-git-history")
            .flex_shrink_0()
            .w_full()
            .h(px(panel.list_h))
            .max_h(relative(MAX_LIST_SHARE))
            .overflow_y_scroll()
            .child(column)
    }

    /// The fill a list row rests on, and the one under the pointer: the
    /// picked row is the panel pill, as a sidebar selection is.
    fn pick_fills(&self, picked: bool) -> (gpui::Hsla, gpui::Hsla) {
        let theme = self.theme;
        if picked {
            (theme.panel.into(), theme.panel.into())
        } else {
            (gpui::transparent_black(), theme.panel_dim.into())
        }
    }

    fn git_group_label(&self, label: &str, count: usize) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_row()
            .gap_2()
            .px_2()
            .pt_2()
            .child(style::section_label(
                self.theme,
                label.to_uppercase(),
                fonts.label,
            ))
            .child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(self.theme.muted)
                    .child(count.to_string()),
            )
    }

    /// A commit: its subject and labels over its hash, author, and age.
    fn git_commit_row(&self, commit: &GitCommit, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let picked = self.git_panel.picked.as_ref() == Some(&commit.sha);
        let (rest, hover) = self.pick_fills(picked);
        let mut labels = div().flex().flex_row().gap_1().flex_shrink_0();
        for reference in commit.refs.iter().take(MAX_REFS) {
            let chip = style::chip(theme, reference.name.clone(), fonts.label)
                .max_w(px(REF_MAX_WIDTH))
                .truncate();
            // The checked-out branch reads as the one that matters.
            labels = labels.child(if reference.kind == CommitRefKind::Head {
                chip.text_color(theme.panel_fg)
            } else {
                chip
            });
        }
        if commit.refs.len() > MAX_REFS {
            labels = labels.child(style::chip(
                theme,
                format!("+{}", commit.refs.len() - MAX_REFS),
                fonts.label,
            ));
        }
        let sha = commit.sha.clone();
        div()
            .id(format!("pilus-git-commit-{}", commit.sha))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px_2()
            .py(px(6.0))
            .rounded_md()
            .cursor_pointer()
            .state_fill(rest, hover, style::RADIUS_MD)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(fonts.body))
                            .font_weight(if picked {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(theme.panel_fg)
                            .child(commit.subject.clone()),
                    )
                    .child(labels),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_family(SharedString::from(style::mono_family(
                                &self.shared_settings,
                            )))
                            .child(commit.sha.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .child(byline(commit)),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.git_select(sha.clone()) {
                    cx.notify();
                }
            }))
    }

    /// The handle between the commit list and the detail.
    fn git_divider(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-git-divider")
            .flex()
            .items_center()
            .h(px(DIVIDER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .cursor_row_resize()
            .hover_fill(theme.panel_dim, style::RADIUS_SM)
            .child(div().h(px(1.0)).w_full().bg(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    let panel = &mut this.git_panel;
                    panel.divider = Some((f32::from(event.position.y), panel.list_h));
                    cx.stop_propagation();
                }),
            )
    }

    /// The picked commit in the detail pane: its message, then its patch.
    fn git_commit_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: String| {
            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .child(self.diff_message(text))
                .into_any_element()
        };
        match &self.git_panel.detail {
            None => message("Select a commit to see what it changed".to_string()),
            Some(Detail::Loading) => message("Reading commit…".to_string()),
            Some(Detail::Failed(error)) => message(format!("Could not read commit: {error}")),
            Some(Detail::Ready(_)) => list(
                self.git_panel.list.clone(),
                cx.processor(|this: &mut PilusView, ix: usize, _window, cx| {
                    this.git_detail_row(ix, cx)
                }),
            )
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .into_any_element(),
        }
    }

    /// One virtualized row of the open commit.
    fn git_detail_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Detail::Ready(opened)) = &self.git_panel.detail else {
            return div().into_any_element();
        };
        let Some(row) = opened.rows.get(ix) else {
            return div().into_any_element();
        };
        match row {
            DetailRow::Head => self.git_commit_head(&opened.detail),
            DetailRow::File(index) => match opened.detail.files.get(*index) {
                Some(file) => self.git_file_head(*index, file, cx),
                None => div().into_any_element(),
            },
            DetailRow::Hunk(header) => self.diff_hunk_row(ix, header),
            DetailRow::Line(src) => div()
                .id(("pilus-git-line", ix))
                .flex()
                .flex_row()
                .w_full()
                .child(self.line_number(src.old_line))
                .child(self.line_number(src.new_line))
                .child(self.code_cell(src))
                .into_any_element(),
            DetailRow::Note(text) => div()
                .px_4()
                .py_2()
                .text_size(px(style::Fonts::resolve(&self.shared_settings).label))
                .text_color(self.theme.muted)
                .child(text.clone())
                .into_any_element(),
        }
    }

    /// The commit's subject and body, who wrote it and when, and its totals.
    fn git_commit_head(&self, detail: &CommitDetail) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let (subject, body) = split_message(&detail.message);
        let mut head = div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .px_3()
            .py_2()
            .child(
                div()
                    .text_size(px(fonts.body))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.panel_fg)
                    .child(subject.to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_x_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(
                        div()
                            .font_family(SharedString::from(style::mono_family(
                                &self.shared_settings,
                            )))
                            .child(detail.sha.chars().take(7).collect::<String>()),
                    )
                    .child(
                        [detail.author.as_str(), detail.date.as_str()]
                            .into_iter()
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    ),
            );
        if !body.is_empty() {
            head = head.child(
                div()
                    .pt_1()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.panel_fg)
                    .child(body.to_string()),
            );
        }
        head.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .pt_1()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(files_label(detail.files.len()))
                .child(
                    div()
                        .text_color(theme.ok)
                        .child(format!("+{}", detail.additions())),
                )
                .child(
                    div()
                        .text_color(theme.alert)
                        .child(format!("−{}", detail.deletions())),
                ),
        )
        .into_any_element()
    }

    /// A changed file's strip above its hunks.
    fn git_file_head(
        &self,
        index: usize,
        file: &pilus_client::CommitFile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let (glyph, color) = status_glyph(theme, DiffLayer::Staged, &file.status);
        let (dir, name) = split_path(&file.path);
        let mut title = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .font_family(SharedString::from(style::mono_family(
                &self.shared_settings,
            )))
            .text_size(px(fonts.secondary));
        if file.renamed() {
            title = title.child(
                div()
                    .flex_shrink_0()
                    .text_color(theme.muted)
                    .child(format!("{} → ", file.old_path)),
            );
        }
        if !dir.is_empty() {
            title = title.child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(theme.muted)
                    .child(format!("{dir}/")),
            );
        }
        title = title.child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.panel_fg)
                .child(name.to_string()),
        );
        let mut counts = div().flex().flex_row().gap_2().flex_shrink_0();
        if !file.binary {
            counts = counts
                .child(
                    div()
                        .text_size(px(fonts.secondary))
                        .text_color(theme.ok)
                        .child(format!("+{}", file.additions)),
                )
                .child(
                    div()
                        .text_size(px(fonts.secondary))
                        .text_color(theme.alert)
                        .child(format!("−{}", file.deletions)),
                );
        }
        let open = !self.git_panel.collapsed.contains(&index);
        let animate = self.git_panel.toggled == Some(index);
        div()
            .id(("pilus-git-file", index))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .mt_2()
            .w_full()
            .px_3()
            .h(px(style::scaled(&self.shared_settings, 30.0)))
            .cursor_pointer()
            .state_fill(
                theme.panel_dim,
                theme.panel,
                gpui::AbsoluteLength::Pixels(px(0.0)),
            )
            .child(style::chevron(
                format!("pilus-git-file-chevron-{index}"),
                open,
                animate,
                fonts.secondary,
                theme.muted,
            ))
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 12.0)))
                    .flex_shrink_0()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(glyph.to_string()),
            )
            .child(title)
            .child(counts)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.git_panel.toggle_file(index);
                cx.notify();
            }))
            .into_any_element()
    }

    /// Placeholder rows while the first read is in flight.
    fn git_skeleton(&self, id: &'static str) -> Vec<Div> {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        [0.7, 0.55, 0.8]
            .into_iter()
            .enumerate()
            .map(|(index, fraction)| {
                div()
                    .flex()
                    .items_center()
                    .h(px(style::scaled(&self.shared_settings, 28.0)))
                    .px_2()
                    .child(style::skeleton(
                        format!("{id}-{index}"),
                        self.theme,
                        relative(fraction),
                        fonts.body,
                    ))
            })
            .collect()
    }
}

/// Shot-harness entry point: a canned repository, since the harness runs
/// without a daemon.
#[cfg(feature = "visual-test")]
impl PilusView {
    /// `commit` opens that row of the list; `None` leaves nothing picked.
    /// Fold the sample commit's `index`th file, as a click on its header does.
    pub(super) fn git_toggle_file_sample(&mut self, index: usize, cx: &mut Context<Self>) {
        self.git_panel.toggle_file(index);
        cx.notify();
    }

    pub(super) fn git_show_sample(&mut self, commit: Option<&str>, cx: &mut Context<Self>) {
        use pilus_client::{CommitFile, CommitRef, DiffHunk, DiffLine, DiffLineKind as Kind};

        use super::right_sidebar::SidebarTab;

        let entry =
            |sha: &str, subject: &str, when: &str, refs: &[(&str, CommitRefKind)]| GitCommit {
                sha: sha.to_string(),
                subject: subject.to_string(),
                author: "faultables".to_string(),
                when: when.to_string(),
                refs: refs
                    .iter()
                    .map(|(name, kind)| CommitRef {
                        name: name.to_string(),
                        kind: *kind,
                    })
                    .collect(),
            };
        let status = GitStatus {
            branch: "git-integration".to_string(),
            upstream: Some("origin/git-integration".to_string()),
            ahead: 2,
            behind: 1,
            stashes: 1,
        };
        let commits = vec![
            entry(
                "3e1a128",
                "docs: cover pilus-agent, pilus-mobile, and the hub's model catalog",
                "4 hours ago",
                &[
                    ("git-integration", CommitRefKind::Head),
                    ("origin/main", CommitRefKind::Branch),
                    ("origin/HEAD", CommitRefKind::Branch),
                    ("main", CommitRefKind::Branch),
                ],
            ),
            entry(
                "13eb551",
                "feat(ios): gate the Terminal kind on the connector's pseudo_kinds",
                "5 hours ago",
                &[],
            ),
            entry(
                "260873d",
                "feat(mobile): add pilus-mobile device client for iOS",
                "5 hours ago",
                &[("v0.3.0", CommitRefKind::Tag)],
            ),
            entry(
                "942efbd",
                "feat(agent): catalog agent models, accept model/effort, add terminal pseudo-kind",
                "6 hours ago",
                &[],
            ),
            entry(
                "733fb0c",
                "feat(ios): add the pilus iPhone app",
                "7 hours ago",
                &[],
            ),
        ];

        self.right.visible = true;
        self.right.select_tab(SidebarTab::Git);
        let panel = &mut self.git_panel;
        panel.endpoint = Some(self.active.clone());
        // Hold the poll off: there is no daemon to answer it.
        panel.polling = true;
        panel.error = None;
        panel.status = Some(status);
        panel.commits = commits;
        panel.picked = None;
        panel.detail = None;
        if let Some(sha) = commit {
            let line = |kind, old, new, text: &str| DiffLine {
                kind,
                old_line: old,
                new_line: new,
                text: text.to_string(),
            };
            let file = |path: &str, status: &str, binary, hunks: Vec<DiffHunk>| {
                let count = |kind| {
                    hunks
                        .iter()
                        .flat_map(|hunk| &hunk.lines)
                        .filter(|line| line.kind == kind)
                        .count() as u32
                };
                CommitFile {
                    path: path.to_string(),
                    old_path: path.to_string(),
                    status: status.to_string(),
                    binary,
                    additions: count(Kind::Addition),
                    deletions: count(Kind::Deletion),
                    hunks,
                }
            };
            let detail = CommitDetail {
                sha: format!("{sha}0c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"),
                author: "faultables".to_string(),
                date: "2026-10-09 14:02".to_string(),
                message: "docs: cover pilus-agent, pilus-mobile, and the hub's model catalog\n\nThe hub grew a model catalog and a terminal pseudo-kind; the\narchitecture notes still described the old surface."
                    .to_string(),
                files: vec![
                    file(
                        "docs/architecture.md",
                        "M",
                        false,
                        vec![DiffHunk {
                            header: "@@ -351,6 +351,9 @@ the snapshot carries no session ids."
                                .to_string(),
                            lines: vec![
                                line(
                                    Kind::Context,
                                    Some(351),
                                    Some(351),
                                    "a resumable session has no daemon API.",
                                ),
                                line(
                                    Kind::Deletion,
                                    Some(352),
                                    None,
                                    "**Right-click menus.** WORKSPACES rows carry the",
                                ),
                                line(
                                    Kind::Addition,
                                    None,
                                    Some(352),
                                    "**Hub.** The agent's hub fronts the daemon's gateway and",
                                ),
                                line(
                                    Kind::Addition,
                                    None,
                                    Some(353),
                                    "adds a model catalog, so a phone can pick a model and effort.",
                                ),
                                line(Kind::Context, Some(353), Some(354), ""),
                            ],
                        }],
                    ),
                    file(
                        "crates/pilus-agent/src/hub.rs",
                        "M",
                        false,
                        vec![DiffHunk {
                            header: "@@ -69,3 +69,4 @@ const REFUSED: &[&str] = &[".to_string(),
                            lines: vec![
                                line(
                                    Kind::Context,
                                    Some(69),
                                    Some(69),
                                    "const OPERATE: &[&str] = &[\"task.add\", \"task.start\"];",
                                ),
                                line(
                                    Kind::Addition,
                                    None,
                                    Some(70),
                                    "const PSEUDO_KINDS: &[&str] = &[\"terminal\"];",
                                ),
                                line(Kind::Context, Some(70), Some(71), ""),
                            ],
                        }],
                    ),
                    file("ios/Pilus/Assets/icon.png", "A", true, Vec::new()),
                ],
                truncated: false,
            };
            panel.picked = Some(sha.to_string());
            panel.open_commit(sha, Ok(detail));
        }
        cx.notify();
    }
}

/// `main · ↑2 ↓1 · 1 stash`: the branch, how far it is from its upstream, and
/// the stashes it hides.
fn summary_line(status: &GitStatus) -> String {
    let mut parts = vec![if status.branch.is_empty() {
        "detached HEAD".to_string()
    } else {
        status.branch.clone()
    }];
    let mut distance = Vec::new();
    if status.ahead > 0 {
        distance.push(format!("↑{}", status.ahead));
    }
    if status.behind > 0 {
        distance.push(format!("↓{}", status.behind));
    }
    if !distance.is_empty() {
        parts.push(distance.join(" "));
    }
    match status.stashes {
        0 => {}
        1 => parts.push("1 stash".to_string()),
        n => parts.push(format!("{n} stashes")),
    }
    parts.join(" · ")
}

/// `author · 4 hours ago`, leaving out whichever the daemon did not send.
fn byline(commit: &GitCommit) -> String {
    [commit.author.as_str(), commit.when.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A message's subject line and the body after its blank separator.
fn split_message(message: &str) -> (&str, &str) {
    match message.split_once('\n') {
        Some((subject, body)) => (subject.trim_end(), body.trim()),
        None => (message.trim_end(), ""),
    }
}

fn files_label(count: usize) -> String {
    format!("{count} {}", if count == 1 { "file" } else { "files" })
}

#[cfg(test)]
mod tests {
    use pilus_client::{CommitFile, DiffHunk, DiffLine, DiffLineKind};

    use super::*;

    fn status(branch: &str, ahead: u32, behind: u32, stashes: u32) -> GitStatus {
        GitStatus {
            branch: branch.to_string(),
            ahead,
            behind,
            stashes,
            ..GitStatus::default()
        }
    }

    fn commit(sha: &str) -> GitCommit {
        GitCommit {
            sha: sha.to_string(),
            subject: String::new(),
            author: String::new(),
            when: String::new(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn summary_names_the_branch_and_only_what_differs() {
        assert_eq!(summary_line(&status("main", 0, 0, 0)), "main");
        assert_eq!(summary_line(&status("main", 2, 0, 0)), "main · ↑2");
        assert_eq!(
            summary_line(&status("main", 2, 1, 1)),
            "main · ↑2 ↓1 · 1 stash"
        );
        assert_eq!(
            summary_line(&status("main", 0, 3, 2)),
            "main · ↓3 · 2 stashes"
        );
        assert_eq!(summary_line(&status("", 0, 0, 0)), "detached HEAD");
    }

    #[test]
    fn byline_skips_missing_parts() {
        let by = |author: &str, when: &str| GitCommit {
            author: author.to_string(),
            when: when.to_string(),
            ..commit("abc")
        };
        assert_eq!(byline(&by("ada", "2 days ago")), "ada · 2 days ago");
        assert_eq!(byline(&by("", "2 days ago")), "2 days ago");
        assert_eq!(byline(&by("", "")), "");
    }

    #[test]
    fn message_splits_into_subject_and_body() {
        assert_eq!(split_message("fix: x"), ("fix: x", ""));
        assert_eq!(
            split_message("fix: x\n\nwhy\nmore\n"),
            ("fix: x", "why\nmore")
        );
        assert_eq!(split_message(""), ("", ""));
    }

    #[test]
    fn a_finished_read_reports_only_real_changes() {
        let mut panel = GitPanel::new();
        let read = || Ok((status("main", 0, 0, 0), Vec::new()));
        assert!(panel.apply(read()), "the first read is new");
        assert!(!panel.apply(read()), "the same answer redraws nothing");
        assert!(panel.apply(Err("not a git repository".to_string())));
        assert!(!panel.apply(Err("not a git repository".to_string())));
        assert!(panel.apply(read()), "recovering clears the error");
        assert!(panel.error.is_none());
    }

    #[test]
    fn show_more_is_offered_only_when_the_page_filled() {
        let mut panel = GitPanel::new();
        panel.commits = vec![commit("abc"); PAGE as usize - 1];
        assert!(!panel.may_have_more());
        panel.commits.push(commit("abc"));
        assert!(panel.may_have_more());
        panel.limit += PAGE;
        assert!(!panel.may_have_more(), "the longer page has not filled");
    }

    fn file(path: &str, binary: bool, hunks: Vec<DiffHunk>) -> CommitFile {
        CommitFile {
            path: path.to_string(),
            old_path: path.to_string(),
            status: "M".to_string(),
            binary,
            additions: 0,
            deletions: 0,
            hunks,
        }
    }

    #[test]
    fn a_commit_lays_out_as_head_files_hunks_and_lines() {
        let hunk = DiffHunk {
            header: "@@ -1 +1 @@".to_string(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(1),
                text: "\tindented".to_string(),
            }],
        };
        let detail = CommitDetail {
            files: vec![
                file("a.rs", false, vec![hunk]),
                file("b.png", true, Vec::new()),
                file("c.sh", false, Vec::new()),
            ],
            truncated: true,
            ..CommitDetail::default()
        };
        let rows = detail_rows(&detail, &HashSet::new());
        assert_eq!(rows[0], DetailRow::Head);
        assert_eq!(rows[1], DetailRow::File(0));
        assert_eq!(rows[2], DetailRow::Hunk("@@ -1 +1 @@".to_string()));
        let DetailRow::Line(line) = &rows[3] else {
            panic!("a line: {:?}", rows[3]);
        };
        assert_eq!(
            (line.kind, line.text.as_str()),
            (SrcKind::Addition, "    indented")
        );
        assert_eq!(rows[4], DetailRow::File(1));
        assert_eq!(rows[5], DetailRow::Note("Binary file".to_string()));
        assert_eq!(rows[6], DetailRow::File(2));
        assert_eq!(rows[7], DetailRow::Note("No textual changes".to_string()));
        assert!(matches!(rows.last(), Some(DetailRow::Note(text)) if text.contains("larger")));
    }

    #[test]
    fn a_collapsed_file_keeps_its_header_and_drops_its_patch() {
        let hunk = DiffHunk {
            header: "@@ -1 +1 @@".to_string(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(1),
                text: "x".to_string(),
            }],
        };
        let detail = CommitDetail {
            files: vec![
                file("a.rs", false, vec![hunk.clone()]),
                file("b.png", true, Vec::new()),
                file("c.rs", false, vec![hunk]),
            ],
            ..CommitDetail::default()
        };
        // Folding a patch and a binary file leaves just their headers; the
        // file between them and the one after are untouched.
        let collapsed: HashSet<usize> = [0, 1].into_iter().collect();
        let rows = detail_rows(&detail, &collapsed);
        assert_eq!(
            rows[..3],
            [DetailRow::Head, DetailRow::File(0), DetailRow::File(1)]
        );
        assert_eq!(rows[3], DetailRow::File(2));
        assert!(matches!(rows[4], DetailRow::Hunk(_)));
        assert_eq!(rows.len(), 6, "head, three files, one hunk and its line");
    }

    #[test]
    fn toggling_a_file_relays_the_rows_and_a_new_pick_expands_again() {
        let hunk = DiffHunk {
            header: "@@".to_string(),
            lines: Vec::new(),
        };
        let detail = CommitDetail {
            files: vec![file("a.rs", false, vec![hunk])],
            ..CommitDetail::default()
        };
        let mut panel = GitPanel::new();
        panel.picked = Some("aaaa111".to_string());
        assert!(panel.open_commit("aaaa111", Ok(detail)));
        let count = |panel: &GitPanel| match &panel.detail {
            Some(Detail::Ready(opened)) => opened.rows.len(),
            _ => 0,
        };
        assert_eq!(count(&panel), 3, "head, file, hunk");
        panel.toggle_file(0);
        assert_eq!(count(&panel), 2, "head and the folded file");
        assert_eq!(panel.toggled, Some(0));
        panel.toggle_file(0);
        assert_eq!(count(&panel), 3, "opened again");
        assert!(panel.collapsed.is_empty());
    }

    #[test]
    fn a_commit_arriving_after_another_was_picked_is_dropped() {
        let mut panel = GitPanel::new();
        panel.commits = vec![commit("aaaa111"), commit("bbbb222")];
        panel.picked = Some("bbbb222".to_string());
        panel.detail = Some(Detail::Loading);
        assert!(!panel.open_commit("aaaa111", Ok(CommitDetail::default())));
        assert!(matches!(panel.detail, Some(Detail::Loading)));
        assert!(panel.open_commit("bbbb222", Err("boom".to_string())));
        assert!(matches!(&panel.detail, Some(Detail::Failed(error)) if error == "boom"));
        assert!(panel.open_commit("bbbb222", Ok(CommitDetail::default())));
        assert!(matches!(panel.detail, Some(Detail::Ready(_))));
    }

    #[test]
    fn a_pick_missing_from_the_new_history_is_dropped() {
        let mut panel = GitPanel::new();
        panel.picked = Some("gone000".to_string());
        panel.detail = Some(Detail::Loading);
        let read = Ok((status("other", 0, 0, 0), vec![commit("aaaa111")]));
        assert!(panel.apply(read));
        assert_eq!(panel.picked, None);
        assert!(panel.detail.is_none());

        panel.picked = Some("aaaa111".to_string());
        panel.detail = Some(Detail::Loading);
        let same = Ok((status("other", 0, 0, 0), vec![commit("aaaa111")]));
        assert!(!panel.apply(same));
        assert_eq!(panel.picked.as_deref(), Some("aaaa111"));
    }
}
