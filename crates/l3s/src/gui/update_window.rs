//! The native Update window: live OTA status with a retry action.
//!
//! Opened when an update check or download fails, and also usable for any
//! status. It observes the shared [`Updater`] entity, so retries and progress
//! repaint without reopening.

use gpui::{
    App, Context, Entity, FocusHandle, KeyDownEvent, Render, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use gpui_updater_core::UpdateStatus;

use super::theme::Theme;
use super::updater::Updater;
use super::{CloseTab, CloseWindow, human_bytes};

/// Width and height of the update window.
const WINDOW_SIZE: (f32, f32) = (460.0, 240.0);

/// Which action the primary button performs for the current status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Start a check.
    Check,
    /// Retry the failed check or download.
    Retry,
    /// Download the available release.
    Download,
    /// Relaunch into the staged update.
    Restart,
}

/// The Update window root view.
pub struct UpdateWindow {
    updater: Entity<Updater>,
    theme: Theme,
    focus_handle: FocusHandle,
}

impl UpdateWindow {
    fn new(
        updater: Entity<Updater>,
        theme: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        // Repaint whenever the shared updater changes state.
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();
        Self {
            updater,
            theme,
            focus_handle,
        }
    }
}

impl Render for UpdateWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let status = self.updater.read(cx).status().clone();
        let (heading, detail, action): (String, String, Option<(Action, &'static str)>) =
            match &status {
                UpdateStatus::Errored(error) => (
                    "Update failed".to_string(),
                    error.clone(),
                    Some((Action::Retry, "Retry")),
                ),
                UpdateStatus::Available(version) => (
                    "Update available".to_string(),
                    format!("Version {version} is ready to download."),
                    Some((Action::Download, "Download")),
                ),
                UpdateStatus::Downloading { downloaded, total } => {
                    let total = total.map_or_else(|| "…".to_string(), human_bytes);
                    (
                        "Downloading update".to_string(),
                        format!("{} / {total}", human_bytes(*downloaded)),
                        None,
                    )
                }
                UpdateStatus::Installing => (
                    "Installing update".to_string(),
                    "Applying the update…".to_string(),
                    None,
                ),
                UpdateStatus::Staged(version) => (
                    "Restart to install".to_string(),
                    format!("Version {version} is staged."),
                    Some((Action::Restart, "Restart")),
                ),
                UpdateStatus::Checking => (
                    "Checking for updates".to_string(),
                    "Contacting the release feed…".to_string(),
                    None,
                ),
                UpdateStatus::UpToDate => (
                    "You are up to date".to_string(),
                    "No newer release was found.".to_string(),
                    Some((Action::Check, "Check again")),
                ),
                UpdateStatus::Idle => (
                    "Updates".to_string(),
                    "Check the release feed for a newer version.".to_string(),
                    Some((Action::Check, "Check for updates")),
                ),
            };

        let button = action.map(|(action, label)| {
            div()
                .id("l3s-update-action")
                .flex()
                .items_center()
                .justify_center()
                .h(px(30.0))
                .px_4()
                .rounded_md()
                .bg(theme.accent)
                .text_color(theme.window_bg)
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::BOLD)
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match action {
                        Action::Check | Action::Retry => {
                            this.updater.update(cx, |updater, cx| updater.check(cx));
                        }
                        Action::Download => {
                            this.updater
                                .update(cx, |updater, cx| updater.download_and_install(cx));
                        }
                        Action::Restart => {
                            this.updater.update(cx, |updater, cx| updater.restart(cx));
                        }
                    }
                    cx.notify();
                }))
        });

        div()
            .id("l3s-update-window")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .on_key_down(|event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_6()
                    .pt_6()
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(heading),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(detail),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_6()
                    .pb_6()
                    .children(button),
            )
    }
}

/// Whether an Update window is already open.
pub fn is_open(cx: &App) -> bool {
    cx.windows()
        .iter()
        .any(|handle| handle.downcast::<UpdateWindow>().is_some())
}

/// Open the Update window for a shared updater entity.
pub fn open(updater: Entity<Updater>, theme: Theme, cx: &mut App) {
    let bounds = gpui::Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Updates".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(move |cx| UpdateWindow::new(updater, theme, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("l3s: could not open the update window: {error}");
    }
}
