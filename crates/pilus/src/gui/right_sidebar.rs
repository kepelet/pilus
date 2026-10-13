//! The right sidebar: a column beside the terminal card, toggled with Cmd+B,
//! holding the Diff review, the Git status and history, the file tree, the
//! workspace's GitHub issues and pull requests, and the machine's listening
//! ports on five tabs.
//!
//! The daemon knows nothing of this column. The window reserves its width from
//! the card, so the daemon sees a smaller viewport and reflows once when the
//! column opens, closes, or finishes a drag; the width tween and the drag
//! resize hold the viewport still until they settle, as the left sidebar's
//! tween does. This module owns the column's state, its pure sizing rules,
//! and the chrome (tab strip, resize handle, focus edge, footer). The open
//! tab's body is drawn by `diff.rs`, `git_tab.rs`, `files_tab.rs`, `github_tab.rs`, or `ports_tab.rs`, which own their data;
//! only the open tab polls the daemon.
//!
//! The column keeps a browser-style history of where it has been: a tab, and
//! the Diff's open file or the Files tab's open document. A click in the Files
//! tab that lands in the Diff is one step, so Back returns to the tree. Pages
//! record a step through [`PilusView::right_navigate`], and the strip's Back and
//! Forward walk it.

use std::time::Instant;

use gpui::{Context, Div, MouseButton, MouseDownEvent, Stateful, div, prelude::*, px};
use pilus_client::DiffLayer;

use super::style::{self, FillExt, RadiusExt};
use super::{HEADER_HEIGHT, PilusView, SIDEBAR_ANIM_DURATION, icon};

/// Narrowest the column drags to: the Diff header still fits.
pub(super) const MIN_WIDTH: f32 = 300.0;
/// Most of the window the column may take, so the terminal keeps a card.
const MAX_WINDOW_SHARE: f32 = 0.6;
/// Width a first open gets.
pub(super) const DEFAULT_WIDTH: f32 = 480.0;
/// Pixels the column is drawn wider than its stored width, growing to its
/// left. The stored width and the drag math stay as the user set them.
const WIDTH_BONUS: f32 = 2.0;
/// The draggable strip on the column's leading edge.
const HANDLE_WIDTH: f32 = 6.0;

/// What the column is showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SidebarTab {
    #[default]
    Diff,
    Git,
    Files,
    Github,
    Ports,
}

impl SidebarTab {
    pub(super) const ALL: [SidebarTab; 5] = [
        SidebarTab::Diff,
        SidebarTab::Git,
        SidebarTab::Files,
        SidebarTab::Github,
        SidebarTab::Ports,
    ];

    /// The tabs the strip draws. Ports is opened by its own button, in the
    /// window's header while the column is closed and in the strip once open.
    pub(super) const STRIP: [SidebarTab; 4] = [
        SidebarTab::Diff,
        SidebarTab::Git,
        SidebarTab::Files,
        SidebarTab::Github,
    ];

    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Git => "git",
            Self::Files => "files",
            Self::Github => "github",
            Self::Ports => "ports",
        }
    }

    /// The saved tab; anything unrecognized opens the Diff.
    pub(super) fn from_wire(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|tab| tab.wire() == text)
            .unwrap_or_default()
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Diff => "Changes",
            Self::Git => "History",
            Self::Files => "Files",
            Self::Github => "GitHub",
            Self::Ports => "Ports",
        }
    }

    /// Whether the tab reads the keyboard. Only the Diff has shortcuts, so
    /// the keys of a column showing Git or Files stay with the terminal.
    fn takes_keys(self) -> bool {
        self == Self::Diff
    }

    /// Whether the tab reads a git repo, so a plain folder has nothing for it.
    fn needs_repo(self) -> bool {
        matches!(self, Self::Diff | Self::Git | Self::Github)
    }
}

/// Where the column was, enough to put it back.
///
/// Only the part its tab uses is kept, so a step on the Files tab does not
/// differ from another by the Diff's selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Place {
    pub(super) tab: SidebarTab,
    /// The Diff's open file.
    pub(super) diff: Option<(DiffLayer, String)>,
    /// The Files tab's open document.
    pub(super) doc: Option<String>,
}

impl Place {
    /// `tab` with what it shows, dropping what belongs to the other tabs.
    pub(super) fn new(
        tab: SidebarTab,
        diff: Option<(DiffLayer, String)>,
        doc: Option<String>,
    ) -> Self {
        Self {
            tab,
            diff: diff.filter(|_| tab == SidebarTab::Diff),
            doc: doc.filter(|_| tab == SidebarTab::Files),
        }
    }
}

/// How many steps the history keeps.
const HISTORY_LIMIT: usize = 64;

/// The places the column has been, with the one it is at.
pub(super) struct History {
    places: Vec<Place>,
    at: usize,
}

impl History {
    pub(super) fn new(first: Place) -> Self {
        Self {
            places: vec![first],
            at: 0,
        }
    }

    pub(super) fn can_back(&self) -> bool {
        self.at > 0
    }

    pub(super) fn can_forward(&self) -> bool {
        self.at + 1 < self.places.len()
    }

    /// Record a move from `before` to `after`. What the history held for
    /// "here" is replaced by `before`, since the page can drift without a step
    /// (the Diff picks its first file by itself), and a move that lands where
    /// it started is no step. Going somewhere new drops the forward steps.
    pub(super) fn visit(&mut self, before: Place, after: Place) {
        self.places[self.at] = before.clone();
        if after == before {
            return;
        }
        self.places.truncate(self.at + 1);
        self.places.push(after);
        if self.places.len() > HISTORY_LIMIT {
            self.places.remove(0);
        }
        self.at = self.places.len() - 1;
    }

    /// Step back, returning where that is.
    pub(super) fn back(&mut self) -> Option<Place> {
        self.can_back().then(|| {
            self.at -= 1;
            self.places[self.at].clone()
        })
    }

    pub(super) fn forward(&mut self) -> Option<Place> {
        self.can_forward().then(|| {
            self.at += 1;
            self.places[self.at].clone()
        })
    }
}

/// A width tween from the openness the column had to the one it is going to,
/// so a mid-flight reversal glides instead of jumping.
struct Tween {
    started: Instant,
    from: f32,
    to: f32,
}

/// An edge drag in progress.
#[derive(Clone, Copy)]
struct Drag {
    pointer_x: f32,
    width: f32,
}

pub(super) struct RightSidebar {
    pub(super) visible: bool,
    pub(super) width: f32,
    pub(super) tab: SidebarTab,
    pub(super) history: History,
    /// The keyboard is on the column rather than the terminal.
    pub(super) focused: bool,
    tween: Option<Tween>,
    drag: Option<Drag>,
    /// The column is stretched to fill the window beside the left sidebar.
    /// A view state, not a preference: it never outlives the column.
    expanded: bool,
    spread_tween: Option<Tween>,
    /// The width the column fills when expanded, kept current by the
    /// window each frame.
    full: f32,
}

impl RightSidebar {
    pub(super) fn new(visible: bool, width: f32, tab: SidebarTab) -> Self {
        Self {
            visible,
            width: if width >= MIN_WIDTH {
                width
            } else {
                DEFAULT_WIDTH
            },
            tab,
            history: History::new(Place::new(tab, None, None)),
            focused: false,
            tween: None,
            drag: None,
            expanded: false,
            spread_tween: None,
            full: 0.0,
        }
    }

    /// Where `tween` has got to, or `rest` once it has settled.
    fn eased(tween: Option<&Tween>, rest: f32) -> f32 {
        match tween {
            Some(tween) => {
                let total = SIDEBAR_ANIM_DURATION.as_secs_f32();
                let step = (tween.started.elapsed().as_secs_f32() / total).clamp(0.0, 1.0);
                (tween.from + (tween.to - tween.from) * style::ease_spring(step)).max(0.0)
            }
            None => rest,
        }
    }

    /// How open the column is, 0 to 1, mid-tween or settled.
    pub(super) fn openness(&self) -> f32 {
        Self::eased(self.tween.as_ref(), if self.visible { 1.0 } else { 0.0 })
    }

    /// How far the column has stretched toward the full width, 0 to 1.
    pub(super) fn spread(&self) -> f32 {
        Self::eased(
            self.spread_tween.as_ref(),
            if self.expanded { 1.0 } else { 0.0 },
        )
    }

    /// The column's width at its current stretch.
    pub(super) fn shown_width(&self) -> f32 {
        let own = self.width + WIDTH_BONUS;
        let full = self.full.max(own);
        own + (full - own) * self.spread().min(1.0)
    }

    /// Pixels the column takes from the card right now.
    pub(super) fn px(&self) -> f32 {
        self.shown_width() * self.openness()
    }

    /// Whether the column fills the window, hiding the terminal behind it.
    pub(super) fn expanded(&self) -> bool {
        self.expanded
    }

    /// Record the width the column fills when expanded.
    pub(super) fn set_full(&mut self, width: f32) {
        self.full = width.max(0.0);
    }

    /// Stretch the column to the full width, or let it back to its own.
    pub(super) fn toggle_expanded(&mut self) {
        self.set_expanded(!self.expanded);
    }

    fn set_expanded(&mut self, expanded: bool) {
        if self.expanded == expanded {
            return;
        }
        let from = self.spread();
        self.expanded = expanded;
        self.spread_tween = Some(Tween {
            started: Instant::now(),
            from,
            to: if expanded { 1.0 } else { 0.0 },
        });
    }

    /// Whether an open, close, or stretch tween is in flight. A drag is not
    /// one: it follows the pointer, so the card may snap to cells beneath it.
    pub(super) fn tweening(&self) -> bool {
        self.tween.is_some() || self.spread_tween.is_some()
    }

    /// Whether the daemon should hold its viewport: the column is moving.
    pub(super) fn moving(&self) -> bool {
        self.tween.is_some() || self.spread_tween.is_some() || self.drag.is_some()
    }

    /// Advance the tweens; true once nothing is in flight.
    pub(super) fn tick(&mut self) -> bool {
        let finished = |tween: &Option<Tween>| {
            tween
                .as_ref()
                .is_none_or(|t| t.started.elapsed() >= SIDEBAR_ANIM_DURATION)
        };
        let opened = finished(&self.tween);
        if opened {
            self.tween = None;
        }
        let spread = finished(&self.spread_tween);
        if spread {
            self.spread_tween = None;
        }
        opened && spread
    }

    /// Show or hide the column. Showing takes the keyboard when the open tab
    /// reads it; hiding gives it back to the terminal.
    pub(super) fn toggle(&mut self) {
        let from = self.openness();
        self.visible = !self.visible;
        if !self.visible {
            self.set_expanded(false);
        }
        self.focused = self.visible && self.tab.takes_keys();
        self.tween = Some(Tween {
            started: Instant::now(),
            from,
            to: if self.visible { 1.0 } else { 0.0 },
        });
    }

    /// Switch tabs. Choosing one is working in the column, so the keyboard
    /// follows when the new tab reads it and returns to the terminal when
    /// it does not.
    pub(super) fn select_tab(&mut self, tab: SidebarTab) {
        self.tab = tab;
        self.focused = self.visible && tab.takes_keys();
    }

    fn begin_drag(&mut self, pointer_x: f32) {
        self.drag = Some(Drag {
            pointer_x,
            width: self.width,
        });
    }

    /// Follow the pointer; dragging left widens the column.
    fn drag_to(&mut self, pointer_x: f32, window_w: f32) -> bool {
        let Some(drag) = self.drag else {
            return false;
        };
        let wanted = drag.width + (drag.pointer_x - pointer_x);
        let width = clamp_width(wanted, window_w);
        let changed = (width - self.width).abs() >= 0.5;
        self.width = width;
        changed
    }

    /// End a drag; true when one was in progress, so the width gets saved.
    fn end_drag(&mut self) -> bool {
        self.drag.take().is_some()
    }
}

/// `wanted` kept between the minimum and the window's share.
pub(super) fn clamp_width(wanted: f32, window_w: f32) -> f32 {
    let most = (window_w * MAX_WINDOW_SHARE).max(MIN_WIDTH);
    wanted.clamp(MIN_WIDTH, most)
}

impl PilusView {
    pub(super) fn toggle_right_expanded(&mut self, cx: &mut Context<Self>) {
        self.right.toggle_expanded();
        cx.notify();
    }

    pub(super) fn toggle_right_sidebar(&mut self, cx: &mut Context<Self>) {
        self.right.toggle();
        self.settings.right_sidebar_visible = self.right.visible;
        self.settings.save(&self.pilus_root);
        if self.right.visible {
            self.mark_open_tab_stale();
        }
        cx.notify();
    }

    /// Open `tab`, remembering it for the next launch.
    pub(super) fn select_right_tab(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        if self.right.tab == tab {
            return;
        }
        self.right_navigate(cx, |this, _| this.right.select_tab(tab));
    }

    /// The Ports button: open the column on Ports, or close it when Ports is
    /// already showing, as the sidebar toggle would.
    pub(super) fn toggle_ports(&mut self, cx: &mut Context<Self>) {
        if self.right.visible && self.right.tab == SidebarTab::Ports {
            self.toggle_right_sidebar(cx);
            return;
        }
        if !self.right.visible {
            self.toggle_right_sidebar(cx);
        }
        self.select_right_tab(SidebarTab::Ports, cx);
    }

    /// Where the column is now.
    fn right_here(&self) -> Place {
        Place::new(
            self.right.tab,
            self.diff_panel.selected_key(),
            self.files_panel.doc_path().map(str::to_string),
        )
    }

    /// Run `change`, which moves the column, and record it as one step.
    ///
    /// Everything a click can do to the column's place goes through here, so
    /// a click that touches two things (open the Files tab's changed file in
    /// the Diff) is still one step back.
    pub(super) fn right_navigate(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Self, &mut Context<Self>),
    ) {
        let before = self.right_here();
        change(self, cx);
        let after = self.right_here();
        self.right.history.visit(before.clone(), after.clone());
        if after.tab != before.tab {
            self.remember_right_tab();
        }
        cx.notify();
    }

    /// Persist the open tab and ask it for a fresh read, as it was off screen.
    fn remember_right_tab(&mut self) {
        self.settings.right_sidebar_tab = self.right.tab.wire().to_string();
        self.settings.save(&self.pilus_root);
        self.mark_open_tab_stale();
    }

    /// Put the column back where `place` says, without recording a step.
    fn right_restore(&mut self, place: Place, cx: &mut Context<Self>) {
        let changed_tab = self.right.tab != place.tab;
        self.right.select_tab(place.tab);
        match place.tab {
            SidebarTab::Diff => {
                if let Some((layer, path)) = place.diff {
                    self.diff_reveal(layer, &path, cx);
                }
            }
            SidebarTab::Files => match place.doc {
                Some(path) if self.files_panel.doc_path() != Some(path.as_str()) => {
                    self.files_read_doc(path);
                }
                Some(_) => {}
                None => self.files_panel.close_doc(),
            },
            SidebarTab::Git | SidebarTab::Github | SidebarTab::Ports => {}
        }
        if changed_tab {
            self.remember_right_tab();
        }
        cx.notify();
    }

    pub(super) fn right_back(&mut self, cx: &mut Context<Self>) {
        if let Some(place) = self.right.history.back() {
            self.right_restore(place, cx);
        }
    }

    pub(super) fn right_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(place) = self.right.history.forward() {
            self.right_restore(place, cx);
        }
    }

    /// One of the strip's step buttons. A step with nowhere to go dims and
    /// ignores the pointer.
    fn right_step_button(
        &self,
        id: &'static str,
        glyph: &'static [u8],
        tip: &'static str,
        enabled: bool,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let size = style::scaled(&self.shared_settings, 24.0);
        let button = div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .w(px(size))
            .h(px(size))
            .radius_md()
            .child(icon(glyph, 14.0, theme.muted));
        if !enabled {
            return button.opacity(0.35);
        }
        button
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .tooltip(super::row_tooltip(
                tip.to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
    }

    /// A tab that was off screen has gone without polls, so what it holds is
    /// old: ask for a fresh read as it comes back.
    fn mark_open_tab_stale(&mut self) {
        match self.right.tab {
            SidebarTab::Diff => self.diff_panel.mark_stale(),
            SidebarTab::Git => self.git_panel.mark_stale(),
            SidebarTab::Files => self.files_panel.mark_stale(),
            SidebarTab::Github => self.github_panel.mark_stale(),
            SidebarTab::Ports => self.ports_panel.mark_stale(),
        }
    }

    /// Whether the active workspace is a git repo, which is what Changes,
    /// History, and GitHub read. The daemon reports a branch only for a repo.
    fn active_is_repo(&self) -> bool {
        self.surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)
            .is_some_and(|workspace| workspace.branch.is_some())
    }

    /// The tab on screen. A git tab in a folder that is not a repo has
    /// nothing to read, so the column shows Files instead; the choice itself
    /// is kept, and returns with the next repo.
    fn shown_tab(&self) -> SidebarTab {
        if self.right.tab.needs_repo() && !self.active_is_repo() {
            return SidebarTab::Files;
        }
        self.right.tab
    }

    /// Whether the Diff is on screen, so it should poll the daemon.
    pub(super) fn diff_visible(&self) -> bool {
        self.right.visible && self.shown_tab() == SidebarTab::Diff
    }

    /// Whether Diff holds the keyboard.
    pub(super) fn diff_focused(&self) -> bool {
        self.diff_visible() && self.right.focused
    }

    /// Whether the Git tab is on screen, so it should poll the daemon.
    pub(super) fn git_visible(&self) -> bool {
        self.right.visible && self.shown_tab() == SidebarTab::Git
    }

    /// Whether the Files tab is on screen, so it should poll the daemon.
    pub(super) fn files_visible(&self) -> bool {
        self.right.visible && self.shown_tab() == SidebarTab::Files
    }

    /// Whether the GitHub tab is on screen, so it should poll `gh`.
    pub(super) fn github_visible(&self) -> bool {
        self.right.visible && self.shown_tab() == SidebarTab::Github
    }

    /// Whether a GitHub text field holds the keyboard. The tab has no
    /// shortcuts of its own, so until a field is open its keys stay with the
    /// terminal.
    pub(super) fn github_focused(&self) -> bool {
        self.github_visible() && self.right.focused && self.github_panel.composing()
    }

    /// Whether the Ports tab is on screen, so it should poll the machine.
    pub(super) fn ports_visible(&self) -> bool {
        self.right.visible && self.right.tab == SidebarTab::Ports
    }

    /// The Ports icon, with a green dot while a forward of ours is running, so
    /// the button says so even with the column on another tab or closed.
    fn ports_glyph(&self, lit: bool) -> Div {
        let theme = self.theme;
        let glyph = div().relative().flex().child(icon(
            include_bytes!("../../assets/icons/ethernet-port.svg"),
            14.0,
            if lit { theme.panel_fg } else { theme.muted },
        ));
        if !self.ports_forwarding() {
            return glyph;
        }
        glyph.child(
            div()
                .absolute()
                .top(px(-2.0))
                .right(px(-2.0))
                .size(px(6.0))
                .rounded_full()
                .bg(theme.ok),
        )
    }

    /// The tooltip of the Ports buttons.
    fn ports_tip(&self) -> String {
        match self.ports_forwarded_count() {
            0 => "Ports".to_string(),
            1 => "Ports · 1 forwarded".to_string(),
            n => format!("Ports · {n} forwarded"),
        }
    }

    /// The Ports button beside the sidebar toggle, lit while Ports is open.
    pub(super) fn right_ports_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-right-ports-toggle")
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(super::TAB_HEIGHT))
            .w(px(super::SIDEBAR_TOGGLE_W))
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .tooltip(super::row_tooltip(
                self.ports_tip(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .child(self.ports_glyph(self.right.visible && self.right.tab == SidebarTab::Ports))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_ports(cx)))
    }

    /// The toggle for the tab strip's trailing edge.
    pub(super) fn right_sidebar_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-right-sidebar-toggle")
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(super::TAB_HEIGHT))
            .w(px(super::SIDEBAR_TOGGLE_W))
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(icon(
                include_bytes!("../../assets/icons/panel-right.svg"),
                14.0,
                if self.right.visible {
                    theme.panel_fg
                } else {
                    theme.muted
                },
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_right_sidebar(cx)))
    }

    /// The column: a tab strip over the open tab and its footer, with a drag
    /// handle on its leading edge.
    ///
    /// `width` is what the card leaves it: the column's own width plus the
    /// grid's sub-cell remainder (see `PilusView::card_layout`).
    pub(super) fn right_sidebar_view(
        &mut self,
        width: f32,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let split = self.is_split();
        let expanded = self.right.expanded();
        let shown = self.shown_tab();
        let is_repo = self.active_is_repo();
        // Stretched under the floating traffic lights when the left sidebar
        // is away, the strip starts past them.
        let lights = if fullscreen {
            0.0
        } else {
            (super::TRAFFIC_LIGHT_CLEARANCE - 12.0)
                * (1.0 - self.sidebar_open)
                * self.right.spread()
        };

        let strip = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_3()
            .pl(px(12.0 + lights))
            .child(
                self.right_step_button(
                    "pilus-right-back",
                    include_bytes!("../../assets/icons/chevron-left.svg"),
                    "Back",
                    self.right.history.can_back(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.right_back(cx))),
            )
            .child(
                self.right_step_button(
                    "pilus-right-forward",
                    include_bytes!("../../assets/icons/chevron-right.svg"),
                    "Forward",
                    self.right.history.can_forward(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.right_forward(cx))),
            )
            // The tabs scroll when the column is narrow; the buttons around
            // them stay put.
            .child(
                div()
                    .id("pilus-right-tabs-scroll")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .overflow_y_hidden()
                    .children(SidebarTab::STRIP.map(|tab| {
                        if tab.needs_repo() && !is_repo {
                            return style::tab_disabled(
                                format!("pilus-right-tab-{}", tab.wire()),
                                tab.label(),
                                theme,
                                fonts.body,
                            )
                            .flex_shrink_0();
                        }
                        style::tab(
                            format!("pilus-right-tab-{}", tab.wire()),
                            tab.label(),
                            shown == tab,
                            theme,
                            fonts.body,
                        )
                        .flex_shrink_0()
                        .on_click(cx.listener(move |this, _, _, cx| this.select_right_tab(tab, cx)))
                    })),
            )
            .child(
                div()
                    .id("pilus-right-sidebar-ports")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(style::scaled(&self.shared_settings, 24.0)))
                    .h(px(style::scaled(&self.shared_settings, 24.0)))
                    .radius_md()
                    .cursor_pointer()
                    .hover_fill(theme.panel_dim, style::RADIUS_MD)
                    .flex_shrink_0()
                    .tooltip(super::row_tooltip(
                        self.ports_tip(),
                        theme.panel_fg,
                        Vec::new(),
                        theme,
                    ))
                    .child(self.ports_glyph(self.right.tab == SidebarTab::Ports))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_ports(cx))),
            )
            .child(
                div()
                    .id("pilus-right-sidebar-expand")
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .w(px(style::scaled(&self.shared_settings, 24.0)))
                    .h(px(style::scaled(&self.shared_settings, 24.0)))
                    .radius_md()
                    .cursor_pointer()
                    .hover_fill(theme.panel_dim, style::RADIUS_MD)
                    .child(icon(
                        if expanded {
                            include_bytes!("../../assets/icons/minimize.svg")
                        } else {
                            include_bytes!("../../assets/icons/maximize.svg")
                        },
                        12.0,
                        theme.muted,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_right_expanded(cx))),
            )
            .child(
                div()
                    .id("pilus-right-sidebar-close")
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .w(px(style::scaled(&self.shared_settings, 24.0)))
                    .h(px(style::scaled(&self.shared_settings, 24.0)))
                    .radius_md()
                    .cursor_pointer()
                    .hover_fill(theme.panel_dim, style::RADIUS_MD)
                    .child(icon(
                        include_bytes!("../../assets/icons/x.svg"),
                        12.0,
                        theme.muted,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_right_sidebar(cx))),
            );

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .child(match shown {
                SidebarTab::Diff => self.diff_page(self.right.shown_width(), cx),
                SidebarTab::Git => self.git_page(cx),
                SidebarTab::Files => self.files_page(cx),
                SidebarTab::Github => self.github_page(cx),
                SidebarTab::Ports => self.ports_page(cx),
            });
        let footer = match shown {
            SidebarTab::Diff => self.diff_footer(cx),
            SidebarTab::Git | SidebarTab::Files | SidebarTab::Github | SidebarTab::Ports => None,
        };

        let handle = div()
            .id("pilus-right-sidebar-handle")
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .bottom(px(0.0))
            .w(px(HANDLE_WIDTH))
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.right.begin_drag(f32::from(event.position.x));
                    cx.stop_propagation();
                }),
            );

        div()
            .id("pilus-right-sidebar")
            .absolute()
            .right(px(0.0))
            .top(px(0.0))
            .bottom(px(0.0))
            .w(px(width))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(self.chrome_fill())
            .border_l_1()
            // A split's tinted surface runs up to the column, so the edge
            // takes that surface's color and the terminal's surface meets the
            // column without a seam; the edge keeps its pixel so the content
            // box does not shift. Focus draws no edge: only Changes takes
            // keys, so it would mark one tab.
            .border_color(if split {
                let glass = self.glass_opacity();
                theme.pane_surface(glass).opacity(glass)
            } else {
                theme.border
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if !this.right.focused && this.shown_tab().takes_keys() {
                        this.right.focused = true;
                        cx.notify();
                    }
                }),
            )
            .child(strip)
            .child(body)
            .children(footer)
            .children((!expanded).then_some(handle))
    }

    /// Track an edge drag across the whole window.
    pub(super) fn right_sidebar_drag_move(
        &mut self,
        x: f32,
        window_w: f32,
        cx: &mut Context<Self>,
    ) {
        if self.right.drag_to(x, window_w) {
            cx.notify();
        }
    }

    pub(super) fn right_sidebar_drag_end(&mut self, cx: &mut Context<Self>) {
        if self.right.end_drag() {
            self.settings.right_sidebar_width = self.right.width;
            self.settings.save(&self.pilus_root);
            cx.notify();
        }
    }
}

/// Shot-harness entry point: put the column on `tab`.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn right_show_tab(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        self.right.visible = true;
        self.right.select_tab(tab);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_stays_between_the_minimum_and_the_window_share() {
        assert_eq!(clamp_width(100.0, 1400.0), MIN_WIDTH);
        assert_eq!(clamp_width(500.0, 1400.0), 500.0);
        assert!((clamp_width(2000.0, 1400.0) - 840.0).abs() < 0.01);
        // A window narrower than the minimum still gets the minimum.
        assert_eq!(clamp_width(400.0, 400.0), MIN_WIDTH);
    }

    #[test]
    fn saved_width_below_the_minimum_falls_back() {
        let diff = SidebarTab::Diff;
        assert_eq!(RightSidebar::new(false, 0.0, diff).width, DEFAULT_WIDTH);
        assert_eq!(RightSidebar::new(true, 520.0, diff).width, 520.0);
    }

    #[test]
    fn toggle_moves_toward_the_new_state_and_takes_focus() {
        let mut side = RightSidebar::new(false, 400.0, SidebarTab::Diff);
        assert_eq!(side.openness(), 0.0);
        side.toggle();
        assert!(side.visible && side.focused && side.moving());
        side.toggle();
        assert!(!side.visible && !side.focused);
    }

    #[test]
    fn dragging_left_widens_and_release_reports_the_drag() {
        let mut side = RightSidebar::new(true, 400.0, SidebarTab::Diff);
        assert!(!side.drag_to(500.0, 1400.0), "no drag, no change");
        side.begin_drag(1000.0);
        assert!(side.moving());
        assert!(side.drag_to(900.0, 1400.0));
        assert_eq!(side.width, 500.0);
        side.drag_to(0.0, 1400.0);
        assert!((side.width - 840.0).abs() < 0.01);
        assert!(side.end_drag());
        assert!(!side.end_drag());
    }

    #[test]
    fn expanding_fills_the_window_and_hiding_lets_go() {
        let mut side = RightSidebar::new(true, 400.0, SidebarTab::Diff);
        side.set_full(1000.0);
        assert_eq!(side.px(), 400.0 + WIDTH_BONUS);
        side.toggle_expanded();
        assert!(side.expanded() && side.moving());
        // Settled, the column is as wide as the window leaves it.
        side.spread_tween = None;
        assert_eq!(side.shown_width(), 1000.0);
        assert_eq!(side.px(), 1000.0);
        // A window narrower than the column never shrinks it.
        side.set_full(200.0);
        assert_eq!(side.shown_width(), 400.0 + WIDTH_BONUS);
        side.set_full(1000.0);
        side.toggle();
        assert!(
            !side.expanded(),
            "a hidden column comes back at its own width"
        );
    }

    fn diff_place(path: &str) -> Place {
        Place::new(
            SidebarTab::Diff,
            Some((DiffLayer::Worktree, path.to_string())),
            None,
        )
    }

    fn files_place(doc: Option<&str>) -> Place {
        Place::new(SidebarTab::Files, None, doc.map(str::to_string))
    }

    #[test]
    fn a_place_keeps_only_what_its_tab_shows() {
        let place = Place::new(
            SidebarTab::Files,
            Some((DiffLayer::Staged, "a.rs".to_string())),
            Some("/repo/README.md".to_string()),
        );
        assert_eq!(place.diff, None);
        assert_eq!(place.doc.as_deref(), Some("/repo/README.md"));
        let git = Place::new(
            SidebarTab::Git,
            Some((DiffLayer::Staged, "a".into())),
            Some("b".into()),
        );
        assert_eq!((git.diff, git.doc), (None, None));
    }

    #[test]
    fn opening_a_changed_file_from_the_tree_is_one_step_back_to_the_tree() {
        let mut history = History::new(files_place(None));
        history.visit(files_place(None), diff_place("src/lib.rs"));
        assert!(history.can_back() && !history.can_forward());
        assert_eq!(history.back(), Some(files_place(None)));
        assert!(!history.can_back() && history.can_forward());
        assert_eq!(history.forward(), Some(diff_place("src/lib.rs")));
        assert_eq!(history.forward(), None);
    }

    #[test]
    fn a_move_that_lands_where_it_started_is_no_step() {
        let mut history = History::new(files_place(None));
        history.visit(files_place(None), files_place(None));
        assert!(!history.can_back());
    }

    #[test]
    fn going_somewhere_new_drops_the_forward_steps() {
        let mut history = History::new(files_place(None));
        history.visit(files_place(None), diff_place("a.rs"));
        history.visit(diff_place("a.rs"), diff_place("b.rs"));
        assert_eq!(history.back(), Some(diff_place("a.rs")));
        history.visit(diff_place("a.rs"), files_place(Some("/r/README.md")));
        assert!(!history.can_forward());
        assert_eq!(history.back(), Some(diff_place("a.rs")));
        assert_eq!(history.back(), Some(files_place(None)));
    }

    #[test]
    fn drift_is_folded_into_the_current_place_rather_than_made_a_step() {
        // The Diff picked its first file by itself since the last step.
        let mut history = History::new(Place::new(SidebarTab::Diff, None, None));
        history.visit(diff_place("first.rs"), files_place(None));
        assert_eq!(history.back(), Some(diff_place("first.rs")));
    }

    #[test]
    fn history_is_bounded() {
        let mut history = History::new(files_place(None));
        let mut here = files_place(None);
        for n in 0..(HISTORY_LIMIT * 2) {
            let next = diff_place(&format!("f{n}.rs"));
            history.visit(here, next.clone());
            here = next;
        }
        let mut steps = 0;
        while history.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY_LIMIT - 1);
    }

    #[test]
    fn saved_tab_falls_back_to_the_diff() {
        assert_eq!(SidebarTab::from_wire("git"), SidebarTab::Git);
        assert_eq!(SidebarTab::from_wire("diff"), SidebarTab::Diff);
        assert_eq!(SidebarTab::from_wire(""), SidebarTab::Diff);
        assert_eq!(SidebarTab::from_wire("terminal"), SidebarTab::Diff);
        for tab in SidebarTab::ALL {
            assert_eq!(SidebarTab::from_wire(tab.wire()), tab);
        }
    }

    #[test]
    fn only_the_diff_takes_the_keyboard() {
        let mut side = RightSidebar::new(false, 400.0, SidebarTab::Git);
        side.toggle();
        assert!(side.visible && !side.focused, "Git has no shortcuts");
        side.select_tab(SidebarTab::Diff);
        assert!(side.focused);
        side.select_tab(SidebarTab::Git);
        assert!(!side.focused, "the keys go back to the terminal");
        // A hidden column never holds the keyboard, whichever tab it names.
        side.toggle();
        side.select_tab(SidebarTab::Diff);
        assert!(!side.visible && !side.focused);
    }
}
