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

use super::machines::AddMachineView;
use super::theme::Theme;
use super::update_window::UpdateWindow;
use super::updater::Updater;
use super::{MachineSignal, PilusView};
use crate::settings::Settings;

/// Where the captured frames are written.
const OUT: &str = "/tmp/pilus-shots";

/// Width of the standalone main-window captures; `PILUS_SHOT_WIDTH` narrows
/// them to exercise compact layouts.
fn shot_width() -> f32 {
    std::env::var("PILUS_SHOT_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1240.0)
}

/// Fresh settings for one captured window. `PILUS_SHOT_FONT_SIZE` overrides the
/// Interface size so scaling can be reviewed without editing the app;
/// `PILUS_SHOT_RADIUS` does the same for the corner radius, and
/// `PILUS_SHOT_OPACITY` for the window.
fn settings() -> Rc<RefCell<Settings>> {
    let mut settings = Settings::default();
    if let Some(size) = std::env::var("PILUS_SHOT_FONT_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        settings.ui_font_size = size;
    }
    if let Some(radius) = std::env::var("PILUS_SHOT_RADIUS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        settings.corner_radius = radius;
    }
    if let Some(opacity) = std::env::var("PILUS_SHOT_OPACITY")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        settings.opacity = opacity;
    }
    super::style::sync_corner_radius(&settings);
    Rc::new(RefCell::new(settings))
}

fn context() -> Result<HeadlessAppContext> {
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    // Shots capture a frame or two after a panel opens; with motion on they
    // would catch fades half-way and spinners mid-turn.
    cx.update(|app| {
        super::fonts::install(app.text_system());
        app.set_reduce_motion(true);
    });
    Ok(cx)
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
        "terminal-glyphs",
        560.0,
        440.0,
        Duration::ZERO,
        |_, cx| cx.new(|_| GlyphGridView),
    )?;
    for (name, id) in [
        ("panes-dawn", "rose-pine-dawn"),
        ("panes-noir", "noir"),
        ("panes-dracula", "dracula"),
        ("panes-paper", "paper"),
    ] {
        let theme = super::scheme::builtin(id).map_or_else(Theme::default, Theme::from_palette);
        shoot(&mut cx, name, 560.0, 420.0, Duration::ZERO, move |_, cx| {
            cx.new(|_| PaneGridView { theme })
        })?;
    }
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
        "add-machine",
        560.0,
        300.0,
        Duration::from_millis(400),
        |window, cx| {
            let root = pilus_client::config_dir();
            cx.new(|cx| {
                AddMachineView::new(
                    root,
                    None,
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
                    pilus_client::ControlTransport::Socket(pilus_client::api_socket_path_for(
                        None::<&str>,
                    )),
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
    // The Tasks page, seeded with one task per interesting status: the list,
    // a selected task's detail pane, and the new-task composer. It needs no
    // daemon, so it rides the standalone captures.
    {
        let cell: Rc<RefCell<Option<gpui::Entity<PilusView>>>> = Rc::new(RefCell::new(None));
        let slot = cell.clone();
        let window = cx
            .open_window(size(px(shot_width()), px(780.0)), move |window, cx| {
                let view = cx.new(move |cx| {
                    PilusView::new(
                        pilus_client::ConnectTarget::local(),
                        None,
                        settings(),
                        Rc::new(RefCell::new(MachineSignal::default())),
                        Rc::new(RefCell::new(Theme::default())),
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(view.clone());
                view
            })
            .context("open tasks")?;
        let view = cell.borrow().clone().context("tasks view captured")?;
        // Let start-up connection churn finish; it respawns the live feed.
        settle(&mut cx, 40);
        cx.update(|app| {
            view.update(app, |view, cx| {
                // Detach the live feed, or the local daemon's own tasks
                // would replace the sample.
                view.agent_feed = None;
                view.agents_loaded = true;
                view.tasks = sample_tasks();
                view.open_tasks(cx);
            });
        });
        settle(&mut cx, 4);
        capture(&mut cx, window.into(), "tasks")?;
        cx.update(|app| view.update(app, |view, cx| view.tasks_panel_select("t3", cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "tasks-detail")?;
        cx.update(|app| view.update(app, |view, cx| view.tasks_panel_compose(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "tasks-compose")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.tasks_panel_pick(super::task_compose::TaskPicker::Deps, cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "tasks-compose-deps")?;
        // The list's own machine picker, with the composer closed.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.tasks_panel_pick(super::task_compose::TaskPicker::Deps, cx);
                view.tasks_panel_pick(super::task_compose::TaskPicker::ScopeMachine, cx);
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "tasks-scope-machine")?;
        // The same queue as the Orchestration board, then a waiting card
        // picked so its links light and the detail pane opens.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.tasks_panel_pick(super::task_compose::TaskPicker::ScopeMachine, cx);
                view.agents = sample_task_agents();
                view.tasks_panel.selected = None;
                view.open_orchestration(cx);
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "orchestration")?;
        cx.update(|app| view.update(app, |view, cx| view.tasks_panel_select("t6", cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "orchestration-detail")?;
        // Two cards folded to their title (the waiting one keeps its badge).
        cx.update(|app| {
            view.update(app, |view, cx| {
                for id in ["t4", "t5"] {
                    view.tasks_panel.folded.insert(id.to_string());
                }
                view.tasks_panel.selected = None;
                cx.notify();
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "orchestration-folded")?;
        // The Diff page on a canned review: stacked and split layouts, a
        // note being written, and the agent picker.
        cx.update(|app| view.update(app, |view, cx| view.diff_show_sample(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-auto")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.diff_show_mode(super::diff_model::LayoutMode::Split, cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-split")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.diff_show_mode(super::diff_model::LayoutMode::Stack, cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-stack")?;
        // The pointer over a line number reveals the add-comment button.
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(point(px(815.0), px(368.0)), cx);
        })
        .ok();
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-hover")?;
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(point(px(300.0), px(600.0)), cx);
        })
        .ok();
        cx.update(|app| view.update(app, |view, cx| view.diff_show_menu(false, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-menu-layout")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_show_menu(true, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-menu-filter")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_close_menu(cx)));
        cx.update(|app| view.update(app, |view, cx| view.diff_show_composer(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-compose")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_show_send(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-send")?;
        // A markdown file in the review: rendered with its notes in place,
        // then the form on a block, then the patch it stands for.
        cx.update(|app| view.update(app, |view, cx| view.diff_show_markdown_sample(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-markdown")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_comment_on_lines(21, 21, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-markdown-compose")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_show_mode_rendered(false, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "diff-markdown-patch")?;
        cx.update(|app| view.update(app, |view, cx| view.diff_show_mode_rendered(true, cx)));
        // The Git tab on a canned repository: a commit picked, with its
        // changes below the list.
        cx.update(|app| view.update(app, |view, cx| view.git_show_sample(Some("3e1a128"), cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "git-tab")?;
        // The first file folded down to its header.
        cx.update(|app| view.update(app, |view, cx| view.git_toggle_file_sample(0, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "git-tab-collapsed")?;
        // The column stretched over the whole window, then let back.
        cx.update(|app| view.update(app, |view, cx| view.toggle_right_expanded(cx)));
        settle(&mut cx, 30);
        capture(&mut cx, window.into(), "git-tab-expanded")?;
        cx.update(|app| view.update(app, |view, cx| view.toggle_right_expanded(cx)));
        settle(&mut cx, 30);
        // The Files tab on a canned tree, then with a folder folded here.
        cx.update(|app| view.update(app, |view, cx| view.files_show_sample(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "files-tab")?;
        // The tree's right-click menu on a file, then its delete confirm and
        // its rename prompt.
        let file = "/work/pilus/crates/pilus-client/src/lib.rs";
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.files_open_menu(
                    file.to_string(),
                    false,
                    false,
                    point(px(930.0), px(330.0)),
                    cx,
                )
            })
        });
        settle(&mut cx, 8);
        capture(&mut cx, window.into(), "files-tab-menu")?;
        let target = super::sidebar_menu::MenuTarget::File {
            endpoint: "local".to_string(),
            path: file.to_string(),
            dir: false,
            root: false,
        };
        for (item, name) in [
            (super::sidebar_menu::Item::Delete, "files-tab-delete"),
            (super::sidebar_menu::Item::Rename, "files-tab-rename"),
        ] {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    view.close_context_menu();
                    view.files_menu_item(&target, item, cx)
                })
            });
            settle(&mut cx, 8);
            capture(&mut cx, window.into(), name)?;
        }
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.menu_prompt = None;
                cx.notify();
            })
        });
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.files_fold_sample("/work/pilus/crates/pilus-client/src", cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "files-tab-folded")?;
        // A markdown file open as a document in place of the tree, with
        // Back lit once the column has somewhere to go back to.
        cx.update(|app| view.update(app, |view, cx| view.files_show_doc_sample(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "files-tab-doc")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.select_right_tab(super::right_sidebar::SidebarTab::Git, cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "right-sidebar-back-lit")?;
        // The Ports tab on a machine, with a forward starting and one failed.
        cx.update(|app| view.update(app, |view, cx| view.ports_show_sample(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "ports-tab")?;
        // The confirm that guards killing the process behind a port.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.open_menu_prompt(
                    super::PromptAction::KillProcess {
                        endpoint: super::EndpointId::Local,
                        pid: 5302,
                        port: 8080,
                        name: "python3".to_string(),
                        label: "pid 5302 · python3 -m http.server 8080".to_string(),
                    },
                    "Kill process",
                    "",
                    "",
                    cx,
                )
            })
        });
        settle(&mut cx, 8);
        capture(&mut cx, window.into(), "ports-kill-confirm")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.menu_prompt = None;
                cx.notify();
            })
        });
        // The GitHub tab: both lists, an issue and a pull request with their
        // threads, the submit form, and the sentence shown when `gh` is not
        // signed in.
        for (scene, name) in [
            (super::github_tab::Scene::Issues, "github-issues"),
            (super::github_tab::Scene::Issue, "github-issue"),
            (super::github_tab::Scene::Pulls, "github-pulls"),
            (super::github_tab::Scene::Pull, "github-pull"),
            (super::github_tab::Scene::NewPull, "github-new-pull"),
            (super::github_tab::Scene::SignedOut, "github-signed-out"),
        ] {
            cx.update(|app| view.update(app, |view, cx| view.github_show_sample(scene, cx)));
            settle(&mut cx, 12);
            capture(&mut cx, window.into(), name)?;
        }
        cx.update(|app| view.update(app, |view, cx| view.github_clear_sample(cx)));
        // The narrowest column: the tabs scroll, the buttons stay.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.right.width = super::right_sidebar::MIN_WIDTH;
                cx.notify();
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "ports-tab-narrow")?;
        cx.update(|app| view.update(app, |view, cx| view.ports_clear_sample(cx)));
        // A folder still opening: placeholders stand in for its children.
        cx.update(|app| view.update(app, |view, cx| view.files_show_sample(cx)));
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.files_loading_sample("/work/pilus/docs", cx)
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "files-tab-loading")?;
        // Later pages show the sidebar too; give it back to the Diff.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.right_show_tab(super::right_sidebar::SidebarTab::Diff, cx)
            })
        });
        // Mission Control on a canned dashboard: the roster with a blocked
        // agent selected and its reply field.
        cx.update(|app| {
            view.update(app, |view, cx| {
                let (snapshot, hints) = sample_mission();
                view.mission_show(snapshot, hints, cx);
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "mission")?;
        // The roster with the side pane closed: the widest it gets, where the
        // context column takes the slack and location holds its width.
        cx.update(|app| view.update(app, |view, cx| view.mission_show_detail(false, cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "mission-wide")?;
        cx.update(|app| view.update(app, |view, cx| view.mission_show_detail(true, cx)));
        settle(&mut cx, 12);
        cx.update(|app| view.update(app, |view, cx| view.mission_show_answer(cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "mission-answer")?;
        // The page beside the terminal, each way.
        cx.update(|app| view.update(app, |view, cx| view.page_split_show("right", cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "mission-split-right")?;
        cx.update(|app| view.update(app, |view, cx| view.page_split_show("down", cx)));
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "mission-split-down")?;
        // The same axis again closes the split, so the Automations page
        // below starts whole.
        cx.update(|app| view.update(app, |view, cx| view.page_split_show("down", cx)));
        // The Automations page on the same canned definitions, then with
        // one selected and its detail loaded.
        cx.update(|app| {
            view.update(app, |view, cx| {
                let (snapshot, _) = sample_mission();
                view.automations_show(snapshot.automation, snapshot.automations, None, cx);
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "automations")?;
        cx.update(|app| {
            view.update(app, |view, cx| {
                let (snapshot, _) = sample_mission();
                view.automations_show(
                    snapshot.automation,
                    snapshot.automations,
                    Some(sample_automation()),
                    cx,
                );
            })
        });
        settle(&mut cx, 12);
        capture(&mut cx, window.into(), "automations-detail")?;
        // A saved machine beside Local, so the picker has a list to show.
        // The machine probe replaces the catalog, so capture promptly.
        cx.update(|app| {
            view.update(app, |view, cx| {
                view.machines.push(pilus_client::MachineInfo {
                    id: "build-box".to_string(),
                    display: "build-box".to_string(),
                    destination: "dev@build-box".to_string(),
                    enabled: true,
                    online: Some(true),
                    ..pilus_client::MachineInfo::default()
                });
                view.automations_show_machines(cx);
            })
        });
        settle(&mut cx, 3);
        capture(&mut cx, window.into(), "automations-machine")?;
    }
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
        .open_window(size(px(shot_width()), px(780.0)), move |window, cx| {
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
    cx.update(|app| {
        view.update(app, |view, _| view.palette.close());
    });

    // Toasts stack at the bottom-right: a daemon push with its session as the
    // title, a success, and a long error that clamps. The test window is never
    // frontmost, so their clocks stay frozen for the capture.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.toasts.push(
                super::toast::ToastKind::Info,
                Some("quiet-comet".to_string()),
                "Build finished in 42s".to_string(),
            );
            view.toast(super::toast::ToastKind::Success, "Opened the workspace");
            view.toast(
                super::toast::ToastKind::Error,
                "Couldn't open workspace: the folder no longer exists on this machine",
            );
            cx.notify();
        });
    });
    settle(&mut cx, 30);
    capture(&mut cx, window.into(), "main-toasts")?;
    cx.update(|app| {
        view.update(app, |view, _| view.toasts = super::toast::Toasts::new());
    });

    // The terminal's own right-click menu, over a selection and without one.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.palette.close();
            view.open_context_menu(
                super::sidebar_menu::MenuTarget::Terminal {
                    selection: true,
                    agent: true,
                },
                point(px(520.0), px(260.0)),
                cx,
            );
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-terminal-menu")?;
    cx.update(|app| view.update(app, |view, _| view.close_context_menu()));

    // The session menu: a running session with its stop control, a stopped
    // one with delete, and a delete waiting on its second press.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.sessions = vec![
                pilus_client::SessionInfo {
                    name: "default".to_string(),
                    running: true,
                },
                pilus_client::SessionInfo {
                    name: "brave-falcon".to_string(),
                    running: true,
                },
                pilus_client::SessionInfo {
                    name: "quiet-otter".to_string(),
                    running: false,
                },
                pilus_client::SessionInfo {
                    name: "lucky-comet".to_string(),
                    running: false,
                },
            ];
            view.settings.set_session_label("brave-falcon", "Work");
            view.session_menu_open = true;
            // Hold the slow disk poll off so the canned list stands.
            view.last_session_poll = std::time::Instant::now();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-session-menu")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.session_confirm = Some((
                "quiet-otter".to_string(),
                super::session_ops::SessionVerb::Delete,
            ));
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-session-menu-confirm")?;
    cx.update(|app| view.update(app, |view, _| view.close_session_menu()));
    // The rename prompt a session row's pencil opens.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.open_session_rename("brave-falcon", cx);
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-session-rename")?;
    cx.update(|app| view.update(app, |view, _| view.menu_prompt = None));

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

    // The Settings page: machines, about pilus, about luvus. Remote access is
    // shown running with a live pairing code and two paired devices, the state
    // with the most to draw.
    super::connector::preview(remote_access_fixture(true));
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.remote = super::connector::snapshot();
            view.open_settings(cx);
        });
    });
    settle(&mut cx, 12);
    capture(&mut cx, window.into(), "main-settings")?;
    // Remote access with the service waiting on Luvus and no code on screen.
    super::connector::preview(remote_access_fixture(false));
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.remote = super::connector::snapshot();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-remote-idle")?;
    // The service log open, with the server down: the row that says what to do.
    let mut logged = remote_access_fixture(false);
    logged.log_open = true;
    logged.log = SERVICE_LOG.iter().map(|line| (*line).to_string()).collect();
    super::connector::preview(logged);
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.remote = super::connector::snapshot();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-remote-log")?;
    // Every warning at once: Luvus missing, an update waiting, and a Linux
    // service that stops at logout.
    let mut stacked = remote_access_fixture(false);
    if let Some(status) = stacked.status.as_mut() {
        status.gateway_error = Some("could not find the `luvus` binary".to_string());
        status.restart_needed = true;
        status.linger = Some(false);
    }
    super::connector::preview(stacked);
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.remote = super::connector::snapshot();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-remote-warnings")?;
    super::connector::preview(super::connector::RemoteAccess::default());
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.remote = super::connector::snapshot();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-remote-off")?;
    // The Relay section: a dedicated relay saved with its (fake) secret, then
    // the secret field mid-edit.
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.settings.relay = pilus_client::RelaySettings {
                secret: Some("fixture-secret".to_string()),
                url: vec![
                    "https://use1-1.relay.example.iroh.link".to_string(),
                    "https://euc1-1.relay.example.iroh.link".to_string(),
                ],
            };
            *view.shared_settings.borrow_mut() = view.settings.clone();
            cx.notify();
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-relay")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.edit_relay(super::RelayField::Secret, cx);
        });
    });
    settle(&mut cx, 8);
    capture(&mut cx, window.into(), "main-settings-relay-editing")?;
    cx.update(|app| {
        view.update(app, |view, cx| {
            view.relay_editing = None;
            view.settings.relay = pilus_client::RelaySettings::default();
            *view.shared_settings.borrow_mut() = view.settings.clone();
            cx.notify();
        });
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

/// The lines the glyph grid paints: the Claude Code logo, box drawing, the
/// block quadrants fonts often lack, wide fallbacks, and descenders.
const GLYPH_LINES: [&str; 14] = [
    " ▐▛███▜▌   Claude Code v2.0",
    "▝▜█████▛▘  Opus 4.5 · Claude Max",
    "  ▘▘ ▝▝    ~/projects/pilus",
    "╭──────────────╮ ⏺ ✻ ⎿ … ● → ✓ ✗ ❯ ⚡",
    "│ gjpqy ÄÉÅ @W │ ▀▄█▌▐░▒▓ ▖▗▘▙▚▛▜▝▞▟",
    "╰──────────────╯ ━┃┏┓┗┛ ╔═╗ ▔▁▏▕",
    "italic: The quick brown fox jumps",
    "bold:   The quick brown fox jumps",
    "wide:   日本語 テスト 🙂 ok",
    "  ⎿  Read 42 lines (ctrl+o to expand)",
    "┌─┬─┐ ╭──╮ ┏━┳━┓ ╔═╦═╗ ╓─╖ ┄┄ ┈┈ ╌╌ ╱╲",
    "│ │ │ │  │ ┃ ┃ ┃ ║ ║ ║ ║ ║ ┆  ┊  ╎  ╲╱",
    "├─┼─┤ │  │ ┣━╋━┫ ╠═╬═╣ ╟─╢ ┆  ┊  ╎  ╳",
    "└─┴─┘ ╰──╯ ┗━┻━┛ ╚═╩═╝ ╙─╜ ┘└ ╴╶ ╼╾",
];

/// A terminal surface painted through [`super::render_grid`] from a synthetic
/// frame, so glyph clipping can be reviewed without a daemon.
struct GlyphGridView;

impl Render for GlyphGridView {
    fn render(
        &mut self,
        window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        use pilus_protocol::{CellData, FrameData};
        let theme = Theme::default();
        let settings = Settings::default();
        let font_size = settings.terminal_font_size;
        let width: u16 = 44;
        let skip_rows = super::SKIPPED_DAEMON_ROWS;
        let lines: Vec<&str> = std::iter::repeat_n("", usize::from(skip_rows))
            .chain(GLYPH_LINES)
            .chain(std::iter::repeat_n(
                "",
                usize::from(super::SKIPPED_DAEMON_BOTTOM_ROWS),
            ))
            .collect();
        let height = lines.len() as u16;
        let mut cells = Vec::with_capacity(usize::from(width) * usize::from(height));
        for (row, line) in lines.iter().enumerate() {
            let logo = (2..5).contains(&row);
            let mods = match row {
                8 => super::M_ITALIC,
                9 => super::M_BOLD,
                _ => 0,
            };
            let mut column = 0u16;
            for grapheme in line.chars() {
                let symbol = grapheme.to_string();
                let wide = pilus_protocol::symbol_width(&symbol) >= 2;
                let fg = if logo && column < 10 {
                    (2 << 24) | 0x00d7_7757
                } else {
                    0
                };
                cells.push(CellData {
                    symbol,
                    fg,
                    bg: 0,
                    mods,
                });
                column += 1;
                if wide {
                    cells.push(CellData {
                        symbol: String::new(),
                        fg,
                        bg: 0,
                        mods,
                    });
                    column += 1;
                }
            }
            while column < width {
                cells.push(CellData::blank(0, 0));
                column += 1;
            }
            cells.truncate((row + 1) * usize::from(width));
        }
        let frame = FrameData {
            width,
            height,
            cells,
            hyperlinks: Vec::new(),
            cursor: None,
            cursor_visible: false,
        };
        let cell_height = (font_size * super::LINE_HEIGHT_RATIO).round().max(1.0);
        let cell_width = grid_cell_width(window, &settings);
        gpui::div()
            .size_full()
            .bg(theme.window_bg)
            .p_4()
            .child(super::render_grid(
                &frame,
                theme,
                font_size,
                settings.terminal_font.as_str(),
                px(cell_height),
                None,
                cell_width,
                0,
                skip_rows,
                width,
                height - skip_rows - super::SKIPPED_DAEMON_BOTTOM_ROWS,
                1.0,
                0.0,
                0.0,
                true,
                None,
            ))
    }
}

/// The width of one terminal cell in the configured terminal font.
fn grid_cell_width(window: &Window, settings: &Settings) -> f32 {
    window
        .text_system()
        .shape_line(
            "M".into(),
            px(settings.terminal_font_size),
            &[gpui::TextRun {
                len: 1,
                font: gpui::font(settings.terminal_font.as_str()),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
        .into()
}

/// A three-pane split painted through [`super::render_grid`] from a synthetic
/// frame, with the caret in the left pane, so the pane surface, frames, and
/// the active pane's inner shadow can be reviewed in any scheme without a
/// daemon.
struct PaneGridView {
    theme: Theme,
}

impl Render for PaneGridView {
    fn render(
        &mut self,
        window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        use pilus_protocol::FrameData;
        let theme = self.theme;
        let settings = Settings::default();
        let skip_rows = super::SKIPPED_DAEMON_ROWS;
        let body: u16 = 16;
        let (width, height) = (60u16, skip_rows + body + super::SKIPPED_DAEMON_BOTTOM_ROWS);
        let mut frame = FrameData::blank(width, height, 0, 0);
        let mut put = |column: u16, row: u16, text: &str| {
            for (offset, c) in text.chars().enumerate() {
                frame.cells[usize::from(row) * usize::from(width) + usize::from(column) + offset]
                    .symbol = c.to_string();
            }
        };
        let top = skip_rows;
        let bottom = skip_rows + body - 1;
        let middle = skip_rows + body / 2;
        let boxed = |put: &mut dyn FnMut(u16, u16, &str), l: u16, t: u16, r: u16, b: u16| {
            let line = |c: char, n: u16| c.to_string().repeat(usize::from(n));
            put(l, t, &format!("┌{}┐", line('─', r - l - 1)));
            put(l, b, &format!("└{}┘", line('─', r - l - 1)));
            for row in t + 1..b {
                put(l, row, "│");
                put(r, row, "│");
            }
        };
        boxed(&mut put, 0, top, 29, bottom);
        boxed(&mut put, 30, top, 59, middle - 1);
        boxed(&mut put, 30, middle, 59, bottom);
        put(2, top + 1, "~/pilus $ cargo test");
        put(2, top + 2, "   Compiling pilus v0.1.0");
        put(2, top + 3, "test result: ok. 214 passed");
        put(2, top + 4, "~/pilus $");
        put(32, top + 1, "~/pilus $ git status");
        put(32, middle + 1, "~/pilus $ tail -f log");
        frame.cursor = Some((12, top + 4));
        frame.cursor_visible = true;
        let font_size = settings.terminal_font_size;
        let cell_height = (font_size * super::LINE_HEIGHT_RATIO).round().max(1.0);
        let cell_width = grid_cell_width(window, &settings);
        // The window's own glass is opaque here: cards in the chrome fill on
        // the tinted surface.
        let card = theme.chrome_cell(1.0);
        let grid_theme = Theme {
            bg: card,
            remap_bg: Some(theme.bg),
            ..theme
        };
        gpui::div()
            .size_full()
            .bg(theme.window_bg)
            .p_4()
            .child(super::render_grid(
                &frame,
                grid_theme,
                font_size,
                settings.terminal_font.as_str(),
                px(cell_height),
                None,
                cell_width,
                0,
                skip_rows,
                width,
                body,
                1.0,
                // A window is rarely a whole number of cells, and the
                // remainder is surface the cards must grow over.
                5.0,
                9.0,
                true,
                Some(
                    super::style::pane_toolbar()
                        .child(super::style::icon_button(
                            "shot-pane-split-right",
                            include_bytes!("../../assets/icons/columns-2.svg"),
                            super::style::ButtonKind::Ghost,
                            theme,
                            24.0,
                        ))
                        .child(super::style::icon_button(
                            "shot-pane-split-down",
                            include_bytes!("../../assets/icons/rows-2.svg"),
                            super::style::ButtonKind::Ghost,
                            theme,
                            24.0,
                        ))
                        .child(super::style::icon_button(
                            "shot-pane-fullscreen",
                            include_bytes!("../../assets/icons/maximize.svg"),
                            super::style::ButtonKind::Ghost,
                            theme,
                            24.0,
                        ))
                        .child(super::style::icon_button(
                            "shot-pane-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            super::style::ButtonKind::Danger,
                            theme,
                            24.0,
                        )),
                ),
            ))
    }
}

/// A queue spanning the lifecycle, for the Tasks page captures.
fn sample_tasks() -> pilus_client::TaskSummary {
    use pilus_client::{Task, TaskStatus};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let task = |id: &str, title: &str, status, age: u64| Task {
        id: id.to_string(),
        title: title.to_string(),
        status: Some(status),
        updated: now.saturating_sub(age),
        created: now.saturating_sub(age + 600),
        attempt: 1,
        project_root: Some("/Users/me/src/pilus".into()),
        ..Task::default()
    };
    let items = vec![
        Task {
            assignee: Some(4),
            paths: vec!["src/auth/**".into(), "tests/auth/**".into()],
            gate: Some("cargo test auth".into()),
            prompt: Some(
                "Move session refresh behind the new token store and cover expiry.".into(),
            ),
            context_percent: Some(46),
            mode: Some("worktree".into()),
            branch: Some("task/t3".into()),
            ..task("t3", "Refactor session refresh", TaskStatus::Running, 240)
        },
        Task {
            deps: vec!["t3".into()],
            paths: vec!["src/api/**".into()],
            ..task("t4", "Rate-limit the public API", TaskStatus::Queued, 3_600)
        },
        Task {
            deps: vec!["t1".into()],
            prompt: Some("Write the token store's guide: rotation, expiry, and storage.".into()),
            ..task("t5", "Document the token store", TaskStatus::Queued, 7_200)
        },
        Task {
            assignee: Some(6),
            deps: vec!["t1".into()],
            outputs: vec!["1 test failed: auth::expiry".into()],
            gate: Some("cargo test auth".into()),
            prompt: Some("Expiry is computed one second early; fix it and add a test.".into()),
            ..task("t2", "Fix expiry off-by-one", TaskStatus::Review, 900)
        },
        Task {
            deps: vec!["t2".into(), "t3".into()],
            prompt: Some("Ship the refreshed auth flow once both fixes merge.".into()),
            ..task("t6", "Release auth 2.0", TaskStatus::Queued, 1_800)
        },
        Task {
            branch: Some("task/t1".into()),
            prompt: Some("Introduce a token store with pluggable backends.".into()),
            ..task("t1", "Add token store", TaskStatus::Merged, 86_400)
        },
    ];
    pilus_client::TaskSummary {
        open: 5,
        total: items.len() as u32,
        items,
    }
}

/// The agents working `sample_tasks`' claimed tasks.
fn sample_task_agents() -> Vec<pilus_client::Agent> {
    let agent = |pane: &str, kind: &str, status: &str| pilus_client::Agent {
        pane: pane.to_string(),
        tab: 1,
        agent: kind.to_string(),
        name: String::new(),
        status: status.to_string(),
        workspace_id: "ws1".to_string(),
        workspace_name: "pilus".to_string(),
        cwd: "~/src/pilus".to_string(),
        branch: None,
        focused: false,
        session: None,
        session_title: None,
    };
    vec![agent("4", "claude", "working"), agent("6", "codex", "done")]
}

/// A fleet with one agent per state, a resumable session, and two
/// automations, for the Mission Control and Automations captures.
fn sample_mission() -> (
    pilus_client::MissionSnapshot,
    std::collections::HashMap<String, String>,
) {
    use pilus_client::{
        AutomationHealth, AutomationRow, MissionRow, MissionSnapshot, MissionUsage,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let usage = |model: &str, tokens_in: u64, tokens_out: u64, context: f32, cost: f64| {
        Some(MissionUsage {
            model: model.to_string(),
            tokens_in,
            tokens_out,
            cache_tokens: tokens_in * 6,
            context: Some(context),
            cost_usd: Some(cost),
        })
    };
    let live = |pane: &str, agent: &str, state: &str, tab: u16, location: &str| MissionRow {
        pane: Some(pane.to_string()),
        agent: agent.to_string(),
        state: state.to_string(),
        workspace_id: "ws1".to_string(),
        workspace_name: "pilus".to_string(),
        tab: Some(tab),
        location: location.to_string(),
        usage: None,
    };
    let rows = vec![
        MissionRow {
            usage: usage("claude-opus-4-1", 9_800_000, 1_400_000, 0.91, 214.62),
            ..live("4", "claude", "blocked", 2, "core p1/2 · t3")
        },
        MissionRow {
            usage: usage("deepseek-chat-v3.1", 2_100_000, 380_000, 0.42, 18.4),
            ..live(
                "6",
                "codex",
                "working",
                3,
                "notes-and-research-for-the-quarterly-planning-review",
            )
        },
        MissionRow {
            usage: usage("claude-sonnet-4-5", 640_000, 92_000, 0.28, 3.1),
            ..live("7", "claude", "done", 3, "notes p2/2")
        },
        live("9", "pi", "idle", 1, "t1"),
        MissionRow {
            pane: None,
            agent: "copilot".to_string(),
            state: "idle".to_string(),
            workspace_id: "ws1".to_string(),
            workspace_name: "pilus".to_string(),
            tab: None,
            location: "—".to_string(),
            usage: None,
        },
    ];
    let snapshot = MissionSnapshot {
        refreshing: false,
        tokens: 14_412_000,
        cost_usd: 236.12,
        burn_usd_per_hour: Some(4.2),
        automation: AutomationHealth {
            definitions: 2,
            enabled: 1,
            scheduled: 1,
            running: 0,
            review: 0,
            failed: 1,
            next_run_at: Some(now + 3 * 3600 + 120),
        },
        automations: vec![
            AutomationRow {
                id: "a1".to_string(),
                name: "Morning review".to_string(),
                state: "scheduled".to_string(),
                next_run_at: Some(now + 3 * 3600 + 120),
                latest_error: None,
                agent_id: "codex".to_string(),
                enabled: true,
            },
            AutomationRow {
                id: "a2".to_string(),
                name: "Nightly dependency sweep".to_string(),
                state: "failed".to_string(),
                next_run_at: None,
                latest_error: Some("gate failed".to_string()),
                agent_id: "claude".to_string(),
                enabled: true,
            },
        ],
        rows,
    };
    let hints = std::collections::HashMap::from([(
        "4".to_string(),
        "Allow edit to crates/pilus/src/gui/mod.rs? (y/n)".to_string(),
    )]);
    (snapshot, hints)
}

/// One automation's full detail, for the Automations capture.
fn sample_automation() -> pilus_client::AutomationDetail {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    pilus_client::AutomationDetail {
        id: "a1".to_string(),
        name: "Morning review".to_string(),
        enabled: true,
        schedule: "Weekly on Mon, Tue, Wed, Thu, Fri at 08:00 (Asia/Makassar)".to_string(),
        policy: "Run latest missed · Skip overlapping".to_string(),
        target: "New worker".to_string(),
        title: "Review changes".to_string(),
        prompt: "Review the workspace and report risks.".to_string(),
        agent_id: "codex".to_string(),
        mode: "workspace".to_string(),
        access: "read_only".to_string(),
        paths: vec!["crates/**".to_string()],
        gate: Some("cargo test".to_string()),
        occurrences: (0..5).map(|day| now + 3 * 3600 + day * 86_400).collect(),
        runs: vec![
            pilus_client::AutomationRun {
                id: "r2".to_string(),
                status: "succeeded".to_string(),
                scheduled_at: now.saturating_sub(21 * 3600),
                ..pilus_client::AutomationRun::default()
            },
            pilus_client::AutomationRun {
                id: "r1".to_string(),
                status: "failed".to_string(),
                scheduled_at: now.saturating_sub(45 * 3600),
                error: Some("gate failed: 2 tests".to_string()),
                ..pilus_client::AutomationRun::default()
            },
        ],
    }
}

/// Advance the simulated clock and drain tasks, so polling loops run.
fn settle(cx: &mut HeadlessAppContext, ticks: usize) {
    for _ in 0..ticks {
        cx.advance_clock(Duration::from_millis(25));
        cx.run_until_parked();
    }
}

/// What a service that cannot find Luvus writes, for the log shot.
const SERVICE_LOG: &[&str] = &[
    "pilus-agent: endpoint 42eb007d41f5…",
    "pilus-agent: gateway is up",
    "pilus-agent: gateway stopped",
    "pilus-agent: audit: pilus companion (42eb007d) [operate] local task.add ok",
    "pilus-agent: audit: pilus companion (42eb007d) [operate] local task.start ok",
    "pilus-agent: watch closed for pilus companion: the device went away",
    "pilus-agent: gateway is down: no luvus server running",
    "pilus-agent: the Luvus server is not running; starting it",
];

/// Remote access as the service would report it, for the shots.
fn remote_access_fixture(with_code: bool) -> super::connector::RemoteAccess {
    use pilus_client::agent_service::{DeviceLevel, EnrolledDevice, PairingCode, ServiceStatus};

    let ticket = "endpointac76o2igihgsjizbgpc2ttxnwmgtgpizxd3d6u3vjrcnhttxlyubsbaaenuhi5dqom5c6l3bobztcljrfzzgk3dbpexg4mbonfzg62bonruw42zof4aqazcyn4gzf2ydaealn7jqbtjrcaiaycube64s5mbq";
    super::connector::RemoteAccess {
        phase: super::connector::Phase::Running,
        status: Some(ServiceStatus {
            label: "Studio Mac".to_string(),
            endpoint_id: "bfe7690641cd24a32133c5a9ceedb30d333d19b8f63f53754c44d3ce775e2819"
                .to_string(),
            ticket: ticket.to_string(),
            gateway_up: with_code,
            gateway_error: (!with_code).then(|| "no luvus server running".to_string()),
            devices: vec![
                EnrolledDevice {
                    endpoint_id: "42eb007d41f599573bd56074118311dfc46fa72913ba3d601d8faad7bc72fd51"
                        .to_string(),
                    label: "pilus companion".to_string(),
                    enrolled_at: 1_790_000_000,
                    level: DeviceLevel::Operate,
                },
                EnrolledDevice {
                    endpoint_id: "9160e4ed3762e43df37fdc3fb8c688916d11bd3eb0c121a6af69444c0000"
                        .to_string(),
                    label: String::new(),
                    enrolled_at: 1_790_100_000,
                    level: DeviceLevel::Control,
                },
            ],
            strict: false,
            restart_needed: false,
            linger: None,
        }),
        code: with_code.then(|| PairingCode {
            level: DeviceLevel::Control,
            code: "832F-21EF-0E12-382C".to_string(),
            expires_at: super::tasks::unix_now() + 9 * 60 + 42,
            link: format!("pilus://pair/{ticket}?code=832F-21EF-0E12-382C"),
        }),
        error: None,
        minting: false,
        log_open: false,
        log: Vec::new(),
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
