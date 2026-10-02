//! Headless screenshots of every native surface, for visual review.
//!
//! Not part of a normal build. Run with the `visual-test` feature:
//!
//! ```sh
//! cargo run -p pilus --features visual-test -- --ui-shots
//! cargo run -p pilus --features visual-test -- --ui-shots-main   # local daemon
//! ```
//!
//! PNGs land in `/tmp/pilus-shots`. The capture runs on the main thread, which
//! macOS requires before it will hand out a Metal device.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, HeadlessAppContext, Render, Window, px, size};

use super::machines::MachineManagerView;
use super::theme::Theme;
use super::update_window::UpdateWindow;
use super::updater::Updater;
use super::{PilusView, MachineSignal};
use crate::settings::Settings;

/// Where the captured frames are written.
const OUT: &str = "/tmp/pilus-shots";

/// Fresh settings for one captured window. `PILUS_SHOT_FONT_SIZE` overrides the
/// Interface size so scaling can be reviewed without editing the app.
fn settings() -> Rc<RefCell<Settings>> {
    let mut settings = Settings::default();
    if let Some(size) = std::env::var("PILUS_SHOT_FONT_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        settings.ui_font_size = size;
    }
    Rc::new(RefCell::new(settings))
}

fn context() -> Result<HeadlessAppContext> {
    let platform = gpui_platform::current_platform(true);
    Ok(HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    ))
}

fn shoot<V: Render + 'static>(
    cx: &mut HeadlessAppContext,
    name: &str,
    width: f32,
    height: f32,
    wait: Duration,
    build: impl FnOnce(&mut Window, &mut App) -> gpui::Entity<V>,
) -> Result<()> {
    let window = cx
        .open_window(size(px(width), px(height)), build)
        .with_context(|| format!("open {name}"))?;
    // Windows that load from a background thread need real time to settle.
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    cx.run_until_parked();
    let image = cx
        .capture_screenshot(window.into())
        .with_context(|| format!("capture {name}"))?;
    std::fs::create_dir_all(OUT).context("create shots dir")?;
    image
        .save(format!("{OUT}/{name}.png"))
        .with_context(|| format!("save {name}"))?;
    println!(
        "shot: {OUT}/{name}.png ({}x{})",
        image.width(),
        image.height()
    );
    Ok(())
}

/// Capture every standalone native window.
pub fn run() -> Result<()> {
    let mut cx = context()?;
    shoot(
        &mut cx,
        "playground",
        820.0,
        3600.0,
        Duration::ZERO,
        |window, cx| {
            cx.new(|cx| {
                super::ui_playground::UiPlaygroundView::new(
                    Theme::default(),
                    settings(),
                    window,
                    cx,
                )
            })
        },
    )?;
    shoot(
        &mut cx,
        "about",
        420.0,
        360.0,
        Duration::ZERO,
        |window, cx| {
            cx.new(|cx| super::about::AboutView::new(Theme::default(), settings(), window, cx))
        },
    )?;
    shoot(
        &mut cx,
        "machines",
        820.0,
        600.0,
        Duration::from_millis(1500),
        |window, cx| {
            let root = pilus_client::config_dir();
            cx.new(|cx| {
                MachineManagerView::new(
                    root,
                    Rc::new(RefCell::new(MachineSignal::default())),
                    Rc::new(RefCell::new(Theme::default())),
                    settings(),
                    window,
                    cx,
                )
            })
        },
    )?;
    shoot(
        &mut cx,
        "appearances",
        640.0,
        600.0,
        Duration::from_millis(400),
        |window, cx| {
            cx.new(|cx| {
                super::appearances::AppearancesView::new(
                    pilus_client::api_socket_path_for(None::<&str>),
                    pilus_client::config_dir(),
                    settings(),
                    window,
                    cx,
                )
            })
        },
    )?;
    shoot(
        &mut cx,
        "update",
        460.0,
        240.0,
        Duration::ZERO,
        |window, cx| {
            let request = crate::update::UpdateRequest::from_settings("stable", None);
            let updater = cx.new(|cx| Updater::from_request(&request, cx));
            cx.new(|cx| UpdateWindow::new(updater, Theme::default(), settings(), window, cx))
        },
    )?;
    Ok(())
}

/// Capture the main window against the running local daemon, including every
/// overlay the window can show.
pub fn run_main() -> Result<()> {
    let view_cell = Rc::new(RefCell::new(None));
    let view_slot = view_cell.clone();
    let target = pilus_client::ConnectTarget::local();
    let connection = pilus_client::connect(&target, 120, 40).context("connect to local daemon")?;
    connection.request_workspace_dock().context("claim dock")?;

    let mut cx = context()?;
    let window = cx
        .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
            let view = cx.new(|cx| {
                PilusView::new(
                    target.clone(),
                    connection,
                    settings(),
                    Rc::new(RefCell::new(MachineSignal::default())),
                    Rc::new(RefCell::new(Theme::default())),
                    window,
                    cx,
                )
            });
            *view_slot.borrow_mut() = Some(view.clone());
            view
        })
        .context("open main window")?;
    let view = view_cell.borrow().clone().context("view captured")?;

    // The reader thread delivers frames over a channel; the view's polling
    // task only sees them when the clock advances and tasks are drained.
    settle(&mut cx, 80);
    capture(&mut cx, window.into(), "main")?;

    // The command palette over the frame.
    cx.update(|app| {
        view.update(app, |view, _| view.palette.open());
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-palette")?;

    // The Home composer.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.palette.close();
            view.open_home(cx);
        });
    });
    settle(&mut cx, 12);
    capture(&mut cx, window.into(), "main-home")?;

    // A Home picker dropdown open: it floats above the composer, so the
    // centered layout does not shift when it opens.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.home.open_dropdown(super::HomeField::Agent);
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-home-dropdown")?;
    cx.update(|app| {
        view.update(app, |view, _| view.home.close_dropdown());
    });

    // The Settings page: projects, machines, agent config, updates.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.open_settings(cx);
        });
    });
    settle(&mut cx, 12);
    capture(&mut cx, window.into(), "main-settings")?;

    // The New Project form on the settings page.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.projects.start_new();
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-project-form")?;
    cx.update(|app| {
        view.update(app, |view, _| view.projects.cancel_form());
    });

    // The Open Workspace folder picker. Its listing happens on a thread, so
    // give it real time before the simulated clock drains the result.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.leave_settings_page();
            view.open_picker(cx);
        });
    });
    std::thread::sleep(Duration::from_millis(800));
    settle(&mut cx, 20);
    capture(&mut cx, window.into(), "main-picker")?;
    cx.update(|app| {
        view.update(app, |view, _| view.close_picker());
    });

    // The footer session menu.
    cx.update(|app| {
        view.update(app, |view, _| view.session_menu_open = true);
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-session-menu")?;

    // The same window under a light built-in scheme, so hard-coded chrome
    // colors (borders, scrims) show up.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.session_menu_open = false;
            view.appearance.active = "catppuccin-latte".to_string();
            view.apply_appearance();
            cx.notify();
        });
    });
    cx.run_until_parked();
    capture(&mut cx, window.into(), "main-latte")?;
    // Drop the entity handle held for the overlay captures, or the app's
    // leak detector reports it at shutdown.
    view_cell.borrow_mut().take();
    drop(view);
    Ok(())
}

/// Advance the simulated clock and drain tasks, so polling loops run.
fn settle(cx: &mut HeadlessAppContext, ticks: usize) {
    for _ in 0..ticks {
        cx.advance_clock(Duration::from_millis(25));
        cx.run_until_parked();
    }
}

fn capture(cx: &mut HeadlessAppContext, window: gpui::AnyWindowHandle, name: &str) -> Result<()> {
    let image = cx.capture_screenshot(window).context("capture")?;
    std::fs::create_dir_all(OUT).context("create shots dir")?;
    image.save(format!("{OUT}/{name}.png")).context("save")?;
    println!(
        "shot: {OUT}/{name}.png ({}x{})",
        image.width(),
        image.height()
    );
    Ok(())
}
