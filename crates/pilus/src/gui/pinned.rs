//! Pinned and recent workspaces: folders kept to open again in one click.
//!
//! A pin is made from a WORKSPACES row's context menu ("Pin this Workspace")
//! and lives in `Settings::pinned_workspaces`; the folders opened last are
//! `Settings::recent_workspaces`. Both are namespaced by Luvus session: a row
//! is drawn and opened only in the session that saved it, because
//! `workspace.open` goes to the attached session's daemon. The sidebar head's
//! pinned menu and Home's Pinned and Recent blocks draw them, and all
//! three open one the same way: `workspace.open` for the folder on the machine that owns it, then the
//! window switches to that endpoint, so the terminal that appears is in the
//! folder.
//!
//! The control client reaches a saved machine over SSH on its own, so a pin on
//! a machine this window is not attached to still opens; the endpoint is
//! connected by the switch afterwards. Nothing here is asked of the daemon
//! except that one call, which runs on a worker thread.

use gpui::{
    AnyElement, Context, Div, MouseButton, MouseDownEvent, Render, Stateful, Window, div,
    prelude::*, px,
};

use super::overview::{card, quiet};
use super::palette::{Command, Entry};
use super::picker::PickerEvent;
use super::sidebar_menu::MenuTarget;
use super::style::{self, FillExt, Fonts, RadiusExt};
use super::theme::Theme;
use super::toast::ToastKind;
use super::{
    EndpointId, HEADER_HEIGHT, PilusView, SESSION_MENU_GAP, SIDEBAR_HEAD_INSET, SIDEBAR_ROW_HEIGHT,
    SIDEBAR_TOGGLE_W, TAB_HEIGHT, icon, row_tooltip,
};

/// Width of the pinned menu, the session menu's.
const PINNED_MENU_WIDTH: f32 = 264.0;
/// Tallest the pinned menu grows before its rows scroll.
const PINNED_MENU_MAX_HEIGHT: f32 = 360.0;
use crate::settings::SavedWorkspace;

impl PilusView {
    /// The saved form of the folder `cwd` on `endpoint_key` in the attached
    /// session, with `~` expanded against that machine so the same folder
    /// spelled two ways is one pin.
    pub(super) fn saved_folder(&self, endpoint_key: &str, cwd: &str, name: &str) -> SavedWorkspace {
        SavedWorkspace {
            session: self.session_name.clone(),
            endpoint: endpoint_key.to_string(),
            path: self.expand_cwd(endpoint_key, cwd),
            name: name.to_string(),
        }
    }

    /// The attached session's pins, in the order they were pinned.
    fn session_pins(&self) -> impl Iterator<Item = &SavedWorkspace> {
        self.settings.pinned_in(self.session_name.as_deref())
    }

    /// The pin that names the folder `cwd` on `endpoint_key`, if the attached
    /// session has one. Both sides are expanded, since a pin made before a
    /// remote's home was known still carries its `~`.
    fn pin_for(&self, endpoint_key: &str, cwd: &str) -> Option<&SavedWorkspace> {
        let want = self.saved_folder(endpoint_key, cwd, "");
        self.session_pins().find(|saved| {
            let have = self.saved_folder(&saved.endpoint, &saved.path, "");
            have.is(self.session_name.as_deref(), &want.endpoint, &want.path)
        })
    }

    /// Whether the folder `cwd` on `endpoint_key` is pinned.
    pub(super) fn folder_pinned(&self, endpoint_key: &str, cwd: &str) -> bool {
        self.pin_for(endpoint_key, cwd).is_some()
    }

    /// Pin the folder, or take its pin away.
    pub(super) fn set_folder_pinned(
        &mut self,
        endpoint_key: &str,
        cwd: &str,
        name: &str,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        if on {
            let saved = self.saved_folder(endpoint_key, cwd, name);
            self.settings.set_workspace_pinned(saved, true);
        } else if let Some(pin) = self.pin_for(endpoint_key, cwd).cloned() {
            self.settings.set_workspace_pinned(pin, false);
        }
        self.save_settings();
        cx.notify();
    }

    /// Note a folder that was just opened, so Home lists it under Recent.
    pub(super) fn remember_folder(&mut self, endpoint_key: &str, cwd: &str) {
        let saved = self.saved_folder(endpoint_key, cwd, "");
        self.settings.remember_workspace(saved);
        self.save_settings();
    }

    /// Open a saved folder as a workspace on its machine and switch to it.
    ///
    /// The request goes out on a worker thread; the window changes only once
    /// the daemon has answered, so a refusal leaves the page it was clicked on
    /// in place to try again.
    pub(super) fn open_saved_workspace(&mut self, saved: SavedWorkspace, cx: &mut Context<Self>) {
        // A menu bar pick can outlive the session it was made in: the window
        // that opened for it may have attached to another one, whose daemon
        // would open the folder instead.
        if saved.session != self.session_name {
            self.toast(
                ToastKind::Error,
                format!("{} belongs to another session", saved.label()),
            );
            cx.notify();
            return;
        }
        let endpoint = self.endpoint_by_key(&saved.endpoint);
        if let EndpointId::Machine(id) = &endpoint
            && !self.machines.iter().any(|machine| &machine.id == id)
        {
            self.toast(
                ToastKind::Error,
                format!("{} is on a machine that is no longer saved", saved.label()),
            );
            cx.notify();
            return;
        }
        let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            cx.notify();
            return;
        };
        self.status = format!("opening {}…", saved.label());
        let path = self.expand_cwd(&saved.endpoint, &saved.path);
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = client
                .open_workspace(&path)
                .map_err(|error| error.to_string());
            tx.send(PickerEvent::SavedOpened { saved, result }).ok();
        });
        cx.notify();
    }

    /// A saved folder's `workspace.open` came back: show it, or say why not.
    pub(super) fn saved_workspace_opened(
        &mut self,
        saved: SavedWorkspace,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.remember_folder(&saved.endpoint, &saved.path);
                let machine = self
                    .endpoint_by_key(&saved.endpoint)
                    .machine_id()
                    .map(str::to_string);
                // Home, Search and the like cover the terminal; leave them so
                // the folder's terminal is what the click lands on.
                self.leave_settings_page();
                self.activate_opened_endpoint(machine, None, cx);
            }
            Err(error) => self.toast(
                ToastKind::Error,
                format!("Couldn't open {}: {error}", saved.label()),
            ),
        }
    }

    /// What names the machine of a saved folder: nothing for this Mac, the
    /// machine's name otherwise.
    pub(super) fn saved_host(&self, saved: &SavedWorkspace) -> Option<String> {
        (saved.endpoint != "local")
            .then(|| self.endpoint_label(&self.endpoint_by_key(&saved.endpoint)))
    }

    /// The context menu of a saved folder, opened where the pointer is.
    pub(super) fn open_saved_menu(&mut self, saved: &SavedWorkspace, cx: &mut Context<Self>) {
        let pinned = self.folder_pinned(&saved.endpoint, &saved.path);
        self.open_context_menu(
            MenuTarget::Saved {
                endpoint: saved.endpoint.clone(),
                path: saved.path.clone(),
                name: saved.name.clone(),
                pinned,
            },
            cx,
        );
    }

    /// Whether the pinned menu is on screen: open, with the sidebar it hangs
    /// from showing and something left to list. A flag left set by a collapsed
    /// sidebar or a last unpin must not keep an invisible overlay eating clicks.
    pub(super) fn pinned_menu_shown(&self) -> bool {
        self.pinned_menu_open && self.sidebar_visible && self.session_pins().next().is_some()
    }

    /// The `index`th pin of the attached session, in pin order.
    pub(super) fn pinned_at(&self, index: usize) -> Option<SavedWorkspace> {
        self.session_pins().nth(index).cloned()
    }

    /// The pins as palette rows (⌘O), in pin order. A remote pin names its
    /// machine in the title, so typing the machine finds it.
    pub(super) fn pinned_palette_entries(&self) -> Vec<Entry> {
        self.session_pins()
            .enumerate()
            .map(|(index, saved)| Entry {
                command: Command::OpenPinned(index),
                title: match self.saved_host(saved) {
                    Some(host) => format!("{} · {host}", saved.label()),
                    None => saved.label().to_string(),
                },
                group: "Pinned",
                shortcut: None,
            })
            .collect()
    }

    /// Close the pinned menu.
    pub(super) fn close_pinned_menu(&mut self) {
        self.pinned_menu_open = false;
    }

    /// The button in the sidebar's head, before the collapse toggle, that
    /// drops the pins down. It is absent until something
    /// is pinned, so a window that never pins one has no dead control.
    pub(super) fn pinned_button(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let count = self.session_pins().count();
        if count == 0 {
            return None;
        }
        let theme = self.theme;
        Some(
            div()
                .id("pilus-pinned-button")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .h(px(TAB_HEIGHT))
                .w(px(SIDEBAR_TOGGLE_W))
                .radius_md()
                .cursor_pointer()
                .state_fill(
                    if self.pinned_menu_open {
                        theme.tint_fill(1.0)
                    } else {
                        gpui::Rgba::default()
                    },
                    theme.tint_fill(1.0),
                    style::RADIUS_MD,
                )
                .child(icon(
                    include_bytes!("../../assets/icons/folder-bookmark.svg"),
                    14.0,
                    theme.muted,
                ))
                .tooltip(row_tooltip(
                    format!("Pinned workspaces · {count}"),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.pinned_menu_open = !this.pinned_menu_open;
                    this.close_session_menu();
                    cx.notify();
                })),
        )
    }

    /// The pinned menu: one row per pin, below its button in the sidebar head.
    ///
    /// Like the session menu, a transparent catcher covers the window so a
    /// click anywhere outside dismisses it, and the panel stops propagation so
    /// its own clicks land.
    pub(super) fn pinned_menu(
        &self,
        cx: &mut Context<Self>,
        cell_width: f32,
        fullscreen: bool,
    ) -> AnyElement {
        if !self.pinned_menu_shown() {
            return div().into_any_element();
        }
        let theme = self.theme;
        let sidebar_left = self
            .sidebar_dock()
            .map_or(0.0, |dock| f32::from(dock.x) * cell_width);
        // The button leads the head, ahead of the collapse toggle.
        let button_left = sidebar_left + Self::head_controls_left(fullscreen);
        let menu_left = button_left.max(SIDEBAR_HEAD_INSET);
        let menu_top = (HEADER_HEIGHT + TAB_HEIGHT) / 2.0 + SESSION_MENU_GAP;
        let mut rows = div().flex().flex_col().gap_1();
        for (index, saved) in self.session_pins().enumerate() {
            rows = rows.child(self.pinned_row(index, saved, cx));
        }
        style::enter(
            "pilus-pinned-menu-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .child(
                    div()
                        .absolute()
                        .left(px(menu_left))
                        .top(px(menu_top))
                        .w(px(PINNED_MENU_WIDTH))
                        .radius_md()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .p_2()
                        .child(
                            div()
                                .id("pilus-pinned-menu-scroll")
                                .max_h(px(PINNED_MENU_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .child(rows),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                // Stop the press here: it would otherwise reach the button
                // beneath, whose click handler reopens the menu this just
                // closed, so a second click on it could never close it.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_pinned_menu();
                        cx.stop_propagation();
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// One pinned folder in the menu: its name, the machine at the right when
    /// it is not this Mac. A click opens it; a right click offers Unpin, and a
    /// drag onto another row takes its slot.
    fn pinned_row(
        &self,
        index: usize,
        saved: &SavedWorkspace,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let host = self.saved_host(saved);
        let tip = row_tooltip(
            saved.label().to_string(),
            theme.panel_fg,
            vec![(self.shorten_cwd(&saved.endpoint, &saved.path), theme.muted)],
            theme,
        );
        let open = saved.clone();
        let menu = saved.clone();
        let drag = PinDrag {
            saved: saved.clone(),
            theme,
            font_size: self.chrome_font(),
        };
        div()
            .id(format!("pilus-pinned-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(28.0))
            .px_2()
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel, style::RADIUS_MD)
            .text_size(px(self.chrome_font()))
            .child(icon(
                include_bytes!("../../assets/icons/folder-bookmark.svg"),
                14.0,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(theme.panel_fg)
                    .child(saved.label().to_string()),
            )
            .children(host.map(|host| {
                div()
                    .flex_shrink_0()
                    .max_w(px(84.0))
                    .truncate()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(host)
            }))
            .tooltip(tip)
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            // The row under a drag shows the fill it would take, unless it is
            // the one being dragged.
            .drag_over::<PinDrag>({
                let saved = saved.clone();
                move |style, drag, _, _| {
                    if drag
                        .saved
                        .is(saved.session.as_deref(), &saved.endpoint, &saved.path)
                    {
                        style
                    } else {
                        style.bg(theme.panel)
                    }
                }
            })
            .on_drop(cx.listener({
                let saved = saved.clone();
                move |this, drag: &PinDrag, _, cx| this.reorder_pin(&drag.saved, &saved, cx)
            }))
            // A click, not a mouse down, so starting a drag does not open it.
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_pinned_menu();
                this.open_saved_workspace(open.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.close_pinned_menu();
                    this.open_saved_menu(&menu, cx);
                }),
            )
    }

    /// Drop the pin `from` into the slot of `to`, and keep the order. Home and
    /// the menu bar list the same order.
    fn reorder_pin(&mut self, from: &SavedWorkspace, to: &SavedWorkspace, cx: &mut Context<Self>) {
        self.settings.move_workspace_pin(from, to);
        self.save_settings();
        cx.notify();
    }

    /// Home's PINNED WORKSPACES. It is always there: with nothing pinned it
    /// says how to pin one, which is the only place that is explained.
    pub(super) fn overview_pinned_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let pinned: Vec<&SavedWorkspace> = self.session_pins().collect();
        let mut body = div().flex().flex_col().gap_1();
        if pinned.is_empty() {
            body = body.child(quiet(
                theme,
                fonts,
                "Nothing pinned",
                "Right-click a workspace in the sidebar and choose Pin this Workspace.",
            ));
        }
        for (index, saved) in pinned.iter().copied().enumerate() {
            body = body.child(self.overview_saved_row(
                format!("pilus-overview-pin-{index}"),
                saved,
                cx,
            ));
        }
        card(theme, fonts, "PINNED WORKSPACES", Some(pinned.len()), body)
    }

    /// Home's RECENT WORKSPACES: the folders opened last that are not pinned.
    /// Left out altogether when there are none.
    pub(super) fn overview_recent_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        let recent: Vec<&SavedWorkspace> = self
            .settings
            .recent_unpinned(self.session_name.as_deref())
            .collect();
        if recent.is_empty() {
            return None;
        }
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let mut body = div().flex().flex_col().gap_1();
        for (index, saved) in recent.into_iter().enumerate() {
            body = body.child(self.overview_saved_row(
                format!("pilus-overview-recent-{index}"),
                saved,
                cx,
            ));
        }
        Some(card(theme, fonts, "RECENT WORKSPACES", None, body))
    }

    /// A saved folder as a Home row: its name over its path, the machine at the
    /// right when it is not this Mac. A click opens it; a right click pins or
    /// unpins it.
    fn overview_saved_row(
        &self,
        id: String,
        saved: &SavedWorkspace,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let host = self.saved_host(saved);
        let glyph: &'static [u8] = if host.is_some() {
            include_bytes!("../../assets/icons/server.svg")
        } else {
            include_bytes!("../../assets/icons/folder.svg")
        };
        let trail = host.map(|host| self.overview_trail(host));
        let open = saved.clone();
        let menu = saved.clone();
        self.overview_row(
            id,
            icon(glyph, 14.0, theme.muted),
            saved.label().to_string(),
            self.shorten_cwd(&saved.endpoint, &saved.path),
            trail,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.open_saved_workspace(open.clone(), cx)))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| this.open_saved_menu(&menu, cx)),
        )
    }
}

/// A pinned row being dragged to a new place.
#[derive(Clone)]
struct PinDrag {
    saved: SavedWorkspace,
    theme: Theme,
    font_size: f32,
}

/// What GPUI draws at the pointer for a pin drag: the selected row's pill.
impl Render for PinDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .px_3()
            .radius_md()
            .bg(self.theme.panel)
            .text_color(self.theme.panel_fg)
            .text_size(px(self.font_size))
            .child(self.saved.label().to_string())
    }
}

/// The pins as the menu bar lists them. `host` names the machine behind an
/// endpoint key; this Mac gets none.
pub(super) fn menu_rows<'a>(
    pinned: impl Iterator<Item = &'a SavedWorkspace>,
    host: impl Fn(&str) -> String,
) -> Vec<super::menubar::Pinned> {
    pinned
        .map(|saved| super::menubar::Pinned {
            endpoint: saved.endpoint.clone(),
            path: saved.path.clone(),
            title: saved.label().to_string(),
            host: if saved.endpoint == "local" {
                String::new()
            } else {
                host(&saved.endpoint)
            },
        })
        .collect()
}
