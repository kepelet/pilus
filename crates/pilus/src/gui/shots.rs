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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, HeadlessAppContext, Render, Window, point, px, size};

use super::machines::MachineManagerView;
use super::session::{HistoryEntry, PickerKind};
use super::theme::Theme;
use super::update_window::UpdateWindow;
use super::updater::Updater;
use super::{MachineSignal, PilusView};
use crate::settings::Settings;

/// Where the captured frames are written.
const OUT: &str = "/tmp/pilus-shots";

/// A deterministic session for the transcript shot, covering every item kind
/// the native window draws.
const SAMPLE_SESSION: &str = r#"{"type":"session","version":3,"id":"shot-0001","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/Users/dev/projects/api"}
{"type":"session_info","id":"n0","parentId":null,"timestamp":"2024-12-03T14:00:00.500Z","name":"Refactor auth module"}
{"type":"model_change","id":"m0","parentId":"n0","timestamp":"2024-12-03T14:00:01.000Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}
{"type":"thinking_level_change","id":"t0","parentId":"m0","timestamp":"2024-12-03T14:00:01.500Z","thinkingLevel":"high"}
{"type":"message","id":"u1","parentId":"m0","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"user","content":"Add a token refresh to the auth module and cover it with a test.","timestamp":1733234402000}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2024-12-03T14:00:04.000Z","message":{"role":"assistant","api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1200,"output":210,"cacheRead":800,"cacheWrite":0,"totalTokens":2210,"cost":{"input":0.0036,"output":0.0031,"cacheRead":0.0002,"cacheWrite":0,"total":0.0069}},"stopReason":"toolUse","timestamp":1733234404000,"content":[{"type":"thinking","thinking":"Read the auth module first, then add refresh."},{"type":"text","text":"I'll read the auth module before changing it."},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"src/auth.rs","offset":0,"limit":120}},{"type":"toolCall","id":"c3","name":"bash","arguments":{"command":"cargo test auth","timeout":30}}]}}
{"type":"message","id":"r1","parentId":"a1","timestamp":"2024-12-03T14:00:05.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","content":[{"type":"text","text":"pub struct Session {\n    token: String,\n    expires_at: i64,\n}\n\npub fn refresh(session: &mut Session) -> Result<(), Error> {\n    // TODO: implement\n    Ok(())\n}"}],"isError":false,"timestamp":1733234405000}}
{"type":"message","id":"r3","parentId":"r1","timestamp":"2024-12-03T14:00:05.500Z","message":{"role":"toolResult","toolCallId":"c3","toolName":"bash","content":[{"type":"text","text":"running 4 tests\ntest auth::refresh ... ok\ntest result: ok. 4 passed"}],"isError":false,"timestamp":1733234405500}}
{"type":"message","id":"b1","parentId":"r1","timestamp":"2024-12-03T14:00:06.000Z","message":{"role":"bashExecution","command":"cargo test auth","output":"running 4 tests\ntest auth::refresh ... ok\ntest result: ok. 4 passed","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1733234406000}}
{"type":"message","id":"a2","parentId":"b1","timestamp":"2024-12-03T14:00:08.000Z","message":{"role":"assistant","api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":2400,"output":180,"cacheRead":0,"cacheWrite":0,"totalTokens":2580,"cost":{"input":0.0072,"output":0.0027,"cacheRead":0,"cacheWrite":0,"total":0.0099}},"stopReason":"toolUse","timestamp":1733234408000,"content":[{"type":"text","text":"**Plan**\n\n1. Implement `refresh`.\n2. Cover it with a test.\n\n```rust\npub fn refresh(s: &mut Session) -> Result<(), Error> {\n    s.token = request_refresh(&s.token)?;\n    Ok(())\n}\n```\n\nSee [the docs](https://example.com) for details, or the **bold** note below.\n\n> Refreshing must be idempotent."},{"type":"toolCall","id":"c2","name":"edit","arguments":{"path":"src/auth.rs","edits":[{"oldText":"// TODO: implement","newText":"self.token = request_refresh(&self.token)?;"}]}}]}}
{"type":"compaction","id":"k1","parentId":"a2","timestamp":"2024-12-03T14:01:00.000Z","summary":"Implemented token refresh and its regression test.","firstKeptEntryId":"a2","tokensBefore":42000}
"#;

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
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    // Shots capture a frame or two after a panel opens; with motion on they
    // would catch fades half-way and spinners mid-turn.
    cx.update(|app| app.set_reduce_motion(true));
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
    // The native Pi Session panel, inside the main window. Written to a temp
    // file so the shot is deterministic and independent of the machine's own
    // pi sessions; no daemon is needed because the spec is an explicit file.
    let sample = std::env::temp_dir().join("pilus-shot-session.jsonl");
    std::fs::write(&sample, SAMPLE_SESSION).context("write sample session")?;
    let spec = sample.to_string_lossy().into_owned();
    shoot(
        &mut cx,
        "session",
        1240.0,
        780.0,
        Duration::from_millis(400),
        move |window, cx| {
            let spec = spec.clone();
            cx.new(move |cx| {
                PilusView::new(
                    pilus_client::ConnectTarget::local(),
                    None,
                    settings(),
                    Rc::new(RefCell::new(MachineSignal::default())),
                    Rc::new(RefCell::new(Theme::default())),
                    Some(spec),
                    window,
                    cx,
                )
            })
        },
    )?;
    // The same panel with a few cards expanded, to review the body layout and
    // the Markdown rendering.
    {
        let cell: Rc<RefCell<Option<gpui::Entity<PilusView>>>> = Rc::new(RefCell::new(None));
        let slot = cell.clone();
        let spec = sample.to_string_lossy().into_owned();
        let window = cx
            .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
                let view = cx.new(move |cx| {
                    PilusView::new(
                        pilus_client::ConnectTarget::local(),
                        None,
                        settings(),
                        Rc::new(RefCell::new(MachineSignal::default())),
                        Rc::new(RefCell::new(Theme::default())),
                        Some(spec),
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(view.clone());
                view
            })
            .context("open session-expanded")?;
        std::thread::sleep(Duration::from_millis(400));
        cx.run_until_parked();
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    // A reported state paints the context chip and the
                    // picker's live model, exactly as an RPC-up panel shows.
                    view.session.pi_state = Some(pilus_session::PiState {
                        context: Some(pilus_session::ContextUsage {
                            tokens: Some(655_360),
                            context_window: 1_048_576,
                            percent: Some(62.5),
                        }),
                        model: Some(pilus_session::PiModel {
                            provider: "opencode-go".to_string(),
                            id: "muse-spark-1.3-contributor".to_string(),
                            name: "Muse Spark 1.3 Contributor".to_string(),
                            ..pilus_session::PiModel::default()
                        }),
                        thinking: Some("high".to_string()),
                        ..pilus_session::PiState::default()
                    });
                    view.session.toggle(4);
                    view.session.toggle_work(6);
                    view.session.toggle_group(6);
                    view.session.toggle(6);
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-expanded")?;
        std::fs::create_dir_all(OUT).context("create shots dir")?;
        image
            .save(format!("{OUT}/session-expanded.png"))
            .context("save session-expanded")?;
        println!("shot: {OUT}/session-expanded.png");
    }
    // The model picker over a catalog far longer than the rows that fit:
    // the filter field, the provider hint, and the virtualized list
    // scrolled to the live model, which sits deep in the catalog.
    {
        let cell: Rc<RefCell<Option<gpui::Entity<PilusView>>>> = Rc::new(RefCell::new(None));
        let slot = cell.clone();
        let spec = sample.to_string_lossy().into_owned();
        let window = cx
            .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
                let view = cx.new(move |cx| {
                    PilusView::new(
                        pilus_client::ConnectTarget::local(),
                        None,
                        settings(),
                        Rc::new(RefCell::new(MachineSignal::default())),
                        Rc::new(RefCell::new(Theme::default())),
                        Some(spec),
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(view.clone());
                view
            })
            .context("open session-picker")?;
        std::thread::sleep(Duration::from_millis(400));
        cx.run_until_parked();
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    view.session.pi_state = Some(pilus_session::PiState {
                        model: Some(pilus_session::PiModel {
                            provider: "opencode-go".to_string(),
                            id: "model-57".to_string(),
                            name: "Model 57".to_string(),
                            ..pilus_session::PiModel::default()
                        }),
                        thinking: Some("high".to_string()),
                        ..pilus_session::PiState::default()
                    });
                    view.session.set_catalog(
                        (0..60)
                            .map(|ix| pilus_client::AgentModel {
                                provider: if ix % 3 == 0 {
                                    "opencode-go".to_string()
                                } else {
                                    "openrouter".to_string()
                                },
                                id: format!("model-{ix}"),
                                name: format!("Model {ix}"),
                                ..pilus_client::AgentModel::default()
                            })
                            .collect(),
                    );
                    view.session.open_picker_shot(PickerKind::Model);
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-picker")?;
        std::fs::create_dir_all(OUT).context("create shots dir")?;
        image
            .save(format!("{OUT}/session-picker.png"))
            .context("save session-picker")?;
        println!("shot: {OUT}/session-picker.png");
        // The thinking picker, whose seven levels all fit, so it draws no
        // filter field.
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    view.session.open_picker_shot(PickerKind::Thinking);
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-picker-thinking")?;
        image
            .save(format!("{OUT}/session-picker-thinking.png"))
            .context("save session-picker-thinking")?;
        println!("shot: {OUT}/session-picker-thinking.png");
        // The picker before the catalog lands: skeleton rows hold the space
        // the options will take, so the menu never flashes empty.
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    view.session.set_catalog_loading(true);
                    view.session.open_picker_shot(PickerKind::Thinking);
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-picker-loading")?;
        std::fs::create_dir_all(OUT).context("create shots dir")?;
        image
            .save(format!("{OUT}/session-picker-loading.png"))
            .context("save session-picker-loading")?;
        println!("shot: {OUT}/session-picker-loading.png");
    }
    // The session history menu under the header's clock button, with the
    // live session marked and older ones titled by their openers.
    {
        let cell: Rc<RefCell<Option<gpui::Entity<PilusView>>>> = Rc::new(RefCell::new(None));
        let slot = cell.clone();
        let spec = sample.to_string_lossy().into_owned();
        let window = cx
            .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
                let view = cx.new(move |cx| {
                    PilusView::new(
                        pilus_client::ConnectTarget::local(),
                        None,
                        settings(),
                        Rc::new(RefCell::new(MachineSignal::default())),
                        Rc::new(RefCell::new(Theme::default())),
                        Some(spec),
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(view.clone());
                view
            })
            .context("open session-history")?;
        std::thread::sleep(Duration::from_millis(400));
        cx.run_until_parked();
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    let entry = |id: &str, title: &str| HistoryEntry {
                        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
                        id: id.to_string(),
                        title: title.to_string(),
                    };
                    view.session.open_history_shot(vec![
                        entry("shot-0001", "Refactor the auth module"),
                        entry("shot-0000", "Add a retry to the uploader"),
                        entry("shot-0002", ""),
                    ]);
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-history")?;
        std::fs::create_dir_all(OUT).context("create shots dir")?;
        image
            .save(format!("{OUT}/session-history.png"))
            .context("save session-history")?;
        println!("shot: {OUT}/session-history.png");
    }
    // The waiting panel a focused pi gets before its extension reports:
    // the auto pin's stand-in while the socket is discovered.
    {
        let cell: Rc<RefCell<Option<gpui::Entity<PilusView>>>> = Rc::new(RefCell::new(None));
        let slot = cell.clone();
        let window = cx
            .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
                let view = cx.new(move |cx| {
                    PilusView::new(
                        pilus_client::ConnectTarget::local(),
                        None,
                        settings(),
                        Rc::new(RefCell::new(MachineSignal::default())),
                        Rc::new(RefCell::new(Theme::default())),
                        None,
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(view.clone());
                view
            })
            .context("open session-waiting")?;
        if let Some(view) = cell.borrow().clone() {
            cx.update(|app| {
                view.update(app, |view, cx| {
                    view.session.open_local_session(
                        PathBuf::from("/tmp/pilus-shot-api.sock"),
                        "~/work/repo".to_string(),
                        None,
                        "shot-pane".to_string(),
                        cx,
                    );
                    cx.notify();
                });
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        let image = cx
            .capture_screenshot(window.into())
            .context("capture session-waiting")?;
        std::fs::create_dir_all(OUT).context("create shots dir")?;
        image
            .save(format!("{OUT}/session-waiting.png"))
            .context("save session-waiting")?;
        println!("shot: {OUT}/session-waiting.png");
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
        .open_window(size(px(1240.0), px(780.0)), move |window, cx| {
            let view = cx.new(|cx| {
                PilusView::new(
                    target.clone(),
                    Some(connection),
                    settings(),
                    Rc::new(RefCell::new(MachineSignal::default())),
                    Rc::new(RefCell::new(Theme::default())),
                    None,
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
        let lines: Vec<&str> = std::iter::repeat_n("", usize::from(super::SKIPPED_DAEMON_ROWS))
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
        let cell_width: f32 = window
            .text_system()
            .shape_line(
                "M".into(),
                px(font_size),
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
            .into();
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
                width,
                height - super::SKIPPED_DAEMON_ROWS - super::SKIPPED_DAEMON_BOTTOM_ROWS,
                1.0,
                0.0,
                0.0,
                1.0,
            ))
    }
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
