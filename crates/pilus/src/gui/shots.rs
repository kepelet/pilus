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
use gpui::{App, AppContext as _, HeadlessAppContext, Render, Window, point, px, size};

use super::machines::MachineManagerView;
use super::theme::Theme;
use super::update_window::UpdateWindow;
use super::updater::Updater;
use super::{MachineSignal, PilusView};
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
                    Some(connection),
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

    // A tab's hover card: full cwd plus the running process.
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(point(px(370.0), px(24.0)), cx);
    })
    .ok();
    settle(&mut cx, 30);
    capture(&mut cx, window.into(), "main-tab-tooltip")?;
    // Move the pointer off the tab strip so the card closes before the
    // overlay captures.
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(point(px(620.0), px(400.0)), cx);
    })
    .ok();

    // A crowded tab strip: tabs overflow into the scroll viewport while the
    // pinned `+` stays put, and tab 2 carries the branded agent icon. Only the
    // view's in-memory lists are swapped; the daemon is untouched.
    let saved_tabs: Rc<RefCell<Option<(Vec<pilus_client::Tab>, Vec<pilus_client::Agent>)>>> =
        Rc::new(RefCell::new(None));
    cx.update(|app| {
        view.update(app, |view, cx| {
            *saved_tabs.borrow_mut() = Some((view.tabs.clone(), view.agents.clone()));
            let workspace_id = view
                .surface
                .workspaces()
                .iter()
                .find(|workspace| workspace.active)
                .map(|workspace| workspace.id.clone())
                .unwrap_or_default();
            view.tabs = (1..=10)
                .map(|index| pilus_client::Tab {
                    index,
                    active: index == 1,
                    cwd: format!("~/Developer/project-{index}"),
                    process: if index == 2 { "devin" } else { "fish" }.to_string(),
                    kind: "panes".to_string(),
                    ..pilus_client::Tab::default()
                })
                .collect();
            view.agents = vec![
                pilus_client::Agent {
                    tab: 2,
                    agent: "devin".to_string(),
                    status: "working".to_string(),
                    workspace_id: workspace_id.clone(),
                    cwd: "~/Developer/project-2".to_string(),
                    ..pilus_client::Agent::default()
                },
                pilus_client::Agent {
                    tab: 3,
                    agent: "claude".to_string(),
                    status: "blocked".to_string(),
                    workspace_id: workspace_id.clone(),
                    cwd: "~/Developer/project-3".to_string(),
                    ..pilus_client::Agent::default()
                },
                pilus_client::Agent {
                    tab: 4,
                    agent: "codex".to_string(),
                    status: "done".to_string(),
                    workspace_id,
                    cwd: "~/Developer/project-4".to_string(),
                    ..pilus_client::Agent::default()
                },
            ];
            cx.notify();
        });
    });
    cx.run_until_parked();
    capture(&mut cx, window.into(), "main-tabs-overflow")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some((tabs, agents)) = saved_tabs.borrow_mut().take() {
                view.tabs = tabs;
                view.agents = agents;
            }
            cx.notify();
        });
    });
    cx.run_until_parked();

    // The command palette over the frame.
    cx.update(|app| {
        view.update(app, |view, _| view.palette.open());
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-palette")?;

    // The WORKSPACES row context menu, with the keyboard cursor on Close,
    // and its Quick Actions submenu underneath.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.palette.close();
            if let Some(workspace) = view.surface.workspaces().first() {
                view.open_context_menu(
                    super::sidebar_menu::MenuTarget::Workspace {
                        endpoint: view.active.key(),
                        workspace_id: workspace.id.clone(),
                        index: workspace.index,
                        name: workspace.name.clone(),
                        cwd: workspace.cwd.clone(),
                        branch: workspace.branch.clone(),
                    },
                    point(px(96.0), px(150.0)),
                    cx,
                );
                if let Some(menu) = view.context_menu.as_mut() {
                    menu.selected = Some(0);
                }
            }
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-workspace-menu")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some(menu) = view.context_menu.as_mut() {
                menu.quick_open = true;
                menu.selected = None;
            }
            cx.notify();
        });
    });
    cx.run_until_parked();
    capture(&mut cx, window.into(), "main-workspace-menu-copy")?;

    // Walk every WORKSPACES row and capture the first menu whose linked-
    // checkout probe confirms a worktree: the Delete Worktree row must render
    // there. The probe runs on a background thread and is drained by the poll
    // loop, so give it real time, then check through the view's own rows.
    let rows: Vec<(String, String, u16, String, String, Option<String>)> = cx.update(|app| {
        view.update(app, |view, _| {
            view.endpoint_workspaces(&view.active)
                .iter()
                .map(|workspace| {
                    (
                        workspace.id.clone(),
                        workspace.name.clone(),
                        workspace.index,
                        view.active.key(),
                        workspace.cwd.clone(),
                        workspace.branch.clone(),
                    )
                })
                .collect()
        })
    });
    let mut captured_delete = false;
    for (id, name, index, endpoint, cwd, branch) in rows {
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.open_context_menu(
                    super::sidebar_menu::MenuTarget::Workspace {
                        endpoint: endpoint.clone(),
                        workspace_id: id.clone(),
                        index,
                        name: name.clone(),
                        cwd: cwd.clone(),
                        branch: branch.clone(),
                    },
                    point(px(96.0), px(150.0)),
                    cx,
                );
            });
        });
        std::thread::sleep(Duration::from_millis(400));
        settle(&mut cx, 10);
        let shows_delete = cx.update(|app| {
            view.update(app, |view, _| {
                view.menu_rows()
                    .contains(&super::SidebarItem::DeleteWorktree)
            })
        });
        if shows_delete && !captured_delete {
            capture(&mut cx, window.into(), "main-workspace-menu-delete")?;
            captured_delete = true;
        }
        cx.update(|app| {
            view.update(app, |view, _| view.close_context_menu());
        });
        if captured_delete {
            break;
        }
    }

    // The AGENTS row context menu. The dev daemon may have no live agent, so
    // the row is injected like the crowded-tab capture does; the daemon is
    // untouched and the real list comes back on the next feed.
    let saved_agents: Rc<RefCell<Option<Vec<pilus_client::Agent>>>> = Rc::new(RefCell::new(None));
    cx.update(|app| {
        view.update(app, |view, cx| {
            if view.agents.is_empty() {
                let workspace_id = view
                    .surface
                    .workspaces()
                    .iter()
                    .find(|workspace| workspace.active)
                    .map(|workspace| workspace.id.clone())
                    .unwrap_or_default();
                *saved_agents.borrow_mut() = Some(std::mem::replace(
                    &mut view.agents,
                    vec![pilus_client::Agent {
                        agent: "claude".to_string(),
                        status: "working".to_string(),
                        workspace_id,
                        cwd: "~/Developer/project-2".to_string(),
                        ..pilus_client::Agent::default()
                    }],
                ));
                cx.notify();
            }
        });
    });
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some(agent) = view.agents.first() {
                view.open_context_menu(
                    super::sidebar_menu::MenuTarget::Agent {
                        endpoint: view.active.key(),
                        pane: agent.pane.clone(),
                        name: agent.name.clone(),
                    },
                    point(px(96.0), px(280.0)),
                    cx,
                );
            }
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-agent-menu")?;
    cx.update(|app| {
        view.update(app, |view, _| {
            view.close_context_menu();
            if let Some(agents) = saved_agents.borrow_mut().take() {
                view.agents = agents;
            }
        });
    });

    // The Delete Worktree confirm. The row only appears on confirmed linked
    // checkouts, so the cache is seeded for the first row and the confirm is
    // opened directly; the daemon is never asked to remove anything.
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some(workspace) = view.surface.workspaces().first() {
                let endpoint = view.active.clone();
                let path = view.expand_cwd(&endpoint.key(), &workspace.cwd);
                view.worktree_linked
                    .insert((endpoint.key(), workspace.id.clone()), Some(path.clone()));
                view.open_menu_prompt(
                    super::PromptAction::DeleteWorktree {
                        endpoint,
                        path,
                        workspace_id: workspace.id.clone(),
                        name: workspace.name.clone(),
                        dirty: false,
                        blockers: None,
                    },
                    "Delete Worktree",
                    "",
                    "",
                    cx,
                );
            }
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-worktree-delete")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some(workspace) = view.surface.workspaces().first() {
                let endpoint = view.active.clone();
                let path = view.expand_cwd(&endpoint.key(), &workspace.cwd);
                view.open_menu_prompt(
                    super::PromptAction::DeleteWorktree {
                        endpoint,
                        path,
                        workspace_id: workspace.id.clone(),
                        name: workspace.name.clone(),
                        dirty: true,
                        blockers: None,
                    },
                    "Delete Worktree Anyway",
                    "",
                    "",
                    cx,
                );
            }
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-worktree-delete-dirty")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            if let Some(workspace) = view.surface.workspaces().first() {
                let endpoint = view.active.clone();
                let path = view.expand_cwd(&endpoint.key(), &workspace.cwd);
                view.open_menu_prompt(
                    super::PromptAction::DeleteWorktree {
                        endpoint,
                        path,
                        workspace_id: workspace.id.clone(),
                        name: workspace.name.clone(),
                        dirty: true,
                        blockers: Some("agent claude is working in pane 7".to_string()),
                    },
                    "Delete Worktree Anyway",
                    "",
                    "",
                    cx,
                );
            }
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-worktree-delete-busy")?;
    cx.update(|app| {
        view.update(app, |view, _| view.menu_prompt = None);
    });

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
            view.home.open_dropdown(super::HomeField::Model);
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-home-dropdown")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.home.close_dropdown();
            view.home.open_dropdown(super::HomeField::Workspace);
            cx.notify();
        });
    });
    settle(&mut cx, 4);
    capture(&mut cx, window.into(), "main-home-workspace-dropdown")?;
    cx.update(|app| {
        view.update(app, |view, _| view.home.close_dropdown());
    });

    // The Settings page: machines, agent config, updates.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.open_settings(cx);
        });
    });
    settle(&mut cx, 12);
    capture(&mut cx, window.into(), "main-settings")?;

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
        view.update(app, |view, cx| {
            view.close_picker();
            cx.notify();
        });
    });

    // The session menu.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.session_menu_open = true;
            cx.notify();
        });
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
