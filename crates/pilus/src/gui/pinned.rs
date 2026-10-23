//! Pinned and recent workspaces: folders kept to open again in one click.
//!
//! A pin is made from a WORKSPACES row's context menu ("Pin this Workspace")
//! and lives in `Settings::pinned_workspaces`; the folders opened last are
//! `Settings::recent_workspaces`. The sidebar's PINNED WORKSPACES section and
//! Home's Pinned and Recent blocks draw them, and all three open one the same
//! way: `workspace.open` for the folder on the machine that owns it, then the
//! window switches to that endpoint, so the terminal that appears is in the
//! folder.
//!
//! The control client reaches a saved machine over SSH on its own, so a pin on
//! a machine this window is not attached to still opens; the endpoint is
//! connected by the switch afterwards. Nothing here is asked of the daemon
//! except that one call, which runs on a worker thread.

use gpui::{Context, Div, MouseButton, MouseDownEvent, div, prelude::*, px};

use super::overview::{card, quiet};
use super::picker::PickerEvent;
use super::sidebar_menu::MenuTarget;
use super::style::{self, Fonts, RadiusExt};
use super::toast::ToastKind;
use super::{
    EndpointId, PilusView, SIDEBAR_GLYPH, SIDEBAR_GLYPH_GAP, SIDEBAR_ROW_HEIGHT, SIDEBAR_ROW_PAD,
    SIDEBAR_SECTION_GAP, SectionHeader, icon, row_tooltip,
};
use crate::settings::SavedWorkspace;

impl PilusView {
    /// The saved form of the folder `cwd` on `endpoint_key`, with `~` expanded
    /// against that machine so the same folder spelled two ways is one pin.
    pub(super) fn saved_folder(&self, endpoint_key: &str, cwd: &str, name: &str) -> SavedWorkspace {
        SavedWorkspace {
            endpoint: endpoint_key.to_string(),
            path: self.expand_cwd(endpoint_key, cwd),
            name: name.to_string(),
        }
    }

    /// The pin that names the folder `cwd` on `endpoint_key`, if there is one.
    /// Both sides are expanded, since a pin made before a remote's home was
    /// known still carries its `~`.
    fn pin_for(&self, endpoint_key: &str, cwd: &str) -> Option<&SavedWorkspace> {
        let want = self.saved_folder(endpoint_key, cwd, "");
        self.settings.pinned_workspaces.iter().find(|saved| {
            let have = self.saved_folder(&saved.endpoint, &saved.path, "");
            have.is(&want.endpoint, &want.path)
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
    pub(super) fn open_saved_menu(
        &mut self,
        saved: &SavedWorkspace,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let pinned = self.folder_pinned(&saved.endpoint, &saved.path);
        self.open_context_menu(
            MenuTarget::Saved {
                endpoint: saved.endpoint.clone(),
                path: saved.path.clone(),
                name: saved.name.clone(),
                pinned,
            },
            position,
            cx,
        );
    }

    /// PINNED WORKSPACES, between the GitHub block and WORKSPACES. It is not
    /// drawn until something is pinned, so a window that never pins one has no
    /// empty heading.
    pub(super) fn pinned_section(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.settings.pinned_workspaces.is_empty() {
            return None;
        }
        let section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(SIDEBAR_SECTION_GAP))
            .child(self.section_header(
                SectionHeader {
                    id: "pilus-pinned-toggle",
                    label: "PINNED WORKSPACES",
                    collapsed: self.pinned_collapsed,
                    toggle: Box::new(|this, cx| {
                        this.pinned_collapsed = !this.pinned_collapsed;
                        cx.notify();
                    }),
                    menu: false,
                },
                cx,
            ));
        if self.pinned_collapsed {
            return Some(section);
        }
        let mut rows = div().flex().flex_col().gap_1();
        for (index, saved) in self.settings.pinned_workspaces.iter().enumerate() {
            rows = rows.child(self.pinned_row(index, saved, cx));
        }
        Some(section.child(style::enter("pilus-pinned-rows", rows)))
    }

    /// One pinned folder: its glyph and name, the machine at the right when it
    /// is not this Mac. A click opens it; a right click offers Unpin.
    fn pinned_row(
        &self,
        index: usize,
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
        let mut lines = vec![(
            format!(
                "Directory: {}",
                self.shorten_cwd(&saved.endpoint, &saved.path)
            ),
            theme.muted,
        )];
        if let Some(host) = host.as_ref() {
            lines.push((format!("Host: {host}"), theme.muted));
        }
        let tip = row_tooltip(saved.label().to_string(), theme.panel_fg, lines, theme);

        let open = saved.clone();
        let menu = saved.clone();
        div()
            .id(format!("pilus-pinned-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .px(px(SIDEBAR_ROW_PAD))
            .radius_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            .child(div().w(px(SIDEBAR_GLYPH)).flex_shrink_0().child(icon(
                glyph,
                SIDEBAR_GLYPH,
                theme.muted,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.open_saved_workspace(open.clone(), cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_saved_menu(&menu, event.position, cx)
                }),
            )
    }

    /// Home's PINNED WORKSPACES. It is always there: with nothing pinned it
    /// says how to pin one, which is the only place that is explained.
    pub(super) fn overview_pinned_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let pinned = &self.settings.pinned_workspaces;
        let mut body = div().flex().flex_col().gap_1();
        if pinned.is_empty() {
            body = body.child(quiet(
                theme,
                fonts,
                "Nothing pinned",
                "Right-click a workspace in the sidebar and choose Pin this Workspace.",
            ));
        }
        for (index, saved) in pinned.iter().enumerate() {
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
        let recent: Vec<&SavedWorkspace> = self.settings.recent_unpinned().collect();
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
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.open_saved_menu(&menu, event.position, cx)
            }),
        )
    }
}

/// The pins as the menu bar lists them. `host` names the machine behind an
/// endpoint key; this Mac gets none.
pub(super) fn menu_rows(
    pinned: &[SavedWorkspace],
    host: impl Fn(&str) -> String,
) -> Vec<super::menubar::Pinned> {
    pinned
        .iter()
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
