//! The GPUI desktop window.
//!
//! The window is a thin, native shell over one daemon connection: it forwards
//! input and resizes and paints the server's rendered cell grid. The daemon
//! still owns layout, but this client claims the workspaces dock so it can draw
//! that one panel natively, and selects text over the grid itself.

mod about;
mod activity;
mod agent_watch;
mod app_icon;
mod appearances;
mod attention;
mod automations;
mod background;
mod box_drawing;
mod browser_data;
mod browser_tab;
mod connector;
mod cursor_blink;
mod database;
mod diff;
mod diff_model;
mod dock;
mod file_find;
mod files_tab;
mod fonts;
mod git_tab;
mod github;
mod github_page;
mod glass;
mod highlight;
mod home;
mod image_view;
mod keys;
mod line_diff;
mod machines;
mod markdown;
mod md_notes;
mod md_notes_view;
mod md_select;
mod md_view;
mod menubar;
mod mermaid;
mod mission;
mod notify;
mod orchestration;
mod overview;
mod overview_peek;
mod page_menu;
mod page_split;
mod palette;
mod pane_chrome;
mod pane_frames;
mod paste_image;
mod perf;
mod picker;
mod pinned;
mod ports_tab;
mod process;
mod quick_open;
mod right_sidebar;
mod scheme;
mod search;
mod selection;
mod session_ops;
mod session_view;
#[cfg(feature = "visual-test")]
pub(crate) mod shots;
mod sidebar_menu;
mod sipil;
mod sipil_chat;
mod sipil_panel;
mod sipil_social;
pub(crate) mod sound;
mod stats;
mod style;
mod task_compose;
mod tasks;
mod text_input;
mod theme;
mod toast;
mod toolbar_fade;
mod ui_playground;
mod update_window;
mod updater;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::settings::{CHROME_FONT, DEFAULT_SESSION_ICON, SESSION_ICONS, Settings, WindowGeometry};
use crate::update::{UpdateChannel, UpdateRequest, describe_source};
use automations::AutomationsPanel;
use browser_tab::{BrowserPanel, PageEdit};
use crossterm::event::{
    KeyEventKind, KeyModifiers, MouseButton as TerminalMouseButton, MouseEvent, MouseEventKind,
};
use database::DatabasePage;
use diff::{DiffInputs, DiffPanel};
use dock::Dock;
use files_tab::FilesPanel;
use git_tab::GitPanel;
use github::GithubFeed;
use github_page::GithubPage;
use glass::Glass;
use gpui::{
    Action, Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, DispatchPhase,
    DisplayId, Div, ElementId, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, Hsla,
    KeyBinding, KeyDownEvent, Menu, MenuItem, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Stateful, SystemNotification, TextRun, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, anchored, canvas, deferred, div, font,
    img, linear_color_stop, linear_gradient, point, prelude::*, px, relative, size, svg,
};
use gpui_platform::application;
use gpui_updater_core::{Release, UpdateStatus, Version};
use home::{HomeField, HomeFocus, HomePanel};
use machines::MachineOp;
use menubar::MenuBar;
use mission::MissionPanel;
use overview::OverviewPage;
use page_split::{PageSplit, SplitAxis, page_extent};
use palette::{Command, Entry, Palette};
use picker::{DirSource, MenuChoice, PickerEvent, WorkspacePicker};
use pilus_client::{
    Agent, AgentConfig, AgentFeed, Appearance, AutomationSummary, BarWidget, ClientEvent,
    ConnectTarget, Connection, ControlClient, ControlTransport, HOME_ENV, MachineInfo, SessionInfo,
    SshTarget, Surface, SurfaceChange, Tab, TaskSummary, WorktreeInfo, agent_binary, config_dir,
    connect_machine, expand_local, expand_remote, generate_session_name, is_behind, list_machines,
    list_session_infos, luvus_binary, pilus_config_dir, refresh_machines, remote_home,
    running_command, split_client_socket,
};
use pilus_protocol::{
    CellData, ClientMessage, FrameData, ShellDockRect, ShellSidebars, ShellWorkspace,
    SurfaceInterest,
};
use ports_tab::PortsPanel;
use process::ProcessPage;
use right_sidebar::{RightSidebar, SidebarTab};
use search::SearchPage;
use selection::{Cell, Selection};
use session_ops::SessionVerb;
use sidebar_menu::{Item as SidebarItem, MenuTarget as SidebarMenuTarget, SidebarMenu};
use stats::StatsPage;
use style::{FillExt, RadiusExt};
use tasks::{TaskLayout, TasksInputs, TasksPanel};
use text_input::{TextField, TextInput};
use theme::Theme;
use toast::{ToastKind, Toasts};
use updater::Updater;

/// Line height as a fraction of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Height of our native tab strip; the grid starts below it.
const TAB_STRIP_HEIGHT: f32 = 36.0;
/// Metrics shared by composer picker chips and dropdowns.
const PICKER_CHIP_H: f32 = 24.0;
/// Dropdown rows and width.
const PICKER_MENU_W: f32 = 260.0;
const PICKER_MENU_MAX_H: f32 = 240.0;
const PICKER_ROW_H: f32 = 30.0;
/// Grid offset from the window top: tab strip plus its top spacing.
const HEADER_HEIGHT: f32 = 40.0;
/// Daemon rows skipped at the top of the grid when it draws a title: its own
/// tab strip plus a cwd breadcrumb, as older Luvus always did and newer Luvus
/// does with `show_title` on. The grid starts below them so only native
/// chrome shows. Selection and click coordinates stay in daemon rows, so copy
/// and gear clicks keep working.
const SKIPPED_DAEMON_ROWS: u16 = 2;
/// Daemon rows skipped above the grid without the title row: newer Luvus
/// draws the tab strip alone, and skipping two rows would hide the prompt a
/// `clear` leaves on the first pane row. The "top spacing" setting picks it.
const SKIPPED_DAEMON_ROWS_UNTITLED: u16 = 1;
/// Daemon rows skipped at the grid bottom: its one-row status line, which the
/// native shell omits. Assumes a single-row status line.
const SKIPPED_DAEMON_BOTTOM_ROWS: u16 = 1;
/// The glyph Luvus draws in a zoomed pane's top-right corner, on the first
/// row under its tab strip. That row is a header strip in a shade of its own
/// across the pane, and the badge exists only while the pane is zoomed. Our
/// toolbar's zoom button already says "restore", so `render_row` blanks the
/// glyph and paints the strip as the pane.
const ZOOM_BADGE_GLYPH: &str = "\u{2921}";
const ZOOM_BADGE_ROW: u16 = 1;
/// The row under the header, where Luvus puts its scroll-offset chip (`↑24`,
/// a filled block in the scheme's accent) at the same right edge.
const SCROLL_CHIP_ROW: u16 = 2;
/// How far from the right edge the badge may sit: it is a padded chip, then
/// the pane's border column.
const ZOOM_BADGE_REACH: u16 = 4;
/// Bottom padding of the sidebar list.
const SIDEBAR_LIST_PAD: f32 = 12.0;
/// Inset between the panel edge and a row's box, so a selection pill and a
/// hover fill never kiss the window edge. The section header's chevron and
/// add button are the two exceptions: they break the inset out to sit flush
/// on the edge, one per side.
const SIDEBAR_ROW_EDGE: f32 = 4.0;
/// Inset of a row's content from its own box: before the leading glyph, and
/// before a trailing badge, status dot, or branch.
const SIDEBAR_ROW_PAD: f32 = 8.0;
/// Extra inset for a row nested under another row. Small, because the elbow
/// connector carries the hierarchy and the indent only has to clear it.
const SIDEBAR_NEST_INDENT: f32 = 4.0;
/// Share of the selected pill's strength a hover fill carries, so hovering a
/// row hints at the selection without matching it.
const SIDEBAR_HOVER_WEIGHT: f32 = 0.6;
/// How much of the selection pill the active workspace keeps while an agent
/// is the focused row: enough to read as "still here", less than the agent.
const SIDEBAR_CONTEXT_WEIGHT: f32 = 0.4;
/// Box every leading sidebar glyph sits in, so labels share one column
/// whatever the glyph is: chevron, folder, brand mark, or status dot.
const SIDEBAR_GLYPH: f32 = 14.0;
/// A sipil's logo in its row: bigger than a glyph, since it is a picture.
const SIDEBAR_LOGO: f32 = 22.0;
/// Gap between a row's leading glyph and its label.
const SIDEBAR_GLYPH_GAP: f32 = 8.0;
/// Height of a single-line sidebar row (nav, open workspace, section header).
const SIDEBAR_ROW_HEIGHT: f32 = 28.0;
/// Inset of the sidebar's head from the panel edge: the collapse toggle's
/// right edge sits on it.
const SIDEBAR_HEAD_INSET: f32 = 8.0;
/// Height of a one-line band that carries no control: a section header, an
/// endpoint group label, or an empty-state placeholder.
const SIDEBAR_BAND_HEIGHT: f32 = 24.0;
/// How long after a sidebar section header is clicked its chevron may still
/// turn: past the turn itself, so a late frame still catches it.
const SECTION_TURN_WINDOW: Duration = Duration::from_millis(400);
/// Space above a section header, on top of the list's row gap.
const SIDEBAR_SECTION_GAP: f32 = 12.0;
/// How long the sidebar takes to collapse or expand: long enough for its
/// spring to overshoot and settle, short enough to stay out of the way.
const SIDEBAR_ANIM_DURATION: Duration = Duration::from_millis(460);
/// Longest the terminal holds its pre-toggle snapshot waiting for a frame
/// drawn with the new dock: a slow link must not leave it stale.
const SIDEBAR_FREEZE_MAX: Duration = Duration::from_secs(2);

/// One sidebar collapse/expand tween, from the current openness to the target
/// so a mid-slide reversal glides instead of jumping.
struct SidebarAnim {
    started: Instant,
    from: f32,
    to: f32,
}

/// Window surfaces read their opacity from `Settings::glass_opacity`, tuned
/// in the Appearances window, so no constant lives here.
/// Tint always veiling the blurred backdrop: keeps grid text readable on
/// any desktop. It deepens as the blur slider drops — a sharper backdrop
/// fights the text more.
const GLASS_TINT: f32 = 0.16;
/// Deepest extra tint over the backdrop, reached as the blur slider
/// approaches (but does not reach) zero.
const FROST_MAX: f32 = 0.28;
/// Share of the tint the top of the pane carries: light passes where the
/// sheen sits, so the glass reads lit from above.
const TINT_TOP: f32 = 0.55;
/// Sheen alpha at the very top of the window and the height share it falls
/// off across: the specular highlight a pane catches from light above.
const SHEEN_MAX: f32 = 0.055;
const SHEEN_HEIGHT: f32 = 0.32;
/// Width of the sidebar collapse/expand button.
const SIDEBAR_TOGGLE_W: f32 = 28.0;
/// Gap between the session picker and the sidebar toggle it precedes.
const HEAD_CONTROLS_GAP: f32 = 4.0;
/// How far the header's session picker and toggle reach together.
const HEAD_CONTROLS_W: f32 = SIDEBAR_TOGGLE_W + HEAD_CONTROLS_GAP + SIDEBAR_TOGGLE_W;
/// Gap between the session picker and the menu it opens below.
const SESSION_MENU_GAP: f32 = 4.0;
/// Height of a tab pill in the top chrome.
const TAB_HEIGHT: f32 = 26.0;
/// Padding inside a pill, between its border and the buttons it holds.
const PILL_PAD: f32 = 2.0;
/// A button inside a pill: the tab height less the pill's border and padding.
const PILL_BUTTON_H: f32 = TAB_HEIGHT - 2.0 - 2.0 * PILL_PAD;
const PILL_BUTTON_W: f32 = SIDEBAR_TOGGLE_W - 4.0;
/// Top-left of the traffic lights: their left edge shares the sidebar glyph
/// column (row edge + row padding) and their 12px discs centre on the 20px
/// midline of the sidebar header and tab strip.
const TRAFFIC_LIGHT_POSITION: Point<Pixels> =
    point(px(SIDEBAR_ROW_EDGE + SIDEBAR_ROW_PAD), px(14.0));
/// Left inset for the collapsed tab strip: clears the floating traffic lights
/// (the third light ends near 64px) with a comfortable gap before the toggle.
const TRAFFIC_LIGHT_CLEARANCE: f32 = 92.0;
/// Inset for the tab strip when nothing floats over its left edge: the sidebar
/// is open, or fullscreen hides the traffic lights.
const TAB_STRIP_EDGE: f32 = 12.0;
/// Section label size (SIPIL / WORKSPACES).
const SECTION_FONT: f32 = 11.0;
/// Opening size of the main window when nothing is remembered.
const WINDOW_SIZE: (f32, f32) = (1240.0, 780.0);
/// Smallest main window the user can resize to, and the floor for a
/// remembered frame.
const WINDOW_MIN_SIZE: (f32, f32) = (640.0, 400.0);
/// How long a moved frame must sit still before it is written to settings, so
/// a live resize does not write the file on every tick.
const WINDOW_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);
/// Least amount of a restored frame that must land on a display for the frame
/// to be usable: enough of the titlebar to grab and drag.
const WINDOW_MIN_VISIBLE: (f32, f32) = (120.0, 40.0);

/// The window's initial backdrop: `Transparent` on macOS, where `Glass`
/// installs its own frosted effect view; `Blurred` elsewhere, where the
/// platform's built-in blur is the only one available.
#[cfg(target_os = "macos")]
const WINDOW_BACKDROP: WindowBackgroundAppearance = WindowBackgroundAppearance::Transparent;
#[cfg(not(target_os = "macos"))]
const WINDOW_BACKDROP: WindowBackgroundAppearance = WindowBackgroundAppearance::Blurred;

// Ratatui `Modifier` bits, as packed by the daemon.
const M_BOLD: u16 = 1 << 0;
const M_ITALIC: u16 = 1 << 2;
const M_UNDERLINED: u16 = 1 << 3;
const M_REVERSED: u16 = 1 << 6;

gpui::actions!([
    /// Open the native Appearances window.
    OpenAppearances,
    /// Quit the application.
    QuitApp,
    /// Copy the current grid selection.
    CopySelection,
    /// Paste the system clipboard into the pane.
    PasteClipboard,
    /// Hide the application, as Cmd+H does in every Mac app.
    HideApp,
    /// Hide every other application.
    HideOtherApps,
    /// Minimize the active window.
    MinimizeWindow,
    /// Zoom (maximize) the active window.
    ZoomWindow,
    /// Close the active window.
    CloseWindow,
    /// Select the whole visible grid.
    SelectAllGrid,
    /// Open another pilus window attached to the same daemon.
    NewWindow,
    /// Open a new daemon tab.
    NewTab,
    /// Open a terminal on the local machine.
    OpenLocalTerminal,
    /// Split the focused pane side by side.
    SplitVertical,
    /// Split the focused pane stacked.
    SplitHorizontal,
    /// Close the active daemon tab.
    CloseTab,
    /// Close every open window.
    CloseAllWindows,
    /// Show the About window.
    OpenAbout,
    /// Check for updates and show the Update window.
    CheckForUpdates,
    /// Grow the terminal font size.
    IncreaseFontSize,
    /// Shrink the terminal font size.
    DecreaseFontSize,
    /// Restore the default terminal font size.
    ResetFontSize,
    /// Show or hide the sidebar.
    ToggleSidebar,
    /// Turn energy mode on or off.
    ToggleEnergyMode,
    /// Save the markdown file being edited in the Files tab.
    SaveDoc,
    /// Find in the file open in the Files tab.
    FindInFile,
    /// Jump to a file in the active workspace by fuzzy name.
    OpenQuickOpen,
    /// Show or hide the right sidebar (Changes and History).
    ToggleRightSidebar,
    /// Step back through the right sidebar's history.
    RightSidebarBack,
    /// Step forward through the right sidebar's history.
    RightSidebarForward,
    /// Open the native Open Workspace picker.
    OpenWorkspacePicker,
    /// Open the footer session switcher.
    OpenSessionMenu,
    /// Open the command palette.
    OpenCommandPalette,
    /// Open the Settings page.
    OpenSettings,
    /// Open the Home composer for a new agent.
    OpenHome,
    /// Focus tab 1.
    FocusTab1,
    /// Focus tab 2.
    FocusTab2,
    /// Focus tab 3.
    FocusTab3,
    /// Focus tab 4.
    FocusTab4,
    /// Focus tab 5.
    FocusTab5,
    /// Focus tab 6.
    FocusTab6,
    /// Focus tab 7.
    FocusTab7,
    /// Focus tab 8.
    FocusTab8,
    /// Focus tab 9.
    FocusTab9
]);

/// A shared counter the machine manager bumps so every main window reloads the
/// owner-local machine catalog after a mutation.
#[derive(Default)]
pub(crate) struct MachineSignal {
    pub(crate) revision: u64,
}

/// Open the desktop window and attach to `target`.
pub fn run(target: ConnectTarget, menubar_only: bool) -> anyhow::Result<()> {
    // The window opens before the daemon is attached, so a stopped or
    // protocol-incompatible daemon cannot stop `pilus` from launching. The
    // view's local reconnect loop owns the first attach and every retry, and
    // the empty grid area names the failure. The real viewport is not known
    // until a window exists; the first resize after the window opens corrects
    // the provisional dimensions.
    let config_root = config_dir();
    let pilus_root = pilus_config_dir();
    migrate_legacy_stores(&config_root, &pilus_root);
    // A helper a quit left behind hands its menu bar item to this process.
    if !menubar_only {
        background::retire_helper(&pilus_root);
    }
    let settings = Rc::new(RefCell::new(Settings::load(&pilus_root)));
    let target = reopen_last_session(target, &settings.borrow(), &config_root);
    let restore_endpoint = settings.borrow().last_endpoint.clone();
    let appearances_socket = api_socket_for_target(&target);
    let new_window_target = target.clone();
    style::sync_corner_radius(&settings.borrow());
    let machine_signal = Rc::new(RefCell::new(MachineSignal::default()));
    let shared_theme = Rc::new(RefCell::new(Theme::default()));

    let app_shared = AppShared {
        target: new_window_target.clone(),
        settings: settings.clone(),
        signal: machine_signal.clone(),
        theme: shared_theme.clone(),
        config_root: config_root.clone(),
        pilus_root: pilus_root.clone(),
        menubar_only,
    };
    let app = application();
    // Clicking the Dock icon with every window closed brings one back.
    app.on_reopen({
        let app_shared = app_shared.clone();
        move |cx| {
            if !app_shared.has_window(cx) {
                app_shared.open_window(None, cx);
            }
            cx.activate(true);
        }
    });
    app.run(move |cx: &mut App| {
        fonts::install(cx.text_system());
        app_icon::install();
        // Name the process for notification and taskbar identity. Read the
        // running bundle so a packaged app reports its `Info.plist` value;
        // this is a no-op on macOS, where the bundle already carries it.
        cx.set_app_identity(&app_icon::bundle_identifier(), "pilus");
        // Clicking a notification takes the user to the agent it was about,
        // not just to the app.
        cx.on_system_notification_response({
            let app_shared = app_shared.clone();
            move |response, cx| {
                let target = notify::target(&response.tag);
                // A sipil message opens its conversation; it is no agent pane.
                let sipil_thread = target
                    .as_ref()
                    .filter(|(endpoint, _)| endpoint == "sipil")
                    .map(|(_, thread)| thread.clone());
                let target = target.filter(|(endpoint, _)| endpoint != "sipil");
                cx.activate(true);
                // With no window (the menu bar helper) there is nothing to
                // raise, so the click opens one on the agent.
                let Some(window) = cx
                    .windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<PilusView>())
                else {
                    app_shared.open_window(target, cx);
                    return;
                };
                let _ = window.update(cx, |view, window, cx| {
                    window.activate_window();
                    if let Some(thread) = &sipil_thread {
                        view.open_sipil_thread(thread, cx);
                    }
                    if let Some((endpoint, pane)) = target {
                        view.menu_pick = Some((endpoint, pane, Instant::now()));
                        view.apply_menu_pick(cx);
                    }
                });
            }
        });
        // Remote access is a background service that outlives this window, so
        // launch only reconciles it with the saved choice: a healthy running
        // service is left alone, and one installed from an older build or app
        // location moves onto this binary.
        connector::ensure(settings.borrow().connector);
        perf::start();
        process::mark_launch();
        // sipil is a program the user starts, so a daemon that is not up yet is
        // waited for rather than reported. The network side starts only for a
        // pilus left reachable or connected to a daemon, so a launch that never
        // used sipil opens no socket.
        {
            let settings = settings.borrow();
            sipil::ensure(&sipil::daemon_addresses(), settings.sipil_online);
        }
        // First launch only: bring the daemon's pane gaps, title rows, and
        // theme in line with what pilus draws, then record it. The control
        // socket may not be up yet because the view starts the daemon, so the
        // work runs off-thread and waits for it; a launch that never finds the
        // daemon leaves `init` unset and retries next time.
        cx.spawn({
            let settings = settings.clone();
            let root = pilus_root.clone();
            let socket = appearances_socket.clone();
            async move |cx| {
                if settings.borrow().initialized() {
                    return;
                }
                let patched = cx
                    .background_executor()
                    .spawn(async move { crate::init::patch_luvus_defaults(&socket) })
                    .await;
                if patched {
                    cx.update(|_| {
                        let mut settings = settings.borrow_mut();
                        settings.init = Some(true);
                        settings.save(&root);
                    });
                }
            }
        })
        .detach();
        cx.on_action({
            let api = appearances_socket.clone();
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &OpenAppearances, cx: &mut App| {
                // The menu and key dispatch run inside the frontmost window's
                // own update, where reading that window fails and would drop
                // us onto the local daemon even on a remote machine. Defer
                // until it has been released.
                let api = api.clone();
                let settings = settings.clone();
                let root = root.clone();
                cx.defer(move |cx| {
                    if frontmost_luvus_missing(cx) {
                        return;
                    }
                    // The frontmost window may have left the session this
                    // process launched in, and appearance belongs to the
                    // daemon the window shows.
                    let transport = frontmost_window(cx)
                        .and_then(|handle| handle.downcast::<PilusView>())
                        .and_then(|handle| handle.read(cx).ok().map(PilusView::control_transport))
                        .unwrap_or(ControlTransport::Socket(api));
                    appearances::open(transport, settings, root, cx)
                });
            }
        });
        cx.on_action(|_: &QuitApp, cx: &mut App| {
            cx.quit();
        });
        // A main window handles these itself, against its own updater; these
        // answer the application menu while none is open.
        cx.on_action({
            let app_shared = app_shared.clone();
            move |_: &OpenAbout, cx: &mut App| app_shared.show_about(cx)
        });
        cx.on_action({
            let app_shared = app_shared.clone();
            move |_: &CheckForUpdates, cx: &mut App| app_shared.check_for_updates(cx)
        });
        // Quitting leaves the menu bar item standing, in a helper process,
        // unless the user ended pilus from the item itself.
        cx.on_app_quit({
            let settings = settings.clone();
            let app_shared = app_shared.clone();
            move |_| {
                if background::hand_off_wanted(settings.borrow().menu_bar) {
                    background::spawn_helper(&app_shared.pilus_root, &app_shared.target);
                }
                async {}
            }
        })
        .detach();
        cx.on_action(|_: &HideApp, cx: &mut App| cx.hide());
        cx.on_action(|_: &HideOtherApps, cx: &mut App| cx.hide_other_apps());
        // Key and menu dispatch run inside the target window's own update,
        // where `window.update` fails because the window is already leased;
        // defer until it has been released.
        cx.on_action(|_: &MinimizeWindow, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(window) = frontmost_window(cx) {
                    window
                        .update(cx, |_, window, _| window.minimize_window())
                        .ok();
                }
            });
        });
        cx.on_action(|_: &ZoomWindow, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(window) = frontmost_window(cx) {
                    window.update(cx, |_, window, _| window.zoom_window()).ok();
                }
            });
        });
        cx.on_action(|_: &CloseWindow, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(window) = frontmost_window(cx) {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
            });
        });
        cx.on_action({
            let app_shared = app_shared.clone();
            move |_: &NewWindow, cx: &mut App| app_shared.open_window(None, cx)
        });
        // The menu bar item outlives every window, so while none is open the
        // app feeds and serves it itself.
        cx.spawn({
            let app_shared = app_shared.clone();
            async move |cx| {
                let mut watch = None;
                loop {
                    cx.update(|cx| app_shared.tick_menubar(&mut watch, cx));
                    cx.background_executor().timer(MENU_BAR_TICK).await;
                }
            }
        })
        .detach();
        cx.on_action(|_: &CloseAllWindows, cx: &mut App| {
            cx.defer(|cx| {
                for window in cx.windows() {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
            });
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &IncreaseFontSize, cx: &mut App| {
                let (settings, root) = (settings.clone(), root.clone());
                cx.defer(move |cx| {
                    if !frontmost_luvus_missing(cx) {
                        bump_font(&settings, &root, 1.0, cx);
                    }
                });
            }
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &DecreaseFontSize, cx: &mut App| {
                let (settings, root) = (settings.clone(), root.clone());
                cx.defer(move |cx| {
                    if !frontmost_luvus_missing(cx) {
                        bump_font(&settings, &root, -1.0, cx);
                    }
                });
            }
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &ResetFontSize, cx: &mut App| {
                let (settings, root) = (settings.clone(), root.clone());
                cx.defer(move |cx| {
                    if !frontmost_luvus_missing(cx) {
                        bump_font(&settings, &root, 0.0, cx);
                    }
                });
            }
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &ToggleEnergyMode, cx: &mut App| {
                let (settings, root) = (settings.clone(), root.clone());
                cx.defer(move |cx| toggle_energy(&settings, &root, cx));
            }
        });
        cx.bind_keys(app_key_bindings());
        cx.set_menus(app_menus(settings.borrow().energy_mode));
        // The helper opens no window: it is only the menu bar item, until a
        // click or a second launch asks for one.
        if menubar_only {
            background::set_accessory(true);
            return;
        }
        let (bounds, display_id) = remembered_frame(&settings.borrow(), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                display_id,
                window_background: WINDOW_BACKDROP,
                window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
                titlebar: Some(TitlebarOptions {
                    title: Some("pilus".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(TRAFFIC_LIGHT_POSITION),
                }),
                app_owns_titlebar_drag: true,
                ..Default::default()
            },
            move |window, cx| {
                let signal = machine_signal.clone();
                let theme = shared_theme.clone();
                let view = cx.new(move |cx| {
                    PilusView::new(target, None, settings.clone(), signal, theme, window, cx)
                });
                // Only this first window reopens on the machine it left; a
                // later one starts on the local daemon, like it always did.
                view.update(cx, |view, _| view.restore_endpoint = restore_endpoint);
                view
            },
        );
        if let Err(error) = opened {
            eprintln!("pilus: could not open a window: {error}");
            return;
        }
        cx.activate(true);
    });

    Ok(())
}

/// What opening a window needs, kept by the app so a window can open when none
/// is left: from the menu bar, the Dock, or File > New Window.
#[derive(Clone)]
struct AppShared {
    target: ConnectTarget,
    settings: Rc<RefCell<Settings>>,
    signal: Rc<RefCell<MachineSignal>>,
    theme: Rc<RefCell<Theme>>,
    config_root: PathBuf,
    pilus_root: PathBuf,
    /// Started as the menu-bar-only helper rather than the full app.
    menubar_only: bool,
}

/// How often the app checks on the menu bar while no window does.
const MENU_BAR_TICK: Duration = Duration::from_millis(250);

impl AppShared {
    /// Whether a main window is open. Other windows (Appearances, About) do
    /// not feed the menu bar or count here.
    fn has_window(&self, cx: &App) -> bool {
        cx.windows()
            .iter()
            .any(|window| window.downcast::<PilusView>().is_some())
    }

    /// Open the About window. It needs no main window, so the menu bar item
    /// and the application menu can reach it with every window closed.
    fn show_about(&self, cx: &mut App) {
        about::open(*self.theme.borrow(), self.settings.clone(), cx);
        cx.activate(true);
    }

    /// Check for updates and show the Update window with no main window open.
    /// A window's own check reuses its updater; this one builds a fresh
    /// updater that the Update window keeps alive.
    fn check_for_updates(&self, cx: &mut App) {
        let request = {
            let settings = self.settings.borrow();
            UpdateRequest::from_settings(&settings.update_channel, settings.update_feed_url.clone())
        };
        let updater = cx.new(|cx| Updater::from_request(&request, cx));
        updater.update(cx, |updater, cx| updater.check(cx));
        update_window::show(updater, *self.theme.borrow(), self.settings.clone(), cx);
        cx.activate(true);
    }

    /// Open a main window, then focus `menu_pick` once its endpoint is up.
    fn open_window(&self, menu_pick: Option<(String, String)>, cx: &mut App) {
        // The helper has no Dock icon; a window makes it the full app, and
        // the user is about to see what the badge was counting.
        background::set_accessory(false);
        notify::set_badge(0);
        let target = reopen_last_session(
            self.target.clone(),
            &self.settings.borrow(),
            &self.config_root,
        );
        open_main_window(
            target,
            self.settings.clone(),
            self.signal.clone(),
            self.theme.clone(),
            menu_pick,
            cx,
        );
    }

    /// Run the menu bar while no window does: watch the daemons, draw their
    /// agents, and turn a click into a window. With a window open the view
    /// feeds the item and the watch is dropped.
    fn tick_menubar(&self, watch: &mut Option<agent_watch::Watch>, cx: &mut App) {
        if self.has_window(cx) {
            *watch = None;
            return;
        }
        // Nobody is looking at a window that is not there, so what polls for one
        // can take its time; the menu bar and notifications still keep current.
        activity::set_front(false);
        let enabled = self.settings.borrow().menu_bar;
        if !enabled {
            *watch = None;
            menubar::headless(false, &[], &[], false);
            // The helper exists for the item alone, so without one it ends.
            if self.menubar_only {
                background::quit_fully(cx);
            }
            return;
        }
        let watch = watch.get_or_insert_with(|| {
            let target = reopen_last_session(
                self.target.clone(),
                &self.settings.borrow(),
                &self.config_root,
            );
            let (root, session) = match &target.socket {
                Some(path) => split_client_socket(path).unwrap_or_else(|| (config_dir(), None)),
                None => (config_dir(), target.session.clone()),
            };
            agent_watch::Watch::new(api_socket_for(&root, session.as_deref()), root, session)
        });
        let rows = watch.rows(*self.theme.borrow());
        let events = watch.alerts();
        if !events.is_empty() {
            let badge = watch.open_alerts();
            for event in events {
                raise_background_alert(event, badge, cx);
            }
            // A batch that only cleared banners posts nothing to carry the
            // new count.
            notify::set_badge(badge);
        }
        let (energy, pinned) = {
            let settings = self.settings.borrow();
            let pinned = pinned::menu_rows(&settings.pinned_workspaces, |key| {
                watch.group_of(key).unwrap_or_else(|| key.to_string())
            });
            (settings.energy_mode, pinned)
        };
        for pick in menubar::headless(true, &rows, &pinned, energy) {
            match pick {
                menubar::Pick::Quit => background::quit_fully(cx),
                menubar::Pick::Energy => toggle_energy(&self.settings, &self.pilus_root, cx),
                menubar::Pick::Show => {
                    self.open_window(None, cx);
                    cx.activate(true);
                }
                menubar::Pick::About => self.show_about(cx),
                menubar::Pick::CheckForUpdates => self.check_for_updates(cx),
                menubar::Pick::Agent { endpoint, pane } => {
                    self.open_window(Some((endpoint, pane)), cx);
                    cx.activate(true);
                }
                menubar::Pick::Pinned { endpoint, path } => {
                    let saved = self.settings.borrow().saved_workspace(&endpoint, &path);
                    self.open_window(None, cx);
                    cx.activate(true);
                    // The window is built inside `open_window`, so it is
                    // there to be handed the folder; it opens it once ready.
                    if let Some(window) = cx
                        .windows()
                        .into_iter()
                        .find_map(|window| window.downcast::<PilusView>())
                    {
                        let _ = window.update(cx, |view, _, _| {
                            view.saved_pick = Some((saved, Instant::now()));
                        });
                    }
                }
            }
        }
    }
}

/// Post a native notification for an agent transition seen with no window
/// open, so a quit pilus still reaches the user. It chimes and badges the app
/// icon with `badge` standing alerts, which the daemon's own cue cannot do
/// here. The tags match the window's, so a banner a window later withdraws is
/// the same one.
fn raise_background_alert(event: attention::Event, badge: u32, cx: &mut App) {
    let (tag, endpoint, pane, title, body) = match event {
        attention::Event::Cleared(tag) => {
            notify::forget(&tag);
            return cx.dismiss_system_notification(&tag);
        }
        attention::Event::Blocked(blocked) => {
            let body = format!("{} is waiting on you", blocked.agent);
            (
                blocked.tag,
                blocked.endpoint,
                blocked.pane,
                blocked.title,
                body,
            )
        }
        attention::Event::Finished(finished) => {
            let body = format!("{} finished", finished.agent);
            (
                finished.tag,
                finished.endpoint,
                finished.pane,
                finished.title,
                body,
            )
        }
    };
    notify::post(
        cx,
        notify::Alert {
            tag,
            title,
            body,
            badge,
            target: (endpoint, pane),
        },
    );
}

/// Swap a plain launch's target for the session the window was last attached
/// to. An explicit `--session`, `--socket`, or `$LUVUS_SESSION` is the user
/// asking for something specific, so only the untouched default is replaced.
fn reopen_last_session(target: ConnectTarget, settings: &Settings, root: &Path) -> ConnectTarget {
    if target != ConnectTarget::local() {
        return target;
    }
    settings
        .restorable_session(&list_session_infos(root))
        .map_or(target, ConnectTarget::session)
}

/// The window the user is looking at. The platform's notion of the active
/// window can lag a menu or key-equivalent dispatch, so fall back to asking
/// each window whether it holds focus.
fn frontmost_window(cx: &mut App) -> Option<gpui::AnyWindowHandle> {
    cx.active_window().or_else(|| {
        cx.windows().into_iter().find(|handle| {
            handle
                .update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false)
        })
    })
}

/// Whether the frontmost window is waiting on a Luvus install, so the
/// app-level shortcuts that belong to a working window can stand down too.
/// Call it deferred: a window mid-dispatch is leased and cannot be read.
fn frontmost_luvus_missing(cx: &mut App) -> bool {
    frontmost_window(cx)
        .and_then(|handle| handle.downcast::<PilusView>())
        .and_then(|handle| handle.read(cx).ok().map(PilusView::luvus_missing))
        .unwrap_or(false)
}

/// Every application-level keybinding, in one place so a duplicate chord is a
/// test failure rather than a shadowed shortcut.
fn app_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-,", OpenAppearances, None),
        KeyBinding::new("cmd-q", QuitApp, None),
        KeyBinding::new("cmd-c", CopySelection, Some("!Input")),
        KeyBinding::new("cmd-v", PasteClipboard, Some("!Input")),
        KeyBinding::new("cmd-a", SelectAllGrid, Some("!Input")),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("cmd-alt-h", HideOtherApps, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-n", NewWindow, None),
        KeyBinding::new("cmd-shift-n", OpenHome, None),
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-shift-t", OpenLocalTerminal, None),
        KeyBinding::new("cmd-d", SplitVertical, None),
        KeyBinding::new("cmd-shift-d", SplitHorizontal, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-shift-w", CloseWindow, None),
        KeyBinding::new("cmd-alt-w", CloseAllWindows, None),
        KeyBinding::new("cmd-=", IncreaseFontSize, None),
        KeyBinding::new("cmd-+", IncreaseFontSize, None),
        KeyBinding::new("cmd--", DecreaseFontSize, None),
        KeyBinding::new("cmd-0", ResetFontSize, None),
        KeyBinding::new("cmd-s", ToggleSidebar, None),
        // The markdown editor takes Cmd+S from the sidebar toggle while it
        // holds the keyboard.
        KeyBinding::new("cmd-s", SaveDoc, Some("MdEditor")),
        KeyBinding::new("cmd-f", FindInFile, None),
        KeyBinding::new("cmd-p", OpenQuickOpen, None),
        KeyBinding::new("cmd-b", ToggleRightSidebar, None),
        KeyBinding::new("cmd-[", RightSidebarBack, None),
        KeyBinding::new("cmd-]", RightSidebarForward, None),
        KeyBinding::new("cmd-shift-p", OpenCommandPalette, None),
        KeyBinding::new("cmd-shift-o", OpenWorkspacePicker, None),
        KeyBinding::new("cmd-shift-s", OpenSessionMenu, None),
        KeyBinding::new("cmd-shift-.", OpenSettings, None),
        KeyBinding::new("cmd-1", FocusTab1, None),
        KeyBinding::new("cmd-2", FocusTab2, None),
        KeyBinding::new("cmd-3", FocusTab3, None),
        KeyBinding::new("cmd-4", FocusTab4, None),
        KeyBinding::new("cmd-5", FocusTab5, None),
        KeyBinding::new("cmd-6", FocusTab6, None),
        KeyBinding::new("cmd-7", FocusTab7, None),
        KeyBinding::new("cmd-8", FocusTab8, None),
        KeyBinding::new("cmd-9", FocusTab9, None),
    ]
}

/// The frame and display for the first window: the remembered one when it
/// still lands on the display it was recorded on, otherwise a centered
/// default. New windows stay centered; only the first is restored.
fn remembered_frame(settings: &Settings, cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let default = || {
        (
            WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)),
                cx,
            )),
            None,
        )
    };
    let Some(saved) = settings.window.as_ref() else {
        return default();
    };
    // Runtime display ids do not survive a reboot; the UUID does.
    let display = match saved.display.as_deref() {
        Some(uuid) => cx
            .displays()
            .into_iter()
            .find(|display| display.uuid().is_ok_and(|id| id.to_string() == uuid)),
        // Geometry from a platform without stable ids still helps when it
        // lands on the primary display.
        None => cx.primary_display(),
    };
    let Some(display) = display else {
        return default();
    };
    let bounds = Bounds::new(
        point(px(saved.x), px(saved.y)),
        size(
            px(saved.width.max(WINDOW_MIN_SIZE.0)),
            px(saved.height.max(WINDOW_MIN_SIZE.1)),
        ),
    );
    if !frame_reachable(&bounds, display.visible_bounds()) {
        return default();
    }
    let bounds = if saved.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    (bounds, Some(display.id()))
}

/// Snapshot the window frame in the form the settings file keeps.
fn window_geometry(window: &Window, cx: &App) -> WindowGeometry {
    let bounds = window.window_bounds().get_bounds();
    WindowGeometry {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized: window.is_maximized(),
        display: window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string()),
    }
}

/// A restored frame must keep enough of itself on its display to be grabbed;
/// otherwise it opens out of sight after the display was unplugged.
fn frame_reachable(bounds: &Bounds<Pixels>, visible: Bounds<Pixels>) -> bool {
    let overlap = bounds.intersect(&visible);
    f32::from(overlap.size.width) >= WINDOW_MIN_VISIBLE.0
        && f32::from(overlap.size.height) >= WINDOW_MIN_VISIBLE.1
}

/// Open another pilus window on the same target.
///
/// The new window attaches in the background like the first one, so a daemon
/// that is down or speaks a different protocol still opens a window that
/// explains the problem and retries.
fn open_main_window(
    target: ConnectTarget,
    settings: Rc<RefCell<Settings>>,
    machine_signal: Rc<RefCell<MachineSignal>>,
    shared_theme: Rc<RefCell<Theme>>,
    menu_pick: Option<(String, String)>,
    cx: &mut App,
) {
    let bounds = WindowBounds::centered(size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_background: WINDOW_BACKDROP,
            window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
            titlebar: Some(TitlebarOptions {
                title: Some("pilus".into()),
                appears_transparent: true,
                traffic_light_position: Some(TRAFFIC_LIGHT_POSITION),
            }),
            app_owns_titlebar_drag: true,
            ..Default::default()
        },
        move |window, cx| {
            let signal = machine_signal.clone();
            let theme = shared_theme.clone();
            let view =
                cx.new(move |cx| PilusView::new(target, None, settings, signal, theme, window, cx));
            view.update(cx, |view, _| {
                view.menu_pick = menu_pick.map(|(endpoint, pane)| (endpoint, pane, Instant::now()));
            });
            view
        },
    );
    if let Err(error) = opened {
        eprintln!("pilus: could not open another window: {error}");
    }
}

/// The application menus. Energy Mode is a checked item in the pilus menu, so the menu
/// bar shows whether it is on; the menus are set again whenever that changes.
fn app_menus(energy: bool) -> Vec<Menu> {
    vec![
        Menu::new("pilus").items([
            MenuItem::action("About pilus", OpenAbout),
            MenuItem::action("Check for Updates", CheckForUpdates),
            MenuItem::action("Energy Mode", ToggleEnergyMode).checked(energy),
            MenuItem::separator(),
            MenuItem::action("Appearances", OpenAppearances),
            MenuItem::action("Settings", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Hide pilus", HideApp),
            MenuItem::action("Hide Others", HideOtherApps),
            MenuItem::separator(),
            MenuItem::action("Quit pilus", QuitApp),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Window", NewWindow),
            MenuItem::action("New Tab", NewTab),
            MenuItem::action("Open Local Terminal", OpenLocalTerminal),
            MenuItem::action("New Agent…", OpenHome),
            MenuItem::action("Open Workspace", OpenWorkspacePicker),
            MenuItem::action("Go to File…", OpenQuickOpen),
            MenuItem::separator(),
            MenuItem::action("Split Vertical", SplitVertical),
            MenuItem::action("Split Horizontal", SplitHorizontal),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::action("Close All Windows", CloseAllWindows),
        ]),
        Menu::new("Edit").items([
            MenuItem::action("Copy", CopySelection),
            MenuItem::action("Paste", PasteClipboard),
            MenuItem::action("Select All", SelectAllGrid),
            MenuItem::separator(),
            MenuItem::action("Find in File", FindInFile),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", OpenCommandPalette),
            MenuItem::separator(),
            MenuItem::action("Increase Font Size", IncreaseFontSize),
            MenuItem::action("Decrease Font Size", DecreaseFontSize),
            MenuItem::action("Reset Font Size", ResetFontSize),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Right Sidebar", ToggleRightSidebar),
            MenuItem::action("Right Sidebar Back", RightSidebarBack),
            MenuItem::action("Right Sidebar Forward", RightSidebarForward),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", MinimizeWindow),
            MenuItem::action("Zoom", ZoomWindow),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
    ]
}

/// Flip energy mode from the menu bar: persist it, publish the squared corners
/// and refresh the menu's check mark. Each window picks up the rest on its next
/// tick (`PilusView::sync_energy`).
fn toggle_energy(settings: &Rc<RefCell<Settings>>, root: &Path, cx: &mut App) {
    let energy = {
        let mut settings = settings.borrow_mut();
        settings.energy_mode = !settings.energy_mode;
        settings.save(root);
        style::sync_corner_radius(&settings);
        settings.energy_mode
    };
    cx.set_menus(app_menus(energy));
    cx.refresh_windows();
}

/// Scale the UI and terminal fonts together and persist them. `delta == 0.0`
/// restores the defaults.
fn bump_font(settings: &Rc<RefCell<Settings>>, root: &Path, delta: f32, cx: &mut App) {
    {
        let mut settings = settings.borrow_mut();
        let defaults = Settings::default();
        let step = |value: f32, fallback: f32| {
            if delta == 0.0 {
                fallback
            } else {
                (value + delta).clamp(8.0, 32.0)
            }
        };
        settings.ui_font_size = step(settings.ui_font_size, defaults.ui_font_size);
        settings.terminal_font_size =
            step(settings.terminal_font_size, defaults.terminal_font_size);
        settings.save(root);
    }
    cx.refresh_windows();
}

/// Move pilus's own stores out of the Luvus state root the first time the
/// `~/.pilus` layout is used, so existing settings are not lost.
fn migrate_legacy_stores(luvus_root: &Path, pilus_root: &Path) {
    if std::fs::create_dir_all(pilus_root).is_err() {
        return;
    }
    let pairs = [(
        luvus_root.join("pilus-client.json"),
        pilus_root.join("settings.json"),
    )];
    for (old, new) in pairs {
        if !new.exists() && old.exists() {
            let _ = std::fs::copy(&old, &new);
        }
    }
}

/// A divider drag in progress.
#[derive(Clone, Copy)]
struct DividerDrag {
    axis: pane_frames::DividerAxis,
    /// The last cell the daemon was told about, so a move within a cell
    /// sends nothing.
    last: Cell,
}

/// Geometry of the terminal card: grid origin, visible cells, and the
/// measurements behind them, so painting, pointer mapping, and the daemon
/// viewport stay pixel-true to one another.
struct CardLayout {
    /// Card's left edge in window pixels: the sidebar's width.
    card_x: f32,
    /// Card's right offset from the window edge: the right column's width.
    card_right: f32,
    /// First visible cell's top-left corner in window pixels.
    origin_x: f32,
    origin_y: f32,
    /// Visible grid size in cells.
    cols: u16,
    rows: u16,
    /// Measured cell size in pixels.
    cell_w: f32,
    cell_h: f32,
    /// Sub-cell remainder pooled at the right and bottom edges. The grid
    /// paints its edge colors over this strip so no card-colored sliver
    /// shows.
    slack_x: f32,
    slack_y: f32,
    /// Pixels a split page takes from the card's leading edge or top, ahead
    /// of the grid's own padding. Zero with no split.
    page_w: f32,
    page_h: f32,
    /// The card's full inner width, before a split page takes its half. A
    /// page that fills the card (or leads a vertical split) is this wide.
    content_w: f32,
}

/// Which relay field of the Settings page owns the keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayField {
    /// The dedicated relay URLs.
    Url,
    /// The Iroh Services project API secret.
    Secret,
}

/// Which surface owns the keyboard, in stacking order.
///
/// Overlays sit above content; [`PilusView::input_layer`] is the single source of
/// truth for both key routing and focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InputLayer {
    /// The folder picker overlay, above everything.
    Picker,
    /// The command palette overlay.
    Palette,
    /// The quick-open file picker overlay.
    QuickOpen,
    /// The Home composer page.
    Home,
    /// The Settings page (machines, updates).
    Settings,
    /// The native Tasks page.
    Tasks,
    /// The native Diff page.
    Diff,
    /// The Browser tab's address bar.
    Browser,
    /// The native Mission Control page.
    Mission,
    /// The native Automations page.
    Automations,
    /// The Search page's query field.
    Search,
    /// The Stats page.
    Stats,
    /// The Process page.
    Process,
    /// The Database page.
    Database,
    /// The chat page for a sipil agent or session.
    Sipil,
    /// The right sidebar's Sipil tab, with a field in it.
    SipilPanel,
    /// The live terminal grid.
    Terminal,
}

/// Resolve the keyboard owner from which surfaces are open.
///
/// Overlays sit above content, so the picker beats the palette and both beat
/// the Home composer, Settings, and the session page. `page` is the native
/// page holding the keyboard (Tasks, Mission Control, or Automations); only
/// one is ever open.
fn top_layer(
    picker: bool,
    palette: bool,
    home: bool,
    settings: bool,
    page: Option<InputLayer>,
) -> InputLayer {
    if picker {
        InputLayer::Picker
    } else if palette {
        InputLayer::Palette
    } else if home {
        InputLayer::Home
    } else if settings {
        InputLayer::Settings
    } else if let Some(page) = page {
        page
    } else {
        InputLayer::Terminal
    }
}

/// One background remote-home answer: the `$HOME` of each probed machine.
struct HomeResolve {
    /// Remote `$HOME` by endpoint key, for expanding `~` cwds.
    homes: Vec<(String, String)>,
}

/// A stable endpoint identity: the local session or one saved machine.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum EndpointId {
    /// The owner-local daemon this window launched against.
    Local,
    /// A saved machine profile, by its stable id.
    Machine(String),
}

impl EndpointId {
    /// A short key for element ids and collapse state.
    fn key(&self) -> String {
        match self {
            Self::Local => "local".to_string(),
            Self::Machine(id) => id.clone(),
        }
    }

    /// The machine profile this endpoint refers to, if any.
    fn machine_id(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Machine(id) => Some(id.as_str()),
        }
    }
}

/// One connected endpoint while another endpoint owns the visible surface.
///
/// The active endpoint's state lives in [`PilusView`]'s own fields so drawing
/// code never changes; this is the mirror image for everything else. Inactive
/// endpoints stay suspended: their catalogs still arrive, so their workspace
/// rows render, but they stream no frames and own no input.
struct Endpoint {
    machine: Option<MachineInfo>,
    /// The bridge, or `None` for an endpoint whose link dropped and whose
    /// reconnect loop has not produced a new one yet.
    connection: Option<Connection>,
    surface: Surface,
    last_rx: Instant,
    probe_sent: Option<Instant>,
    answers_probes: bool,
    status: String,
    tabs: Vec<Tab>,
    agents: Vec<Agent>,
    /// The feed has delivered at least once, so an empty list is real.
    agents_loaded: bool,
    bar_widgets: Vec<BarWidget>,
    automations: AutomationSummary,
    tasks: TaskSummary,
    /// Hidden-endpoint feed: keeps its agents listed and caches the chrome
    /// a later switch swaps in.
    feed: AgentFeed,
}

/// A sidebar section header's click handler: collapse the section.
type SectionAction = Box<dyn Fn(&mut PilusView, &mut Context<PilusView>)>;

/// A sidebar section header's identity and its toggle. Bundling them lets
/// every section share one header builder, so their labels stay in one
/// column.
struct SectionHeader {
    /// Clickable chevron-and-label row.
    id: &'static str,
    label: &'static str,
    collapsed: bool,
    toggle: SectionAction,
    /// Whether a right-click on the header opens the GitHub section's menu.
    menu: bool,
}

/// One row of the SIPIL section.
struct SipilRow<'a> {
    id: String,
    /// What the saved order names it by (`daemon:<id>`, `channel:<id>`,
    /// `person:<id>`); `None` for a row that is not placed, like a knock.
    key: Option<String>,
    glyph: &'static [u8],
    name: &'a str,
    error: Option<&'a str>,
    /// Indented under the agent above it.
    nested: bool,
    /// Bold at rest, for the primary agent.
    strong: bool,
    /// The conversation a click opens.
    open: sipil::Watch,
    /// Lines not read yet, shown as a count at the right.
    badge: u64,
    /// A presence dot at the right.
    dot: Option<gpui::Rgba>,
    /// A click opens the Sipil tab instead of a conversation.
    window: bool,
    /// A picture in place of the glyph, for an agent that has a logo.
    logo: Option<std::sync::Arc<gpui::RenderImage>>,
    /// What the tooltip says besides the name, when it is not a failed turn:
    /// why a sipil is offline.
    note: Option<&'a str>,
}

/// A requested endpoint handoff waiting for its prepared frame.
struct PendingSwitch {
    target: EndpointId,
    /// Workspace to focus after the handoff commits.
    focus: Option<String>,
    /// Pane to focus after the handoff commits.
    pane: Option<String>,
    ticket: u64,
    /// Whether a prepared frame has been requested for `ticket`.
    prepared: bool,
    /// When the handoff started, for the stall timeout.
    started: Instant,
}

/// The endpoint a click asked for, before its connection exists.
#[derive(Clone)]
struct EndpointRequest {
    id: EndpointId,
    /// Workspace to focus once the endpoint is active.
    focus: Option<String>,
    /// Pane to focus once the endpoint is active.
    pane: Option<String>,
}

/// A background endpoint connection's outcome.
enum MachineConnect {
    Connected {
        machine: MachineInfo,
        connection: Connection,
    },
    Failed {
        id: String,
        error: String,
    },
    /// The owner-local daemon answered again after its link dropped.
    LocalConnected {
        connection: Connection,
    },
    /// The owner-local daemon could not be reached or restarted.
    LocalFailed {
        error: String,
        /// No `luvus` binary exists to start, as opposed to a daemon that
        /// failed to come up.
        luvus_missing: bool,
    },
}

/// Automatic reconnect state for one endpoint, with capped backoff.
struct Reconnect {
    /// Consecutive failed connection attempts since the last healthy link.
    failures: u32,
    /// Earliest instant for the next automatic attempt.
    next_at: Instant,
}

/// What a context-menu prompt asks for; its answer runs on the same endpoint
/// the right-clicked row lives on.
#[derive(Clone, Debug)]
enum PromptAction {
    /// Rename a workspace's display label.
    RenameWorkspace {
        endpoint: EndpointId,
        workspace_id: String,
    },
    /// Give a live agent's pane an addressable alias (`agent.name`).
    RenameAgent { endpoint: EndpointId, pane: String },
    /// Label a Luvus session, in pilus only. The session keeps its real name.
    RenameSession { name: String },
    /// Name the focused terminal pane, resolved when the prompt is answered.
    RenamePane { endpoint: EndpointId },
    /// Move the focused terminal pane to a tab, resolved when answered.
    MovePane { endpoint: EndpointId },
    /// Name a tab in the strip. Holds the tab's id rather than its position,
    /// so a reorder while the prompt is open cannot rename the wrong tab.
    RenameTab { tab_id: String },
    /// Create a linked worktree of the right-clicked workspace's repo.
    NewWorktree { endpoint: EndpointId, index: u16 },
    /// Create an empty file, or a folder, inside `dir` in the Files tab.
    CreateEntry {
        endpoint: EndpointId,
        dir: String,
        folder: bool,
    },
    /// Rename a Files-tab entry within its folder.
    RenameEntry { endpoint: EndpointId, path: String },
    /// Delete a Files-tab entry after the confirm prompt.
    DeleteEntry {
        endpoint: EndpointId,
        path: String,
        folder: bool,
    },
    /// Stop the process holding a listening port, after the confirm prompt.
    KillProcess {
        endpoint: EndpointId,
        pid: u32,
        port: u16,
        /// The process's name, for the result toast.
        name: String,
        /// What the confirm shows: the pid and the command line.
        label: String,
    },
    /// Throw away a Diff-page file's working-tree changes, after the confirm
    /// prompt.
    DiscardChanges {
        endpoint: EndpointId,
        /// The repository root the daemon reported, which `paths` are under.
        repo: String,
        paths: Vec<String>,
        kind: pilus_client::Discard,
        /// What the confirm shows: the file's path.
        label: String,
    },
    /// Delete a linked worktree (and its files) after the confirm prompt.
    DeleteWorktree {
        endpoint: EndpointId,
        /// Absolute checkout path; `worktree.remove` resolves it against git.
        path: String,
        /// The row's stable id, so the in-use guard can exempt the target.
        workspace_id: String,
        /// Display name for the confirmation and status line.
        name: String,
        /// The checkout holds uncommitted work, found on the first confirm;
        /// the second, stronger confirm discards it.
        dirty: bool,
        /// What the daemon says still uses the worktree (working agent panes,
        /// unfinished tasks, leases), found on the first confirm.
        blockers: Option<String>,
    },
}

impl PromptAction {
    /// Whether this is the second, stronger delete confirm, the only one that
    /// may discard work or stop what is still running.
    fn escalated(&self) -> bool {
        matches!(
            self,
            Self::DeleteWorktree { dirty, blockers, .. } if *dirty || blockers.is_some()
        )
    }

    /// Whether the prompt is a yes/no confirm with no field, so the keyboard
    /// stays on the window root, which routes its Enter and Escape.
    fn is_confirm(&self) -> bool {
        matches!(
            self,
            Self::DeleteWorktree { .. }
                | Self::DeleteEntry { .. }
                | Self::KillProcess { .. }
                | Self::DiscardChanges { .. }
        )
    }
}

/// What the first delete confirm found that needs a second, explicit yes.
struct DeleteEscalation {
    endpoint: EndpointId,
    path: String,
    workspace_id: String,
    name: String,
    dirty: bool,
    blockers: Option<String>,
}

/// One background context-menu operation's outcome, surfaced as a toast so a
/// slow SSH round trip never blocks the window.
struct MenuOpResult {
    /// Toast text when the operation finished; empty for a silent probe.
    ok: String,
    /// Error text, or `None` when it succeeded.
    error: Option<String>,
    /// A workspace rename — `(workspace id, new label)` — so the projected
    /// catalog updates without waiting for the next daemon projection.
    renamed_workspace: Option<(String, String)>,
    /// An agent alias change — `(endpoint key, pane, new alias)` — so the
    /// AGENTS row updates immediately.
    renamed_agent: Option<(String, String, String)>,
    /// A `worktree.list` probe for `(endpoint key, workspace id)`. A successful
    /// answer carries the linked checkout root, or `None` for a main/plain
    /// workspace; an error is not cached so the next menu open retries it.
    worktree_probe: Option<(String, String, Result<Option<String>, String>)>,
    /// A deletion confirmed, so `(endpoint key, workspace id)` leaves the
    /// linked-checkout cache that used to gate its menu.
    worktree_cleared: Option<(String, String)>,
    /// A removal held back for a second confirm because the checkout is dirty
    /// or still in use, so the window can ask again before forcing.
    worktree_escalated: Option<DeleteEscalation>,
}

/// The outcome of one background Luvus daemon operation.
enum DaemonOutcome {
    /// A release check finished. `installed` is the binary's version and
    /// `result` is the newer release, `None` when current, or an error string.
    Checked {
        installed: Option<Version>,
        result: Result<Option<Release>, String>,
    },
    /// An install finished; carries the new version and where it landed.
    Installed { version: Version, path: PathBuf },
    /// A restart or a failure finished; carries a message for the status line.
    Message(Result<String, String>),
}

/// Progress of installing Luvus from the prompt shown when it is missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum LuvusInstall {
    #[default]
    Idle,
    Installing,
    /// The reason the last attempt failed, shown under the buttons.
    Failed(String),
}

/// The little modal a context-menu action opens: rename, alias, or branch.
struct MenuPrompt {
    /// Modal heading, e.g. `Rename workspace`.
    title: &'static str,
    placeholder: &'static str,
    action: PromptAction,
}

/// How long a cached tab strip may stand in for a workspace click before the
/// daemon's list is taken as it is. Spans a remote click's SSH round trips.
const TABS_PICK_WINDOW: Duration = Duration::from_secs(4);

/// A cached tab strip shown for a workspace click the daemon has not reported.
struct TabsPick {
    /// Tab ids the daemon last listed, which mark a reply as still stale.
    from: Vec<String>,
    /// When the strip was swapped in.
    at: Instant,
}

impl TabsPick {
    /// Whether `tabs` is still the list the click moved away from.
    fn is_stale(&self, tabs: &[Tab]) -> bool {
        self.at.elapsed() < TABS_PICK_WINDOW && tabs.iter().map(|tab| &tab.id).eq(self.from.iter())
    }
}
/// How long reordered tabs take to spring into their new slots: the selection
/// pill's flight, so the two land together.
const TAB_SLIDE: Duration = Duration::from_millis(340);

/// Tabs travelling from the slots they were dropped out of to their new ones.
struct TabSlide {
    /// Each tab's distance from its layout slot when the slide began, by id.
    /// Positive means drawn right of it.
    from: HashMap<String, f32>,
    started: Instant,
}

impl TabSlide {
    fn progress(&self, now: Instant) -> f32 {
        now.duration_since(self.started).as_secs_f32() / TAB_SLIDE.as_secs_f32()
    }

    fn running(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }

    /// How far tab `id` is drawn from its layout slot: the start distance
    /// released along the spring, so it overshoots a hair and settles.
    fn offset(&self, id: &str, now: Instant) -> f32 {
        let from = self.from.get(id).copied().unwrap_or(0.0);
        from * (1.0 - style::ease_spring(self.progress(now).clamp(0.0, 1.0)))
    }
}

/// Where each tab starts its slide, given the strip as last drawn.
///
/// `drawn` lists the old order as `(id, drawn x, width, current offset)`;
/// `new_order` is the order after the move. Tabs sit end to end with the
/// strip's gap, so a tab's new slot is found by laying the new order out from
/// the first slot, and its start is where it was drawn minus that.
fn slide_starts(drawn: &[(String, f32, f32, f32)], new_order: &[String]) -> HashMap<String, f32> {
    let Some(first) = drawn.first() else {
        return HashMap::new();
    };
    let slot_x = |entry: &(String, f32, f32, f32)| entry.1 - entry.3;
    let gap = drawn
        .get(1)
        .map_or(0.0, |second| slot_x(second) - (slot_x(first) + first.2));
    let widths: HashMap<&str, f32> = drawn.iter().map(|e| (e.0.as_str(), e.2)).collect();
    let mut x = slot_x(first);
    let mut starts = HashMap::new();
    for id in new_order {
        let Some(width) = widths.get(id.as_str()) else {
            return HashMap::new();
        };
        if let Some(entry) = drawn.iter().find(|e| &e.0 == id) {
            let start = entry.1 - x;
            if start.abs() >= 0.5 {
                starts.insert(id.clone(), start);
            }
        }
        x += width + gap;
    }
    starts
}

/// First automatic reconnect delay.
const RECONNECT_BASE: Duration = Duration::from_secs(1);
/// Ceiling for the exponential backoff.
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Consecutive failures after which the status line says the endpoint is
/// still being retried rather than merely erroring.
const RECONNECT_ATTEMPTS: u32 = 3;

struct PilusView {
    target: ConnectTarget,
    /// The active endpoint's bridge, or `None` while the local daemon is
    /// unreachable. The window opens and keeps rendering without a bridge so
    /// a stopped or protocol-incompatible daemon cannot abort the launch; the
    /// reconnect loop owns the first attach and every retry.
    connection: Option<Connection>,
    surface: Surface,
    theme: Theme,
    /// The app's live palette, shared with the Remote Machines window.
    shared_theme: Rc<RefCell<Theme>>,
    focus_handle: FocusHandle,
    /// Font choices, shared with the Appearances window.
    settings: Settings,
    shared_settings: Rc<RefCell<Settings>>,
    /// The window's frosted backdrop view, reconfigured whenever the blur
    /// setting (or the opaque frame) changes it.
    glass: Glass,
    /// Whether energy mode was in effect when the app last applied it, so a
    /// change is applied once, not on every tick.
    energy_applied: bool,
    /// The Energy Mode check mark the menu bar last showed.
    menu_energy: bool,
    /// Collapse/expand tween in flight, if any; the panel, tab strip, and
    /// terminal read its openness so they glide together.
    sidebar_anim: Option<SidebarAnim>,
    /// The terminal cursor's fade, advanced by the tick loop.
    cursor_blink: cursor_blink::CursorBlink,
    toolbar_fade: toolbar_fade::ToolbarFade,
    /// Last dock rectangle seen while the sidebar was open. Hiding the sidebar
    /// makes the daemon drop its `ShellDock`, so a live-only width would snap
    /// to zero the instant it answers; the collapse tween folds this one away
    /// instead.
    last_sidebar_dock: Option<ShellDockRect>,
    /// The frame on screen when the sidebar started to move, with its dock
    /// skip and when it was taken. The daemon moves its panes the moment
    /// it hears the toggle, so the live grid would reflow under the sliding
    /// card — the flicker, the jump, and a glimpse of the daemon's own dock
    /// columns. The card slides this snapshot instead and swaps to the live
    /// grid once both have landed.
    sidebar_freeze: Option<(FrameData, u16, Instant)>,
    /// Sidebar openness sampled once at the top of `render`. Every surface in
    /// one frame reads this instead of re-sampling the clock, so the panel,
    /// tab strip, and terminal cannot land on edges from different tween
    /// instants — which leaves an unpainted seam between them.
    sidebar_open: f32,
    /// The sidebar section header clicked last, and when, so only that
    /// chevron turns.
    section_toggled: Option<(&'static str, Instant)>,
    /// Whether the remembered frame changed since the last settings write.
    window_dirty: bool,
    /// When the frame last changed, so a drag settles before the write.
    window_moved_at: Instant,
    cols: u16,
    rows: u16,
    status: String,
    /// Whether this window is frontmost, tracked each render so toasts only
    /// run their clock while someone can read them.
    window_active: bool,
    /// Every notice pilus raises: daemon pushes and action outcomes.
    toasts: Toasts,
    /// Dock bounce and badge for pushes that arrive while pilus is behind
    /// another app.
    dock: Dock,
    /// The menu bar's agent list; one window at a time owns the item.
    menubar: MenuBar,
    /// This window, so a menu bar click can bring it forward.
    window_handle: gpui::AnyWindowHandle,
    /// An agent chosen in the menu bar, waiting for its endpoint to connect:
    /// a click with no window open starts a window, and the endpoint is not
    /// there yet. Dropped once it lands or after [`MENU_PICK_PATIENCE`].
    menu_pick: Option<(String, String, Instant)>,
    /// A pinned folder the menu bar chose, opened once the machine catalog
    /// has loaded. Dropped when it runs.
    saved_pick: Option<(crate::settings::SavedWorkspace, Instant)>,
    /// Spots agents that start waiting on the user, across every endpoint.
    attention: attention::Watch,
    /// Measured monospace advance width, resolved once from the font.
    cell_width: Option<f32>,
    /// Last cell size pushed to the daemon, so a font change re-sends it even
    /// when the row/column counts stay the same.
    sent_cell: (u16, u16),
    /// Active native selection, if any.
    selection: Option<Selection>,
    /// Whether the left button is held and dragging across the grid.
    dragging: bool,
    /// Whether a drag left its anchor, making it a selection rather than a click.
    drag_moved: bool,
    /// Where the current left press landed. A selection is only created once
    /// the pointer moves off this cell, so a plain click never highlights.
    press_anchor: Option<Cell>,
    /// Last cell a hover move was forwarded for, so the daemon isn't flooded.
    hover_cell: Option<Cell>,
    /// The seams between the panes of the frame last painted, for grabbing
    /// one to resize a split.
    dividers: Vec<pane_frames::Divider>,
    /// The seam under the pointer, which turns the cursor into a resize arrow.
    divider_hover: Option<pane_frames::DividerAxis>,
    /// A seam held by the left button. The daemon owns the layout, so the
    /// drag is forwarded to it as the press, moves, and release it already
    /// reads as a divider drag, and the new split arrives as a frame.
    divider_drag: Option<DividerDrag>,
    /// Whether `Ctrl` is held, tracked from modifier changes. macOS turns a
    /// `Ctrl`+left-click into a right-click before the window sees it, so the
    /// flag is what lets a secondary click still reach the daemon as the
    /// `Ctrl`+left-click that opens a link.
    control_held: bool,
    /// The right press in flight was taken as a link open, so its release is
    /// the window's too and never reaches the daemon.
    link_press: bool,
    /// Background poller for the current session's tabs and widgets.
    agent_feed: Option<AgentFeed>,
    /// State root the sessions list, derived from the launch target.
    session_root: PathBuf,
    /// pilus's own state directory: settings and personal sessions.
    pilus_root: PathBuf,
    /// Current session; `None` is the default session.
    session_name: Option<String>,
    /// Visible sessions under `session_root`.
    sessions: Vec<SessionInfo>,
    /// Last session-directory poll, so the disk isn't hit every frame.
    last_session_poll: Instant,
    /// Whether the session menu is open.
    session_menu_open: bool,
    /// The row waiting on a second press before it stops or deletes.
    session_confirm: Option<(String, SessionVerb)>,
    /// The session whose icon choices are unfolded under its menu row.
    session_icon_editing: Option<String>,
    /// Sessions with a start, stop, or delete in flight, so a second press
    /// does not run the same command twice.
    session_working: HashSet<String>,
    /// The ⌘⇧P command palette's query and selection.
    palette: Palette,
    /// The palette's query field, an IME-capable field over `palette`'s buffer.
    palette_input: Entity<TextInput>,
    /// Keeps the highlighted palette row scrolled into view.
    palette_scroll: ScrollHandle,
    /// The native Open Workspace folder picker.
    picker: WorkspacePicker,
    /// The picker's path field, an IME-capable field over `picker`'s buffer.
    picker_path: Entity<TextInput>,
    /// Keeps the highlighted picker row scrolled into view.
    picker_scroll: ScrollHandle,
    /// Results from background picker listings and opens.
    picker_rx: Receiver<PickerEvent>,
    /// Sender half for picker tasks.
    picker_tx: Sender<PickerEvent>,
    /// Results from background session create/start operations.
    session_rx: Receiver<SessionOpResult>,
    /// Sender half, handed to each background session operation.
    session_tx: Sender<SessionOpResult>,
    /// Latest status-bar widgets from `ui.bar.list`.
    bar_widgets: Vec<BarWidget>,
    /// Latest tabs of the active workspace.
    tabs: Vec<Tab>,
    /// Latest live agent sessions for the AGENTS section.
    agents: Vec<Agent>,
    /// The agent feed has delivered at least once, so an empty AGENTS list
    /// is real rather than still loading.
    agents_loaded: bool,
    /// The focused pane is zoomed, so the toolbar offers to restore it.
    /// Known only where the feed holds an event stream.
    focused_zoomed: bool,
    /// Latest automation/task counts for the nav badges.
    automations: AutomationSummary,
    tasks: TaskSummary,
    /// Saved machine profiles under `session_root`.
    machines: Vec<MachineInfo>,
    /// Last machine-profile poll; the CLI is only re-read on a slow tick.
    last_machine_poll: Instant,
    /// A machine refresh is running off-thread; no overlapping spawns.
    machine_refreshing: bool,
    /// Fresh machine profiles from the background refresh.
    machine_rx: Receiver<MachineCatalog>,
    /// Sender half for the background machine refresh.
    machine_tx: Sender<MachineCatalog>,
    /// Cheap catalog reloads (no probe) from the background `list_machines`.
    machine_list_rx: Receiver<MachineCatalog>,
    /// Sender half for the cheap catalog reload.
    machine_list_tx: Sender<MachineCatalog>,
    /// Status lines from background machine mutations (enable/disable/remove).
    machine_op_rx: Receiver<machines::OpOutcome>,
    /// Sender half for background machine mutation outcomes.
    machine_op_tx: Sender<machines::OpOutcome>,
    /// The machine id whose CLI mutation is running, if any.
    machine_busy: Option<String>,
    /// Remote access as the background service last reported it, read each tick.
    remote: connector::RemoteAccess,
    /// A machine remove awaiting its second click, if any.
    machine_pending_remove: Option<String>,
    /// The next steps the last machine operation left on its row.
    machine_follow: machines::FollowUps,
    /// The `luvus` version installed on this Mac, to flag outdated machines.
    luvus_local_version: Option<String>,
    luvus_version_rx: Receiver<Option<String>>,
    luvus_version_tx: Sender<Option<String>>,
    /// Whether the next pairing code lets its device start work (tasks,
    /// agents, workspaces). Off until the user asks, every time the page opens.
    pairing_operate: bool,
    /// The last whole second the pairing countdown was drawn at.
    pairing_second: u64,
    /// Registered appearance themes and the selected id, from `theme.list`.
    appearance: Appearance,
    /// Last theme poll; the registry only changes on user action.
    last_theme_poll: Instant,
    /// The endpoint `appearance` was last polled for.
    theme_endpoint: EndpointId,
    /// A theme refresh is running off-thread; no overlapping spawns.
    theme_refreshing: bool,
    /// The endpoint whose registry `appearance` holds. It lags `active` for
    /// a poll after a switch, and the device scheme must not be judged
    /// against the previous endpoint's selection.
    appearance_for: Option<EndpointId>,
    /// The endpoint and scheme the device appearance last selected, so a
    /// scheme picked by hand afterwards stands until the device flips.
    auto_theme_applied: Option<(EndpointId, String)>,
    /// Fresh appearance snapshots from the background refresh.
    theme_rx: Receiver<(EndpointId, pilus_client::Result<Appearance>)>,
    /// Sender half for the background theme refresh.
    theme_tx: Sender<(EndpointId, pilus_client::Result<Appearance>)>,
    /// Optimistic tab pick from a click, until `tab.list` confirms it.
    tab_sel: Option<u16>,
    /// When the optimistic tab pick was made, so stale picks age out.
    tab_sel_at: Instant,
    /// Tabs last seen for each workspace, so a workspace click can show its
    /// strip before the daemon reports it.
    tabs_cache: HashMap<(EndpointId, String), Vec<Tab>>,
    /// A cached strip standing in for the daemon's until `tab.list` moves on.
    tabs_pick: Option<TabsPick>,
    /// Keeps the tab list scrolled when tabs overflow the strip.
    tab_scroll: ScrollHandle,
    /// The rows the sidebar's shared hover pill can land on.
    sidebar_hover: style::HoverSlot,
    /// The row the sidebar's selection pill is gliding to.
    sidebar_selection: style::GlideSlot,
    /// The active workspace's quieter pill while a focused agent holds the
    /// main selection, so the workspace the agent lives in stays marked.
    sidebar_context: style::GlideSlot,
    /// The tab index the strip last scrolled to, so it only follows changes.
    tab_scrolled: std::cell::Cell<Option<u16>>,
    /// Where each tab was drawn last frame, so a reorder slides from there.
    tab_bounds: style::BoundsLedger,
    /// The spring a reorder set running, until the next one replaces it.
    tab_slide: Option<TabSlide>,
    /// Optimistic endpoint pick from a click, until the handoff commits or
    /// every stage of the switch has ended.
    endpoint_sel: Option<EndpointId>,
    /// Optimistic focused agent (endpoint, pane), until `agent.list` confirms.
    agent_sel: Option<(EndpointId, String)>,
    /// When the optimistic agent pick was made, so stale picks age out.
    agent_sel_at: Instant,
    /// The sidebar layout we superseded the daemon's with, once known.
    sidebar_layout: Option<pilus_protocol::SidebarsConfig>,
    /// Whether workspace paths are shown in that layout. Persisted in the
    /// client's settings, not adopted from the daemon: the sidebar is ours.
    sidebar_workspace_paths: bool,
    /// Whether the AGENTS rows draw their path line. The daemon projects no
    /// client flag for the agents list, so this is purely local. Both path
    /// toggles default to hidden and persist through `Settings`.
    sidebar_agent_paths: bool,
    /// Whether AGENTS shows only the active workspace's agents.
    agents_this_workspace: bool,
    /// The open right-click menu on a WORKSPACES or AGENTS row, if any.
    context_menu: Option<SidebarMenu>,
    /// The rename/alias/branch prompt a menu action opened, if any.
    menu_prompt: Option<MenuPrompt>,
    /// The prompt's buffer, shared with `menu_prompt_input` so edits stay in
    /// one place and the entity keeps IME and selection.
    menu_prompt_field: TextField,
    /// The prompt's field, an IME-capable entity over `menu_prompt_field`.
    menu_prompt_input: Entity<TextInput>,
    /// Revision we last sent, so the daemon accepts each update.
    sidebar_revision: u64,
    /// Whether the native sidebar is expanded.
    sidebar_visible: bool,
    /// Whether the PINNED WORKSPACES section is collapsed to its header.
    pinned_collapsed: bool,
    /// Whether the WORKSPACES section is collapsed to its header.
    workspaces_collapsed: bool,
    /// Keys of the machines whose workspaces are folded under their row.
    machines_collapsed: HashSet<String>,
    /// The machine row toggled last and when, so only its chevron turns.
    machine_toggled: Option<(String, Instant)>,
    /// Whether the AGENTS section is collapsed to its header.
    agents_collapsed: bool,
    /// Whether the SIPIL section is collapsed to its header.
    sipil_collapsed: bool,
    /// A system notification for a sipil message raised the app icon's badge.
    sipil_badged: bool,
    /// What sipil last reported, for the sidebar's two sections.
    sipil: std::sync::Arc<sipil::SipilState>,
    /// The right sidebar's Sipil tab, made when it is first opened.
    sipil_panel: Option<Entity<sipil_panel::SipilPanel>>,
    /// The chat page for a sipil agent or session.
    sipil_chat: sipil_chat::SipilChat,
    /// Successful `worktree.list` answers by `(endpoint key, workspace id)`.
    /// The inner option is the linked checkout root; `None` means a main
    /// checkout or plain folder. Missing keys are unknown and remain retryable.
    /// Keyed by the stable workspace id, not the daemon's index: closing a
    /// workspace renumbers the rows, and an index reuse would hide a fresh
    /// worktree behind a stale answer.
    worktree_linked: HashMap<(String, String), Option<String>>,
    /// Workspaces whose worktree probe is still in flight, so it never
    /// double-runs.
    worktree_probing: HashSet<(String, String)>,
    /// Results from background context-menu operations.
    menu_rx: Receiver<MenuOpResult>,
    /// Sender half, handed to each background menu operation.
    menu_tx: Sender<MenuOpResult>,
    /// Keeps the sidebar's sections scrolled under the fixed top clearance.
    sidebar_scroll: ScrollHandle,
    /// The endpoint currently loaded into the active fields above.
    active: EndpointId,
    /// The active machine's profile, when a machine owns the surface.
    active_machine: Option<MachineInfo>,
    /// Connected endpoints that are not currently active.
    endpoints: HashMap<EndpointId, Endpoint>,
    /// Endpoint ids with a background connection in flight.
    connecting: HashSet<EndpointId>,
    /// The most recent endpoint a click asked for, awaiting its connection.
    wanted: Option<EndpointRequest>,
    /// A handoff waiting for the target's prepared frame.
    pending_switch: Option<PendingSwitch>,
    /// Monotonic ticket source for prepared frames.
    switch_ticket: u64,
    /// Nonce source for hidden-endpoint liveness probes.
    health_nonce: u64,
    /// Background machine connections.
    machine_connect_rx: Receiver<MachineConnect>,
    machine_connect_tx: Sender<MachineConnect>,
    /// Enabled machines auto-connected once this lifetime, so a slow catalog
    /// poll never restarts a bridge or retries a host that already failed.
    auto_connected: HashSet<String>,
    /// Automatic reconnect state per machine id, with capped backoff.
    reconnects: HashMap<String, Reconnect>,
    /// Automatic reconnect state for the owner-local daemon.
    local_reconnect: Option<Reconnect>,
    /// The last local attach failed because no `luvus` binary exists. The
    /// reconnect loop keeps probing, so installing it clears this by itself.
    local_luvus_missing: bool,
    /// The in-window Luvus install, driven from the missing-Luvus prompt.
    luvus_install: LuvusInstall,
    luvus_install_rx: Receiver<Result<PathBuf, String>>,
    luvus_install_tx: Sender<Result<PathBuf, String>>,
    /// Whether the active connection is still alive; a dropped machine
    /// endpoint is replaced in place when its reconnect succeeds.
    active_live: bool,
    /// A `MachineCatalogChanged` arrived; refresh profiles on the next tick.
    catalog_dirty: bool,
    /// OTA updater entity and the last status the view painted.
    updater: Entity<Updater>,
    last_update_status: UpdateStatus,
    /// Update feed URL while the Settings page is editing it.
    update_feed: TextField,
    /// The feed-URL input, an IME-capable field over `update_feed`.
    update_feed_input: Entity<TextInput>,
    /// Whether the update feed URL field owns the keyboard.
    update_url_editing: bool,
    /// The relay URL and secret inputs on the Settings page, over their fields.
    relay_url: TextField,
    relay_url_input: Entity<TextInput>,
    relay_secret: TextField,
    relay_secret_input: Entity<TextInput>,
    /// The relay field being edited, if any.
    relay_editing: Option<RelayField>,
    /// One line under the relay fields: a validation error or what saving did.
    relay_status: String,
    /// Results from background `luvus update` / `server restart` runs.
    daemon_rx: Receiver<DaemonOutcome>,
    /// Sender half, handed to each background daemon command.
    daemon_tx: Sender<DaemonOutcome>,
    /// Whether a daemon check, update, or restart is running.
    daemon_busy: bool,
    /// Result text shown under the daemon controls.
    daemon_status: String,
    /// The installed `luvus` binary's version from the last check.
    daemon_installed: Option<Version>,
    /// The latest published Luvus version from the last check.
    daemon_latest: Option<Version>,
    /// A restart awaiting its second click, because it stops every pane.
    daemon_restart_pending: bool,
    /// Machine-manager revision signal, shared with the manager window.
    machine_signal: Rc<RefCell<MachineSignal>>,
    /// Last revision this window applied, so a manager mutation reloads once.
    seen_machine_revision: u64,
    /// [`mermaid::generation`] at the last repaint, so a diagram that finishes
    /// drawing replaces its code card.
    seen_diagrams: u64,
    /// Fresh remote-home answers from the background resolver.
    home_resolve_rx: Receiver<HomeResolve>,
    /// Sender half for the background home resolver.
    home_resolve_tx: Sender<HomeResolve>,
    /// Remote `$HOME` by endpoint key, for expanding `~` workspace dirs.
    remote_home: HashMap<String, String>,
    /// A home resolution is running off-thread; no overlapping spawns.
    home_resolve_pending: bool,
    /// The Home composer's state.
    home: HomePanel,
    /// The composer's prompt, an IME-capable field over `home`'s buffer.
    home_prompt: Entity<TextInput>,
    /// The Tasks page's own state; the tasks come from `tasks` (the feed).
    tasks_panel: TasksPanel,
    /// The Tasks composer's text fields.
    tasks_inputs: TasksInputs,
    /// Daemon answers to Tasks page mutations, run off the GUI thread.
    tasks_op_rx: Receiver<task_compose::TasksMsg>,
    tasks_op_tx: Sender<task_compose::TasksMsg>,
    /// The native Diff page: the daemon's read-only change review.
    diff_panel: DiffPanel,
    /// The Diff tab's note and search fields.
    diff_inputs: DiffInputs,
    /// The Git tab: the daemon's working-tree status and recent commits.
    git_panel: GitPanel,
    /// The account's GitHub sections above WORKSPACES.
    github: GithubFeed,
    /// The page a GitHub row opens in the terminal's card.
    github_page: GithubPage,
    /// The page that searches the agents' saved sessions.
    search: SearchPage,
    /// The Search page's query field, over `search`'s buffer.
    search_input: Entity<TextInput>,
    /// The page that totals the agents' token use.
    overview: OverviewPage,
    stats: StatsPage,
    process: ProcessPage,
    database: DatabasePage,
    files_panel: FilesPanel,
    /// Notes on excerpts of markdown files, and the field they are written in.
    md_notes: md_notes_view::MdNotes,
    md_note_input: Entity<TextInput>,
    /// The source of the markdown file being edited in the Files tab.
    md_editor_input: Entity<TextInput>,
    /// Find in the file being edited, and its query field.
    file_find: file_find::FileFind,
    find_input: Entity<TextInput>,
    /// The ⌘P file picker, and its query field.
    quick_open: quick_open::QuickOpen,
    quick_open_input: Entity<TextInput>,
    ports_panel: PortsPanel,
    browser_panel: BrowserPanel,
    /// The Browser tab's address bar, an IME-capable field of its own.
    browser_input: Entity<TextInput>,
    /// The right sidebar (Changes and History tabs).
    right: RightSidebar,
    /// The native Mission Control page: its state and the daemon's last
    /// dashboard snapshot.
    mission: MissionPanel,
    /// Mission Control's reply field for a blocked agent.
    mission_answer: Entity<TextInput>,
    /// The reply field under an agent opened on the Home page.
    overview_reply: Entity<TextInput>,
    /// The native Automations page: its state and the daemon's last read of
    /// the definitions.
    automations_page: AutomationsPanel,
    /// The terminal sharing the card with the open Mission Control, Tasks,
    /// or Automations page. Cleared whenever none of them is open.
    page_split: Option<PageSplit>,
    /// Agent availability and models for the Home composer.
    agent_configs: Vec<AgentConfig>,
    /// Agent configs by machine key (`local` or a machine id), so switching
    /// back to a machine is instant.
    agent_config_cache: HashMap<String, Vec<AgentConfig>>,
    /// Machine keys with an agent-config fetch in flight.
    agent_config_pending: HashSet<String>,
    /// The machine key `agent_configs` belongs to.
    agent_config_key: Option<String>,
    /// Fresh agent configs from the background fetcher.
    agent_config_rx: Receiver<(String, Vec<AgentConfig>)>,
    /// Sender half for the background agent-config fetcher.
    agent_config_tx: Sender<(String, Vec<AgentConfig>)>,
    /// Signature of the last Home option build, so a large model list is not
    /// cloned every frame.
    home_sync_key: String,
    /// The agent whose defaults were last applied to model and effort.
    home_defaults_for: Option<String>,
    /// The model whose reasoning levels are currently in the effort picker.
    home_efforts_for: Option<String>,
    /// A Home launch is in flight; the composer ignores a second send until it
    /// settles, since Home stays open until the agent is ready.
    home_launching: bool,
    /// A workspace opened from the composer's picker (its endpoint and absolute
    /// path), awaiting its catalog row so the Workspace picker can select it.
    home_pending_open: Option<(EndpointId, String)>,
    /// Whether the Settings page covers the terminal card.
    settings_open: bool,
    /// The Settings page's one-line status, above the page footer.
    settings_status: String,
    /// The machine the last run ended on, until the local daemon is attached
    /// and its profile is in the catalog so the window can move there.
    restore_endpoint: Option<String>,
}

impl PilusView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        target: ConnectTarget,
        connection: Option<Connection>,
        shared_settings: Rc<RefCell<Settings>>,
        machine_signal: Rc<RefCell<MachineSignal>>,
        shared_theme: Rc<RefCell<Theme>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // Remember the main window's frame across launches; the observer
        // reports moves and resizes alike.
        cx.observe_window_bounds(window, |view, window, cx| {
            view.remember_window_bounds(window, cx);
        })
        .detach();
        // A quit flushes the frame without waiting for the debounce.
        cx.on_app_quit(|view, _cx| {
            view.remember_location();
            view.settings.save(&view.pilus_root);
            view.ports_stop_all();
            async {}
        })
        .detach();

        // Tabs and widgets come from UHP, which is addressed by session. Find
        // the state root from the launch target, including explicit sockets,
        // so `--socket` builds still list sessions and feed the right tabs.
        let (session_root, session_name) = match &target.socket {
            Some(path) => split_client_socket(path).unwrap_or_else(|| (config_dir(), None)),
            None => (config_dir(), target.session.clone()),
        };
        let pilus_root = pilus_config_dir();
        let github = GithubFeed::new(&pilus_root);
        let agent_feed = Some(AgentFeed::spawn(api_socket_for(
            &session_root,
            session_name.as_deref(),
        )));
        let sessions = list_session_infos(&session_root);
        // The machine catalog is loaded off-thread below, so a slow `luvus`
        // CLI never delays the window opening.
        let (machine_tx, machine_rx) = std::sync::mpsc::channel();
        let (machine_list_tx, machine_list_rx) = std::sync::mpsc::channel();
        let (machine_op_tx, machine_op_rx) = std::sync::mpsc::channel();
        let (luvus_version_tx, luvus_version_rx) = std::sync::mpsc::channel();
        let (tasks_op_tx, tasks_op_rx) = std::sync::mpsc::channel();
        let (home_resolve_tx, home_resolve_rx) = std::sync::mpsc::channel();
        let (agent_config_tx, agent_config_rx) = std::sync::mpsc::channel();
        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let (machine_connect_tx, machine_connect_rx) = std::sync::mpsc::channel();
        let (picker_tx, picker_rx) = std::sync::mpsc::channel();
        let (menu_tx, menu_rx) = std::sync::mpsc::channel();
        let (theme_tx, theme_rx) = std::sync::mpsc::channel();
        let (daemon_tx, daemon_rx) = std::sync::mpsc::channel();
        let (luvus_install_tx, luvus_install_rx) = std::sync::mpsc::channel();
        let settings = shared_settings.borrow().clone();
        // The path toggles seed the sidebar flags; the daemon's own
        // `workspace_paths` is deliberately not adopted, since the client
        // owns its sidebar and these are the user's persisted choices.
        let sidebar_workspace_paths = settings.sidebar_workspace_paths;
        let sidebar_agent_paths = settings.sidebar_agent_paths;
        let update_request = UpdateRequest::from_settings(
            &settings.update_channel,
            settings.update_feed_url.clone(),
        );
        let updater = cx.new(|cx| Updater::from_request(&update_request, cx));
        let auto_check = settings.auto_check_updates;
        let initial_theme = *shared_theme.borrow();
        let initial_fonts = style::Fonts::resolve(&shared_settings);
        let home = HomePanel::default();
        let home_prompt = cx.new(|cx| {
            TextInput::from_shared(
                home.prompt().shared(),
                "Describe the work…",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                true,
                initial_theme,
                cx,
            )
            .submit_on_enter()
        });
        let sipil_chat = sipil_chat::SipilChat::new(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let mission_answer = mission::answer_input(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let overview_reply = mission::answer_input(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let tasks_inputs = TasksInputs::new(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let mut diff_panel = DiffPanel::new();
        diff_panel.set_list_height(settings.diff_list_height);
        diff_panel.set_list_width(settings.diff_list_width);
        let mut git_panel = GitPanel::new();
        git_panel.set_list_height(settings.git_list_height);
        let mut files_panel = FilesPanel::new();
        files_panel.restore_folds(&settings.files_folded);
        let right = RightSidebar::new(
            settings.right_sidebar_visible,
            settings.right_sidebar_width,
            SidebarTab::from_wire(&settings.right_sidebar_tab),
        );
        let diff_inputs = DiffInputs::new(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let md_notes = md_notes_view::MdNotes::new(&pilus_root);
        let md_note_input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "What should the agent do about this?",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                true,
                initial_theme,
                cx,
            )
            .submit_on_enter()
        });
        let md_editor_input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                true,
                initial_theme,
                cx,
            )
        });
        let find_input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Find",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });
        let quick_open = quick_open::QuickOpen::new();
        let quick_open_input = cx.new(|cx| {
            TextInput::from_shared(
                quick_open.input().shared(),
                "Go to file…",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });
        let picker = WorkspacePicker::default();
        let picker_path = cx.new(|cx| {
            TextInput::from_shared(
                picker.path().shared(),
                "Path",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });

        let menu_prompt_field = TextField::default();
        let menu_prompt_input = cx.new(|cx| {
            TextInput::from_shared(
                menu_prompt_field.shared(),
                "",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
            .submit_on_enter()
        });

        let update_feed = TextField::default();
        let update_feed_input = cx.new(|cx| {
            TextInput::from_shared(
                update_feed.shared(),
                "Feed URL",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });

        let relay_url = TextField::default();
        let relay_url_input = cx.new(|cx| {
            TextInput::from_shared(
                relay_url.shared(),
                "https://relay.example, https://relay2.example",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });
        let relay_secret = TextField::default();
        let relay_secret_input = cx.new(|cx| {
            TextInput::from_shared(
                relay_secret.shared(),
                "Project API secret",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });

        let palette = Palette::default();
        let palette_input = cx.new(|cx| {
            TextInput::from_shared(
                palette.input().shared(),
                "Type a command…",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });
        // A query edit re-points the highlight at the best match. The field
        // also notifies on caret blinks, which must not move the selection.
        cx.observe(&palette_input, |this, _, cx| {
            this.palette.refresh_selection();
            cx.notify();
        })
        .detach();

        // A query edit ranks the files again. The field also notifies on caret
        // blinks, which `refresh` ignores.
        cx.observe(&quick_open_input, |this, _, cx| {
            let root = this.files_panel.tree_root();
            this.quick_open.refresh(root.as_deref());
            cx.notify();
        })
        .detach();

        let search = SearchPage::default();
        let search_input = cx.new(|cx| {
            TextInput::from_shared(
                search.field.shared(),
                "Search sessions by title, folder or branch",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
        });
        // The field also notifies on caret blinks, which must not rerun the
        // query.
        cx.observe(&search_input, |this, input, cx| {
            let query = input.read(cx).value();
            if query != this.search.query() {
                this.search.set_query(query);
                this.search_requery();
                cx.notify();
            }
        })
        .detach();

        // The file list filters on what the search field holds. The field also
        // notifies on caret blinks, which must not repaint the page.
        cx.observe(&diff_inputs.search, |this, input, cx| {
            let query = input.read(cx).value();
            if this.diff_panel.set_query(query) {
                cx.notify();
            }
        })
        .detach();

        let browser_input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Search or enter address",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                false,
                initial_theme,
                cx,
            )
            .submit_on_enter()
        });

        let active_live = connection.is_some();
        let mut view = Self {
            target,
            connection,
            surface: Surface::new(),
            theme: Theme::default(),
            shared_theme,
            focus_handle,
            settings,
            shared_settings,
            glass: Glass::new(),
            energy_applied: false,
            menu_energy: false,
            sidebar_anim: None,
            cursor_blink: cursor_blink::CursorBlink::new(Instant::now()),
            toolbar_fade: toolbar_fade::ToolbarFade::new(Instant::now()),
            last_sidebar_dock: None,
            sidebar_freeze: None,
            sidebar_open: 1.0,
            section_toggled: None,
            window_dirty: false,
            window_moved_at: Instant::now(),
            cols: 120,
            rows: 40,
            status: "connecting…".to_string(),
            window_active: false,
            toasts: Toasts::new(),
            dock: Dock::new(),
            menubar: MenuBar::new(cx.entity_id().as_u64()),
            window_handle: window.window_handle(),
            menu_pick: None,
            saved_pick: None,
            attention: attention::Watch::default(),
            cell_width: None,
            sent_cell: (0, 0),
            selection: None,
            dragging: false,
            drag_moved: false,
            press_anchor: None,
            hover_cell: None,
            dividers: Vec::new(),
            divider_hover: None,
            divider_drag: None,
            control_held: false,
            link_press: false,
            agent_feed,
            session_root,
            pilus_root,
            session_name,
            sessions,
            last_session_poll: Instant::now(),
            session_menu_open: false,
            session_confirm: None,
            session_icon_editing: None,
            session_working: HashSet::new(),
            palette,
            palette_input,
            palette_scroll: ScrollHandle::new(),
            picker: WorkspacePicker::default(),
            picker_path,
            picker_scroll: ScrollHandle::new(),
            picker_rx,
            picker_tx,
            session_rx,
            session_tx,
            bar_widgets: Vec::new(),
            tabs: Vec::new(),
            agents: Vec::new(),
            agents_loaded: false,
            focused_zoomed: false,
            automations: AutomationSummary::default(),
            tasks: TaskSummary::default(),
            machines: Vec::new(),
            last_machine_poll: Instant::now() - Duration::from_secs(11),
            machine_refreshing: false,
            machine_rx,
            machine_list_rx,
            machine_tx,
            machine_list_tx,
            machine_op_rx,
            machine_op_tx,
            machine_busy: None,
            remote: connector::snapshot(),
            machine_pending_remove: None,
            machine_follow: machines::FollowUps::default(),
            luvus_local_version: None,
            luvus_version_rx,
            luvus_version_tx,
            pairing_operate: false,
            pairing_second: 0,
            appearance: Appearance::default(),
            last_theme_poll: Instant::now() - Duration::from_secs(11),
            theme_endpoint: EndpointId::Local,
            theme_refreshing: false,
            appearance_for: None,
            auto_theme_applied: None,
            theme_rx,
            theme_tx,
            tab_sel: None,
            tabs_cache: HashMap::new(),
            tabs_pick: None,
            tab_sel_at: Instant::now(),
            tab_scroll: ScrollHandle::new(),
            sidebar_hover: style::HoverSlot::default(),
            sidebar_selection: style::GlideSlot::default(),
            sidebar_context: style::GlideSlot::default(),
            tab_scrolled: std::cell::Cell::new(None),
            tab_bounds: style::BoundsLedger::default(),
            tab_slide: None,
            endpoint_sel: None,
            agent_sel: None,
            agent_sel_at: Instant::now(),
            sidebar_layout: None,
            sidebar_workspace_paths,
            sidebar_agent_paths,
            agents_this_workspace: false,
            context_menu: None,
            menu_prompt: None,
            menu_prompt_field,
            menu_prompt_input,
            worktree_linked: HashMap::new(),
            worktree_probing: HashSet::new(),
            sidebar_revision: 0,
            sidebar_visible: true,
            pinned_collapsed: false,
            workspaces_collapsed: false,
            machines_collapsed: HashSet::new(),
            machine_toggled: None,
            agents_collapsed: false,
            sipil_collapsed: false,
            sipil_badged: false,
            sipil: sipil::snapshot(),
            sipil_panel: None,
            sipil_chat,
            menu_rx,
            menu_tx,
            sidebar_scroll: ScrollHandle::new(),
            active: EndpointId::Local,
            active_machine: None,
            endpoints: HashMap::new(),
            connecting: HashSet::new(),
            wanted: None,
            pending_switch: None,
            switch_ticket: 0,
            health_nonce: 0,
            machine_connect_rx,
            machine_connect_tx,
            auto_connected: HashSet::new(),
            reconnects: HashMap::new(),
            local_reconnect: None,
            local_luvus_missing: false,
            luvus_install: LuvusInstall::Idle,
            luvus_install_rx,
            luvus_install_tx,
            active_live,
            // Force the first probe, which also seeds the Local row; the cheap
            // reload only supplies the saved profiles.
            catalog_dirty: true,
            updater: updater.clone(),
            last_update_status: UpdateStatus::Idle,
            update_feed,
            update_feed_input,
            update_url_editing: false,
            relay_url,
            relay_url_input,
            relay_secret,
            relay_secret_input,
            relay_editing: None,
            relay_status: String::new(),
            daemon_rx,
            daemon_tx,
            daemon_busy: false,
            daemon_status: String::new(),
            daemon_installed: None,
            daemon_latest: None,
            daemon_restart_pending: false,
            machine_signal,
            seen_machine_revision: 0,
            seen_diagrams: 0,
            home_resolve_rx,
            home_resolve_tx,
            home_resolve_pending: false,
            remote_home: HashMap::new(),
            home,
            home_prompt,
            tasks_panel: TasksPanel::default(),
            tasks_inputs,
            tasks_op_rx,
            tasks_op_tx,
            diff_panel,
            diff_inputs,
            git_panel,
            github,
            github_page: GithubPage::default(),
            search,
            search_input,
            overview: OverviewPage::default(),
            stats: StatsPage::default(),
            process: ProcessPage::default(),
            database: DatabasePage::default(),
            files_panel,
            md_notes,
            md_note_input,
            md_editor_input,
            file_find: file_find::FileFind::default(),
            find_input,
            quick_open,
            quick_open_input,
            ports_panel: PortsPanel::new(),
            browser_panel: BrowserPanel::new(),
            browser_input,
            right,
            mission: MissionPanel::default(),
            mission_answer,
            overview_reply,
            automations_page: AutomationsPanel::default(),
            page_split: None,
            agent_configs: Vec::new(),
            agent_config_cache: HashMap::new(),
            agent_config_pending: HashSet::new(),
            agent_config_key: None,
            agent_config_rx,
            agent_config_tx,
            home_sync_key: String::new(),
            home_defaults_for: None,
            home_efforts_for: None,
            home_launching: false,
            home_pending_open: None,
            settings_open: false,
            settings_status: String::new(),
            restore_endpoint: None,
        };
        // Seed the remembered frame from the one the window actually opened
        // with; moves and resizes update it from here on.
        view.remember_window_bounds(window, cx);

        if auto_check {
            updater.update(cx, |updater, cx| updater.check(cx));
        }

        // Attach the owner-local daemon in the background, exactly like a
        // machine bridge, so a handshake rejection lands in the empty-grid
        // message instead of aborting the launch.
        if view.connection.is_none() {
            view.spawn_local_connect();
        }
        // Seed the catalog off-thread; the first probe follows once it lands.
        view.reload_machines();
        // Bridge every enabled machine up front so their workspaces are ready.
        view.auto_connect_machines();

        cx.spawn(async move |this, cx| {
            loop {
                let wait = this.update(cx, |view, cx| {
                    view.persist_window_bounds();
                    let changed = view.sync_settings(cx);
                    let energy = view.sync_energy(cx);
                    // The sidebar tween rides this loop: it ticks and
                    // repaints here, so no second driver can stall unseen.
                    // A settling tween repaints once more so a trailing
                    // resize still lands when the daemon is otherwise idle:
                    // `was_animating` covers the tick that returns settled.
                    let was_animating = view.sidebar_anim.is_some() || view.right.moving();
                    let settled = view.tick_sidebar_anim() & view.right.tick();
                    let released = view.release_sidebar_freeze();
                    let blinked = view.tick_cursor_blink(cx);
                    let faded = view.tick_toolbar_fade(cx);
                    perf::count(perf::Count::Tick);
                    let moving = !settled
                        || was_animating
                        || released
                        || blinked
                        || faded
                        || view.toasts.any()
                        || view.sidebar_freeze.is_some();
                    let state = view.poll(cx) || changed;
                    if state || moving {
                        perf::count(if state {
                            perf::Count::RepaintState
                        } else {
                            perf::Count::RepaintMotion
                        });
                        cx.notify();
                    }
                    // Every window reports the same answer, so the pollers
                    // elsewhere can tell when the app is behind another one.
                    let front = dock::app_active().unwrap_or(view.window_active);
                    activity::set_front(front);
                    next_tick(moving, front, energy)
                });
                let Ok(wait) = wait else {
                    break;
                };
                tick_or_arrival(cx.background_executor().timer(wait)).await;
            }
        })
        .detach();

        view
    }

    /// The active bridge's session name, or an empty label while none is
    /// attached. The status line and notification title read this, so a
    /// missing bridge degrades to a bare name instead of failing.
    fn active_session(&self) -> String {
        self.connection
            .as_ref()
            .map(|connection| connection.identity().session.clone())
            .unwrap_or_default()
    }

    /// Send a message to the active bridge when one exists. A missing bridge
    /// is a no-op: the reconnect loop owns recovery, and every caller already
    /// ignores send failures.
    fn send_active(&self, message: ClientMessage) {
        if let Some(connection) = self.connection.as_ref() {
            let _ = connection.send(message);
        }
    }

    /// Whether the window has no daemon to show because Luvus is not
    /// installed. Everything that talks to the daemon, from the tab strip to
    /// the shortcuts, stands down until the install appears.
    fn luvus_missing(&self) -> bool {
        self.local_luvus_missing && !self.active_live && self.active == EndpointId::Local
    }

    /// Probe for the daemon now instead of waiting out the backoff.
    fn recheck_luvus(&mut self, cx: &mut Context<Self>) {
        self.local_reconnect = None;
        self.spawn_local_connect();
        cx.notify();
    }

    /// Download and install Luvus off the GUI thread. The poll loop picks up
    /// the result and attaches to the daemon it starts.
    fn install_luvus(&mut self, cx: &mut Context<Self>) {
        if self.luvus_install == LuvusInstall::Installing {
            return;
        }
        self.luvus_install = LuvusInstall::Installing;
        let tx = self.luvus_install_tx.clone();
        std::thread::spawn(move || {
            let engine = crate::luvus_update::first_install_engine();
            let result = match crate::luvus_update::check(&engine) {
                Ok(Some(release)) => crate::luvus_update::install(&engine, &release, |_, _| {})
                    .map_err(|error| error.to_string()),
                Ok(None) => Err("no Luvus release is published for this platform".to_string()),
                Err(error) => Err(error.to_string()),
            };
            tx.send(result).ok();
        });
        cx.notify();
    }

    /// The call to action that fills the card while Luvus is not installed.
    fn install_luvus_prompt(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let installing = self.luvus_install == LuvusInstall::Installing;
        let checking = self.connecting.contains(&EndpointId::Local);
        let install_label = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .when(installing, |label| {
                label.child(style::spinner(
                    "pilus-install-luvus-spinner",
                    fonts.secondary,
                    theme.window_bg,
                ))
            })
            .child(if installing {
                "Installing…"
            } else {
                "Install Luvus"
            });
        let mut prompt = style::empty_state(
            theme,
            fonts,
            icon(
                include_bytes!("../../assets/icons/server.svg"),
                fonts.headline,
                theme.muted,
            ),
            "Luvus is not installed",
            "pilus is a client for the Luvus daemon. Install Luvus to start workspaces and agents.",
        )
        .size_full()
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    style::button_enabled(
                        "pilus-install-luvus",
                        install_label,
                        style::ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                        !installing,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.install_luvus(cx))),
                )
                .child(
                    style::button(
                        "pilus-recheck-luvus",
                        if checking {
                            "Checking…"
                        } else {
                            "Check again"
                        },
                        style::ButtonKind::Secondary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.recheck_luvus(cx))),
                )
                .child(
                    style::button(
                        "pilus-install-luvus-manually",
                        "Install manually",
                        style::ButtonKind::Ghost,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.open_url(LUVUS_INSTALL_URL))),
                ),
        );
        if let LuvusInstall::Failed(error) = &self.luvus_install {
            prompt = prompt.child(
                div()
                    .max_w(px(fonts.secondary * 30.0))
                    .text_size(px(fonts.secondary))
                    .text_color(theme.alert)
                    .child(format!("Install failed: {error}")),
            );
        }
        prompt
    }

    /// Tell the active bridge how much surface we want, when one exists.
    fn set_active_interest(&self, interest: SurfaceInterest) {
        if let Some(connection) = self.connection.as_ref() {
            let _ = connection.set_interest(interest);
        }
    }

    /// Drain pending connection events. Returns whether a repaint is needed.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        // Track updater transitions here so a failure from an automatic check
        // can open the Update window once.
        let update_status = self.updater.read(cx).status().clone();
        if update_status != self.last_update_status {
            let was_error = matches!(self.last_update_status, UpdateStatus::Errored(_));
            self.last_update_status = update_status.clone();
            changed = true;
            // A failure surfaces in a window with a retry button.
            if matches!(update_status, UpdateStatus::Errored(_))
                && !was_error
                && !update_window::is_open(cx)
            {
                update_window::open(
                    self.updater.clone(),
                    self.theme,
                    self.shared_settings.clone(),
                    cx,
                );
            }
        }
        // The machine manager bumps this on every successful mutation; reload
        // the catalog so added, removed, or re-enabled hosts appear at once.
        let diagrams = mermaid::generation();
        if diagrams != self.seen_diagrams {
            self.seen_diagrams = diagrams;
            changed = true;
        }
        let machine_revision = self.machine_signal.borrow().revision;
        if machine_revision != self.seen_machine_revision {
            self.seen_machine_revision = machine_revision;
            self.catalog_dirty = true;
            changed = true;
        }
        let remote = connector::snapshot();
        if remote != self.remote {
            self.remote = remote;
            changed = true;
        }
        let sipil = sipil::snapshot();
        if !std::sync::Arc::ptr_eq(&sipil, &self.sipil) {
            self.sipil = sipil;
            changed = true;
        }
        changed |= self.announce_sipil(cx);
        changed |= self.sipil_chat_tick(cx);
        // The pairing card counts down in whole seconds.
        if self.settings_open && self.remote.code.is_some() {
            let second = tasks::unix_now();
            if second != self.pairing_second {
                self.pairing_second = second;
                changed = true;
            }
        }
        changed |= self.poll_active(cx);
        changed |= self
            .toasts
            .tick(Instant::now(), self.window_active, cx.reduce_motion());
        self.dock.sync();
        changed |= self.poll_endpoints();
        self.sync_menubar(cx);
        changed |= self.poll_machine_connects();
        self.pump_reconnects();
        self.drive_pending_switch();
        self.expire_pending_switch();
        let updates: Vec<_> = self
            .agent_feed
            .as_ref()
            .map(|feed| std::iter::from_fn(|| feed.try_recv()).collect())
            .unwrap_or_default();
        for update in updates {
            self.attention.observe(&self.active.key(), &update.agents);
            self.focused_zoomed = update.focused_zoomed();
            self.bar_widgets = update.bar;
            self.adopt_tabs(update.tabs);
            self.agents = update.agents;
            self.agents_loaded = true;
            self.automations = update.automations;
            self.tasks = update.tasks;
            changed = true;
        }

        changed |= self.raise_attention(cx);
        // Drop the optimistic endpoint pick once the handoff commits (the
        // committed highlight needs no pick) or every stage of the switch has
        // ended without one (the highlight must revert to the real surface).
        if let Some(selected) = self.endpoint_sel.clone()
            && (selected == self.active || !self.switch_in_flight(&selected))
        {
            self.endpoint_sel = None;
            if selected != self.active {
                changed = true;
            }
        }
        // Drop the optimistic tab pick once `tab.list` agrees or it goes stale.
        // The window covers a remote click's two SSH round trips (command,
        // then the refresh that reads it back) with headroom for a slow link.
        if let Some(selected) = self.tab_sel {
            let confirmed = self
                .tabs
                .iter()
                .any(|tab| tab.active && tab.index == selected);
            if confirmed || self.tab_sel_at.elapsed() > Duration::from_secs(4) {
                self.tab_sel = None;
            }
        }
        // Same for the optimistic focused agent, confirmed by `agent.list`.
        if let Some((endpoint, pane)) = self.agent_sel.clone() {
            let confirmed = endpoint == self.active
                && self
                    .agents
                    .iter()
                    .any(|agent| agent.pane == pane && agent.focused);
            if confirmed || self.agent_sel_at.elapsed() > Duration::from_secs(4) {
                self.agent_sel = None;
            }
        }
        if self.sidebar_layout.is_none()
            && let Some(sidebars) = self.surface.shell_sidebars().cloned()
        {
            // Own the whole sidebar: drop the daemon's agents dock so it stops
            // rendering, which lets the workspaces dock grow to fill the slot.
            let mut layout = sidebars.layout;
            remove_dock(&mut layout.left, "agents");
            remove_dock(&mut layout.right, "agents");
            self.sidebar_revision = sidebars.revision;
            self.sidebar_layout = Some(layout);
            self.push_sidebar_layout();
        }
        while let Ok(op) = self.session_rx.try_recv() {
            match op {
                SessionOpResult::Started(name) => {
                    self.session_working.remove(&name);
                    self.sessions = list_session_infos(&self.session_root);
                    self.switch_session(&name);
                }
                SessionOpResult::StartFailed(name, error) => {
                    self.session_working.remove(&name);
                    self.toast(ToastKind::Error, format!("couldn't start {name}: {error}"));
                }
                SessionOpResult::Done(name, verb) => {
                    self.session_working.remove(&name);
                    // A deleted session's name can be generated again later,
                    // and must not come back wearing the old label or icon.
                    if verb == SessionVerb::Delete
                        && (self.settings.session_labels.contains_key(&name)
                            || self.settings.session_icons.contains_key(&name))
                    {
                        self.settings.set_session_label(&name, "");
                        self.settings.set_session_icon(&name, DEFAULT_SESSION_ICON);
                        self.save_settings();
                    }
                    self.sessions = list_session_infos(&self.session_root);
                    self.toast(ToastKind::Success, format!("{} {name}", verb.done()));
                }
                SessionOpResult::Failed(name, verb, error) => {
                    self.session_working.remove(&name);
                    self.toast(
                        ToastKind::Error,
                        format!("couldn't {} {name}: {error}", verb.verb()),
                    );
                }
            }
            changed = true;
        }
        // Sessions change rarely; re-read the state root on a slow tick so a
        // created or stopped session appears without rescanning every frame.
        if self.last_session_poll.elapsed() >= activity::scaled(Duration::from_secs(2)) {
            self.last_session_poll = Instant::now();
            let fresh = list_session_infos(&self.session_root);
            if fresh != self.sessions {
                self.sessions = fresh;
                changed = true;
            }
        }
        changed |= self.restore_last_endpoint(cx);
        let needs_probe = self.machines.iter().any(|machine| {
            machine.id != "local"
                && machine.enabled
                && !self.reconnects.contains_key(&machine.id)
                && !self.machine_connected(&machine.id)
        });
        if (self.catalog_dirty
            || (needs_probe
                && self.last_machine_poll.elapsed() >= activity::scaled(Duration::from_secs(10)))
            // Reachability drifts, and a connected or disabled machine never
            // sets `needs_probe`, so a slow fallback keeps every status
            // honest instead of stuck on `probing…`.
            || self.last_machine_poll.elapsed() >= activity::scaled(Duration::from_secs(30)))
            && !self.machine_refreshing
        {
            self.catalog_dirty = false;
            self.probe_machines();
        }
        while let Ok((session, machines)) = self.machine_rx.try_recv() {
            self.machine_refreshing = false;
            // A probe that outlived a session switch describes the session the
            // window left; read the current one's catalog instead.
            if session != self.session_name {
                self.catalog_dirty = true;
                continue;
            }
            changed |= self.apply_machine_catalog(machines);
        }
        while let Ok((session, mut machines)) = self.machine_list_rx.try_recv() {
            if session != self.session_name {
                continue;
            }
            // `list_machines` omits the Local row and reachability; keep both
            // from the last probe so the section never flickers.
            if !machines.iter().any(|machine| machine.id == "local")
                && let Some(local) = self.machines.iter().find(|machine| machine.id == "local")
            {
                machines.insert(0, local.clone());
            }
            for machine in &mut machines {
                if machine.online.is_none()
                    && let Some(existing) =
                        self.machines.iter().find(|known| known.id == machine.id)
                {
                    machine.online = existing.online;
                    machine.latency_ms = existing.latency_ms;
                    machine.version.clone_from(&existing.version);
                }
            }
            changed |= self.apply_machine_catalog(machines);
        }
        while let Ok(outcome) = self.machine_op_rx.try_recv() {
            self.machine_busy = None;
            self.machine_follow.apply(&outcome);
            // Tell every open main window and the Add Machine window to reload.
            self.machine_signal.borrow_mut().revision += 1;
            let message = outcome.message();
            if self.settings_open {
                self.settings_status = message;
            } else {
                self.toast(ToastKind::Info, message);
            }
            changed = true;
        }
        while let Ok(version) = self.luvus_version_rx.try_recv() {
            self.luvus_local_version = version;
            changed = true;
        }
        if self.tasks_panel.is_open() {
            self.tasks_tick();
        }
        while let Ok(message) = self.tasks_op_rx.try_recv() {
            self.tasks_receive(message);
            changed = true;
        }
        changed |= self.diff_tick();
        changed |= self.git_tick();
        changed |= self.github_tick();
        changed |= self.github_page_tick();
        changed |= self.files_tick(cx);
        changed |= self.quick_open_tick();
        changed |= self.ports_tick();
        changed |= self.browser_tick(cx);
        changed |= self.mission_tick();
        changed |= self.automations_tick();
        changed |= self.search_tick(cx);
        changed |= self.overview_tick();
        changed |= self.process_tick();
        while let Ok(resolved) = self.home_resolve_rx.try_recv() {
            self.home_resolve_pending = false;
            for (endpoint_key, home) in resolved.homes {
                self.remote_home.insert(endpoint_key, home);
            }
        }
        // Remote workspaces need their machine's `$HOME` to shorten cwds;
        // resolve the unknown endpoints off-thread.
        self.resolve_remote_homes();
        // The theme registry changes only when the user selects one, and the
        // selection can also come from another window or the CLI, so a slow
        // poll keeps every pilus window on the same appearance. It asks the
        // daemon the window is showing: on a remote machine that is the
        // machine's, not the local one's. A machine is polled less often, and
        // moving to another endpoint polls it at once.
        if self.theme_endpoint != self.active {
            self.theme_endpoint = self.active.clone();
            self.last_theme_poll = Instant::now() - THEME_POLL_REMOTE;
        }
        let theme_poll = match self.active {
            EndpointId::Local => THEME_POLL_LOCAL,
            EndpointId::Machine(_) => THEME_POLL_REMOTE,
        };
        if !self.theme_refreshing && self.last_theme_poll.elapsed() >= activity::scaled(theme_poll)
        {
            self.theme_refreshing = true;
            self.last_theme_poll = Instant::now();
            let endpoint = self.active.clone();
            let transport = self.control_transport();
            let tx = self.theme_tx.clone();
            std::thread::spawn(move || {
                let result = ControlClient::from_transport(transport).appearance();
                tx.send((endpoint, result)).ok();
            });
        }
        while let Ok((endpoint, result)) = self.theme_rx.try_recv() {
            self.theme_refreshing = false;
            // A reply for an endpoint the window has since left is stale.
            if endpoint != self.active {
                continue;
            }
            if let Ok(appearance) = result {
                self.appearance_for = Some(endpoint);
                if appearance != self.appearance {
                    self.appearance = appearance;
                    self.apply_appearance();
                    changed = true;
                }
            }
        }
        self.follow_device_appearance(cx);
        while let Ok(result) = self.luvus_install_rx.try_recv() {
            match result {
                Ok(_) => {
                    self.luvus_install = LuvusInstall::Idle;
                    // The binary is in place; attach now rather than waiting
                    // out the reconnect backoff.
                    self.recheck_luvus(cx);
                }
                Err(error) => self.luvus_install = LuvusInstall::Failed(error),
            }
            changed = true;
        }
        while let Ok(outcome) = self.daemon_rx.try_recv() {
            self.daemon_busy = false;
            self.daemon_restart_pending = false;
            match outcome {
                DaemonOutcome::Checked { installed, result } => {
                    let installed_label = installed
                        .as_ref()
                        .map_or_else(|| "unknown".to_string(), |version| version.to_string());
                    self.daemon_installed = installed;
                    match result {
                        Ok(Some(release)) => {
                            self.daemon_status = format!(
                                "Luvus {} is available (installed {installed_label}).",
                                release.version
                            );
                            self.daemon_latest = Some(release.version);
                        }
                        Ok(None) => {
                            self.daemon_latest = None;
                            self.daemon_status =
                                format!("Luvus is up to date ({installed_label}).");
                        }
                        Err(error) => {
                            self.daemon_status = format!("could not check Luvus: {error}");
                        }
                    }
                }
                DaemonOutcome::Installed { version, path } => {
                    self.daemon_installed = Some(version.clone());
                    self.daemon_latest = None;
                    self.daemon_status = format!(
                        "Installed Luvus {version} at {}. Restart the daemon to load it.",
                        path.display()
                    );
                }
                DaemonOutcome::Message(result) => {
                    self.daemon_status = match result {
                        Ok(text) | Err(text) => text,
                    };
                }
            }
            changed = true;
        }
        while let Ok(event) = self.picker_rx.try_recv() {
            match event {
                PickerEvent::Listed { path, result } => {
                    self.picker.apply_listing(&path, result);
                    changed = true;
                }
                PickerEvent::Opened(result) => {
                    match result {
                        Ok(()) => self.toast(ToastKind::Success, "Opened the workspace"),
                        Err(error) => {
                            self.toast(
                                ToastKind::Error,
                                format!("Couldn't open workspace: {error}"),
                            );
                        }
                    }
                    changed = true;
                }
                PickerEvent::SavedOpened { saved, result } => {
                    self.saved_workspace_opened(saved, result, cx);
                    changed = true;
                }
                PickerEvent::WorktreeOpened(result) => {
                    match result {
                        Ok(()) => self.toast(ToastKind::Success, "Created a new worktree"),
                        Err(error) => {
                            self.toast(
                                ToastKind::Error,
                                format!("Couldn't create worktree: {error}"),
                            );
                        }
                    }
                    changed = true;
                }
                PickerEvent::LocalTerminalOpened(result) => {
                    match result {
                        Ok(()) => {
                            self.toast(ToastKind::Success, "Opened a local terminal");
                            self.activate_opened_endpoint(None, None, cx);
                        }
                        Err(error) => {
                            self.toast(
                                ToastKind::Error,
                                format!("Couldn't open a local terminal: {error}"),
                            );
                        }
                    }
                    changed = true;
                }
                PickerEvent::HomeLaunched { machine, result } => {
                    match result {
                        Ok((message, pane)) => {
                            // The agent is up and focused; only now leave the
                            // composer, so no empty terminal flashes first.
                            self.home_launching = false;
                            self.close_home_page();
                            self.toast(ToastKind::Success, message);
                            self.activate_opened_endpoint(machine, pane, cx);
                        }
                        // Keep the composer up so the launch can be retried.
                        Err(error) => {
                            self.home_launching = false;
                            self.home
                                .set_status(format!("couldn't start the agent: {error}"))
                        }
                    }
                    changed = true;
                }
            }
        }
        while let Ok(op) = self.menu_rx.try_recv() {
            match op.error {
                Some(error) => self.toast(ToastKind::Error, error),
                None if !op.ok.is_empty() => self.toast(ToastKind::Success, op.ok),
                None => {}
            }
            if let Some((workspace_id, name)) = op.renamed_workspace {
                self.rename_local_workspace_label(&workspace_id, &name);
            }
            if let Some((endpoint, pane, alias)) = op.renamed_agent {
                self.rename_local_agent(&endpoint, &pane, &alias);
            }
            // A probe answer updates the open menu's rows in place: a linked
            // checkout adds the Delete Worktree row without another
            // right-click. Errors are reported but not cached, so the next
            // menu open retries the probe.
            if let Some((endpoint, workspace_id, result)) = op.worktree_probe {
                let key = (endpoint, workspace_id);
                self.worktree_probing.remove(&key);
                match result {
                    Ok(path) => {
                        // The inserted row shifts every row index below it, so
                        // the keyboard cursor is dropped for that menu: Enter
                        // must not retarget a different item than the one the
                        // user highlighted.
                        let open_target = self.context_menu.as_ref().is_some_and(|menu| {
                            matches!(
                                &menu.target,
                                SidebarMenuTarget::Workspace {
                                    endpoint,
                                    workspace_id,
                                    ..
                                } if endpoint == &key.0 && workspace_id == &key.1
                            )
                        });
                        self.worktree_linked.insert(key, path);
                        if open_target && let Some(menu) = self.context_menu.as_mut() {
                            menu.selected = None;
                        }
                    }
                    Err(error) => {
                        self.toast(
                            ToastKind::Error,
                            format!("couldn't check worktree: {error}"),
                        );
                    }
                }
            }
            if let Some((endpoint, workspace_id)) = op.worktree_cleared {
                self.worktree_linked.remove(&(endpoint, workspace_id));
            }
            if let Some(held) = op.worktree_escalated {
                self.open_menu_prompt(
                    PromptAction::DeleteWorktree {
                        endpoint: held.endpoint,
                        path: held.path,
                        workspace_id: held.workspace_id,
                        name: held.name,
                        dirty: held.dirty,
                        blockers: held.blockers,
                    },
                    "Delete Worktree Anyway",
                    "",
                    "",
                    cx,
                );
            }
            changed = true;
        }
        while let Ok((key, configs)) = self.agent_config_rx.try_recv() {
            self.agent_config_pending.remove(&key);
            self.agent_config_cache.insert(key.clone(), configs.clone());
            if self.agent_config_key.as_deref() == Some(key.as_str()) {
                self.agent_configs = configs;
                changed = true;
            }
        }
        if self.home.is_open() {
            self.ensure_home_agent_configs();
        }
        changed
    }

    /// Drain the active endpoint's connection.
    fn poll_active(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let mut catalog = false;
        // Drain into an owned list first: the loop below mutates `self`, so it
        // must not hold a borrow of `self.connection`.
        let events: Vec<ClientEvent> = self
            .connection
            .as_ref()
            .map(Connection::drain)
            .unwrap_or_default();
        for event in events {
            match event {
                ClientEvent::Ready { identity, .. } => {
                    self.status = identity.session;
                    self.active_live = true;
                    changed = true;
                }
                ClientEvent::Message(message) => {
                    if let pilus_protocol::ServerMessage::MachineCatalogChanged { .. } = &message {
                        catalog = true;
                    }
                    if matches!(
                        message,
                        pilus_protocol::ServerMessage::Frame(_)
                            | pilus_protocol::ServerMessage::FrameDiff(_)
                    ) {
                        perf::frame();
                    }
                    if self.surface.apply_owned(message) != SurfaceChange::None {
                        changed = true;
                    }
                    // A rejected diff leaves the grid stale; the daemon answers
                    // a resize with a full frame, which is the only recovery
                    // the protocol offers.
                    if self.surface.take_resync() {
                        self.send_active(ClientMessage::Resize {
                            cols: self.cols,
                            rows: self.rows,
                            cell_width_px: self.sent_cell.0,
                            cell_height_px: self.sent_cell.1,
                        });
                    }
                    if let Some(text) = self.surface.take_notify() {
                        self.notify_daemon(text);
                        changed = true;
                    }
                    // The daemon copies through us when it cannot reach the
                    // host clipboard itself (remote sessions, OSC 52): it
                    // ships the text and expects the window to install it.
                    if let Some(text) = self.surface.take_clipboard() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        // A tracked foreground copy expects an acknowledgement
                        // so the daemon can confirm it with a success toast.
                        if let Some(receipt) = self.surface.take_clipboard_receipt() {
                            self.send_active(ClientMessage::ClipboardSucceeded { receipt });
                        }
                    }
                    if let Some(url) = self.surface.take_open_url() {
                        cx.open_url(&url);
                    }
                    for cue in self.surface.take_sounds() {
                        sound::play(cue);
                    }
                }
                ClientEvent::Disconnected { reason } => {
                    self.status = format!("disconnected · {reason}");
                    self.active_live = false;
                    // The remote feed is its own SSH bridge; stop it while the
                    // render bridge is down, and queue a backoff reconnect.
                    // Local reconnects restart the daemon; a machine reconnects
                    // start a fresh SSH bridge.
                    self.agent_feed = None;
                    match self.active_machine.clone() {
                        Some(machine) => {
                            if let Some(target) = machine.ssh_target() {
                                target.release_control_master();
                            }
                            self.schedule_reconnect(machine);
                        }
                        None => self.schedule_local_reconnect(),
                    }
                    changed = true;
                }
            }
        }
        self.catalog_dirty |= catalog;
        changed
    }

    /// Show a daemon push as a toast titled with the session it came from.
    ///
    /// Only daemon pushes come through here; the outcome of something the user
    /// just did surfaces on its own path.
    fn notify_daemon(&mut self, text: String) {
        let title = Some(self.active_session())
            .filter(|session| !session.is_empty())
            .map(|session| self.settings.session_label(&session).to_string());
        self.toasts.push(ToastKind::Info, title, text);
        self.dock.alert();
    }

    /// Tell the user about agents that started or stopped waiting on them.
    ///
    /// Behind another app a toast cannot be seen, so the notification goes to
    /// the OS (with a Dock bounce); in front it would only duplicate what the
    /// sidebar's blocked dot already shows, so it is a toast. A
    /// cleared banner is withdrawn either way so Notification Center never
    /// lists a stale ask.
    fn raise_attention(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        for event in self.attention.drain() {
            match event {
                attention::Event::Cleared(tag) => {
                    notify::forget(&tag);
                    cx.dismiss_system_notification(&tag);
                }
                // The daemon already sounds a finished turn into a live
                // window and the sidebar shows it; only the windowless
                // helper notifies.
                attention::Event::Finished(_) => {}
                attention::Event::Blocked(blocked) => {
                    let body = format!("{} is waiting on you", blocked.agent);
                    let in_front = dock::app_active().unwrap_or(self.window_active);
                    if in_front {
                        self.toasts.push(ToastKind::Info, Some(blocked.title), body);
                        changed = true;
                    } else {
                        notify::remember(&blocked.tag, (blocked.endpoint, blocked.pane));
                        cx.show_system_notification(SystemNotification {
                            tag: SharedString::from(blocked.tag),
                            title: SharedString::from(blocked.title),
                            body: SharedString::from(body),
                            actions: Vec::new(),
                        });
                        self.dock.alert();
                    }
                }
            }
        }
        changed
    }

    /// Raise an in-app toast for something that just happened or failed.
    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(kind, None, text.into());
    }

    /// Re-read the machine catalog without probing, off the GUI thread.
    ///
    /// `list_machines` shells out to the `luvus` CLI, so it must never run on
    /// the GUI thread. The result lands through the same channel the probe
    /// uses, and the next probe tick fills in reachability.
    fn reload_machines(&mut self) {
        let root = self.session_root.clone();
        let session = self.session_name.clone();
        let tx = self.machine_list_tx.clone();
        std::thread::spawn(move || {
            let machines = list_machines(&root, session.as_deref());
            tx.send((session, machines)).ok();
        });
    }

    /// Apply a machine catalog and reconcile endpoints, retries, and bridges.
    ///
    /// Returns whether anything changed. Shared by the reachability probe and
    /// the cheap off-thread list so both keep the sidebar consistent.
    fn apply_machine_catalog(&mut self, machines: Vec<MachineInfo>) -> bool {
        if machines == self.machines {
            return false;
        }
        self.machines = machines;
        // A machine removed from the catalog keeps its connection only while it
        // is the one on screen; every other bridge closes now.
        let known: Vec<String> = self.machines.iter().map(|m| m.id.clone()).collect();
        let active = self.active_machine.as_ref().map(|m| m.id.clone());
        self.endpoints.retain(|id, _| match id {
            EndpointId::Local => true,
            EndpointId::Machine(machine) => {
                known.iter().any(|known| known == machine)
                    || active.as_deref() == Some(machine.as_str())
            }
        });
        // Forget attempts for hosts that are gone or disabled, so re-enabling
        // one auto-connects again without retrying the rest.
        let enabled: HashSet<String> = self
            .machines
            .iter()
            .filter(|machine| machine.enabled)
            .map(|machine| machine.id.clone())
            .collect();
        self.auto_connected.retain(|id| enabled.contains(id));
        self.reconnects.retain(|id, _| enabled.contains(id));
        // A host that answers its probe again can retry at once instead of
        // waiting out the backoff its last failure earned.
        for machine in self.machines.iter().filter(|m| m.online == Some(true)) {
            if let Some(entry) = self.reconnects.get_mut(&machine.id)
                && entry.failures > 0
            {
                entry.failures = 0;
                entry.next_at = Instant::now();
            }
        }
        self.auto_connect_machines();
        true
    }

    /// Re-probe machine reachability off-thread, unless one already runs.
    fn probe_machines(&mut self) {
        if self.machine_refreshing {
            return;
        }
        self.machine_refreshing = true;
        self.last_machine_poll = Instant::now();
        let root = self.session_root.clone();
        let session = self.session_name.clone();
        let tx = self.machine_tx.clone();
        std::thread::spawn(move || {
            let machines = refresh_machines(&root, session.as_deref());
            tx.send((session, machines)).ok();
        });
    }
    /// The active endpoint as a picker/control source.
    fn active_dir_source(&self) -> DirSource {
        match &self.active_machine {
            Some(machine) => match machine.ssh_target() {
                Some(target) => DirSource::Remote(target),
                None => DirSource::Local,
            },
            None => DirSource::Local,
        }
    }

    /// Drain suspended endpoints and complete a prepared handoff.
    fn poll_endpoints(&mut self) -> bool {
        let mut changed = false;
        let mut catalog = false;
        let mut prepared: Option<EndpointId> = None;
        let mut dropped: Vec<EndpointId> = Vec::new();
        let pending = self
            .pending_switch
            .as_ref()
            .map(|pending| (pending.target.clone(), pending.ticket));
        let our_revision = self.sidebar_revision;
        let our_layout = self.sidebar_layout.clone();
        let mut newer_sidebar_revision = false;
        let ids: Vec<EndpointId> = self.endpoints.keys().cloned().collect();
        for id in ids {
            let Some(endpoint) = self.endpoints.get_mut(&id) else {
                continue;
            };
            let mut disconnected = None;
            let events: Vec<ClientEvent> = endpoint
                .connection
                .as_ref()
                .map(Connection::drain)
                .unwrap_or_default();
            for event in events {
                endpoint.last_rx = Instant::now();
                endpoint.probe_sent = None;
                match event {
                    ClientEvent::Ready { identity, .. } => {
                        endpoint.status = identity.session;
                        changed = true;
                    }
                    ClientEvent::Message(message) => {
                        // Only a rejected layout needs a re-push; a daemon that adopted
                        // ours under its own revision must not trigger an echo loop.
                        if let pilus_protocol::ServerMessage::ShellSidebars(sidebars) = &message
                            && sidebars.revision > our_revision
                            && !adopts_layout(sidebars, our_layout.as_ref())
                        {
                            newer_sidebar_revision = true;
                        }
                        if matches!(&message, pilus_protocol::ServerMessage::HealthCheck { .. }) {
                            endpoint.answers_probes = true;
                        }
                        if let pilus_protocol::ServerMessage::MachineCatalogChanged { .. } =
                            &message
                        {
                            catalog = true;
                        }
                        if let pilus_protocol::ServerMessage::PreparedFrame { ticket, .. } =
                            &message
                            && pending.as_ref() == Some(&(id.clone(), *ticket))
                        {
                            prepared = Some(id.clone());
                        }
                        if endpoint.surface.apply_owned(message) != SurfaceChange::None {
                            changed = true;
                        }
                    }
                    ClientEvent::Disconnected { reason } => disconnected = Some(reason),
                }
            }
            // A hidden endpoint keeps its agent list fresh through its own
            // feed, and caches the tabs and bar a later switch will swap in,
            // so committing a handoff never lands on a stale or empty strip.
            // The cached chrome draws nowhere while hidden, so only an agent
            // change costs a repaint.
            while let Some(update) = endpoint.feed.try_recv() {
                if !endpoint.agents_loaded || update.agents != endpoint.agents {
                    self.attention.observe(&id.key(), &update.agents);
                    endpoint.agents = update.agents;
                    endpoint.agents_loaded = true;
                    changed = true;
                }
                endpoint.tabs = update.tabs;
                endpoint.bar_widgets = update.bar;
            }
            if disconnected.is_none() {
                if endpoint.probe_sent.is_some_and(|sent| {
                    endpoint.answers_probes && sent.elapsed() >= Duration::from_secs(10)
                }) {
                    disconnected = Some("health check timed out".to_string());
                } else if endpoint.probe_sent.is_none()
                    && endpoint.last_rx.elapsed() >= Duration::from_secs(5)
                {
                    self.health_nonce = self.health_nonce.wrapping_add(1);
                    if let Some(connection) = endpoint.connection.as_ref() {
                        let _ = connection.health_check(self.health_nonce);
                    }
                    endpoint.probe_sent = Some(Instant::now());
                }
            }
            if disconnected.is_some() {
                dropped.push(id.clone());
                changed = true;
            }
        }
        if newer_sidebar_revision && self.sidebar_layout.is_some() {
            self.push_sidebar_layout();
        }
        // A dropped suspended endpoint is torn down and reconnected from
        // scratch; its surface closes with its bridge. Local reconnects
        // restart the daemon, so a stopped server recovers on its own.
        for id in dropped {
            let pending_switch = self
                .pending_switch
                .as_ref()
                .is_some_and(|pending| pending.target == id);
            let label = self.endpoint_label(&id);
            let endpoint = self.endpoints.remove(&id);
            if pending_switch {
                self.pending_switch = None;
                self.toast(ToastKind::Error, format!("lost connection to {label}"));
            }
            match &id {
                EndpointId::Machine(machine_id) => {
                    if let Some(target) = endpoint
                        .as_ref()
                        .and_then(|endpoint| endpoint.machine.as_ref())
                        .and_then(MachineInfo::ssh_target)
                    {
                        target.release_control_master();
                    }
                    if let Some(machine) =
                        endpoint.and_then(|endpoint| endpoint.machine).or_else(|| {
                            self.machines
                                .iter()
                                .find(|machine| &machine.id == machine_id)
                                .cloned()
                        })
                    {
                        self.schedule_reconnect(machine);
                    }
                }
                EndpointId::Local => self.schedule_local_reconnect(),
            }
        }
        self.catalog_dirty |= catalog;
        if prepared.is_some() {
            self.finish_switch();
            changed = true;
        }
        changed
    }

    /// Turn finished background connections into endpoints and honor the
    /// endpoint a click requested while the connection was in flight.
    fn poll_machine_connects(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.machine_connect_rx.try_recv() {
            match result {
                MachineConnect::Connected {
                    machine,
                    connection,
                } => {
                    let id = EndpointId::Machine(machine.id.clone());
                    // A session switch empties `connecting`; a result for a
                    // target no longer being dialed is stale and must not
                    // install a bridge from the previous session.
                    if !self.connecting.remove(&id) {
                        continue;
                    }
                    self.reconnects.remove(&machine.id);
                    // A reconnect of the machine already on screen swaps its
                    // connection in place; the stale surface stays until the
                    // new bridge sends a frame. Otherwise this is a fresh
                    // suspended endpoint.
                    if self
                        .active_machine
                        .as_ref()
                        .is_some_and(|active| active.id == machine.id)
                    {
                        if !self.active_live {
                            self.replace_active_connection(machine, connection);
                            changed = true;
                        }
                        continue;
                    }
                    self.install_suspended_endpoint(id, Some(machine), connection);
                    changed = true;
                }
                MachineConnect::LocalConnected { connection } => {
                    if !self.connecting.remove(&EndpointId::Local) {
                        continue;
                    }
                    self.local_reconnect = None;
                    self.local_luvus_missing = false;
                    if self.active == EndpointId::Local {
                        self.replace_active_local(connection);
                    } else {
                        self.install_suspended_endpoint(EndpointId::Local, None, connection);
                    }
                    changed = true;
                }
                MachineConnect::Failed { id, error } => {
                    if !self.connecting.remove(&EndpointId::Machine(id.clone())) {
                        continue;
                    }
                    // Count the failure and arm the next capped-backoff attempt;
                    // a healthy link or a fresh probe resets the budget.
                    let entry = self.reconnects.entry(id.clone()).or_insert(Reconnect {
                        failures: 0,
                        next_at: Instant::now(),
                    });
                    entry.failures = entry.failures.saturating_add(1);
                    entry.next_at = Instant::now() + reconnect_delay(entry.failures);
                    // A protocol mismatch never heals by retrying; point at the
                    // upgrade flow instead of leaving the user with a bare error.
                    let hint = if error.contains("protocol mismatch") {
                        " — open Settings and choose Update on that machine, then Restart"
                    } else {
                        ""
                    };
                    self.status = if entry.failures >= RECONNECT_ATTEMPTS {
                        format!("{id}: {error}{hint} (still retrying)")
                    } else {
                        format!("{id}: {error}{hint}")
                    };
                    changed = true;
                }
                MachineConnect::LocalFailed {
                    error,
                    luvus_missing,
                } => {
                    if !self.connecting.remove(&EndpointId::Local) {
                        continue;
                    }
                    let entry = self.local_reconnect.get_or_insert(Reconnect {
                        failures: 0,
                        next_at: Instant::now(),
                    });
                    entry.failures = entry.failures.saturating_add(1);
                    entry.next_at = Instant::now() + reconnect_delay(entry.failures);
                    // Only the visible endpoint owns the status line; a
                    // suspended local endpoint retries quietly.
                    if self.active == EndpointId::Local {
                        self.active_live = false;
                        self.status = format!("local server unavailable: {error}");
                    }
                    self.local_luvus_missing = luvus_missing;
                    changed = true;
                }
            }
        }
        if let Some(request) = self.wanted.clone()
            && self.endpoints.contains_key(&request.id)
        {
            self.wanted = None;
            self.begin_switch(request.id, request.focus, request.pane);
            changed = true;
        }
        changed
    }

    /// Send the pending handoff's prepare request once its endpoint is known.
    fn drive_pending_switch(&mut self) {
        let Some(pending) = self.pending_switch.as_ref() else {
            return;
        };
        if pending.prepared {
            return;
        }
        let target = pending.target.clone();
        let ticket = pending.ticket;
        let (cols, rows) = (self.cols, self.rows);
        if let Some(endpoint) = self.endpoints.get(&target)
            && let Some(connection) = endpoint.connection.as_ref()
        {
            if let Some(workspace_id) = pending.focus.as_ref() {
                let _ = connection.send(ClientMessage::ShellWorkspaceFocus {
                    workspace_id: workspace_id.clone(),
                });
            }
            let _ = connection.set_interest(SurfaceInterest::Prepared);
            let _ = connection.prepare_surface(ticket, cols, rows);
            if let Some(pending) = self.pending_switch.as_mut() {
                pending.prepared = true;
            }
        }
    }

    /// Start the two-phase handoff to an already-connected endpoint.
    fn begin_switch(&mut self, target: EndpointId, focus: Option<String>, pane: Option<String>) {
        if target == self.active {
            self.cancel_pending_switch();
            if let Some(workspace) = focus {
                self.focus_workspace(&workspace);
            }
            if let Some(pane) = pane {
                self.focus_agent(&pane);
            }
            return;
        }
        // Abandon an earlier handoff so its endpoint stops preparing frames.
        if self
            .pending_switch
            .as_ref()
            .is_some_and(|pending| pending.target != target)
        {
            self.cancel_pending_switch();
        }
        self.switch_ticket = self.switch_ticket.wrapping_add(1);
        let ticket = self.switch_ticket;
        self.pending_switch = Some(PendingSwitch {
            target: target.clone(),
            focus,
            pane,
            ticket,
            prepared: false,
            started: Instant::now(),
        });
        self.status = format!("switching to {}…", self.endpoint_label(&target));
    }

    /// Drop a pending handoff, returning its endpoint to a suspended state.
    fn cancel_pending_switch(&mut self) {
        let Some(pending) = self.pending_switch.take() else {
            return;
        };
        if let Some(endpoint) = self.endpoints.get(&pending.target)
            && let Some(connection) = endpoint.connection.as_ref()
        {
            let _ = connection.set_interest(SurfaceInterest::Suspended);
        }
        // The visible surface keeps the window, so its session name is the
        // honest status again; the "switching…" hint belongs to the handoff
        // that just died. A superseding `begin_switch` or the timeout report
        // overwrites this immediately.
        self.status = self.active_session();
    }

    /// Give up on a handoff whose target never produced a frame.
    fn expire_pending_switch(&mut self) {
        const SWITCH_TIMEOUT: Duration = Duration::from_secs(8);
        let expired = self
            .pending_switch
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= SWITCH_TIMEOUT);
        if !expired {
            return;
        }
        // A stalled handoff still commits when the target holds a frame —
        // even a stale one keeps the window usable and refreshes once
        // streaming resumes. A target with no frame at all is reported and
        // cancelled: committing would blank a usable surface, and a failed or
        // timed-out destination must not replace the current one.
        let can_commit = self.pending_switch.as_ref().is_some_and(|pending| {
            self.endpoints.get(&pending.target).is_some_and(|endpoint| {
                endpoint.surface.frame().is_some() && endpoint.last_rx >= pending.started
            })
        });
        if can_commit {
            self.finish_switch();
            return;
        }
        let label = self
            .pending_switch
            .as_ref()
            .map(|pending| self.endpoint_label(&pending.target))
            .unwrap_or_default();
        self.cancel_pending_switch();
        self.toast(ToastKind::Error, format!("timed out switching to {label}"));
    }

    /// Commit a prepared handoff: the target's candidate frame has arrived, so
    /// the surfaces trade places without ever blanking the window.
    fn finish_switch(&mut self) {
        let Some(pending) = self.pending_switch.take() else {
            return;
        };
        let target = pending.target;
        if target == self.active {
            return;
        }
        // The outgoing endpoint stops streaming but keeps its panes and size.
        self.set_active_interest(SurfaceInterest::Suspended);
        // Give the outgoing endpoint a hidden-endpoint feed so its agents
        // stay in the list, and its cached chrome stays current, while hidden.
        let outgoing_feed = self.spawn_agents_feed(self.active_machine.as_ref());

        let Some(endpoint) = self.endpoints.remove(&target) else {
            return;
        };
        let Endpoint {
            machine,
            connection,
            surface,
            last_rx: _,
            probe_sent: _,
            answers_probes: _,
            status,
            tabs,
            agents,
            agents_loaded,
            bar_widgets,
            automations,
            tasks,
            // The incoming hidden-endpoint feed is superseded by the full
            // feed spawned below; the snapshots it already produced are moved
            // into the active slots, so neither list nor tabs ever flicker.
            feed: _incoming_feed,
        } = endpoint;
        let outgoing_id = std::mem::replace(&mut self.active, target.clone());
        // The incoming machine owns its own sidebar width; drop the old
        // geometry so a stale rectangle cannot size the new panel.
        self.last_sidebar_dock = None;
        let outgoing = Endpoint {
            machine: self.active_machine.take(),
            connection: std::mem::replace(&mut self.connection, connection),
            surface: std::mem::replace(&mut self.surface, surface),
            status: std::mem::replace(&mut self.status, status),
            tabs: std::mem::replace(&mut self.tabs, tabs),
            agents: std::mem::replace(&mut self.agents, agents),
            agents_loaded: std::mem::replace(&mut self.agents_loaded, agents_loaded),
            bar_widgets: std::mem::replace(&mut self.bar_widgets, bar_widgets),
            automations: std::mem::replace(&mut self.automations, automations),
            tasks: std::mem::replace(&mut self.tasks, tasks),
            last_rx: Instant::now(),
            probe_sent: None,
            answers_probes: false,
            feed: outgoing_feed,
        };
        self.endpoints.insert(outgoing_id, outgoing);
        self.active_machine = machine;

        self.set_active_interest(SurfaceInterest::Active);
        self.send_active(ClientMessage::Resize {
            cols: self.cols,
            rows: self.rows,
            cell_width_px: self.sent_cell.0,
            cell_height_px: self.sent_cell.1,
        });
        self.agent_feed = Some(self.spawn_feed_for_active());
        self.selection = None;
        self.hover_cell = None;
        self.press_anchor = None;
        self.dragging = false;
        self.tab_sel = None;
        self.tabs_pick = None;
        self.endpoint_sel = None;
        self.close_picker();
        // The swapped-in tabs carry their own active kind; follow it so the
        // nav rows highlight the surface that just took the window.
        self.active_live = true;
        self.status = self.active_session();
        let adopted = self
            .surface
            .shell_sidebars()
            .is_some_and(|sidebars| adopts_layout(sidebars, self.sidebar_layout.as_ref()));
        // Re-pushing an adopted layout forces a needless relayout.
        if !adopted {
            self.push_sidebar_layout();
        }
        if let Some(workspace) = pending.focus {
            self.focus_workspace(&workspace);
        }
        if let Some(pane) = pending.pane {
            self.focus_agent(&pane);
        }
        self.remember_location();
    }

    /// Select an endpoint, connecting it first when necessary.
    ///
    /// `focus` and `pane` name a workspace and pane to focus once the endpoint
    /// owns the surface; both are deferred through the handoff.
    fn activate_endpoint(
        &mut self,
        id: EndpointId,
        focus: Option<String>,
        pane: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // The row picks optimistically: the highlight reads the click now,
        // while the handoff itself may still be crossing SSH.
        self.endpoint_sel = Some(id.clone());
        if id == self.active {
            // Re-selecting the visible endpoint abandons any pending handoff.
            self.cancel_pending_switch();
            // A dropped active machine reconnects on demand; a healthy one
            // just focuses the requested workspace or pane.
            if !self.active_live {
                match self.active_machine.clone() {
                    Some(machine) => {
                        self.reconnects.remove(&machine.id);
                        self.spawn_machine_connect(machine);
                    }
                    None => {
                        self.local_reconnect = None;
                        self.spawn_local_connect();
                    }
                }
                cx.notify();
                return;
            }
            if let Some(workspace) = focus {
                self.focus_workspace(&workspace);
            }
            if let Some(pane) = pane {
                self.focus_agent(&pane);
            }
            cx.notify();
            return;
        }
        // A stashed endpoint without a bridge is a dead link, not a switch
        // target: fall through and reconnect it instead of stalling on a
        // handoff that can never produce a prepared frame.
        if self
            .endpoints
            .get(&id)
            .is_some_and(|endpoint| endpoint.connection.is_some())
        {
            self.begin_switch(id, focus, pane);
            cx.notify();
            return;
        }
        if id == EndpointId::Local {
            // The local daemon is raised on demand, so a missing local
            // endpoint is a request to reconnect rather than an unknown host.
            self.wanted = Some(EndpointRequest { id, focus, pane });
            self.local_reconnect = None;
            self.status = "connecting to local…".to_string();
            self.spawn_local_connect();
            cx.notify();
            return;
        }
        let Some(machine_id) = id.machine_id().map(str::to_string) else {
            return;
        };
        self.wanted = Some(EndpointRequest {
            id: id.clone(),
            focus,
            pane,
        });
        let Some(machine) = self
            .machines
            .iter()
            .find(|machine| machine.id == machine_id)
            .cloned()
        else {
            self.toast(ToastKind::Error, format!("unknown machine `{machine_id}`"));
            return;
        };
        self.status = format!("connecting to {}…", machine.display());
        // A manual click always retries and resets the backoff budget, even
        // after automatic attempts paused.
        self.reconnects.remove(&machine.id);
        self.auto_connected.insert(machine.id.clone());
        self.spawn_machine_connect(machine);
        cx.notify();
    }

    /// Start a background SSH bridge for `machine`.
    ///
    /// Each bridge runs on its own thread, so connecting the whole catalog is
    /// parallel rather than one handshake at a time.
    fn spawn_machine_connect(&mut self, machine: MachineInfo) {
        let id = EndpointId::Machine(machine.id.clone());
        if !self.connecting.insert(id) {
            return;
        }
        let (cols, rows) = (self.cols, self.rows);
        let tx = self.machine_connect_tx.clone();
        std::thread::spawn(move || {
            let result = match connect_machine(&machine, cols, rows) {
                Ok(connection) => MachineConnect::Connected {
                    machine,
                    connection,
                },
                Err(error) => MachineConnect::Failed {
                    id: machine.id.clone(),
                    error: error.to_string(),
                },
            };
            tx.send(result).ok();
        });
    }

    /// Start a background attach for the owner-local daemon.
    ///
    /// Unlike a machine bridge this may start the daemon (`ensure_server`), so
    /// it runs off the GUI thread like every other handshake and reports back
    /// through the same channel.
    fn spawn_local_connect(&mut self) {
        if !self.connecting.insert(EndpointId::Local) {
            return;
        }
        let target = self.target.clone();
        let (cols, rows) = (self.cols, self.rows);
        let tx = self.machine_connect_tx.clone();
        std::thread::spawn(move || {
            let result = match pilus_client::connect(&target, cols, rows) {
                Ok(connection) => MachineConnect::LocalConnected { connection },
                Err(error) => MachineConnect::LocalFailed {
                    luvus_missing: matches!(error, pilus_client::Error::NotFound { .. })
                        && luvus_binary().is_none(),
                    error: error.to_string(),
                },
            };
            tx.send(result).ok();
        });
    }

    /// Whether `machine_id` already has a bridge, is connecting, or is active.
    fn machine_connected(&self, machine_id: &str) -> bool {
        self.active_machine
            .as_ref()
            .is_some_and(|active| active.id == machine_id)
            || self
                .endpoints
                .contains_key(&EndpointId::Machine(machine_id.to_string()))
            || self
                .connecting
                .contains(&EndpointId::Machine(machine_id.to_string()))
    }

    /// Bridge every enabled machine that has not been attempted yet.
    ///
    /// Connecting all saved hosts up front means their workspace rows are
    /// populated and a click switches immediately instead of waiting on a
    /// handshake. Attempts are parallel and recorded, so a slow catalog poll
    /// neither reconnects a healthy host nor retries one that already failed.
    fn auto_connect_machines(&mut self) {
        let machines: Vec<MachineInfo> = self
            .machines
            .iter()
            .filter(|machine| machine.id != "local" && machine.enabled)
            .cloned()
            .collect();
        for machine in machines {
            if self.machine_connected(&machine.id)
                || !self.auto_connected.insert(machine.id.clone())
            {
                continue;
            }
            self.spawn_machine_connect(machine);
        }
    }

    /// Arm capped-backoff reconnect after a machine bridge drops.
    fn schedule_reconnect(&mut self, machine: MachineInfo) {
        let entry = self
            .reconnects
            .entry(machine.id.clone())
            .or_insert(Reconnect {
                failures: 0,
                next_at: Instant::now(),
            });
        entry.next_at = Instant::now() + reconnect_delay(entry.failures);
    }

    /// Arm capped-backoff reconnect after the owner-local daemon drops.
    fn schedule_local_reconnect(&mut self) {
        let entry = self.local_reconnect.get_or_insert(Reconnect {
            failures: 0,
            next_at: Instant::now(),
        });
        entry.next_at = Instant::now() + reconnect_delay(entry.failures);
    }

    /// Start the automatic attempts whose backoff has elapsed, local first.
    ///
    /// One attempt per due endpoint and none while one is in flight, so a slow
    /// handshake is never restarted and an unreachable host is not hammered.
    /// Retries continue at the capped backoff until a link sticks; a host that
    /// answers its probe again resets the budget so recovery is prompt.
    fn pump_reconnects(&mut self) {
        let now = Instant::now();
        let local_due = self
            .local_reconnect
            .as_ref()
            .is_some_and(|entry| now >= entry.next_at)
            && !self.connecting.contains(&EndpointId::Local);
        if local_due {
            if let Some(entry) = self.local_reconnect.as_mut() {
                entry.next_at = now + reconnect_delay(entry.failures);
            }
            self.spawn_local_connect();
        }
        if self.reconnects.is_empty() {
            return;
        }
        let in_flight: HashSet<String> = self
            .connecting
            .iter()
            .filter_map(|id| id.machine_id().map(str::to_string))
            .collect();
        let due: Vec<String> = self
            .reconnects
            .iter()
            .filter(|(id, entry)| now >= entry.next_at && !in_flight.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let Some(machine) = self
                .machines
                .iter()
                .find(|machine| machine.id == id && machine.enabled)
                .cloned()
            else {
                self.reconnects.remove(&id);
                continue;
            };
            // Move the deadline forward now so a slow handshake is not
            // restarted on the next tick; the result handler rebases it.
            if let Some(entry) = self.reconnects.get_mut(&id) {
                entry.next_at = now + reconnect_delay(entry.failures);
            }
            self.spawn_machine_connect(machine);
        }
    }

    /// Swap a reconnected bridge into the active slots without a handoff.
    fn replace_active_connection(&mut self, machine: MachineInfo, connection: Connection) {
        let machine_id = machine.id.clone();
        let _ = connection.request_workspace_dock();
        self.connection = Some(connection);
        self.active_live = true;
        self.active_machine = Some(machine);
        self.endpoints
            .remove(&EndpointId::Machine(machine_id.clone()));
        self.set_active_interest(SurfaceInterest::Active);
        self.send_active(ClientMessage::Resize {
            cols: self.cols,
            rows: self.rows,
            cell_width_px: self.sent_cell.0,
            cell_height_px: self.sent_cell.1,
        });
        self.push_sidebar_layout();
        self.agent_feed = Some(self.spawn_feed_for_active());
        self.toast(ToastKind::Success, "reconnected");
    }

    /// Install a freshly connected endpoint that is not taking the window.
    ///
    /// The endpoint is suspended: it owns its panes and PTY size but streams
    /// no frames until a handoff makes it active.
    fn install_suspended_endpoint(
        &mut self,
        id: EndpointId,
        machine: Option<MachineInfo>,
        connection: Connection,
    ) {
        let _ = connection.request_workspace_dock();
        let _ = connection.set_interest(SurfaceInterest::Suspended);
        let _ = connection.send(ClientMessage::Resize {
            cols: self.cols,
            rows: self.rows,
            cell_width_px: self.sent_cell.0,
            cell_height_px: self.sent_cell.1,
        });
        if let Some(message) = self.sidebar_message() {
            let _ = connection.send(message);
        }
        let feed = self.spawn_agents_feed(machine.as_ref());
        self.endpoints.insert(
            id,
            Endpoint {
                machine,
                connection: Some(connection),
                surface: Surface::new(),
                last_rx: Instant::now(),
                probe_sent: None,
                answers_probes: false,
                status: "connecting…".to_string(),
                tabs: Vec::new(),
                agents: Vec::new(),
                agents_loaded: false,
                bar_widgets: Vec::new(),
                automations: AutomationSummary::default(),
                tasks: TaskSummary::default(),
                feed,
            },
        );
    }

    /// Swap a reconnected local bridge into the active slots without a handoff.
    fn replace_active_local(&mut self, connection: Connection) {
        let _ = connection.request_workspace_dock();
        self.connection = Some(connection);
        self.active = EndpointId::Local;
        self.active_machine = None;
        self.active_live = true;
        self.endpoints.remove(&EndpointId::Local);
        self.set_active_interest(SurfaceInterest::Active);
        self.send_active(ClientMessage::Resize {
            cols: self.cols,
            rows: self.rows,
            cell_width_px: self.sent_cell.0,
            cell_height_px: self.sent_cell.1,
        });
        self.push_sidebar_layout();
        self.agent_feed = Some(self.spawn_feed_for_active());
        self.toast(ToastKind::Success, "reconnected");
    }

    /// The workspace catalog of an endpoint, active or stashed.
    fn endpoint_workspaces(&self, id: &EndpointId) -> &[ShellWorkspace] {
        if *id == self.active {
            return self.surface.workspaces();
        }
        self.endpoints
            .get(id)
            .map(|endpoint| endpoint.surface.workspaces())
            .unwrap_or(&[])
    }

    /// Whether an endpoint has reported its workspace catalog yet.
    fn endpoint_workspaces_loaded(&self, id: &EndpointId) -> bool {
        if *id == self.active {
            return self.surface.workspaces_loaded();
        }
        self.endpoints
            .get(id)
            .is_some_and(|endpoint| endpoint.surface.workspaces_loaded())
    }

    /// Whether an endpoint's agent feed has delivered yet.
    fn endpoint_agents_loaded(&self, id: &EndpointId) -> bool {
        if *id == self.active {
            return self.agents_loaded;
        }
        self.endpoints
            .get(id)
            .is_some_and(|endpoint| endpoint.agents_loaded)
    }

    /// The agent list of an endpoint, active or stashed.
    fn endpoint_agents(&self, id: &EndpointId) -> &[Agent] {
        if *id == self.active {
            return &self.agents;
        }
        self.endpoints
            .get(id)
            .map(|endpoint| endpoint.agents.as_slice())
            .unwrap_or(&[])
    }

    /// The endpoints with a live bridge: Local, then every connected machine.
    fn connected_endpoints(&self) -> Vec<EndpointId> {
        let mut ids = vec![EndpointId::Local];
        for machine in self.machines.iter().filter(|machine| machine.id != "local") {
            let id = EndpointId::Machine(machine.id.clone());
            if id == self.active || self.endpoints.contains_key(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Whether any stage of a switch to `endpoint` is still in flight: a
    /// pending handoff, a bridge still connecting, or a requested endpoint
    /// waiting for its bridge. The optimistic row highlight reads this, and
    /// the poll loop drops the pick when every stage has ended.
    fn switch_in_flight(&self, endpoint: &EndpointId) -> bool {
        self.pending_switch
            .as_ref()
            .is_some_and(|pending| &pending.target == endpoint)
            || self
                .wanted
                .as_ref()
                .is_some_and(|wanted| &wanted.id == endpoint)
            || self.connecting.contains(endpoint)
    }

    /// Whether an endpoint's rows draw as selected: an optimistic pick from
    /// a click wins while the switch is in flight, then the committed surface.
    /// Exactly one endpoint's rows light, so a click reads instantly even
    /// while the two-phase handoff is still crossing SSH.
    fn endpoint_selected(&self, endpoint: &EndpointId) -> bool {
        match &self.endpoint_sel {
            Some(selected) if self.switch_in_flight(selected) => selected == endpoint,
            _ => *endpoint == self.active,
        }
    }

    /// A feed for the active endpoint: local socket or, for a machine, its
    /// remote UHP surface over the same SSH bridge family.
    fn spawn_feed_for_active(&self) -> AgentFeed {
        match &self.active_machine {
            Some(machine) => match machine.ssh_target() {
                Some(target) => AgentFeed::spawn_ssh(target),
                None => AgentFeed::spawn(self.api_socket()),
            },
            None => AgentFeed::spawn(self.api_socket()),
        }
    }

    /// A hidden endpoint's feed, so its agents still list and the chrome a
    /// later switch swaps in stays current.
    fn spawn_agents_feed(&self, machine: Option<&MachineInfo>) -> AgentFeed {
        match machine.and_then(MachineInfo::ssh_target) {
            Some(target) => AgentFeed::spawn_ssh_agents(target),
            None => AgentFeed::spawn_agents(self.api_socket()),
        }
    }

    /// With `auto_theme` on, select the color scheme that matches the device
    /// appearance on the daemon the window shows, a remote machine's included.
    ///
    /// Each endpoint is switched once per device appearance, when its registry
    /// has been polled; a scheme chosen by hand afterwards is left alone until
    /// the device flips again. A scheme the daemon does not register is
    /// skipped rather than retried every poll.
    fn follow_device_appearance(&mut self, cx: &mut Context<Self>) {
        if !self.settings.auto_theme {
            // Turning it back on must reapply, even to the same scheme.
            self.auto_theme_applied = None;
            return;
        }
        if self.theme_refreshing || self.appearance_for.as_ref() != Some(&self.active) {
            return;
        }
        let wanted = self
            .settings
            .device_theme(appearances::device_is_dark(cx))
            .to_string();
        let key = (self.active.clone(), wanted.clone());
        if self.auto_theme_applied.as_ref() == Some(&key) {
            return;
        }
        let registered = self.appearance.themes.iter().any(|info| info.id == wanted);
        self.auto_theme_applied = Some(key);
        if !registered || self.appearance.active == wanted {
            return;
        }
        self.theme_refreshing = true;
        self.last_theme_poll = Instant::now();
        let endpoint = self.active.clone();
        let transport = self.control_transport();
        let tx = self.theme_tx.clone();
        std::thread::spawn(move || {
            let mut client = ControlClient::from_transport(transport);
            let result = client
                .select_theme(&wanted)
                .and_then(|()| client.appearance());
            tx.send((endpoint, result)).ok();
        });
    }

    /// Rebuild the chrome palette from the selected appearance. A vendored
    /// built-in resolves exactly; anything else falls back to the colors the
    /// daemon projects in its shell-dock chrome.
    fn apply_appearance(&mut self) {
        let builtin = scheme::builtin(&self.appearance.active).map(Theme::from_palette);
        let projected = self
            .surface
            .shell_dock()
            .map(|dock| Theme::from_chrome(dock.chrome));
        self.theme = builtin.or(projected).unwrap_or_default();
        *self.shared_theme.borrow_mut() = self.theme;
    }

    /// Record the window's frame in settings. The tick loop flushes it once
    /// the frame stops changing.
    fn remember_window_bounds(&mut self, window: &Window, cx: &App) {
        let geometry = window_geometry(window, cx);
        if self.settings.window.as_ref() == Some(&geometry) {
            return;
        }
        self.settings.window = Some(geometry);
        // The Settings page and other windows read the shared copy, so the
        // frame must land there too or `sync_settings` would restore the old
        // one on the next tick.
        *self.shared_settings.borrow_mut() = self.settings.clone();
        self.window_dirty = true;
        self.window_moved_at = Instant::now();
    }

    /// Write a settled frame to the settings file.
    fn persist_window_bounds(&mut self) {
        if !self.window_dirty || self.window_moved_at.elapsed() < WINDOW_SAVE_DEBOUNCE {
            return;
        }
        self.window_dirty = false;
        self.settings.save(&self.pilus_root);
    }

    /// Record which session and endpoint own the window, so the next launch
    /// reopens there. Writes only on a change.
    fn remember_location(&mut self) {
        let endpoint = match &self.active {
            EndpointId::Local => None,
            EndpointId::Machine(id) => Some(id.clone()),
        };
        // Until the restore has run, `Local` is where the window happens to
        // be, not where the user left it; quitting early must not forget.
        let endpoint = if self.restore_endpoint.is_some() {
            self.settings.last_endpoint.clone()
        } else {
            endpoint
        };
        if self.settings.last_session == self.session_name
            && self.settings.last_endpoint == endpoint
        {
            return;
        }
        self.settings.last_session = self.session_name.clone();
        self.settings.last_endpoint = endpoint;
        self.save_settings();
    }

    /// Move to the machine the last run ended on, once the local daemon is
    /// attached and the catalog can name it.
    ///
    /// Anything the user did in the meantime wins: a click on any endpoint
    /// cancels the restore, as does a machine that is gone or disabled.
    fn restore_last_endpoint(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.restore_endpoint.clone() else {
            return false;
        };
        if self.endpoint_sel.is_some()
            || self.wanted.is_some()
            || self.pending_switch.is_some()
            || self.active != EndpointId::Local
        {
            self.restore_endpoint = None;
            return false;
        }
        if self.connection.is_none() {
            return false;
        }
        let Some(machine) = self.machines.iter().find(|machine| machine.id == id) else {
            // The Local row only arrives with the first full probe, so a
            // catalog without it may simply not have listed the machine yet.
            if self.machines.iter().any(|machine| machine.id == "local") {
                self.restore_endpoint = None;
            }
            return false;
        };
        let enabled = machine.enabled;
        self.restore_endpoint = None;
        if !enabled {
            return false;
        }
        self.activate_endpoint(EndpointId::Machine(id), None, None, cx);
        true
    }

    /// Pull shared settings the Appearances window or Settings page changed.
    ///
    /// A change to the update channel or feed URL rebuilds the updater engine
    /// in place; turning auto-check on checks right away, like a launch would.
    /// Switch the whole app between its normal and its energy-saving look when
    /// energy mode changes, whichever surface flipped it. Motion goes through GPUI's reduce-motion flag, which the
    /// tweens, spinners, fades and the cursor blink already honor; corners and
    /// opacity read the settings' `drawn_*` values. Returns whether energy mode
    /// is in effect, for the tick loop's cadence.
    fn sync_energy(&mut self, cx: &mut Context<Self>) -> bool {
        if self.settings.energy_mode != self.menu_energy {
            self.menu_energy = self.settings.energy_mode;
            cx.set_menus(app_menus(self.menu_energy));
        }
        let on = self.settings.energy_saving();
        if on != self.energy_applied {
            self.energy_applied = on;
            cx.set_reduce_motion(on);
            style::sync_corner_radius(&self.settings);
            // A tween in flight would otherwise finish the slide it began.
            self.sidebar_anim = None;
            self.right.settle();
            cx.notify();
        }
        on
    }

    fn sync_settings(&mut self, cx: &mut Context<Self>) -> bool {
        let latest = self.shared_settings.borrow().clone();
        if latest == self.settings {
            return false;
        }
        let update_changed = latest.update_channel != self.settings.update_channel
            || latest.update_feed_url != self.settings.update_feed_url;
        let auto_check_turned_on = latest.auto_check_updates && !self.settings.auto_check_updates;
        self.settings = latest;
        // The cell advance depends on the terminal font, so re-measure and let
        // `sync_metrics` push the new size to the daemon.
        self.cell_width = None;
        if update_changed {
            let request = UpdateRequest::from_settings(
                &self.settings.update_channel,
                self.settings.update_feed_url.clone(),
            );
            self.updater
                .update(cx, |updater, cx| updater.reconfigure(&request, cx));
        }
        if auto_check_turned_on {
            self.updater.update(cx, |updater, cx| updater.check(cx));
        }
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // With no daemon there is nothing to type into or navigate.
        if self.luvus_missing() {
            return;
        }
        // A page the user clicked into owns its keys.
        if self.browser_page_focused(cx) {
            self.browser_forward_key(event, cx);
            return;
        }
        // An open context menu owns up/down/enter/escape until one runs or it
        // closes; an open prompt owns Enter (submit) and Escape (cancel).
        if self.context_menu.is_some() {
            self.context_menu_key(event, cx);
            return;
        }
        if self.menu_prompt.is_some() {
            self.menu_prompt_key(event, cx);
            return;
        }
        // A note being written owns its keys until it is saved or dropped.
        if self.md_composing() {
            self.md_note_key(event, cx);
            return;
        }
        // So does the markdown editor, whose field reads the typing itself.
        if self.md_editing() {
            self.md_edit_key(event, window, cx);
            return;
        }
        match self.input_layer() {
            InputLayer::Picker => self.picker_key(event, cx),
            InputLayer::Palette => self.palette_key(event, window, cx),
            InputLayer::QuickOpen => self.quick_open_key(event, cx),
            InputLayer::Home => self.home_key(event, cx),
            InputLayer::Settings => self.settings_key(event, cx),
            InputLayer::Tasks => self.tasks_key(event, cx),
            InputLayer::Diff => self.diff_key(event, cx),
            InputLayer::Browser => self.browser_key(event, cx),
            InputLayer::Mission => self.mission_key(event, cx),
            InputLayer::Automations => self.automations_key(event, cx),
            InputLayer::Search => self.search_key(event, cx),
            InputLayer::Stats => self.stats_key(event, cx),
            InputLayer::Process => self.process_key(event, cx),
            InputLayer::Database => self.database_key(event, cx),
            InputLayer::Sipil => self.sipil_chat_key(event, cx),
            // The panel's own handler has read it; nothing is left for the window.
            InputLayer::SipilPanel => {}
            InputLayer::Terminal => self.terminal_key(event, cx),
        }
    }

    /// The surface that owns the keyboard this frame, in stacking order.
    ///
    /// Overlays sit above content: the folder picker over everything, the
    /// command palette over the Home composer or the Settings page. Key
    /// routing and focus both read this, so they cannot disagree about who is
    /// on top.
    fn input_layer(&self) -> InputLayer {
        if self.quick_open.is_open() && !self.picker.is_open() {
            return InputLayer::QuickOpen;
        }
        top_layer(
            self.picker.is_open(),
            self.palette.is_open(),
            self.home.is_open(),
            self.settings_open,
            self.diff_focused()
                .then_some(InputLayer::Diff)
                .or(self.browser_focused().then_some(InputLayer::Browser))
                .or(self.sipil_panel_focused().then_some(InputLayer::SipilPanel))
                .or_else(|| self.page_layer().filter(|_| !self.terminal_beside_page()))
                .or_else(|| self.search.is_open().then_some(InputLayer::Search))
                .or_else(|| self.stats.is_open().then_some(InputLayer::Stats))
                .or_else(|| self.process.is_open().then_some(InputLayer::Process))
                .or_else(|| self.database.is_open().then_some(InputLayer::Database))
                .or_else(|| self.sipil_chat.open.then_some(InputLayer::Sipil)),
        )
    }

    /// The open native page's input layer, if one is open.
    fn page_layer(&self) -> Option<InputLayer> {
        if self.tasks_panel.is_open() {
            Some(InputLayer::Tasks)
        } else if self.mission.is_open() {
            Some(InputLayer::Mission)
        } else if self.automations_page.is_open() {
            Some(InputLayer::Automations)
        } else {
            None
        }
    }

    /// The split of the open Mission Control, Tasks, or Automations page,
    /// if any.
    fn page_split(&self) -> Option<PageSplit> {
        self.page_split.filter(|_| self.split_page_open())
    }

    /// Whether a page that can share the card with the terminal is open.
    fn split_page_open(&self) -> bool {
        self.page_layer().is_some()
    }

    /// Whether the terminal beside a split page holds the keyboard.
    fn terminal_beside_page(&self) -> bool {
        self.page_split()
            .is_some_and(|split| split.terminal_focused)
    }

    /// Forward a keystroke to the daemon's terminal.
    fn terminal_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Chords the native menu owns: consume them so they never reach the
        // pane as input. The keymap dispatches the matching action.
        if keystroke.modifiers.platform
            && matches!(
                keystroke.key.to_ascii_lowercase().as_str(),
                "," | "comma"
                    | "."
                    // The OS window switch, Cmd+` and Cmd+Shift+`.
                    | "`"
                    | "-"
                    | "0"
                    | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "="
                    | "+"
                    | "a"
                    | "b"
                    | "c"
                    | "d"
                    | "n"
                    | "p"
                    | "s"
                    | "t"
                    | "v"
                    | "m"
                    | "h"
                    | "w"
                    | "q"
            )
        {
            return;
        }
        // An open session menu owns Escape; the daemon never sees it.
        if self.session_menu_open && keystroke.key == "escape" {
            self.close_session_menu();
            cx.notify();
            return;
        }
        if let Some(key) = keys::to_wire(keystroke, KeyEventKind::Press) {
            self.send_active(ClientMessage::Key(key));
        }
    }

    /// Route a keystroke to the open palette.
    fn palette_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" | "esc" => self.palette.close(),
            "enter" => {
                if let Some(command) = self.highlighted_command() {
                    self.palette.close();
                    self.run_command(command, window, cx);
                }
            }
            "up" => {
                let len = self.palette.matches(&self.palette_entries()).len();
                self.palette.move_selection(-1, len);
                self.palette_scroll
                    .scroll_to_item(self.palette.selected().min(len.saturating_sub(1)));
            }
            "down" | "tab" => {
                let len = self.palette.matches(&self.palette_entries()).len();
                self.palette.move_selection(1, len);
                self.palette_scroll
                    .scroll_to_item(self.palette.selected().min(len.saturating_sub(1)));
            }
            // Everything else belongs to the query field: the entity handles
            // editing and stops propagation, so only these keys reach here.
            _ => {}
        }
        cx.notify();
    }

    /// The live command list, rebuilt from current view state.
    fn palette_entries(&self) -> Vec<Entry> {
        let entry = |command: Command, title: &str, group: &'static str, shortcut| Entry {
            command,
            title: title.to_string(),
            group,
            shortcut,
        };
        let mut entries = vec![
            entry(Command::NewTab, "New Tab", "Session", Some("⌘T")),
            entry(
                Command::OpenLocalTerminal,
                "Open Local Terminal",
                "Session",
                Some("⌘⇧T"),
            ),
            entry(Command::CloseTab, "Close Tab", "Session", Some("⌘W")),
            entry(
                Command::SplitVertical,
                "Split Vertical",
                "Session",
                Some("⌘D"),
            ),
            entry(
                Command::SplitHorizontal,
                "Split Horizontal",
                "Session",
                Some("⌘⇧D"),
            ),
            entry(Command::NewSession, "New Session", "Session", None),
            entry(Command::RenameSession, "Rename Session", "Session", None),
            entry(Command::NewWindow, "New Window", "Application", Some("⌘N")),
            entry(
                Command::OpenAppearances,
                "Open Appearances",
                "Application",
                Some("⌘,"),
            ),
            entry(Command::OpenAbout, "About pilus", "Application", None),
            entry(Command::Quit, "Quit pilus", "Application", Some("⌘Q")),
            entry(Command::ToggleSidebar, "Toggle Sidebar", "View", Some("⌘S")),
            entry(
                Command::ToggleRightSidebar,
                "Toggle Right Sidebar",
                "View",
                Some("⌘B"),
            ),
            entry(
                Command::ToggleEnergyMode,
                if self.settings.energy_mode {
                    "Turn Off Energy Mode"
                } else {
                    "Turn On Energy Mode"
                },
                "View",
                None,
            ),
            entry(
                Command::TogglePaneToolbar,
                if self.settings.pane_toolbar {
                    "Hide Pane Toolbar"
                } else {
                    "Show Pane Toolbar"
                },
                "View",
                None,
            ),
            entry(
                Command::IncreaseFontSize,
                "Increase Font Size",
                "View",
                Some("⌘+"),
            ),
            entry(
                Command::DecreaseFontSize,
                "Decrease Font Size",
                "View",
                Some("⌘−"),
            ),
            entry(
                Command::ResetFontSize,
                "Reset Font Size",
                "View",
                Some("⌘0"),
            ),
            entry(Command::CopySelection, "Copy", "Edit", Some("⌘C")),
            entry(Command::PasteClipboard, "Paste", "Edit", Some("⌘V")),
            entry(Command::SelectAllGrid, "Select All", "Edit", Some("⌘A")),
            entry(Command::CloseWindow, "Close Window", "Window", Some("⌘⇧W")),
            entry(
                Command::CloseAllWindows,
                "Close All Windows",
                "Window",
                Some("⌘⌥W"),
            ),
            entry(
                Command::OpenWorkspacePicker,
                "Open Workspace",
                "Workspace",
                Some("⌘⇧O"),
            ),
            entry(Command::OpenHome, "New Agent", "Application", Some("⌘⇧N")),
            entry(Command::OpenSearch, "Search Sessions", "Workspace", None),
            entry(Command::OpenOverview, "Home", "Workspace", None),
            entry(Command::OpenStats, "Insights", "Workspace", None),
            entry(
                Command::OpenSettings,
                "Settings",
                "Application",
                Some("⌘⇧."),
            ),
            entry(
                Command::OpenUiPlayground,
                "Open UI Playground",
                "Application",
                None,
            ),
            entry(Command::OpenProcess, "Process", "Workspace", None),
            entry(Command::OpenDatabase, "Database", "Workspace", None),
        ];
        for tab in &self.tabs {
            entries.push(entry(
                Command::FocusTab(tab.index),
                &format!("Go to Tab {}", tab.label()),
                "Navigate",
                None,
            ));
        }
        for session in &self.sessions {
            entries.push(entry(
                Command::SwitchSession(session.name.clone()),
                &format!(
                    "Switch to Session {}",
                    self.settings.session_label(&session.name)
                ),
                "Navigate",
                None,
            ));
        }
        for workspace in self.surface.workspaces() {
            entries.push(entry(
                Command::FocusWorkspace(workspace.id.clone()),
                &format!("Open Workspace {}", workspace.name),
                "Workspace",
                None,
            ));
        }
        for agent in &self.agents {
            entries.push(entry(
                Command::FocusAgent(agent.pane.clone()),
                &format!("Focus Agent {}", agent.label()),
                "Navigate",
                None,
            ));
        }
        entries
    }

    /// The command under the palette's highlight.
    fn highlighted_command(&self) -> Option<Command> {
        let entries = self.palette_entries();
        let matches = self.palette.matches(&entries);
        self.palette
            .highlighted(&matches)
            .map(|entry| entry.command.clone())
    }

    /// Execute a command chosen in the palette.
    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::NewWindow => window.dispatch_action(NewWindow.boxed_clone(), cx),
            Command::NewTab => window.dispatch_action(NewTab.boxed_clone(), cx),
            Command::OpenLocalTerminal => self.open_local_terminal(cx),
            Command::SplitVertical => window.dispatch_action(SplitVertical.boxed_clone(), cx),
            Command::SplitHorizontal => window.dispatch_action(SplitHorizontal.boxed_clone(), cx),
            Command::CloseTab => window.dispatch_action(CloseTab.boxed_clone(), cx),
            Command::CloseWindow => window.dispatch_action(CloseWindow.boxed_clone(), cx),
            Command::CloseAllWindows => window.dispatch_action(CloseAllWindows.boxed_clone(), cx),
            Command::ToggleSidebar => window.dispatch_action(ToggleSidebar.boxed_clone(), cx),
            Command::ToggleRightSidebar => {
                window.dispatch_action(ToggleRightSidebar.boxed_clone(), cx)
            }
            Command::TogglePaneToolbar => self.toggle_pane_toolbar(cx),
            Command::ToggleEnergyMode => self.toggle_energy_mode(cx),
            Command::IncreaseFontSize => window.dispatch_action(IncreaseFontSize.boxed_clone(), cx),
            Command::DecreaseFontSize => window.dispatch_action(DecreaseFontSize.boxed_clone(), cx),
            Command::ResetFontSize => window.dispatch_action(ResetFontSize.boxed_clone(), cx),
            Command::OpenAppearances => window.dispatch_action(OpenAppearances.boxed_clone(), cx),
            Command::OpenAbout => window.dispatch_action(OpenAbout.boxed_clone(), cx),
            Command::CopySelection => window.dispatch_action(CopySelection.boxed_clone(), cx),
            Command::PasteClipboard => window.dispatch_action(PasteClipboard.boxed_clone(), cx),
            Command::SelectAllGrid => window.dispatch_action(SelectAllGrid.boxed_clone(), cx),
            Command::Quit => window.dispatch_action(QuitApp.boxed_clone(), cx),
            Command::OpenWorkspacePicker => self.open_picker(cx),
            Command::OpenSettings => self.open_settings(cx),
            Command::OpenHome => self.open_home(cx),
            Command::OpenSearch => self.open_search(cx),
            Command::OpenOverview => self.open_overview(cx),
            Command::OpenStats => self.open_stats(cx),
            Command::OpenProcess => self.open_process(cx),
            Command::OpenDatabase => self.open_database(cx),
            Command::OpenUiPlayground => {
                ui_playground::open(self.theme, self.shared_settings.clone(), cx);
            }
            Command::FocusTab(index) => self.focus_tab(index, cx),
            Command::SwitchSession(name) => self.switch_session(&name),
            Command::NewSession => self.start_new_session(),
            Command::RenameSession => {
                let name = self.current_session_name().to_string();
                self.open_session_rename(&name, cx);
            }
            Command::FocusWorkspace(id) => {
                self.leave_settings_page();
                self.focus_workspace(&id);
            }
            Command::FocusAgent(pane) => {
                self.leave_settings_page();
                self.focus_agent(&pane);
            }
        }
    }

    /// Send the system clipboard to the daemon, which applies the pane's own
    /// bracketed-paste rules.
    ///
    /// A clipboard that holds a picture instead of words (a screenshot, an image
    /// file copied in Finder) is stored on the pane's machine and pastes its
    /// path, the way an agent or viewer in the pane takes an image.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let images = paste_image::clipboard_images(&item);
        if !images.is_empty() {
            self.paste_images(images, cx);
            return;
        }
        if let Some(text) = item.text()
            && !text.is_empty()
        {
            self.send_active(ClientMessage::Paste(text));
        }
    }

    /// Focus an agent's pane, switching workspace and tab as needed.
    ///
    /// The AGENTS row highlights immediately from the optimistic pick; the
    /// feed's next `agent.list` confirms it once the daemon has focused it.
    fn focus_agent(&mut self, pane: &str) {
        self.agent_sel = Some((self.active.clone(), pane.to_string()));
        self.agent_sel_at = Instant::now();
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.focus_pane(pane);
        }
    }

    /// Whether an agent row draws as the focused pane: an optimistic click
    /// wins across a slow link, then the daemon's own focus.
    fn agent_focused(&self, endpoint: &EndpointId, agent: &Agent) -> bool {
        if let Some((selected, pane)) = &self.agent_sel
            && self.agent_sel_at.elapsed() < Duration::from_secs(4)
        {
            return selected == endpoint && *pane == agent.pane;
        }
        self.endpoint_selected(endpoint) && agent.focused
    }

    /// Whether an agent row in the sidebar is drawn as the selected one.
    fn agent_pill_shown(&self) -> bool {
        if self.agents_collapsed {
            return false;
        }
        let scoped_workspace = if self.agents_this_workspace {
            self.endpoint_workspaces(&self.active)
                .iter()
                .find(|workspace| workspace.active)
                .map(|workspace| workspace.id.clone())
        } else {
            None
        };
        self.connected_endpoints().iter().any(|id| {
            self.endpoint_agents(id).iter().any(|agent| {
                scoped_workspace
                    .as_ref()
                    .is_none_or(|workspace| agent.workspace_id == *workspace)
                    && self.agent_focused(id, agent)
            })
        })
    }

    /// Focus a workspace by id, switching the daemon's active workspace.
    fn focus_workspace(&mut self, workspace_id: &str) {
        self.show_cached_tabs(workspace_id);
        self.send_active(ClientMessage::ShellWorkspaceFocus {
            workspace_id: workspace_id.to_string(),
        });
        // The focus rides the surface connection, which the feed never sees:
        // ask it to read the new workspace's tabs back now, not on its tick.
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.refresh_soon();
        }
    }

    /// Take the daemon's tab list, remembering it for its workspace.
    ///
    /// While a cached strip stands in for a workspace click, a list that is
    /// still the old workspace's is the daemon catching up, not news: showing
    /// it would flick the strip back before it moves forward again.
    fn adopt_tabs(&mut self, tabs: Vec<Tab>) {
        if self
            .tabs_pick
            .as_ref()
            .is_some_and(|pick| pick.is_stale(&tabs))
        {
            return;
        }
        self.tabs_pick = None;
        if let Some(workspace) = self.active_workspace_id() {
            self.tabs_cache
                .insert((self.active.clone(), workspace), tabs.clone());
        }
        self.tabs = tabs;
    }

    /// Swap in the strip last seen for `workspace_id`, if any, so the click
    /// reads at once instead of a feed round trip later (several over SSH).
    fn show_cached_tabs(&mut self, workspace_id: &str) {
        let Some(cached) = self
            .tabs_cache
            .get(&(self.active.clone(), workspace_id.to_string()))
            .filter(|cached| !cached.is_empty())
            .cloned()
        else {
            return;
        };
        let from: Vec<String> = self.tabs.iter().map(|tab| tab.id.clone()).collect();
        if cached.iter().map(|tab| &tab.id).eq(from.iter()) {
            return;
        }
        // A pick that has not landed yet keeps its original baseline: the
        // daemon is still showing that list, not the one we swapped in.
        let from = match self.tabs_pick.take() {
            Some(pick) if pick.at.elapsed() < TABS_PICK_WINDOW => pick.from,
            _ => from,
        };
        self.tabs_pick = Some(TabsPick {
            from,
            at: Instant::now(),
        });
        self.tab_sel = None;
        self.tabs = cached;
    }

    /// Leave the full-card pages (Home and Settings): sidebar actions address
    /// the terminal surface they cover, so acting on one dismisses the other.
    fn leave_settings_page(&mut self) {
        self.settings_open = false;
        self.tasks_panel.close();
        self.mission.close();
        self.automations_page.close();
        self.github_page.close();
        self.search.close();
        self.overview.close();
        self.stats.close();
        self.process.close();
        self.database.close();
        self.close_sipil_chat();
        self.home.close();
        self.update_url_editing = false;
        self.relay_editing = None;
    }

    /// A control client for an endpoint's control surface.
    fn control_client_for(&self, source: &DirSource) -> Option<ControlClient> {
        match source {
            DirSource::Local => ControlClient::connect(&self.api_socket()).ok(),
            DirSource::Remote(target) => ControlClient::connect_ssh(target.clone()).ok(),
        }
    }

    /// Open the native Open Workspace picker on the active endpoint.
    fn open_picker(&mut self, cx: &mut Context<Self>) {
        let source = self.active_dir_source();
        let start = self.picker_start_dir();
        self.picker.open(source, start);
        self.list_picker(cx);
        cx.notify();
    }

    /// Close the folder picker.
    fn close_picker(&mut self) {
        self.picker.close();
    }

    /// Where the picker starts: the active workspace's folder, else the home.
    fn picker_start_dir(&self) -> String {
        if let Some(active) = self.surface.workspaces().iter().find(|w| w.active)
            && !active.cwd.is_empty()
        {
            return active.cwd.clone();
        }
        match &self.active_machine {
            Some(_) => "~".to_string(),
            None => std::env::var("HOME").unwrap_or_else(|_| "/".to_string()),
        }
    }

    /// Start a background listing of the picker's current directory.
    fn list_picker(&mut self, _cx: &mut Context<Self>) {
        picker::spawn_list(
            self.picker_tx.clone(),
            self.picker.source().clone(),
            self.picker.cwd().to_string(),
        );
    }

    /// Open `path` as a workspace on the picker's endpoint, then close it.
    fn open_workspace_path(&mut self, path: String, cx: &mut Context<Self>) {
        let source = self.picker.source().clone();
        let Some(mut client) = self.control_client_for(&source) else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            return;
        };
        self.close_picker();
        if let Some(endpoint) = self.endpoint_for_source(&source) {
            self.remember_folder(&endpoint.key(), &path);
        }
        // Opening from the Home composer's picker should leave the composer
        // targeting the workspace it just opened, once the catalog catches up.
        if self.home.is_open() {
            self.home_pending_open = self.endpoint_for_source(&source).map(|endpoint| {
                let path = self.expand_cwd(&endpoint.key(), &path);
                (endpoint, path)
            });
        }
        self.status = format!("opening {path}…");
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = client
                .open_workspace(&path)
                .map_err(|error| error.to_string());
            tx.send(PickerEvent::Opened(result)).ok();
        });
        cx.notify();
    }

    /// Create a worktree of the endpoint's active repo with a generated branch.
    fn open_new_worktree(&mut self, cx: &mut Context<Self>) {
        let source = self.picker.source().clone();
        let Some(mut client) = self.control_client_for(&source) else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            return;
        };
        let branch = generate_session_name(&[]);
        self.close_picker();
        self.status = format!("creating worktree {branch}…");
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = client
                .create_worktree(&branch)
                .map_err(|error| error.to_string());
            tx.send(PickerEvent::WorktreeOpened(result)).ok();
        });
        cx.notify();
    }

    /// Route a keystroke to the open picker.
    fn picker_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        // The host dropdown is its own modal layer.
        if self.picker.menu_open() {
            match key {
                "escape" => self.picker.close_menu(),
                "up" => self.picker.menu_move(-1),
                "down" => self.picker.menu_move(1),
                "enter" => {
                    if let Some(source) = self.picker.take_menu_source() {
                        self.picker.rebase(source, "~".to_string());
                        self.list_picker(cx);
                        self.picker_scroll.scroll_to_item(0);
                    }
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        if key == "escape" {
            if self.picker.editing() {
                self.picker.end_edit();
            } else {
                self.close_picker();
            }
            cx.notify();
            return;
        }
        if self.picker.editing() {
            // The path entity handles editing; a bare Enter is all that
            // reaches here.
            if key == "enter" && self.picker.jump().is_some() {
                self.list_picker(cx);
                self.picker_scroll.scroll_to_item(0);
            }
            cx.notify();
            return;
        }
        // Shift+`.` toggles hidden entries; GPUI may report `.`, `>`, or a
        // `key_char` of `>` depending on layout.
        // Command chords belong to the menu: Cmd+Shift+. opens Settings.
        if !event.keystroke.modifiers.platform
            && (key == ">"
                || event.keystroke.key_char.as_deref() == Some(">")
                || (event.keystroke.modifiers.shift && key == "."))
        {
            self.picker.toggle_hidden();
            cx.notify();
            return;
        }
        match key {
            "up" => self.picker.move_selection(-1),
            "down" => self.picker.move_selection(1),
            "left" => {
                if self.picker.go_up().is_some() {
                    self.list_picker(cx);
                }
            }
            "right" => {
                if self.picker.go_back().is_some() {
                    self.list_picker(cx);
                }
            }
            "enter" => {
                if self.picker.enter().is_some() {
                    self.list_picker(cx);
                }
            }
            "backspace" => {
                if self.picker.go_up().is_some() {
                    self.list_picker(cx);
                }
            }
            "tab" => self.picker.begin_edit(),
            _ => {}
        }
        self.picker_scroll.scroll_to_item(self.picker.selected());
        cx.notify();
    }

    /// The endpoint choices shown in the picker's host dropdown.
    fn picker_menu_choices(&self) -> Vec<MenuChoice> {
        let current = self.picker.source().clone();
        let mut choices = vec![MenuChoice {
            label: "Local".to_string(),
            source: DirSource::Local,
            current: current == DirSource::Local,
            online: Some(true),
        }];
        for machine in self
            .machines
            .iter()
            .filter(|machine| machine.id != "local" && machine.enabled)
        {
            if let Some(target) = machine.ssh_target() {
                let source = DirSource::Remote(target);
                choices.push(MenuChoice {
                    label: machine.display().to_string(),
                    current: source == current,
                    online: machine.online,
                    source,
                });
            }
        }
        choices
    }

    /// The card's content: the Settings page while it is open, else the
    /// terminal grid. Sharing the card keeps the page sized exactly like
    /// the terminal, with the sidebar still showing beside it.
    ///
    /// Each page fades in under its own id, so switching pages replays it.
    /// The terminal itself takes no fade, which would show the desktop
    /// through a translucent grid.
    fn card_content(
        &mut self,
        cx: &mut Context<Self>,
        grid: Div,
        layout: &CardLayout,
        terminal_fill: gpui::Rgba,
    ) -> AnyElement {
        if self.luvus_missing() {
            return style::enter("pilus-page-install-luvus", self.install_luvus_prompt(cx))
                .into_any_element();
        }
        if self.overview.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.overview_page(cx));
            // Flush, so the hero photograph runs to the window's edges.
            return self.flush_page_card(
                "pilus-page-overview",
                page,
                grid,
                layout,
                terminal_fill,
                cx,
            );
        }
        if self.home.is_open() {
            return style::enter(
                "pilus-page-home",
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.home_page(cx)),
            )
            .into_any_element();
        }
        if self.tasks_panel.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.tasks_page(cx));
            // Each layout fades in under its own id, so moving between the
            // list and the board replays the fade like any page switch.
            let id = match self.tasks_panel.layout() {
                TaskLayout::List => "pilus-page-tasks",
                TaskLayout::Board => "pilus-page-orchestration",
            };
            return self.page_card(id, page, grid, layout, terminal_fill, cx);
        }
        if self.mission.is_open() {
            // The page fills the card, or the leading half of a right split.
            let page_w = match self.page_split().map(|split| split.axis) {
                Some(SplitAxis::Right) => layout.page_w,
                _ => layout.content_w,
            };
            let page_w = if self.settings.pane_ui {
                page_w - 2.0 * pane_chrome::GAP_PX
            } else {
                page_w
            };
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.mission_page(page_w, cx));
            return self.page_card("pilus-page-mission", page, grid, layout, terminal_fill, cx);
        }
        if self.automations_page.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.automations_page(cx));
            return self.page_card(
                "pilus-page-automations",
                page,
                grid,
                layout,
                terminal_fill,
                cx,
            );
        }
        if self.sipil_chat.open {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.sipil_chat_page(cx));
            // The chat runs flush to the sidebar and the window edge: a long log
            // wants the width an inset card would take.
            return self.flush_page_card("pilus-page-sipil", page, grid, layout, terminal_fill, cx);
        }
        if self.github_page.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.github_page(cx));
            return self.page_card("pilus-page-github", page, grid, layout, terminal_fill, cx);
        }
        if self.search.is_open() {
            let page = div().flex_1().min_h(px(0.0)).overflow_hidden();
            let page = if self
                .overview
                .session
                .is_open_from(session_view::Origin::Search)
            {
                page.child(self.session_page(cx))
            } else {
                page.child(self.search_page(cx))
            };
            return self.page_card("pilus-page-search", page, grid, layout, terminal_fill, cx);
        }
        if self.stats.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.stats_page(cx));
            return self.page_card("pilus-page-stats", page, grid, layout, terminal_fill, cx);
        }
        if self.process.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.process_page(cx));
            return self.page_card("pilus-page-process", page, grid, layout, terminal_fill, cx);
        }
        if self.database.is_open() {
            let page = div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(self.database_page(cx));
            return self.page_card("pilus-page-database", page, grid, layout, terminal_fill, cx);
        }
        if self.settings_open {
            return style::enter(
                "pilus-page-settings",
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.settings_page(cx)),
            )
            .into_any_element();
        }
        div()
            .flex_1()
            .overflow_hidden()
            .child(grid)
            .into_any_element()
    }

    /// A native page as a rounded card in gutters of the chrome's own fill, the
    /// way each pane of a split terminal sits in its gutters: the same gap on
    /// every side, a hairline border, no shadow. With pane frames off the
    /// terminal has no cards, so the page runs flush on the chrome's fill
    /// instead.
    ///
    /// The gutter is the sidebar's fill, not the tinted pane surface: a page
    /// has no terminal cards for that surface to frame, and the tint would
    /// read as a different shade from the sidebar beside it. It is the page's
    /// own layer rather than the card's, so a split page carries it on its
    /// half and the terminal half keeps the grid's.
    fn inset_page(&self, page: Div) -> Div {
        let glass = self.glass_opacity();
        if !self.settings.pane_ui {
            return div().size_full().bg(self.chrome_fill()).child(page);
        }
        div()
            .size_full()
            .p(px(pane_chrome::GAP_PX))
            .bg(self.chrome_fill())
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .radius_md()
                    .border_1()
                    .border_color(self.theme.border)
                    .bg(self.theme.bg.opacity(glass))
                    .child(page),
            )
    }

    /// A page with no card around it: the page's own layer on the card's fill,
    /// to the edges of the area it has.
    fn flush_page(&self, page: Div) -> Div {
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(self.theme.bg.opacity(self.glass_opacity()))
            .child(page)
    }

    /// A native page in the card: alone, or leading the terminal when split.
    ///
    /// The page keeps its fade under its own id; the terminal half takes
    /// none (see [`Self::card_content`]). A press on either half hands it
    /// the keyboard, and the hairline between them is the chrome's.
    fn page_card(
        &mut self,
        id: &'static str,
        page: Div,
        grid: Div,
        layout: &CardLayout,
        terminal_fill: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.framed_page_card(id, page, grid, layout, terminal_fill, true, cx)
    }

    /// [`Self::page_card`] without the card: the page runs flush to the sidebar
    /// and the window's edges on the card's fill, with no gutter or rounded
    /// border, for a page whose content wants all the room there is.
    fn flush_page_card(
        &mut self,
        id: &'static str,
        page: Div,
        grid: Div,
        layout: &CardLayout,
        terminal_fill: gpui::Rgba,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.framed_page_card(id, page, grid, layout, terminal_fill, false, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn framed_page_card(
        &mut self,
        id: &'static str,
        page: Div,
        grid: Div,
        layout: &CardLayout,
        terminal_fill: gpui::Rgba,
        inset: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = if inset {
            self.inset_page(page)
        } else {
            self.flush_page(page)
        };
        let Some(split) = self.page_split() else {
            return style::enter(id, page).into_any_element();
        };
        let page_extent = match split.axis {
            SplitAxis::Right => layout.page_w,
            SplitAxis::Down => layout.page_h,
        };
        let mut lead = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .overflow_hidden()
            // The card is clear beside a page, so each half carries its own
            // coat: the page's inset card sits on the pane surface, the
            // terminal on the grid's.
            .border_color(self.theme.border)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if let Some(split) = this.page_split.as_mut()
                        && split.terminal_focused
                    {
                        split.terminal_focused = false;
                        cx.notify();
                    }
                }),
            );
        lead = match split.axis {
            SplitAxis::Right => lead.w(px(page_extent)).h_full().border_r_1(),
            SplitAxis::Down => lead.h(px(page_extent)).w_full().border_b_1(),
        };
        let terminal = div()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .bg(terminal_fill)
            .child(grid);
        let row = div()
            .flex()
            .size_full()
            .child(style::enter(id, lead.child(page)));
        match split.axis {
            SplitAxis::Right => row.flex_row(),
            SplitAxis::Down => row.flex_col(),
        }
        .child(terminal)
        .into_any_element()
    }

    /// The label for the picker's current endpoint.
    fn picker_source_label(&self) -> String {
        match self.picker.source() {
            DirSource::Local => "Local".to_string(),
            DirSource::Remote(target) => self
                .machines
                .iter()
                .find(|machine| machine.ssh_target().as_ref() == Some(target))
                .map(|machine| machine.display().to_string())
                .unwrap_or_else(|| target.destination.clone()),
        }
    }

    /// Show the Settings page.
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.close_home_page();
        self.settings_open = true;
        self.settings_status.clear();
        self.probe_machines();
        let version_tx = self.luvus_version_tx.clone();
        std::thread::spawn(move || {
            version_tx
                .send(crate::luvus_update::installed_version().map(|version| version.to_string()))
                .ok();
        });
        cx.notify();
    }

    /// Show the Home composer.
    ///
    /// The composer picks the machine, model, and effort for a launch, so it
    /// probes the machine catalog and resolves the local agent list first.
    fn open_home(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.home_begin();
        cx.notify();
    }

    /// Bring the composer's state up to date and live. The Home page embeds the
    /// same composer, so both entrances start it here.
    fn home_begin(&mut self) {
        self.picker.close();
        self.home.open();
        self.home_sync_key.clear();
        self.probe_machines();
        self.ensure_home_agent_configs();
        self.sync_home();
        self.home_focus_active_workspace();
    }

    /// Leave the composer, and the Home page when the composer is embedded in it.
    fn close_home_page(&mut self) {
        self.home.close();
        self.overview.close();
    }

    /// Point the composer's Workspace picker at the active workspace, so a
    /// bare Enter launches into what the user is already looking at.
    fn home_focus_active_workspace(&mut self) {
        let active = self
            .home_workspaces()
            .iter()
            .position(|(id, workspace)| *id == self.active && workspace.active);
        if let Some(index) = active {
            self.home.choice_mut(HomeField::Workspace).choose(index);
        }
    }

    /// The live agent aliases and personal session directories, so a generated
    /// name avoids a collision.
    fn session_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .filter(|name| !name.is_empty())
            .collect();
        names.extend(
            std::fs::read_dir(self.pilus_root.join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok()),
        );
        names
    }

    /// Move Home focus by one field.
    fn home_move_focus(&mut self, backward: bool) {
        if backward {
            self.home.focus_prev();
        } else {
            self.home.focus_next();
        }
    }

    /// The agent the Home composer launches.
    ///
    /// Home is Pi-first: when Pi is installed it is always chosen, so the
    /// composer is a one-key launch for the one agent this client targets.
    /// Otherwise it falls back to the highlighted catalog entry.
    fn home_agent(&self) -> Option<&AgentConfig> {
        self.agent_configs
            .iter()
            .find(|config| config.kind == "pi")
            .or_else(|| {
                let label = self.home.choice(HomeField::Agent).selected()?;
                self.agent_configs
                    .iter()
                    .find(|config| config.label == label)
            })
    }

    /// Every open workspace the Home composer can launch into, across the
    /// connected endpoints, in picker order.
    ///
    /// A machine's home directory is its default, not a project, so it is not
    /// offered: a launch with nothing picked is a fresh personal session.
    fn home_workspaces(&self) -> Vec<(EndpointId, &ShellWorkspace)> {
        let mut rows = Vec::new();
        for id in self.connected_endpoints() {
            for workspace in self.endpoint_workspaces(&id) {
                if self.is_home_workspace(&id, workspace) {
                    continue;
                }
                rows.push((id.clone(), workspace));
            }
        }
        rows
    }

    /// The label a workspace reads as in the Home picker: its name, plus the
    /// endpoint's label when it lives on a saved machine.
    fn home_workspace_label(&self, id: &EndpointId, workspace: &ShellWorkspace) -> String {
        match id {
            EndpointId::Local => workspace.name.clone(),
            EndpointId::Machine(_) => format!("{} ({})", workspace.name, self.endpoint_label(id)),
        }
    }

    /// The workspace the Home composer currently targets, if any.
    fn home_workspace(&self) -> Option<(EndpointId, ShellWorkspace)> {
        let index = self.home.workspace_index()?;
        let workspaces = self.home_workspaces();
        let (id, workspace) = workspaces.get(index)?;
        Some((id.clone(), (*workspace).clone()))
    }

    /// The machine key the Home composer's agents come from: the target
    /// workspace's own machine, because that is the one whose pi will run
    /// and whose catalog its flags must name. A personal launch has no
    /// workspace and stays local.
    fn home_machine_key(&self) -> String {
        self.home_workspace()
            .and_then(|(id, _)| id.machine_id().map(str::to_string))
            .unwrap_or_else(|| "local".to_string())
    }

    /// The SSH target for a machine key, or `None` for the local machine.
    fn machine_target_by_key(&self, key: &str) -> Option<SshTarget> {
        if key == "local" {
            return None;
        }
        self.machines
            .iter()
            .find(|machine| machine.id == key)
            .and_then(MachineInfo::ssh_target)
    }

    /// Fetch `key`'s agent configs when they are neither cached nor in flight.
    fn ensure_agent_configs_for(&mut self, key: &str) {
        if self.agent_config_cache.contains_key(key) || self.agent_config_pending.contains(key) {
            return;
        }
        self.agent_config_pending.insert(key.to_string());
        let target = self.machine_target_by_key(key);
        let tx = self.agent_config_tx.clone();
        let key = key.to_string();
        std::thread::spawn(move || {
            let configs = match target {
                Some(target) => pilus_client::agents::remote(&target),
                None => pilus_client::agents::local(),
            };
            tx.send((key, configs)).ok();
        });
    }

    /// Point `agent_configs` at the Home composer's machine, fetching if needed.
    fn ensure_home_agent_configs(&mut self) {
        let key = self.home_machine_key();
        if self.agent_config_key.as_deref() != Some(key.as_str()) {
            self.agent_configs = self
                .agent_config_cache
                .get(&key)
                .cloned()
                .unwrap_or_default();
            self.agent_config_key = Some(key.clone());
            // Another machine is another catalog: drop the picks so the new
            // one's defaults are applied rather than a model it never had.
            self.home_defaults_for = None;
            self.home_efforts_for = None;
        }
        self.ensure_agent_configs_for(&key);
    }

    /// Rebuild the Home picker options from live state.
    ///
    /// A signature guard skips the work when nothing relevant changed, so a
    /// large model catalog is not rebuilt on every frame.
    fn sync_home(&mut self) {
        if !self.home.is_open() {
            return;
        }
        // The composer follows the target workspace's machine, so a
        // workspace switch has to re-point the catalog before it paints.
        self.ensure_home_agent_configs();
        let workspace_labels: Vec<String> = self
            .home_workspaces()
            .iter()
            .map(|(id, workspace)| self.home_workspace_label(id, workspace))
            .collect();
        let signature = format!(
            "{}|{}|{}|{}|{}",
            self.home
                .choice(HomeField::Agent)
                .selected()
                .unwrap_or_default(),
            self.home
                .choice(HomeField::Model)
                .selected()
                .unwrap_or_default(),
            self.agent_configs.len(),
            self.agent_config_key.as_deref().unwrap_or_default(),
            workspace_labels.join("\u{1f}"),
        );
        if signature == self.home_sync_key {
            // Opening a folder that is already open changes no label, so the
            // pending pick is settled here too.
            self.home_select_pending_open();
            return;
        }
        self.home_sync_key = signature;

        let mut agents: Vec<String> = self
            .agent_configs
            .iter()
            .map(|config| config.label.clone())
            .collect();
        if agents.is_empty() {
            agents.push(
                if self.agent_config_pending.contains(&self.home_machine_key()) {
                    "Loading…".to_string()
                } else {
                    "No agent found".to_string()
                },
            );
        }
        self.home.choice_mut(HomeField::Agent).set(agents);

        self.home
            .choice_mut(HomeField::Workspace)
            .set(workspace_labels);

        self.home_select_pending_open();

        let models = self
            .home_agent()
            .map(AgentConfig::choices)
            .unwrap_or_else(|| vec!["Default".to_string()]);
        self.home.choice_mut(HomeField::Model).set(models);

        // Apply the agent's default model once per agent switch, so a later
        // pick by the user is not overwritten on the next frame.
        let kind = self.home_agent().map(|config| config.kind.clone());
        if kind != self.home_defaults_for {
            self.home_defaults_for = kind;
            if let Some(default) = self
                .home_agent()
                .and_then(|config| config.default_model.clone())
                && let Some(index) = self
                    .home
                    .choice(HomeField::Model)
                    .options()
                    .iter()
                    .position(|model| *model == default)
            {
                self.home.choice_mut(HomeField::Model).choose(index);
            }
        }

        // The effort picker follows the selected model's own reasoning levels.
        let model = self
            .home
            .choice(HomeField::Model)
            .selected()
            .unwrap_or("Default")
            .to_string();
        let efforts = self
            .home_agent()
            .map(|config| config.efforts_for(&model))
            .unwrap_or_else(|| vec!["Default".to_string()]);
        self.home.choice_mut(HomeField::Effort).set(efforts);
        if self.home_efforts_for.as_deref() != Some(model.as_str()) {
            self.home_efforts_for = Some(model.clone());
            if let Some(default) = self
                .home_agent()
                .and_then(|config| config.default_effort_for(&model))
                && let Some(index) = self
                    .home
                    .choice(HomeField::Effort)
                    .options()
                    .iter()
                    .position(|level| *level == default)
            {
                self.home.choice_mut(HomeField::Effort).choose(index);
            }
        }
    }

    /// Select the workspace the composer's own picker just opened, once the
    /// catalog lists it, so Send targets it rather than the previous pick.
    ///
    /// The folder is matched on its endpoint as well as its path: the same
    /// path is usually open on this Mac and on a saved machine.
    fn home_select_pending_open(&mut self) {
        let Some((endpoint, path)) = self.home_pending_open.as_ref() else {
            return;
        };
        let index = self.home_workspaces().iter().position(|(id, workspace)| {
            let cwd = self.expand_cwd(&id.key(), &workspace.cwd);
            id == endpoint && (cwd == *path || canonical(&cwd) == canonical(path))
        });
        if let Some(index) = index {
            self.home.choice_mut(HomeField::Workspace).choose(index);
            self.home_pending_open = None;
        }
    }

    /// The endpoint a folder picker source belongs to.
    fn endpoint_for_source(&self, source: &DirSource) -> Option<EndpointId> {
        match source {
            DirSource::Local => Some(EndpointId::Local),
            DirSource::Remote(target) => self
                .machines
                .iter()
                .find(|machine| machine.ssh_target().as_ref() == Some(target))
                .map(|machine| EndpointId::Machine(machine.id.clone())),
        }
    }

    /// Route a keystroke to the Home composer.
    fn home_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;

        // A session open on the Home page covers the composer, so its keys are
        // not the composer's: Escape goes back to the list.
        if self
            .overview
            .session
            .is_open_from(session_view::Origin::Home)
        {
            if key == "escape" {
                self.overview.session.close();
                cx.notify();
            }
            return;
        }

        // The reply under an agent on the Home page holds the keyboard while
        // it is focused, ahead of the composer around it.
        if self.overview.reply_focused() {
            self.overview_reply_key(event, cx);
            return;
        }

        // An open dropdown owns the keyboard: arrows move, Enter commits.
        if self.home.dropdown().is_some() {
            match key {
                "escape" => {
                    self.home.close_dropdown();
                }
                "up" => self.home.move_dropdown(-1),
                "down" => self.home.move_dropdown(1),
                "enter" | "tab" => self.home.close_dropdown(),
                "backspace" | "delete" | "del" => self.home.filter_backspace(),
                _ => {
                    if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
                        return;
                    }
                    if let Some(text) = event
                        .keystroke
                        .key_char
                        .as_deref()
                        .filter(|text| !text.is_empty())
                    {
                        self.home.filter_insert(text);
                    } else if event.keystroke.key.chars().count() == 1 {
                        let text = event.keystroke.key.clone();
                        self.home.filter_insert(&text);
                    }
                }
            }
            cx.notify();
            return;
        }

        if key == "escape" {
            self.close_home_page();
            cx.notify();
            return;
        }
        if mods.platform && key == "enter" {
            self.home_send(cx);
            return;
        }

        if self.home.focus() == HomeFocus::Prompt {
            // The prompt entity handles editing and stops propagation; only
            // the keys it declines (Tab, a bare Enter) reach here.
            match key {
                "enter" => self.home_send(cx),
                "tab" => self.home_move_focus(mods.shift),
                _ => {}
            }
            cx.notify();
            return;
        }

        // Picker focus: arrows and Tab move between chips, Enter opens one.
        match key {
            "tab" => self.home_move_focus(mods.shift),
            "left" | "up" => self.home_move_focus(true),
            "right" | "down" => self.home_move_focus(false),
            "enter" | "space" => {
                if let Some(field) = self.home.focused_picker() {
                    self.home.open_dropdown(field);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Launch the composer's agent in a fresh personal workspace under
    /// pilus's own directory.
    ///
    /// Workspace setup and the agent start share one background thread and
    /// report through the picker channel.
    fn home_send(&mut self, cx: &mut Context<Self>) {
        if self.home_launching {
            return;
        }
        let prompt = self.home.prompt().value().trim().to_string();
        if prompt.is_empty() {
            self.home.set_status("Write a prompt to send.".to_string());
            cx.notify();
            return;
        }
        let Some(agent) = self.home_agent().map(|config| config.kind.clone()) else {
            self.home.set_status("Pick an installed agent.".to_string());
            cx.notify();
            return;
        };
        let name = generate_session_name(&self.session_names());
        match self.home_workspace() {
            Some((endpoint, workspace)) => {
                self.launch_in_workspace(endpoint, workspace, name, agent, prompt, cx)
            }
            None => self.launch_personal_workspace(name, agent, prompt, cx),
        }
        cx.notify();
    }

    /// The launch flags for the agent, model, and effort the composer names.
    fn home_launch_args(&self) -> Vec<String> {
        let Some(config) = self.home_agent() else {
            return Vec::new();
        };
        let pick = |field| {
            self.home
                .choice(field)
                .selected()
                .unwrap_or("Default")
                .to_string()
        };
        config.launch_args(&pick(HomeField::Model), &pick(HomeField::Effort))
    }

    /// Start the composer's agent in an already-open workspace.
    ///
    /// The workspace may live on a saved machine, so the launch resolves that
    /// endpoint's own control connection and reports through the picker
    /// channel so the surface can hand off to it.
    fn launch_in_workspace(
        &mut self,
        endpoint: EndpointId,
        workspace: ShellWorkspace,
        name: String,
        agent: String,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
            self.home
                .set_status(format!("No control connection for `{}`.", workspace.name));
            cx.notify();
            return;
        };
        let machine = endpoint.machine_id().map(str::to_string);
        // The daemon is handed the same absolute spelling the picker opens
        // folders with, not the sidebar's `~` shorthand.
        let location = self.expand_cwd(&endpoint.key(), &workspace.cwd);
        let args = self.home_launch_args();
        self.home_launching = true;
        self.home.set_status(format!("starting {agent}…"));
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = launch_home_agent(&mut client, &location, &name, &agent, &args, &prompt);
            tx.send(PickerEvent::HomeLaunched { machine, result }).ok();
        });
        cx.notify();
    }

    /// Create a fresh personal session directory and start the agent in it.
    ///
    /// Each session gets its own git repository under `~/.pilus/sessions/<name>`,
    /// so the agent has an isolated tree to commit into.
    fn launch_personal_workspace(
        &mut self,
        name: String,
        agent: String,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut client) = ControlClient::connect(&self.api_socket()) else {
            self.home
                .set_status("No control connection for this machine.".to_string());
            cx.notify();
            return;
        };
        let dir = self.pilus_root.join("sessions").join(&name);
        let args = self.home_launch_args();
        self.home_launching = true;
        self.home.set_status(format!("starting {agent}…"));
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = pilus_client::init_repository(&dir).and_then(|()| {
                let location = dir.to_string_lossy().into_owned();
                launch_home_agent(&mut client, &location, &name, &agent, &args, &prompt)
            });
            tx.send(PickerEvent::HomeLaunched {
                machine: None,
                result,
            })
            .ok();
        });
        cx.notify();
    }

    /// Keep the keyboard where the Home composer says it is.
    ///
    /// The prompt is its own entity, so the window root cannot simply claim
    /// focus each frame; when the composer is on a picker (or closed) the root
    /// takes it back so chip navigation still works.
    /// The form input entity for `field`.
    fn sync_focus(&self, window: &mut Window, cx: &mut App) {
        // The prompt's field owns the keyboard while it is up; without this
        // the layer routing would hand focus back to the window root. A
        // destructive confirm has no field — keys stay on the root, which
        // already routes Enter and Escape to it.
        if let Some(prompt) = self.menu_prompt.as_ref()
            && !prompt.action.is_confirm()
        {
            let handle = self.menu_prompt_input.focus_handle(cx);
            if !handle.is_focused(window) {
                window.focus(&handle, cx);
            }
            return;
        }
        // The note field owns the keyboard while a note is being written.
        if self.md_composing() {
            let handle = self.md_note_input.focus_handle(cx);
            if !handle.is_focused(window) {
                window.focus(&handle, cx);
            }
            return;
        }
        // The markdown editor holds the keyboard while it is up.
        if self.md_editing() {
            // The find bar's field keeps the keyboard while the person types
            // in it; anywhere else it is the editor's.
            let editor = self.md_editor_input.focus_handle(cx);
            let finding = self.file_find.is_open() && self.find_input.read(cx).is_focused(window);
            if !finding && !editor.is_focused(window) {
                window.focus(&editor, cx);
            }
            return;
        }
        // The Sipil tab's fields hold focus themselves; the panel takes it only
        // when nothing inside it has.
        if self.input_layer() == InputLayer::SipilPanel
            && let Some(panel) = self.sipil_panel.as_ref()
        {
            let handle = panel.focus_handle(cx);
            if !handle.contains_focused(window, cx) {
                window.focus(&handle, cx);
            }
            return;
        }
        let target = match self.input_layer() {
            // The picker owns the keyboard; its path field only while editing,
            // otherwise the window root so list navigation keeps working.
            InputLayer::Picker => {
                if self.picker.editing() {
                    Some(self.picker_path.focus_handle(cx))
                } else {
                    None
                }
            }
            InputLayer::Palette => Some(self.palette_input.focus_handle(cx)),
            InputLayer::QuickOpen => Some(self.quick_open_input.focus_handle(cx)),
            InputLayer::Home => {
                if self.overview.reply_focused() {
                    Some(self.overview_reply.focus_handle(cx))
                } else if self.home.focus() == HomeFocus::Prompt {
                    Some(self.home_prompt.focus_handle(cx))
                } else {
                    None
                }
            }
            InputLayer::Settings => {
                if self.update_url_editing {
                    Some(self.update_feed_input.focus_handle(cx))
                } else {
                    match self.relay_editing {
                        Some(RelayField::Url) => Some(self.relay_url_input.focus_handle(cx)),
                        Some(RelayField::Secret) => Some(self.relay_secret_input.focus_handle(cx)),
                        None => None,
                    }
                }
            }
            // The composer's text fields hold focus; its pickers leave it
            // with the window, which reads their keys.
            InputLayer::Tasks if self.tasks_panel.composing() => self
                .tasks_inputs
                .focus_handle(self.tasks_panel.compose_focus(), cx),
            InputLayer::Tasks => None,
            // The note field holds focus while it is open, and the search
            // field while it is being typed in; otherwise the window root
            // reads the page's shortcuts.
            InputLayer::Diff if self.diff_panel.composing() => {
                Some(self.diff_inputs.focus_handle(cx))
            }
            InputLayer::Diff if self.diff_panel.searching() => {
                Some(self.diff_inputs.search_focus_handle(cx))
            }
            InputLayer::Diff => None,
            InputLayer::Browser => Some(self.browser_input.focus_handle(cx)),
            // The reply field holds focus while it is open; otherwise the
            // window root reads the page's shortcuts.
            InputLayer::Mission if self.mission.answering() => {
                Some(self.mission_answer.focus_handle(cx))
            }
            InputLayer::Mission => None,
            InputLayer::Automations => None,
            // The query field holds the keyboard, except over the permission
            // card, which has none.
            InputLayer::Search
                if self.search.typing(self.settings.index_agents)
                    && !self.overview.session.is_open() =>
            {
                Some(self.search_input.focus_handle(cx))
            }
            InputLayer::Search => None,
            InputLayer::Stats => None,
            InputLayer::Process => None,
            InputLayer::Database => None,
            // The composer holds the keyboard.
            InputLayer::Sipil => Some(self.sipil_chat.active_input().focus_handle(cx)),
            InputLayer::SipilPanel => None,
            InputLayer::Terminal => None,
        };
        match target {
            Some(handle) => {
                if !handle.is_focused(window) {
                    window.focus(&handle, cx);
                }
            }
            None => {
                let root = self.focus_handle.clone();
                if !root.is_focused(window) {
                    window.focus(&root, cx);
                }
            }
        }
    }

    /// The Home screen: the launch composer that fills the card.
    ///
    /// The app mark sits to the left of the card.
    fn home_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let status = self.home.status().to_string();
        let composer = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .child(self.home_composer(cx));
        let mut column = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_4()
            .w_full()
            .max_w(px(720.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_3()
                    .w_full()
                    .child(self.home_mark())
                    .child(composer),
            );
        if !status.is_empty() {
            column = column.child(
                div()
                    .w_full()
                    .px_2()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child(status),
            );
        }
        div()
            .id("pilus-home-scroll")
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px_6()
            .py_8()
            .child(column)
    }

    /// The app mark to the left of the composer, falling back to the wordmark.
    fn home_mark(&self) -> AnyElement {
        let theme = self.theme;
        match about::app_icon() {
            Some(icon) => img(icon).w(px(52.0)).h(px(52.0)).into_any_element(),
            None => div()
                .text_size(px(style::scaled(&self.shared_settings, 30.0)))
                .text_color(theme.accent)
                .child("pilus")
                .into_any_element(),
        }
    }

    /// The prompt edit area, with a caret on the cursor's line.
    fn home_prompt_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let entity = self.home_prompt.clone();
        entity.update(cx, |input, _| {
            input.set_style(
                self.theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        div()
            .id("pilus-home-prompt")
            .flex()
            .flex_col()
            .min_h(px(84.0))
            .px_4()
            .pt_3()
            .pb(px(0.0))
            .text_size(px(style::scaled(&self.shared_settings, 15.0)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.overview.blur_reply();
                    this.home.set_focus(HomeFocus::Prompt);
                    this.home.close_dropdown();
                    cx.notify();
                }),
            )
            .child(entity)
    }

    /// The composer card: the prompt above a row of pickers and the send button.
    ///
    /// The card rounds on every corner and floats above the meta footer.
    fn home_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .w_full()
            .radius_lg()
            .bg(theme.panel.opacity(0.0))
            .border_t_1()
            .border_l_1()
            .border_r_1()
            .border_b_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(self.home_prompt_area(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .pl_4()
                    .pr_2()
                    .pt_2()
                    .pb_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(self.home_chip(HomeField::Workspace, cx))
                            .child(self.home_chip(HomeField::Model, cx))
                            .child(self.home_chip(HomeField::Effort, cx)),
                    )
                    .child(self.home_send_button(cx)),
            )
    }

    /// One picker chip inside the composer.
    fn home_chip(&self, field: HomeField, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.home.choice(field).selected().unwrap_or_default();
        // An empty Workspace list still needs a label; it reads as a prompt to
        // open one, and sending falls back to a fresh scratch workspace.
        let value = if field == HomeField::Workspace && selected.is_empty() {
            "Open a workspace".to_string()
        } else {
            selected.to_string()
        };
        let leading = self.home_picker_leading(field, &value);
        self.home_picker_face(field, leading, value, cx)
    }

    /// The icon-only circular send button.
    fn home_send_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div().child(
            div()
                .id("pilus-home-send")
                .flex()
                .items_center()
                .justify_center()
                .w(px(30.0))
                .h(px(30.0))
                .radius_full()
                .cursor_pointer()
                .state_fill(theme.panel, theme.border, style::RADIUS_FULL)
                .on_click(cx.listener(|this, _, _, cx| this.home_send(cx)))
                .child(icon(
                    include_bytes!("../../assets/icons/arrow-up.svg"),
                    15.0,
                    theme.fg,
                )),
        )
    }

    /// The fill color of a picker's leading glyph, brighter while its
    /// dropdown is open.
    fn home_picker_fg(&self, field: HomeField) -> gpui::Rgba {
        if self.home.dropdown() == Some(field) {
            self.theme.panel_fg
        } else {
            self.theme.muted
        }
    }

    /// The leading glyph shared by a composer chip and a picker.
    fn home_picker_leading(&self, field: HomeField, value: &str) -> AnyElement {
        let fg = self.home_picker_fg(field);
        let glyph = |bytes: &'static [u8]| icon(bytes, 14.0, fg).into_any_element();
        match field {
            HomeField::Agent => glyph(agent_icon(value)),
            HomeField::Workspace => glyph(include_bytes!("../../assets/icons/folder.svg")),
            HomeField::Model => glyph(include_bytes!("../../assets/icons/cpu.svg")),
            HomeField::Effort => glyph(include_bytes!("../../assets/icons/gauge.svg")),
        }
    }

    /// The compact face shared by every composer picker.
    ///
    /// Agent, model, and effort all draw through this, so the chips read as
    /// one control with one interaction.
    fn home_picker_face(
        &self,
        field: HomeField,
        leading: AnyElement,
        value: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let open = self.home.dropdown() == Some(field);
        let mut face = picker_face(
            format!("pilus-home-picker-{}", field.label()),
            leading,
            value,
            180.0,
            open,
            theme.panel,
            theme,
            self.footer_font(),
            cx.listener(move |this, _, _, cx| this.toggle_home_dropdown(field, cx)),
        );
        if self.home.dropdown() == Some(field) {
            face = face.relative().child(self.home_dropdown_anchor(field, cx));
        }
        face.into_any_element()
    }

    /// Toggle `field`'s dropdown from its chip.
    ///
    /// A press outside the dropdown already closed it in the capture phase;
    /// in that case this click is a toggle-off and must not reopen the picker.
    fn toggle_home_dropdown(&mut self, field: HomeField, cx: &mut Context<Self>) {
        self.overview.blur_reply();
        self.home.set_focus(HomeFocus::Picker(field));
        if self.home.take_dismissed() == Some(field) {
            // Already closed by the outside press; stay closed.
        } else if self.home.dropdown() == Some(field) {
            self.home.close_dropdown();
        } else {
            self.home.open_dropdown(field);
        }
        cx.notify();
    }

    /// A zero-size anchor at the chip's top-left corner.
    ///
    /// The chip centers its real children, so an anchored child would inherit
    /// that centering; pinning this wrapper to the top-left gives the dropdown
    /// a stable origin without adding a flex item (and its gap) to the chip.
    fn home_dropdown_anchor(&self, field: HomeField, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .child(self.home_dropdown(field, cx))
            .into_any_element()
    }

    /// The open picker's option list, floated under the chip that owns it.
    ///
    /// Rendering it through `deferred`/`anchored` keeps it out of the centered
    /// composer's flow, so opening a picker no longer shifts the layout.
    /// Large catalogs (Pi can list hundreds of models) are filtered and
    /// capped, so opening the dropdown never builds an unbounded element tree.
    fn home_dropdown(&self, field: HomeField, cx: &mut Context<Self>) -> AnyElement {
        const MAX_ROWS: usize = 100;
        let theme = self.theme;
        let cursor = self.home.choice(field).cursor();
        let options = self.home.choice(field).options();
        let matches = self.home.dropdown_matches();
        let total = matches.len();
        let filter = self.home.filter().to_string();
        let mut list = picker_menu("pilus-home-dropdown", theme)
            .max_h(px(PICKER_MENU_MAX_H))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.home.dismiss_dropdown();
                cx.notify();
            }));
        if !filter.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .pt_1()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(format!("Filter: {filter}")),
            );
        }
        if field == HomeField::Workspace {
            list = list.child(
                div()
                    .id("pilus-home-open-workspace")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(30.0))
                    .px_3()
                    .radius_md()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.panel_fg)
                    .cursor_pointer()
                    .hover_fill(theme.panel, style::RADIUS_MD)
                    .child(icon(
                        include_bytes!("../../assets/icons/plus.svg"),
                        13.0,
                        theme.muted,
                    ))
                    .child("Open Workspace…")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.home.close_dropdown();
                        this.open_picker(cx);
                    })),
            );
        }
        if matches.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No matches"),
            );
        }
        for index in matches.into_iter().take(MAX_ROWS) {
            let active = index == cursor;
            let option = options.get(index).cloned().unwrap_or_default();
            list = list.child(picker_row(
                format!("pilus-home-option-{index}"),
                option,
                active,
                theme,
                self.chrome_font(),
                cx.listener(move |this, _, _, cx| {
                    this.home.choose_dropdown(index);
                    this.home.close_dropdown();
                    cx.notify();
                }),
            ));
        }
        if total > MAX_ROWS {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(format!("…{} more · type to filter", total - MAX_ROWS)),
            );
        }
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .offset(point(px(0.0), px(PICKER_CHIP_H + 4.0)))
                .snap_to_window_with_margin(px(8.0))
                .child(style::enter("pilus-home-dropdown-enter", list)),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// Show the endpoint a launch just opened a workspace on.
    ///
    /// Opening happens on the owning daemon, which may not own the visible
    /// surface; without the handoff the click looks like a no-op until the
    /// user manually switches to that machine.
    fn activate_opened_endpoint(
        &mut self,
        machine: Option<String>,
        pane: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let target = match machine {
            None => EndpointId::Local,
            Some(id) => EndpointId::Machine(id),
        };
        self.activate_endpoint(target, None, pane, cx);
    }

    /// Route a keystroke to the open Settings page.
    fn settings_key(&mut self, _event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = _event.keystroke.key.as_str();
        // The feed-URL input owns the keyboard while it is being edited; only
        // the keys it declines reach here.
        if self.update_url_editing {
            match key {
                "escape" => self.update_url_editing = false,
                "enter" => self.commit_update_feed(cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        if self.relay_editing.is_some() {
            match key {
                "escape" => {
                    self.relay_editing = None;
                    self.relay_status.clear();
                }
                "enter" => self.commit_relay(cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        if key == "escape" {
            self.settings_open = false;
            cx.notify();
            return;
        }
        if self.machine_pending_remove.is_some() && key != "shift" {
            self.machine_pending_remove = None;
        }
        cx.notify();
    }

    /// The Settings page, drawn in place of the terminal card.
    ///
    /// It fills the same card the grid lives in, so it reads as a native
    /// surface with the sidebar still showing. A Back button at the top left closes it; there is no close
    /// button by design.
    fn settings_page(&self, cx: &mut Context<Self>) -> Div {
        if !self.settings_open {
            return div();
        }
        let theme = self.theme;
        // The relay only matters to the connector, so it appears with it.
        let relay = self.remote.on().then(|| {
            style::section_card_tab(theme, "IROH RELAY", self.section_font()).child(style::enter(
                "pilus-settings-relay",
                self.relay_section_block(cx),
            ))
        });
        let identity = self
            .remote
            .status
            .as_ref()
            .filter(|_| self.remote.on())
            .map(|status| {
                style::section_card_tab(theme, "IROH IDENTITY", self.section_font()).child(
                    style::enter(
                        "pilus-settings-iroh-identity",
                        self.iroh_identity_section_block(status, cx),
                    ),
                )
            });
        let body = div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                style::section_card_tab(theme, "REMOTE ACCESS", self.section_font())
                    .child(self.remote_access_section_block(cx)),
            )
            .children(identity)
            .children(relay)
            .children(cfg!(target_os = "macos").then(|| {
                style::section_card_tab(theme, "MENU BAR", self.section_font())
                    .child(self.menu_bar_section_block(cx))
            }))
            .child(
                style::section_card_tab(theme, "GITHUB", self.section_font())
                    .child(self.github_settings_block(cx)),
            )
            .child(
                style::section_card_tab(theme, "REMOTE MACHINES", self.section_font())
                    .child(self.machines_section_block(cx)),
            )
            .child(
                style::section_card_tab(theme, "ABOUT PILUS", self.section_font())
                    .child(self.about_pilus_section_block(cx)),
            )
            .child(
                style::section_card_tab(theme, "ABOUT LUVUS", self.section_font())
                    .child(self.about_luvus_section_block(cx)),
            );
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(
                div().flex().flex_row().flex_shrink_0().px_4().py_3().child(
                    style::small_button(
                        "pilus-settings-back",
                        "‹ Back",
                        style::ButtonKind::Secondary,
                        theme,
                        self.footer_font(),
                    )
                    .text_color(theme.muted)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_open = false;
                        cx.notify();
                    })),
                ),
            )
            .child(
                div()
                    .id("pilus-settings-page")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(div().flex().flex_col().py_3().child(body)),
            )
            .child(if self.settings_status.is_empty() {
                div()
            } else {
                div()
                    .px_4()
                    .py_2()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(self.settings_status.clone())
            })
    }

    /// The REMOTE MACHINES section of the page: every saved machine profile
    /// with the same status and enable/remove actions as the manager window.
    /// Adding and provisioning stay in the full manager window.
    /// The REMOTE ACCESS section: reach this machine from another device.
    ///
    /// The switch installs a background service, so the section describes the
    /// service, not a window feature: it keeps running after pilus closes and
    /// starts at login. Pairing is a button, not a startup side effect: "Add
    /// device" mints a fresh code on demand and shows it as a QR.
    fn remote_access_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let remote = &self.remote;
        if !pilus_client::agent_service::SUPPORTED {
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .px_3()
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(
                            "Remote access runs as a background service, which pilus supports on macOS and Linux. It is not available on this platform yet.",
                        ),
                );
        }
        let toggle = style::toggle("pilus-remote-access-toggle", remote.on(), theme).on_click(
            cx.listener(|this, _, _, cx| {
                this.toggle_connector();
                cx.notify();
            }),
        );
        let blurb = if remote.on() {
            "Runs in the background and starts at login, so your phone can reach this machine and the machines it manages. Turn off to stop it."
        } else {
            "Reach this machine, and every machine it manages, from your phone. Keeps running after pilus closes."
        };
        let mut section = div().flex().flex_col().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .pb_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(blurb),
                )
                .child(toggle),
        );
        if let Some(error) = &remote.error {
            section = section.child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.alert)
                    .child(error.clone()),
            );
        }
        match (remote.phase, &remote.status) {
            (connector::Phase::Off, _) => {
                if agent_binary().is_none() {
                    section = section.child(
                        div()
                            .px_3()
                            .text_size(px(self.footer_font()))
                            .text_color(theme.muted)
                            .child("pilus-agent is not installed next to pilus."),
                    );
                }
            }
            (connector::Phase::Starting, _) | (connector::Phase::Running, None) => {
                section = section.child(
                    div()
                        .px_3()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(style::spinner(
                            "pilus-remote-starting",
                            self.footer_font(),
                            theme.muted,
                        ))
                        .child("Starting…"),
                );
            }
            (connector::Phase::Running, Some(status)) => {
                section = section.child(self.remote_access_running(status, cx));
            }
        }
        section
    }

    /// What the running service shows: its health, the Add device action, the
    /// pairing card while a code is live, and the enrolled devices.
    fn remote_access_running(
        &self,
        status: &pilus_client::agent_service::ServiceStatus,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let remote = &self.remote;
        let small = self.footer_font();
        let missing = status.luvus_missing();
        let health = if status.gateway_up {
            "Running"
        } else if missing {
            "Luvus not found"
        } else {
            "Waiting for Luvus"
        };
        let add = if remote.minting {
            "Adding…"
        } else if remote.code.is_some() {
            "New code"
        } else {
            "Add device"
        };
        let mut block = div().flex().flex_col().gap_2().mx_3();
        block = block.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .pb_2()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .min_w(px(0.0))
                        .text_size(px(small))
                        .text_color(theme.muted)
                        .child(
                            div()
                                .w(px(7.0))
                                .h(px(7.0))
                                .flex_shrink_0()
                                .radius_full()
                                .bg(if status.gateway_up {
                                    theme.accent
                                } else {
                                    theme.muted
                                }),
                        )
                        .child(div().truncate().child(health)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_shrink_0()
                        .items_center()
                        .gap_2()
                        // Retrying cannot conjure a binary that is not installed.
                        .when(!status.gateway_up && !missing, |actions| {
                            actions.child(
                                style::small_button(
                                    "pilus-start-luvus",
                                    "Start Luvus",
                                    style::ButtonKind::Secondary,
                                    theme,
                                    small,
                                )
                                .on_click(cx.listener(
                                    |_, _, _, cx| {
                                        connector::retry_luvus();
                                        cx.notify();
                                    },
                                )),
                            )
                        })
                        .child(
                            style::small_button(
                                "pilus-restart-service",
                                "Restart",
                                style::ButtonKind::Secondary,
                                theme,
                                small,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                connector::restart_service();
                                this.settings_status = "Restarting the service…".to_string();
                                cx.notify();
                            })),
                        )
                        .when(!status.strict, |actions| {
                            actions.child(
                                style::small_button(
                                    "pilus-add-device",
                                    add,
                                    style::ButtonKind::Primary,
                                    theme,
                                    small,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        // A fresh pairing starts at the safe level;
                                        // a re-mint keeps the choice on screen.
                                        if this.remote.code.is_none() {
                                            this.pairing_operate = false;
                                        }
                                        connector::mint_code(if this.pairing_operate {
                                            pilus_client::agent_service::DeviceLevel::Operate
                                        } else {
                                            pilus_client::agent_service::DeviceLevel::Control
                                        });
                                        cx.notify();
                                    },
                                )),
                            )
                        }),
                ),
        );
        // Say what is wrong and what to do about it, not just that it is.
        if !status.gateway_up {
            let advice = if missing {
                "pilus can't find the luvus program. Install Luvus (or set LUVUS_BIN), then turn Remote access off and on."
            } else {
                "The Luvus server isn't running, so there is nothing to reach yet. Start it and Remote access connects by itself."
            };
            block = block.child(style::enter(
                "pilus-remote-advice",
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_size(px(small))
                    .text_color(theme.muted)
                    .child(advice)
                    .when_some(status.gateway_error.clone(), |note, why| {
                        note.child(div().truncate().child(why))
                    }),
            ));
        }
        if status.restart_needed {
            block = block.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(small))
                    .text_color(theme.muted)
                    .child(style::spinner(
                        "pilus-remote-restarting",
                        small,
                        theme.muted,
                    ))
                    .child("Updated. Restarting the service to use the new version…"),
            );
        }
        // A Linux user service stops with the last login session unless lingering is on.
        if status.linger == Some(false) {
            block = block
                .child(
                    div()
                        .text_size(px(small))
                        .text_color(theme.muted)
                        .child("This service stops when you log out. To keep it running, run:"),
                )
                .child(self.copy_row(
                    "command",
                    "loginctl enable-linger".to_string(),
                    "pilus-copy-linger",
                    cx,
                ));
        }
        if let Some(code) = &remote.code {
            let remaining = code.expires_at.saturating_sub(tasks::unix_now());
            block = block.child(style::enter(
                "pilus-pairing-card",
                div()
                    .px_3()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .radius_md()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .child(style::qr_code(&code.link, 176.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .text_size(px(small))
                                    .text_color(theme.muted)
                                    .child(format!(
                                        "Scan this in the pilus companion app, or paste the link. It works once, for 10 minutes. The device will be: {}.",
                                        code.level.label()
                                    ))
                                    .child(self.copy_row(
                                        "code",
                                        code.code.clone(),
                                        "pilus-copy-code",
                                        cx,
                                    ))
                                    .child(self.copy_row(
                                        "link",
                                        code.link.clone(),
                                        "pilus-copy-link",
                                        cx,
                                    ))
                                    // The level is part of the code, so changing it mints a new one.
                                    .child(
                                        style::checkbox(
                                            "pilus-pairing-operate",
                                            self.pairing_operate,
                                            "Allow this device to start work (tasks, agents, workspaces)",
                                            theme,
                                            small,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if this.remote.minting {
                                                return;
                                            }
                                            this.pairing_operate = !this.pairing_operate;
                                            connector::mint_code(if this.pairing_operate {
                                                pilus_client::agent_service::DeviceLevel::Operate
                                            } else {
                                                pilus_client::agent_service::DeviceLevel::Control
                                            });
                                            cx.notify();
                                        })),
                                    )
                                    .child(format!(
                                        "Expires in {}:{:02}",
                                        remaining / 60,
                                        remaining % 60
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_end()
                            .child(
                                style::small_button(
                                    "pilus-pairing-done",
                                    "Done",
                                    style::ButtonKind::Primary,
                                    theme,
                                    small,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pairing_operate = false;
                                    connector::dismiss_code();
                                    cx.notify();
                                })),
                            ),
                    ),
            ));
        }
        let mut devices = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .radius_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border);
        devices = devices.child(style::section_label(theme, "PAIRED DEVICES", small));
        if status.devices.is_empty() {
            devices = devices.child(
                div()
                    .text_size(px(small))
                    .text_color(theme.muted)
                    .child("No devices yet. Add one to get a code."),
            );
        }
        for (index, device) in status.devices.iter().enumerate() {
            let name = if device.label.is_empty() {
                format!(
                    "Device {}",
                    device.endpoint_id.chars().take(8).collect::<String>()
                )
            } else {
                device.label.clone()
            };
            let id = device.endpoint_id.clone();
            // Raising a level takes a new code, so only the levels below the
            // device's own can be picked; the rest show the range.
            let limit = (!status.strict).then(|| {
                let mut levels = style::segmented(theme);
                for (label, level) in [
                    ("View", pilus_client::agent_service::DeviceLevel::View),
                    ("Control", pilus_client::agent_service::DeviceLevel::Control),
                    ("Operate", pilus_client::agent_service::DeviceLevel::Operate),
                ] {
                    let id = id.clone();
                    let lowers = level < device.level;
                    levels = levels.child(
                        style::segment(
                            SharedString::from(format!("pilus-limit-{index}-{label}")),
                            label,
                            level == device.level,
                            lowers,
                            theme,
                            small,
                        )
                        .when(lowers, |segment| {
                            segment.on_click(cx.listener(move |_, _, _, cx| {
                                connector::lower(id.clone(), level);
                                cx.notify();
                            }))
                        }),
                    );
                }
                levels
            });
            let revoke = (!status.strict).then(|| {
                style::small_button(
                    SharedString::from(format!("pilus-revoke-{index}")),
                    "Unpair",
                    style::ButtonKind::Secondary,
                    theme,
                    small,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    connector::revoke(id.clone());
                    cx.notify();
                }))
            });
            devices = devices.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(small))
                    .text_color(theme.fg)
                    .child(
                        div().flex_1().min_w(px(0.0)).truncate().child(name).child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .text_color(theme.muted)
                                .child(
                                    div()
                                        .text_color(
                                            if device.level
                                                == pilus_client::agent_service::DeviceLevel::Operate
                                            {
                                                theme.alert
                                            } else {
                                                theme.muted
                                            },
                                        )
                                        .child(device.level.label()),
                                )
                                .when(device.enrolled_at > 0, |line| {
                                    line.child(format!(
                                        "· Paired {}",
                                        mission::utc_label(device.enrolled_at)
                                    ))
                                }),
                        ),
                    )
                    .children(limit)
                    .children(revoke),
            );
        }
        // Say why a segment is greyed out, or the control reads as broken.
        if !status.strict && !status.devices.is_empty() {
            devices = devices.child(
                div()
                    .pt_1()
                    .text_size(px(small))
                    .text_color(theme.muted)
                    .child("A device's level can only be lowered. To raise it, unpair the device and pair it again with a new code."),
            );
        }
        block = block.child(devices);
        block
    }

    /// A muted read-only `label: value` with a Copy button beside it.
    fn copy_row(
        &self,
        label: &'static str,
        value: String,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        self.copy_shown(label, Some(label), value, id, cx)
    }

    /// A muted read-only value with a Copy button beside it, for a row whose
    /// heading already names it.
    fn copy_value_row(
        &self,
        label: &'static str,
        value: String,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        self.copy_shown(label, None, value, id, cx)
    }

    /// `value` in monospace, behind an optional `prefix:` in the chrome font,
    /// beside a Copy button that puts `value` on the clipboard.
    fn copy_shown(
        &self,
        label: &'static str,
        prefix: Option<&'static str>,
        value: String,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let size = self.section_font();
        let shown = value.clone();
        let button = style::small_button(id, "Copy", style::ButtonKind::Secondary, theme, size)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                this.settings_status = format!("Copied {label}.");
                cx.notify();
            }));
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
                    .text_size(px(size))
                    .text_color(theme.muted)
                    .children(prefix.map(|prefix| format!("{prefix}: ")))
                    .child(
                        div()
                            .font_family(style::mono_family(&self.shared_settings))
                            .child(shown),
                    ),
            )
            .child(button)
    }

    /// Turn Remote access on or off from the settings page.
    fn toggle_connector(&mut self) {
        if self.remote.on() {
            connector::disable();
            self.set_connector(false);
            self.settings_status = "Remote access off.".to_string();
        } else if agent_binary().is_none() {
            self.settings_status = "pilus-agent is not installed next to pilus.".to_string();
        } else {
            connector::enable();
            self.set_connector(true);
        }
        self.remote = connector::snapshot();
    }

    /// Persist the connector choice so it restarts on the next launch.
    fn set_connector(&mut self, on: bool) {
        self.shared_settings.borrow_mut().connector = on;
        self.shared_settings.borrow().save(&pilus_config_dir());
    }

    fn machines_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .px_3()
                .child(
                    style::small_button(
                        "pilus-machines-add",
                        "Add Machine",
                        style::ButtonKind::Primary,
                        theme,
                        self.footer_font(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Opened from here, not through a global action: that
                        // handler runs inside this window's update, where the
                        // view cannot be read back to learn its session.
                        machines::open(
                            this.session_root.clone(),
                            this.session_name.clone(),
                            this.machine_signal.clone(),
                            this.shared_theme.clone(),
                            this.shared_settings.clone(),
                            cx,
                        );
                    })),
                ),
        );
        let mut any = false;
        for machine in self.machines.iter().filter(|machine| machine.id != "local") {
            any = true;
            section = section.child(self.machine_page_row(machine, cx));
        }
        if !any {
            section = section.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("No remote machines yet. Add one to work on other hosts."),
            );
        }
        section
    }

    /// One saved machine row: name, destination, status, and its actions.
    fn machine_page_row(&self, machine: &MachineInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let id = machine.id.clone();
        let busy = self.machine_busy.as_deref() == Some(machine.id.as_str());
        let follow = &self.machine_follow;
        let is = |pending: &Option<String>| pending.as_deref() == Some(machine.id.as_str());
        let confirming = is(&self.machine_pending_remove);
        let (status, status_color) = machine_status_text(machine, theme);
        let behind = match (
            machine.version.as_deref(),
            self.luvus_local_version.as_deref(),
        ) {
            (Some(remote), Some(local)) => is_behind(remote, local),
            _ => false,
        };
        let toggle_id = id.clone();
        let toggle_label = if machine.enabled {
            "Disable Machine"
        } else {
            "Enable Machine"
        };
        let update_id = id.clone();
        let restart_id = id.clone();
        let stop_id = id.clone();
        let remove_id = id.clone();
        let restart_button = (machine.enabled && is(&follow.restart_due)).then(|| {
            self.machine_action(
                format!("pilus-machine-page-restart-{id}"),
                if is(&follow.confirming_restart) {
                    "Confirm restart"
                } else {
                    "Restart"
                },
                theme.alert,
                cx.listener(move |this, _, _, cx| {
                    this.restart_machine_page(restart_id.clone());
                    cx.notify();
                }),
            )
        });
        let stop_button = is(&follow.stop_due).then(|| {
            self.machine_action(
                format!("pilus-machine-page-stop-{id}"),
                if is(&follow.confirming_stop) {
                    "Confirm stop"
                } else {
                    "Stop & upgrade"
                },
                theme.alert,
                cx.listener(move |this, _, _, cx| {
                    this.stop_machine_page(stop_id.clone());
                    cx.notify();
                }),
            )
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .mx_3()
            .px_2()
            .py_2()
            .radius_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(self.chrome_font()))
                                    .text_color(theme.fg)
                                    .child(machine.display().to_string()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(self.section_font()))
                                    .text_color(theme.muted)
                                    .child(machine.id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(self.section_font()))
                            .text_color(theme.muted)
                            .child(format!("{} via SSH", machine.destination))
                            .children(
                                machine
                                    .version
                                    .clone()
                                    .map(|version| div().child(format!("Luvus {version}"))),
                            )
                            .children(behind.then(|| {
                                div().text_color(theme.alert).child(format!(
                                    "behind v{}",
                                    self.luvus_local_version.clone().unwrap_or_default()
                                ))
                            }))
                            .child(div().text_color(status_color).child(status)),
                    ),
            )
            .child(self.machine_action(
                format!("pilus-machine-page-toggle-{id}"),
                if busy { "…" } else { toggle_label },
                if busy { theme.muted } else { theme.fg },
                cx.listener(move |this, _, _, cx| {
                    this.toggle_machine_page(toggle_id.clone());
                    cx.notify();
                }),
            ))
            .children(stop_button)
            .children(restart_button)
            .child(self.machine_action(
                format!("pilus-machine-page-update-{id}"),
                if behind {
                    "Upgrade Luvus"
                } else {
                    "Update Luvus"
                },
                if behind { theme.fg } else { theme.muted },
                cx.listener(move |this, _, _, cx| {
                    this.update_machine_page(update_id.clone());
                    cx.notify();
                }),
            ))
            .child(self.machine_action(
                format!("pilus-machine-page-remove-{id}"),
                if confirming {
                    "Confirm"
                } else {
                    "Remove Machine"
                },
                if confirming { theme.alert } else { theme.muted },
                cx.listener(move |this, _, _, cx| {
                    this.remove_machine_page(remove_id.clone());
                    cx.notify();
                }),
            ))
    }

    /// One ghost button of a machine row.
    fn machine_action(
        &self,
        id: String,
        label: &'static str,
        color: gpui::Rgba,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        style::small_button(
            id,
            label,
            style::ButtonKind::Ghost,
            self.theme,
            self.section_font(),
        )
        .text_color(color)
        .on_click(on_click)
    }

    /// Enable a disabled machine, or disable an enabled one, off-thread.
    fn toggle_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        let Some(enabled) = self
            .machines
            .iter()
            .find(|machine| machine.id == id)
            .map(|machine| machine.enabled)
        else {
            return;
        };
        self.clear_machine_confirms();
        let op = if enabled {
            MachineOp::Disable { id: id.clone() }
        } else {
            MachineOp::Enable { id: id.clone() }
        };
        self.run_machine_page_op(&id, op);
    }

    /// Re-provision a machine's Luvus from this Mac's, off-thread. The remote
    /// daemon keeps running the old binary until it is restarted.
    fn update_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        self.clear_machine_confirms();
        self.run_machine_page_op(&id.clone(), MachineOp::Update { id });
    }

    /// Restart a machine's daemon after a second confirming click, since it
    /// stops every pane in that session.
    fn restart_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        if self.machine_follow.confirming_restart.as_deref() != Some(id.as_str()) {
            self.clear_machine_confirms();
            self.settings_status = format!(
                "Restart `{id}`? This stops every pane in its session. \
                 Click Confirm restart to continue."
            );
            self.machine_follow.confirming_restart = Some(id);
            return;
        }
        self.machine_follow.confirming_restart = None;
        self.run_machine_page_op(&id.clone(), MachineOp::Restart { id });
    }

    /// Stop a machine's old daemon and install the new Luvus, after a second
    /// confirming click, since the daemon's panes end.
    fn stop_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        if self.machine_follow.confirming_stop.as_deref() != Some(id.as_str()) {
            self.clear_machine_confirms();
            self.settings_status = format!(
                "Stop `{id}`'s daemon? Its panes end (the session is saved). \
                 Click Confirm stop to continue."
            );
            self.machine_follow.confirming_stop = Some(id);
            return;
        }
        self.machine_follow.confirming_stop = None;
        self.run_machine_page_op(&id.clone(), MachineOp::StopAndUpdate { id });
    }

    /// Drop every machine action waiting on a second click.
    fn clear_machine_confirms(&mut self) {
        self.machine_pending_remove = None;
        self.machine_follow.confirming_restart = None;
        self.machine_follow.confirming_stop = None;
    }

    /// Remove a machine after a second confirming click.
    fn remove_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        if self.machine_pending_remove.as_deref() != Some(id.as_str()) {
            self.clear_machine_confirms();
            self.machine_pending_remove = Some(id.clone());
            self.settings_status = format!("Remove `{id}`? Click Confirm to delete the profile.");
            return;
        }
        self.machine_pending_remove = None;
        self.run_machine_page_op(&id.clone(), MachineOp::Remove { id });
    }

    /// Run one machine operation off-thread, then report back.
    ///
    /// The outcome and a fresh catalog travel through the existing channels,
    /// so the window reloads the section and tells other windows.
    fn run_machine_page_op(&mut self, id: &str, op: MachineOp) {
        self.machine_busy = Some(id.to_string());
        self.settings_status = "Running…".to_string();
        let root = self.session_root.clone();
        let session = self.session_name.clone();
        let op_tx = self.machine_op_tx.clone();
        let machine_tx = self.machine_tx.clone();
        std::thread::spawn(move || {
            op_tx
                .send(machines::run_op(&root, session.as_deref(), op))
                .ok();
            let machines = list_machines(&root, session.as_deref());
            machine_tx.send((session, machines)).ok();
        });
    }

    /// The ABOUT PILUS section: the running version, the update channel, the
    /// feed override (preview only), auto-check, and a manual check.
    /// Committing a change rebuilds the updater engine.
    fn about_pilus_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let request = UpdateRequest::from_settings(
            &self.settings.update_channel,
            self.settings.update_feed_url.clone(),
        );
        let source = describe_source(&request);
        let editing = self.update_url_editing;
        let explicit = self
            .settings
            .update_feed_url
            .clone()
            .filter(|url| !url.trim().is_empty());

        // The live input is a bare canvas with no intrinsic width, so the line
        // must fill the field or the input collapses to nothing and its text
        // is clipped away.
        let mut line = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(18.0));
        if editing {
            let input = self.update_feed_input.clone();
            input.update(cx, |input, _| {
                // The saved value beside it inherits the body size, so the
                // live input matches or the text jumps when focus moves.
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(style::Fonts::resolve(&self.shared_settings).body),
                );
            });
            line = line.child(input);
        } else if let Some(url) = &explicit {
            line = line.child(div().truncate().text_color(theme.fg).child(url.clone()));
        } else {
            line = line.child(
                div()
                    .truncate()
                    .text_color(theme.muted)
                    .child(format!("Default · {source}")),
            );
        }

        let field = div()
            .id("pilus-update-feed")
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .h(px(28.0))
            .px_2()
            .radius_md()
            .bg(theme.panel_dim)
            .border_1()
            .border_color(if editing {
                theme.border_focus
            } else {
                theme.border
            })
            .cursor_pointer()
            .child(line)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.edit_update_feed(cx)),
            );

        let feed_action = if editing {
            div().child(
                style::button(
                    "pilus-update-feed-save",
                    "Save",
                    style::ButtonKind::Primary,
                    theme,
                    self.section_font(),
                )
                .flex_shrink_0()
                .on_click(cx.listener(|this, _, _, cx| this.commit_update_feed(cx))),
            )
        } else if explicit.is_some() {
            div().child(
                style::button(
                    "pilus-update-feed-reset",
                    "Use Default",
                    style::ButtonKind::Secondary,
                    theme,
                    self.section_font(),
                )
                .flex_shrink_0()
                .on_click(cx.listener(|this, _, _, cx| this.reset_update_feed(cx))),
            )
        } else {
            div()
        };

        let auto_check = self.settings.auto_check_updates;
        let checkbox = style::checkbox(
            "pilus-update-auto-check",
            auto_check,
            "Check for updates on launch",
            theme,
            self.footer_font(),
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_auto_check(cx)));

        let pinned = crate::update::pinned_channel();
        let preview = pinned.unwrap_or_else(|| UpdateChannel::parse(&self.settings.update_channel))
            == UpdateChannel::Preview;
        let label = |text: &'static str| {
            div()
                .text_size(px(self.section_font()))
                .text_color(theme.muted)
                .child(text)
        };

        div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .px_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(label("Update channel"))
                        .child(match pinned {
                            // A Nightly build can only follow its own feed.
                            Some(channel) => label(channel.label()).into_any_element(),
                            None => style::segmented(theme)
                                .child(self.update_channel_segment(UpdateChannel::Stable, cx))
                                .child(self.update_channel_segment(UpdateChannel::Preview, cx))
                                .into_any_element(),
                        }),
                )
                .when(preview, |card| {
                    card.child(style::enter(
                        "pilus-update-feed-override",
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label("Feed URL override"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(field)
                                    .child(feed_action),
                            ),
                    ))
                })
                .child(checkbox)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .text_size(px(self.section_font()))
                                .child(div().text_color(theme.muted).child("Version"))
                                .child(
                                    div()
                                        .text_color(theme.fg)
                                        .child(crate::update::current_version().to_string()),
                                ),
                        )
                        .child(
                            style::small_button(
                                "pilus-update-check-now",
                                "Check for Updates",
                                style::ButtonKind::Secondary,
                                theme,
                                self.section_font(),
                            )
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.check_for_updates(cx);
                            })),
                        ),
                ),
        )
    }

    /// The ABOUT LUVUS section: the installed daemon version, a release check,
    /// and the restart that loads a newly installed binary. Update Luvus only
    /// appears once a check has found a newer release.
    fn about_luvus_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        // `pilus` checks the public manifest and installs the daemon archive
        // itself; only the restart goes through the CLI, which knows the
        // selected session. A restart is two-click because it stops every pane.
        let daemon_busy = self.daemon_busy;
        let mut check_daemon = style::small_button(
            "pilus-check-daemon",
            "Check for Updates",
            style::ButtonKind::Secondary,
            theme,
            self.section_font(),
        )
        .flex_shrink_0();
        if daemon_busy {
            check_daemon = check_daemon.text_color(theme.muted);
        }
        let check_daemon = check_daemon.on_click(cx.listener(|this, _, _, cx| {
            this.check_luvus_daemon(cx);
        }));
        let mut update_daemon = style::small_button(
            "pilus-update-daemon",
            "Update Luvus",
            style::ButtonKind::Primary,
            theme,
            self.section_font(),
        )
        .flex_shrink_0();
        if daemon_busy {
            update_daemon = update_daemon.text_color(theme.muted);
        }
        let update_daemon = update_daemon.on_click(cx.listener(|this, _, _, cx| {
            this.update_luvus_daemon(cx);
        }));
        let restart_pending = self.daemon_restart_pending;
        let mut restart_daemon = style::small_button(
            "pilus-restart-daemon",
            if restart_pending {
                "Confirm Restart"
            } else {
                "Restart Luvus"
            },
            style::ButtonKind::Secondary,
            theme,
            self.section_font(),
        )
        .flex_shrink_0()
        .text_color(if restart_pending {
            theme.alert
        } else {
            theme.muted
        });
        if daemon_busy {
            restart_daemon = restart_daemon.text_color(theme.muted);
        }
        let restart_daemon = restart_daemon.on_click(cx.listener(|this, _, _, cx| {
            if this.daemon_busy {
                return;
            }
            if this.daemon_restart_pending {
                this.restart_luvus_daemon(cx);
            } else {
                this.daemon_restart_pending = true;
                this.daemon_status =
                    "Restarting Luvus stops every pane in this session. Click Confirm Restart."
                        .to_string();
                cx.notify();
            }
        }));
        let daemon_status = self.daemon_status.clone();
        let installed = self
            .daemon_installed
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| self.luvus_local_version.clone());
        let version = installed.unwrap_or_else(|| "not found".to_string());
        let update_available = self.daemon_latest.clone();

        div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .px_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(self.section_font()))
                                .text_color(theme.muted)
                                .child("Version"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .text_size(px(self.section_font()))
                                .child(div().text_color(theme.fg).child(version))
                                .children(update_available.as_ref().map(|latest| {
                                    div()
                                        .text_color(theme.accent)
                                        .child(format!("{latest} available"))
                                })),
                        ),
                )
                .children((!daemon_status.is_empty()).then(|| {
                    div()
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(daemon_status)
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .child(restart_daemon)
                        .child(check_daemon)
                        .when(update_available.is_some(), |row| row.child(update_daemon)),
                ),
        )
    }

    /// The IROH IDENTITY section: who this machine is on the iroh network.
    ///
    /// The endpoint ID is the connector's stable key, which every paired device
    /// pins. The ticket wraps it with how to reach it, for a device that
    /// already trusts this host; a new device pairs with a code instead.
    fn iroh_identity_section_block(
        &self,
        status: &pilus_client::agent_service::ServiceStatus,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let mut row = |title: &'static str, blurb: &'static str, value: &str, id: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .text_color(theme.fg)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(blurb),
                )
                .child(self.copy_value_row(title, value.to_string(), id, cx))
        };
        let endpoint = row(
            "Endpoint ID",
            "This machine's permanent address on the iroh network. Paired devices pin it, so it stays the same across restarts.",
            &status.endpoint_id,
            "pilus-copy-endpoint",
        );
        let ticket = row(
            "Ticket",
            "The endpoint ID plus how to reach it right now (relay and direct addresses). It lets a device that already trusts this machine reconnect; to pair a new device, use Add device.",
            &status.ticket,
            "pilus-copy-ticket",
        );
        div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .px_3()
                .child(endpoint)
                .child(ticket),
        )
    }

    /// The IROH RELAY section: where the host connector dials out, and the project
    /// secret that lets it use a managed relay. Both live in `settings.json`
    /// (the agent reads them there), and the agent only reads them at start.
    fn relay_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let relay = &self.settings.relay;
        let configured = !relay.url.is_empty() || relay.secret.is_some();
        let url_value = relay.url.join(", ");
        let secret_set = relay.secret.is_some();

        let mut section = div().flex().flex_col().gap_3();
        let url_row = self.relay_field_row(
            RelayField::Url,
            "Relay URLs",
            (!url_value.is_empty()).then_some(url_value),
            "n0 public relays",
            cx,
        );
        let secret_row = self.relay_field_row(
            RelayField::Secret,
            "API secret",
            secret_set.then(|| "\u{2022}".repeat(16)),
            "Not set",
            cx,
        );
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .px_3()
            .child(url_row)
            .child(secret_row)
            .child(
                div()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child(
                        "A dedicated relay from services.iroh.computer needs both its URL and the project API secret. A secret alone keeps the public relays. Remote access reads these when it starts.",
                    ),
            );
        if !self.relay_status.is_empty() {
            let failed = self.relay_status.starts_with("Not saved");
            card = card.child(
                div()
                    .text_size(px(self.footer_font()))
                    .text_color(if failed { theme.alert } else { theme.muted })
                    .child(self.relay_status.clone()),
            );
        }
        if configured && self.relay_editing.is_none() {
            card = card.child(
                div().flex().flex_row().items_center().justify_end().child(
                    style::small_button(
                        "pilus-relay-clear",
                        "Reset to Public Relays",
                        style::ButtonKind::Primary,
                        theme,
                        self.section_font(),
                    )
                    .flex_shrink_0()
                    .on_click(cx.listener(|this, _, _, cx| this.clear_relay(cx))),
                ),
            );
        }
        section = section.child(card);
        section
    }

    /// One relay field: the boxed value (or the live input while it is being
    /// edited) with a Save button beside it.
    fn relay_field_row(
        &self,
        which: RelayField,
        label: &str,
        saved: Option<String>,
        empty: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let editing = self.relay_editing == Some(which);
        let (tag, input) = match which {
            RelayField::Url => ("url", self.relay_url_input.clone()),
            RelayField::Secret => ("secret", self.relay_secret_input.clone()),
        };
        let body = if editing {
            input.update(cx, |input, _| {
                // The saved value beside it inherits the body size, so the
                // live input matches or the text jumps when focus moves.
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(style::Fonts::resolve(&self.shared_settings).body),
                );
            });
            div().flex_1().min_w(px(0.0)).child(input)
        } else {
            match saved {
                Some(value) => div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(theme.fg)
                    .child(value),
                None => div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(theme.muted)
                    .child(empty.to_string()),
            }
        };
        let field = style::field(
            format!("pilus-relay-{tag}"),
            label,
            editing,
            theme,
            28.0,
            self.section_font(),
            body,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.edit_relay(which, cx)),
        );
        let mut row = div()
            .flex()
            .flex_row()
            .items_end()
            .gap_2()
            .child(div().flex_1().min_w(px(0.0)).child(field));
        if editing {
            row = row.child(
                style::button(
                    format!("pilus-relay-{tag}-save"),
                    "Save",
                    style::ButtonKind::Primary,
                    theme,
                    self.section_font(),
                )
                .flex_shrink_0()
                .on_click(cx.listener(|this, _, _, cx| this.commit_relay(cx))),
            );
        }
        row
    }

    /// Seed `which` from the saved settings and give it the keyboard. Moving
    /// from one relay field to the other keeps what was typed in the first.
    fn edit_relay(&mut self, which: RelayField, cx: &mut Context<Self>) {
        if self.relay_editing == Some(which) {
            return;
        }
        if self.relay_editing.is_some() {
            self.commit_relay(cx);
            if self.relay_editing.is_some() {
                // The first field did not validate; stay on it.
                return;
            }
        }
        match which {
            RelayField::Url => self.relay_url.set_value(self.settings.relay.url.join(", ")),
            RelayField::Secret => self
                .relay_secret
                .set_value(self.settings.relay.secret.clone().unwrap_or_default()),
        }
        self.relay_status.clear();
        self.relay_editing = Some(which);
        cx.notify();
    }

    /// Validate and save the field being edited. A blank value unsets it.
    fn commit_relay(&mut self, cx: &mut Context<Self>) {
        let Some(which) = self.relay_editing else {
            return;
        };
        match which {
            RelayField::Url => {
                match pilus_client::RelaySettings::parse_urls(&self.relay_url.value()) {
                    Ok(urls) => self.settings.relay.url = urls,
                    Err(error) => {
                        self.relay_status = format!("Not saved: {error}");
                        cx.notify();
                        return;
                    }
                }
            }
            RelayField::Secret => {
                let secret = self.relay_secret.value().trim().to_string();
                self.settings.relay.secret = (!secret.is_empty()).then_some(secret);
            }
        }
        self.relay_editing = None;
        self.save_relay_settings(cx);
    }

    /// Drop the relay block so the connector uses the n0 public relays again.
    fn clear_relay(&mut self, cx: &mut Context<Self>) {
        self.relay_editing = None;
        self.settings.relay = pilus_client::RelaySettings::default();
        self.save_relay_settings(cx);
    }

    /// Persist the relay block and restart Remote access if it is running, so
    /// the agent rebinds through the new relay.
    fn save_relay_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        if self.remote.on() {
            connector::restart_service();
            self.relay_status = "Saved. Remote access is restarting to use it.".to_string();
        } else {
            self.relay_status =
                "Saved. Remote access will use it the next time it starts.".to_string();
        }
        cx.notify();
    }

    /// One segment of the channel control.
    fn update_channel_segment(
        &self,
        value: UpdateChannel,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active = UpdateChannel::parse(&self.settings.update_channel) == value;
        style::segment(
            format!("pilus-update-channel-{}", value.label()),
            value.label(),
            active,
            true,
            self.theme,
            self.section_font(),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_update_channel(value, cx);
        }))
    }

    /// The Open Workspace picker overlay.
    fn picker_overlay(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.picker.is_open() {
            return div().into_any_element();
        }
        // Fit the panel inside the window so a short or narrow window never
        // clips the footer buttons.
        let viewport = window.viewport_size();
        let panel_w = (f32::from(viewport.width) - 32.0).clamp(320.0, 760.0);
        // Fill the window below the header so the footer buttons sit at the
        // bottom; clamp so a tiny window still shows a usable panel.
        let panel_h = (f32::from(viewport.height) - HEADER_HEIGHT - 36.0).clamp(220.0, 900.0);
        let theme = self.theme;
        let cwd = self.picker.cwd().to_string();
        let editing = self.picker.editing();
        let picker_path = self.picker_path.clone();
        picker_path.update(cx, |input, _| {
            let fonts = style::Fonts::resolve(&self.shared_settings);
            input.set_style(
                self.theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let line = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .child(if editing {
                picker_path.into_any_element()
            } else {
                div()
                    .text_color(theme.fg)
                    .child(cwd.clone())
                    .into_any_element()
            });
        let path_field = div()
            .id("pilus-picker-path")
            .flex()
            .items_center()
            .h(px(38.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .child(line)
            .on_click(cx.listener(|this, _, _, cx| {
                this.picker.begin_edit();
                cx.notify();
            }));

        let source_label = self.picker_source_label();
        let menu_button = div()
            .id("pilus-picker-endpoint")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(26.0))
            .px_2()
            .radius_md()
            .text_size(px(self.footer_font()))
            .text_color(theme.fg)
            .cursor_pointer()
            .state_fill(theme.panel, theme.border, style::RADIUS_MD)
            .child(icon(
                include_bytes!("../../assets/icons/monitor.svg"),
                13.0,
                theme.muted,
            ))
            .child(source_label)
            .child(icon(
                include_bytes!("../../assets/icons/chevron-down.svg"),
                11.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                let choices = this.picker_menu_choices();
                this.picker.toggle_menu(choices);
                cx.notify();
            }));
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(42.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .child(style::title(
                theme,
                "Open Workspace",
                style::Fonts::resolve(&self.shared_settings).title,
            ))
            .child(menu_button);
        let menu = if self.picker.menu_open() {
            let choices = self.picker.menu_choices();
            let highlighted = self.picker.menu_selected();
            let mut list = div().flex().flex_col().gap_1().p_1();
            for (index, choice) in choices.iter().enumerate() {
                let active = index == highlighted;
                let (status_text, status_color) = match choice.online {
                    Some(true) => ("online", theme.ok),
                    Some(false) => ("offline", theme.alert),
                    None => ("…", theme.muted),
                };
                list = list.child(
                    div()
                        .id(format!("pilus-picker-endpoint-{index}"))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(28.0))
                        .px_2()
                        .radius_md()
                        .text_size(px(self.chrome_font()))
                        .text_color(if active {
                            theme.selection_fg
                        } else {
                            theme.panel_fg
                        })
                        .cursor_pointer()
                        .state_fill(
                            if active {
                                theme.selection_bg
                            } else {
                                theme.sidebar_bg
                            },
                            theme.panel,
                            style::RADIUS_MD,
                        )
                        .child(icon(
                            include_bytes!("../../assets/icons/circle.svg"),
                            8.0,
                            status_color,
                        ))
                        .child(choice.label.clone())
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(self.section_font()))
                                .text_color(if active {
                                    theme.selection_fg.opacity(0.75)
                                } else {
                                    theme.muted
                                })
                                .child(status_text),
                        )
                        .child(if choice.current {
                            div()
                                .text_size(px(self.section_font()))
                                .text_color(theme.muted)
                                .child("current")
                        } else {
                            div()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(source) = this.picker.choose_menu(index) {
                                this.picker.rebase(source, "~".to_string());
                                this.list_picker(cx);
                                this.picker_scroll.scroll_to_item(0);
                            }
                            cx.notify();
                        })),
                );
            }
            style::enter(
                "pilus-picker-endpoint-menu-enter",
                div()
                    .absolute()
                    .occlude()
                    .top(px(44.0))
                    .right(px(8.0))
                    .w(px(240.0))
                    .radius_md()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .child(list),
            )
            .into_any_element()
        } else {
            div().into_any_element()
        };

        let entries = self.picker.entries();
        let selected = self.picker.selected();
        let mut rows = div()
            .id("pilus-picker-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_1()
            .p_1()
            .overflow_y_scroll()
            .track_scroll(&self.picker_scroll);
        if let Some(error) = self.picker.error() {
            rows = rows.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.alert)
                    .child(error.to_string()),
            );
        }
        if entries.is_empty() && self.picker.loading() {
            rows = rows.child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("Loading…"),
            );
        }
        for (index, entry) in entries.iter().enumerate() {
            let active = index == selected;
            let name = entry.name.clone();
            let row_icon: &[u8] = if entry.is_home {
                include_bytes!("../../assets/icons/house.svg")
            } else if entry.is_parent {
                include_bytes!("../../assets/icons/arrow-up.svg")
            } else {
                include_bytes!("../../assets/icons/folder.svg")
            };
            let (bg, fg) = if active {
                (theme.selection_bg, theme.selection_fg)
            } else {
                (theme.sidebar_bg, theme.panel_fg)
            };
            rows = rows.child(
                div()
                    .id(format!("pilus-picker-entry-{index}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.0))
                    .px_2()
                    .radius_md()
                    .text_color(fg)
                    .cursor_pointer()
                    .text_size(px(self.chrome_font()))
                    .state_fill(bg, theme.panel, style::RADIUS_MD)
                    .child(icon(
                        row_icon,
                        14.0,
                        if active {
                            theme.selection_fg
                        } else {
                            theme.muted
                        },
                    ))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.picker.select(index);
                        if this.picker.enter().is_some() {
                            this.list_picker(cx);
                        }
                        cx.notify();
                    })),
            );
        }

        let hidden = self.picker.show_hidden();
        let hidden_toggle = style::checkbox(
            "pilus-picker-hidden",
            hidden,
            "Hidden",
            theme,
            self.footer_font(),
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.picker.toggle_hidden();
            cx.notify();
        }));
        let open = style::button(
            "pilus-picker-open",
            "Open this folder",
            style::ButtonKind::Primary,
            theme,
            self.footer_font(),
        )
        .flex_shrink_0()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.open_workspace_path(cwd.clone(), cx);
        }));
        let worktree = style::button(
            "pilus-picker-worktree",
            "Open with new worktree",
            style::ButtonKind::Secondary,
            theme,
            self.footer_font(),
        )
        .flex_shrink_0()
        .on_click(cx.listener(|this, _, _, cx| {
            this.open_new_worktree(cx);
        }));
        let mut footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(42.0))
            .px_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .child(hidden_toggle)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child("↑↓ · ↵ open · ← up · → back · ⇧. hidden"),
            );
        footer = footer.child(worktree).child(open);

        style::enter(
            "pilus-picker-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .occlude()
                .bg(theme.window_bg.opacity(0.55))
                .flex()
                .justify_center()
                .pt(px(HEADER_HEIGHT + 12.0))
                .child(
                    div()
                        .w(px(panel_w))
                        .h(px(panel_h))
                        .relative()
                        .flex()
                        .flex_col()
                        .radius_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(header)
                        .child(path_field)
                        .child(rows)
                        .child(footer)
                        .child(menu)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_picker();
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// This client's session name: `default` when no session is named.
    fn current_session_name(&self) -> &str {
        self.session_name.as_deref().unwrap_or("default")
    }

    /// The control socket matching the current session under our state root.
    fn api_socket(&self) -> PathBuf {
        api_socket_for(&self.session_root, self.session_name.as_deref())
    }

    /// How to reach the daemon the window is showing: the session's socket
    /// locally, SSH to the machine otherwise. A machine with no SSH route
    /// falls back to the local daemon rather than to nothing.
    fn control_transport(&self) -> ControlTransport {
        if let EndpointId::Machine(id) = &self.active
            && let Some(target) = self
                .machines
                .iter()
                .find(|machine| &machine.id == id)
                .and_then(MachineInfo::ssh_target)
        {
            return ControlTransport::Ssh(target);
        }
        ControlTransport::Socket(self.api_socket())
    }

    /// Attach to another session under the same state root.
    ///
    /// A failed attach keeps the old connection and reports why in the
    /// status bar, so a stopped session can't strand the window.
    fn switch_session(&mut self, name: &str) {
        let session = (name != "default").then(|| name.to_string());
        let dir = match &session {
            Some(named) => self.session_root.join("sessions").join(named),
            None => self.session_root.clone(),
        };
        // Only a successful attach moves the window: every feed, poll, and
        // control call derives its socket from `session_name`, so changing it
        // first would point them at a session the surface is not showing.
        let target = ConnectTarget::socket(dir.join("luvus-client.sock"));
        match pilus_client::connect(&target, self.cols, self.rows) {
            Ok(connection) => {
                self.session_name = session;
                self.target = target;
                self.forget_session_scoped_state();
                let _ = connection.request_workspace_dock();
                self.connection = Some(connection);
                // Machine profiles are owner-local to a named session, so a
                // session switch drops every remote endpoint and its SSH link.
                self.endpoints.clear();
                self.connecting.clear();
                self.wanted = None;
                self.pending_switch = None;
                self.active = EndpointId::Local;
                self.active_machine = None;
                self.surface = Surface::new();
                self.last_sidebar_dock = None;
                self.selection = None;
                self.hover_cell = None;
                self.press_anchor = None;
                self.dragging = false;
                self.sidebar_layout = None;
                self.sidebar_revision = 0;
                self.bar_widgets = Vec::new();
                self.tabs = Vec::new();
                self.tabs_cache.clear();
                self.tabs_pick = None;
                self.tab_sel = None;
                self.endpoint_sel = None;
                self.agent_sel = None;
                self.close_picker();
                self.agents = Vec::new();
                self.agents_loaded = false;
                self.automations = AutomationSummary::default();
                self.tasks = TaskSummary::default();
                self.agent_feed = Some(AgentFeed::spawn(self.api_socket()));
                self.sessions = list_session_infos(&self.session_root);
                self.machines.clear();
                self.auto_connected.clear();
                self.reconnects.clear();
                self.local_reconnect = None;
                self.active_live = true;
                self.catalog_dirty = true;
                // The new session's catalog loads off-thread; the poll
                // auto-connects it once it lands.
                self.reload_machines();
                self.status = format!("connecting to {name}…");
                self.restore_endpoint = None;
                self.remember_location();
            }
            Err(error) => {
                self.toast(
                    ToastKind::Error,
                    format!("couldn't attach to {name}: {error}"),
                );
            }
        }
    }

    /// Drop what was learned about the session the window is leaving.
    ///
    /// Pane and workspace ids restart in every daemon, so a cache keyed by
    /// one would answer for the wrong session: a remembered linked-worktree
    /// path would offer to delete it.
    fn forget_session_scoped_state(&mut self) {
        self.worktree_linked.clear();
        self.worktree_probing.clear();
        // Pane ids restart too, so attention would compare unrelated panes.
        self.attention.forget_all();
        self.home_pending_open = None;
    }

    /// Create a fresh session and attach to it once its socket appears.
    ///
    /// The name is generated docker-style; creation runs off the GUI thread
    /// and reports back through the session channel the poll loop drains.
    fn start_new_session(&mut self) {
        let existing: Vec<String> = self.sessions.iter().map(|info| info.name.clone()).collect();
        let name = generate_session_name(&existing);
        self.close_session_menu();
        self.start_session(name);
    }

    /// Start `name`'s daemon off the GUI thread and attach once it is up.
    ///
    /// Attaching straight to a stopped session would start it inside
    /// `pilus_client::connect`, on this thread, for as long as the daemon takes.
    fn start_session(&mut self, name: String) {
        if !self.session_working.insert(name.clone()) {
            return;
        }
        self.status = format!("starting {name}…");
        let root = self.session_root.clone();
        let tx = self.session_tx.clone();
        std::thread::spawn(move || {
            tx.send(start_session_blocking(&root, &name)).ok();
        });
    }

    /// Stop or delete `name` off the GUI thread.
    ///
    /// Stopping the session the window is attached to first moves the window
    /// to another running one: a dropped local daemon is restarted by the
    /// reconnect loop, which would undo the stop.
    fn run_session_verb(&mut self, name: &str, verb: SessionVerb) {
        self.session_confirm = None;
        if !self.session_working.insert(name.to_string()) {
            return;
        }
        if verb == SessionVerb::Stop && name == self.current_session_name() {
            match session_ops::fallback(&self.sessions, name) {
                Some(next) => self.switch_session(&next),
                None => {
                    self.session_working.remove(name);
                    self.toast(
                        ToastKind::Info,
                        format!("{name} is the only running session"),
                    );
                    return;
                }
            }
            // A failed attach leaves the window where it was; stopping from
            // here would strand it on a dead session.
            if self.current_session_name() == name {
                self.session_working.remove(name);
                return;
            }
        }
        let root = self.session_root.clone();
        let name = name.to_string();
        let tx = self.session_tx.clone();
        std::thread::spawn(move || {
            let result = match verb {
                SessionVerb::Stop => pilus_client::stop_session(&root, &name),
                SessionVerb::Delete => pilus_client::delete_session(&root, &name),
            };
            tx.send(match result {
                Ok(()) => SessionOpResult::Done(name, verb),
                Err(error) => SessionOpResult::Failed(name, verb, error.to_string()),
            })
            .ok();
        });
    }

    /// Close the session menu and forget a half-confirmed stop or delete.
    fn close_session_menu(&mut self) {
        self.session_menu_open = false;
        self.session_confirm = None;
        self.session_icon_editing = None;
    }

    /// Check the public Luvus release manifest off the GUI thread.
    fn check_luvus_daemon(&mut self, cx: &mut Context<Self>) {
        if self.daemon_busy {
            return;
        }
        self.daemon_busy = true;
        self.daemon_restart_pending = false;
        self.daemon_status = "Checking Luvus releases…".to_string();
        let tx = self.daemon_tx.clone();
        std::thread::spawn(move || {
            let engine = crate::luvus_update::engine();
            let installed = crate::luvus_update::installed_version();
            let result = crate::luvus_update::check(&engine).map_err(|error| error.to_string());
            tx.send(DaemonOutcome::Checked { installed, result }).ok();
        });
        cx.notify();
    }

    /// Download, verify, and install the latest Luvus release off the GUI
    /// thread. The running daemon keeps its current binary until a restart.
    fn update_luvus_daemon(&mut self, cx: &mut Context<Self>) {
        if self.daemon_busy {
            return;
        }
        self.daemon_busy = true;
        self.daemon_restart_pending = false;
        self.daemon_status = "Updating Luvus…".to_string();
        let tx = self.daemon_tx.clone();
        std::thread::spawn(move || {
            let engine = crate::luvus_update::engine();
            let outcome = match crate::luvus_update::check(&engine) {
                Ok(Some(release)) => {
                    let version = release.version.clone();
                    match crate::luvus_update::install(&engine, &release, |_, _| {}) {
                        Ok(path) => DaemonOutcome::Installed { version, path },
                        Err(error) => {
                            DaemonOutcome::Message(Err(format!("install failed: {error}")))
                        }
                    }
                }
                Ok(None) => DaemonOutcome::Message(Ok("Luvus is already up to date.".to_string())),
                Err(error) => DaemonOutcome::Message(Err(format!("update failed: {error}"))),
            };
            tx.send(outcome).ok();
        });
        cx.notify();
    }

    /// Restart the local daemon so it loads a newly installed binary.
    ///
    /// The caller confirms first: a restart stops every pane in the session,
    /// so it is never a side effect of the update click.
    fn restart_luvus_daemon(&mut self, cx: &mut Context<Self>) {
        if self.daemon_busy {
            return;
        }
        self.daemon_busy = true;
        self.daemon_restart_pending = false;
        self.daemon_status = "Restarting Luvus…".to_string();
        let root = self.session_root.clone();
        let session = self.session_name.clone();
        let tx = self.daemon_tx.clone();
        std::thread::spawn(move || {
            tx.send(run_luvus_command(
                &root,
                session.as_deref(),
                &["server", "restart"],
            ))
            .ok();
        });
        cx.notify();
    }

    /// Manual check from the menu bar or Settings: run the check and
    /// foreground the Update window so the result is visible immediately.
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.updater.update(cx, |updater, cx| updater.check(cx));
        update_window::show(
            self.updater.clone(),
            self.theme,
            self.shared_settings.clone(),
            cx,
        );
    }

    /// Persist update preferences and rebuild the updater engine in place.
    fn save_update_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        let request = UpdateRequest::from_settings(
            &self.settings.update_channel,
            self.settings.update_feed_url.clone(),
        );
        self.updater
            .update(cx, |updater, cx| updater.reconfigure(&request, cx));
        cx.notify();
    }

    /// Switch the update channel from the Settings page.
    fn set_update_channel(&mut self, channel: UpdateChannel, cx: &mut Context<Self>) {
        self.settings.update_channel = channel.label().to_string();
        // The override field only exists on preview, so leaving it must not
        // keep redirecting stable checks to a feed nobody can see.
        if channel == UpdateChannel::Stable {
            self.settings.update_feed_url = None;
            self.update_url_editing = false;
        }
        self.save_update_settings(cx);
    }

    /// The MENU BAR section: whether the agent list lives in the menu bar.
    fn menu_bar_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let on = self.settings.menu_bar;
        div().flex().flex_col().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .pb_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(self.footer_font()))
                        .text_color(theme.muted)
                        .child(
                            "Lists your agents in the menu bar, with a dot for each one's status. Choosing one brings it into focus. Quitting pilus leaves the menu bar running; choose Quit pilus from it to exit fully.",
                        ),
                )
                .child(
                    style::toggle("pilus-menu-bar-toggle", on, theme).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.settings.menu_bar = !this.settings.menu_bar;
                            this.save_settings();
                            cx.notify();
                        },
                    )),
                ),
        )
    }

    /// Mirror the agents into the menu bar and act on its clicks.
    fn sync_menubar(&mut self, cx: &mut Context<Self>) {
        let theme = self.theme;
        let rows: Vec<menubar::Row> = self
            .connected_endpoints()
            .into_iter()
            .flat_map(|id| {
                let key = id.key();
                let group = self.endpoint_label(&id);
                self.endpoint_agents(&id)
                    .iter()
                    .map(|agent| {
                        let path = self.shorten_cwd(&key, &agent.cwd);
                        agent_menu_row(&key, &group, agent, path, theme)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let pinned = pinned::menu_rows(&self.settings.pinned_workspaces, |key| {
            self.endpoint_label(&self.endpoint_by_key(key))
        });
        self.menubar.sync(
            self.settings.menu_bar,
            &rows,
            &pinned,
            self.settings.energy_mode,
        );
        for pick in self.menubar.drain() {
            match pick {
                menubar::Pick::Quit => {
                    background::quit_fully(cx);
                    return;
                }
                menubar::Pick::Show => self.raise_window(cx),
                menubar::Pick::About => about::open(self.theme, self.shared_settings.clone(), cx),
                menubar::Pick::CheckForUpdates => self.check_for_updates(cx),
                menubar::Pick::Energy => self.toggle_energy_mode(cx),
                menubar::Pick::Agent { endpoint, pane } => {
                    self.raise_window(cx);
                    self.menu_pick = Some((endpoint, pane, Instant::now()));
                }
                menubar::Pick::Pinned { endpoint, path } => {
                    self.raise_window(cx);
                    self.saved_pick = Some((
                        self.settings.saved_workspace(&endpoint, &path),
                        Instant::now(),
                    ));
                }
            }
        }
        self.apply_menu_pick(cx);
        self.apply_saved_pick(cx);
    }

    /// Bring this window in front of whatever app was active.
    fn raise_window(&self, cx: &mut Context<Self>) {
        cx.activate(true);
        let window = self.window_handle;
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        });
    }

    /// Open the pinned folder a menu bar click chose. A folder on a saved
    /// machine waits for the catalog, which a window that has just opened has
    /// not read yet, since `open_saved_workspace` refuses an unknown machine;
    /// past [`MENU_PICK_PATIENCE`] it runs anyway so the refusal is shown.
    fn apply_saved_pick(&mut self, cx: &mut Context<Self>) {
        let Some((saved, picked)) = self.saved_pick.clone() else {
            return;
        };
        let known = match self.endpoint_by_key(&saved.endpoint) {
            EndpointId::Local => true,
            EndpointId::Machine(id) => self.machines.iter().any(|machine| machine.id == id),
        };
        if known || picked.elapsed() > MENU_PICK_PATIENCE {
            self.saved_pick = None;
            self.open_saved_workspace(saved, cx);
        }
    }

    /// Focus the agent a menu bar click chose, once its endpoint is ready: a
    /// window that has just opened is still attaching to its daemon, and
    /// focusing before then would only start the reconnect.
    fn apply_menu_pick(&mut self, cx: &mut Context<Self>) {
        let Some((endpoint, pane, picked)) = self.menu_pick.clone() else {
            return;
        };
        let ready = self
            .connected_endpoints()
            .into_iter()
            .find(|id| id.key() == endpoint)
            .filter(|id| *id != self.active || self.active_live);
        if let Some(endpoint) = ready {
            self.menu_pick = None;
            self.leave_settings_page();
            self.activate_endpoint(endpoint, None, Some(pane), cx);
        } else if picked.elapsed() > MENU_PICK_PATIENCE {
            self.menu_pick = None;
        }
    }

    /// Toggle the launch-time auto-check from the Settings page.
    fn toggle_auto_check(&mut self, cx: &mut Context<Self>) {
        self.settings.auto_check_updates = !self.settings.auto_check_updates;
        self.save_update_settings(cx);
    }

    /// Seed the feed-URL input and give it the keyboard.
    fn edit_update_feed(&mut self, cx: &mut Context<Self>) {
        self.update_feed
            .set_value(self.settings.update_feed_url.clone().unwrap_or_default());
        self.update_url_editing = true;
        cx.notify();
    }

    /// Commit the feed-URL input; blank falls back to the channel default.
    fn commit_update_feed(&mut self, cx: &mut Context<Self>) {
        let value = self.update_feed.value().trim().to_string();
        self.settings.update_feed_url = (!value.is_empty()).then_some(value);
        self.update_url_editing = false;
        self.save_update_settings(cx);
    }

    /// Drop the explicit feed URL so the channel default applies again.
    fn reset_update_feed(&mut self, cx: &mut Context<Self>) {
        self.update_url_editing = false;
        self.settings.update_feed_url = None;
        self.save_update_settings(cx);
    }

    /// Send our sidebar layout to the daemon with a fresh revision.
    fn push_sidebar_layout(&mut self) {
        if self.sidebar_layout.is_none() {
            return;
        }
        let max_revision = self
            .endpoints
            .values()
            .filter_map(|endpoint| endpoint.surface.shell_sidebars())
            .map(|sidebars| sidebars.revision)
            .fold(
                self.surface
                    .shell_sidebars()
                    .map(|sidebars| sidebars.revision)
                    .unwrap_or(0)
                    .max(self.sidebar_revision),
                u64::max,
            );
        self.sidebar_revision = max_revision.saturating_add(1);
        let Some(message) = self.sidebar_message() else {
            return;
        };
        self.send_active(message.clone());
        for endpoint in self.endpoints.values() {
            if let Some(connection) = endpoint.connection.as_ref() {
                let _ = connection.send(message.clone());
            }
        }
    }

    fn sidebar_message(&self) -> Option<ClientMessage> {
        Some(ClientMessage::ShellSidebars(ShellSidebars {
            revision: self.sidebar_revision,
            layout: self.sidebar_layout.clone()?,
            workspace_paths: self.sidebar_workspace_paths,
        }))
    }

    /// Collapse or expand the native sidebar, reclaiming the columns for panes.
    /// The daemon hears the handoff immediately (optimistic final state) and
    /// tracks the flight with throttled resizes, so it has converged by the
    /// time the panel lands and nothing pops at settle.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        // Capture the current openness before flipping the flag: with no
        // tween in flight `sidebar_factor` reads `sidebar_visible`, so
        // sampling after the flip would pin `from` to the new target and
        // the tween would run from `to` to `to` (no motion at all).
        let from = self.sidebar_factor();
        // A toggle mid-flight keeps the snapshot it already holds: that is
        // the grid the user last saw settled.
        if self.sidebar_freeze.is_none() {
            let skip = self.skipped_cols();
            self.sidebar_freeze = self
                .surface
                .frame()
                .cloned()
                .map(|frame| (frame, skip, Instant::now()));
        }
        self.sidebar_visible = !self.sidebar_visible;
        if let Some(layout) = self.sidebar_layout.as_mut() {
            layout.left.visible = self.sidebar_visible;
        }
        self.push_sidebar_layout();
        let to = if self.sidebar_visible { 1.0 } else { 0.0 };
        // Reduced motion lands the panel at once; the freeze above is
        // released by the same path as after a slide.
        self.sidebar_anim = (!cx.reduce_motion()).then(|| SidebarAnim {
            started: Instant::now(),
            from,
            to,
        });
        cx.notify();
    }

    /// Advance the cursor's blink; true when its opacity moved and the grid
    /// needs a repaint. The setting being off, a window in the background or
    /// reduced motion keeps the cursor solid, like the other tweens.
    fn tick_cursor_blink(&mut self, cx: &App) -> bool {
        let cursor = self
            .shown_frame()
            .filter(|frame| frame.cursor_visible)
            .and_then(|frame| frame.cursor);
        let blinking = self.settings.cursor_blink && self.window_active && !cx.reduce_motion();
        self.cursor_blink.tick(Instant::now(), cursor, blinking)
    }

    /// Advance the pane toolbar's fade; true when its opacity moved. It clears
    /// while either sidebar slides and returns once both have landed.
    fn tick_toolbar_fade(&mut self, cx: &App) -> bool {
        let sliding = self.sidebar_anim.is_some() || self.right.tweening();
        self.toolbar_fade
            .tick(Instant::now(), sliding, !cx.reduce_motion())
    }

    /// Advance the sidebar tween; true once it settles at its target.
    fn tick_sidebar_anim(&mut self) -> bool {
        let settled = match &self.sidebar_anim {
            Some(anim) => anim.started.elapsed() >= SIDEBAR_ANIM_DURATION,
            None => true,
        };
        if settled {
            self.sidebar_anim = None;
        }
        settled
    }

    /// Sidebar openness, easing toward its target while a tween flies so the
    /// panel, tab strip, and terminal glide together.
    fn sidebar_factor(&self) -> f32 {
        match &self.sidebar_anim {
            Some(anim) => {
                let elapsed = anim.started.elapsed().as_secs_f32();
                let total = SIDEBAR_ANIM_DURATION.as_secs_f32();
                let step = (elapsed / total).clamp(0.0, 1.0);
                // A spring, not an ease: the panel and the terminal beside
                // it overshoot their landing a touch and settle, so the
                // slide reads as elastic. Collapsing never folds past
                // closed — a negative width has nothing to show.
                (anim.from + (anim.to - anim.from) * style::ease_spring(step)).max(0.0)
            }
            None => {
                if self.sidebar_visible {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Sidebar width in pixels, animated while collapsing or expanding, so
    /// every surface that abuts it stays glued to its edge.
    fn sidebar_px(&self, cell_width: f32) -> f32 {
        f32::from(self.sidebar_cells()) * cell_width * self.sidebar_open
    }

    /// Left inset of the tab strip and its menus: clears the traffic lights
    /// when the sidebar is gone, hugs the edge otherwise, gliding between
    /// the two while the panel tweens.
    fn strip_pad(&self, fullscreen: bool) -> f32 {
        if fullscreen {
            TAB_STRIP_EDGE
        } else {
            TRAFFIC_LIGHT_CLEARANCE + (TAB_STRIP_EDGE - TRAFFIC_LIGHT_CLEARANCE) * self.sidebar_open
        }
    }

    /// The measured cell advance in pixels, resolved from the font once.
    fn cell_width(&mut self, window: &Window) -> f32 {
        if let Some(width) = self.cell_width {
            return width;
        }
        let font = font(self.settings.terminal_font.as_str());
        let run = TextRun {
            len: 1,
            font,
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let width = window
            .text_system()
            .shape_line(
                "M".into(),
                px(self.settings.terminal_font_size),
                &[run],
                None,
            )
            .width
            .into();
        self.cell_width = Some(width);
        width
    }

    /// The measured cell height in whole pixels. The daemon is told the
    /// rounded value in `Resize`, so the client floors its own row, padding,
    /// and pointer math to the same integer; a fractional height lets each
    /// painted row snap to the device grid and drift out of the card's centering.
    fn cell_height(&self) -> f32 {
        (self.settings.terminal_font_size * LINE_HEIGHT_RATIO)
            .round()
            .max(1.0)
    }

    /// The chrome/base text size, following the user's UI font size.
    fn chrome_font(&self) -> f32 {
        self.settings.ui_font_size
    }

    /// The section-label size, following the base like the mock.
    fn section_font(&self) -> f32 {
        (self.settings.ui_font_size - (CHROME_FONT - SECTION_FONT)).max(8.0)
    }

    /// Secondary chrome text (rows, badges), one step below the base.
    fn footer_font(&self) -> f32 {
        (self.settings.ui_font_size - 1.0).max(8.0)
    }

    /// Convert a window-space pointer position to a grid cell, if it lands on
    /// the grid rather than the native header.
    /// Whether a pointer position lands on native chrome instead of the grid.
    ///
    /// GPUI delivers mouse events to every listener under the cursor, so a
    /// press on a sidebar button also reaches the grid beneath it. Without
    /// this guard the grid forwards phantom clicks through our own panels:
    /// opening the workspace picker and instantly dismissing it with the
    /// release click, or toggling the daemon menu twice per gear press.
    /// While the session menu is open the whole content area counts, since
    /// the overlay sits above the grid.
    fn overlay_open(&self) -> bool {
        self.session_menu_open
            || self.palette.is_open()
            || self.quick_open.is_open()
            || self.picker.is_open()
            || self.context_menu.is_some()
            || self.menu_prompt.is_some()
    }

    /// Start dragging the window from the blank header, or zoom it on a double
    /// click. The window opts out of AppKit's titlebar drag (it would swallow
    /// the tabs' own clicks and drags), so the bare header does this itself.
    fn drag_window_from_header(&self, event: &MouseDownEvent, window: &mut Window) {
        if self.overlay_open() {
            return;
        }
        if event.click_count >= 2 {
            window.titlebar_double_click();
        } else {
            window.start_window_move();
        }
    }

    fn hit_chrome(&mut self, window: &Window, position: Point<Pixels>) -> bool {
        if self.overlay_open() {
            return true;
        }
        let x = f32::from(position.x);
        let y = f32::from(position.y);
        if y < HEADER_HEIGHT {
            return true;
        }
        if self.sidebar_visible
            && let Some(dock) = self.sidebar_dock()
        {
            let cell_width = self.cell_width(window);
            let left = f32::from(dock.x) * cell_width;
            let right = left + f32::from(dock.width) * cell_width;
            let bottom = f32::from(window.viewport_size().height);
            if x >= left && x < right && y < bottom {
                return true;
            }
        }
        let layout = self.card_layout(window);
        let grid_right = layout.origin_x + f32::from(layout.cols) * layout.cell_w;
        let grid_bottom = layout.origin_y + f32::from(layout.rows) * layout.cell_h;
        if x < layout.origin_x || y < layout.origin_y || x >= grid_right || y >= grid_bottom {
            return true;
        }
        false
    }

    fn cell_at(&mut self, window: &Window, position: Point<Pixels>) -> Option<Cell> {
        let (width, height) = match self.shown_frame() {
            Some(frame) if frame.width > 0 && frame.height > 0 => (frame.width, frame.height),
            _ => return None,
        };
        let layout = self.card_layout(window);
        let skip = u32::from(self.skipped_cols());
        let skip_rows = u32::from(self.skipped_rows());
        let paint_w = u32::from(layout.cols).min(u32::from(width).saturating_sub(skip));
        let paint_h = u32::from(layout.rows).min(
            u32::from(height).saturating_sub(skip_rows + u32::from(SKIPPED_DAEMON_BOTTOM_ROWS)),
        );
        let y = (f32::from(position.y) - layout.origin_y) / layout.cell_h;
        let x = (f32::from(position.x) - layout.origin_x) / layout.cell_w;
        if y < 0.0 || x < 0.0 {
            return None;
        }
        let column = skip.saturating_add((x as u32).min(paint_w.saturating_sub(1)));
        let row = skip_rows.saturating_add((y as u32).min(paint_h.saturating_sub(1)));
        Some(Cell {
            column: column as u16,
            row: row as u16,
        })
    }

    /// Whether the pointer is over something the daemon opens as a link.
    ///
    /// Protocol 20 projects link spans directly; when a cell is not covered by
    /// one, fall back to the underline the daemon paints on a link and an
    /// obvious URL token (which most agents underline anyway). Drives the
    /// pointer cursor.
    fn hovered_link(&self) -> bool {
        let Some(hover) = self.hover_cell else {
            return false;
        };
        let Some(frame) = self.shown_frame() else {
            return false;
        };
        if hover.row >= frame.height || hover.column >= frame.width {
            return false;
        }
        let index = u32::from(hover.row) * u32::from(frame.width) + u32::from(hover.column);
        if frame
            .hyperlinks
            .iter()
            .any(|link| link.start <= index && index < link.end)
        {
            return true;
        }
        let width = usize::from(frame.width);
        let row_start = usize::from(hover.row) * width;
        let Some(row) = frame.cells.get(row_start..row_start + width) else {
            return false;
        };
        if row
            .get(usize::from(hover.column))
            .is_some_and(|cell| cell.mods & M_UNDERLINED != 0)
        {
            return true;
        }
        // Walk out to the whitespace-delimited token under the pointer.
        let mut text = String::new();
        let mut at = 0;
        for (column, cell) in row.iter().enumerate() {
            if column == usize::from(hover.column) {
                at = text.len();
            }
            text.push_str(&cell.symbol);
        }
        let start = text[..at]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        let end = text[at..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(text.len(), |(index, _)| at + index);
        let token = text[start..end].to_ascii_lowercase();
        token.starts_with("http://") || token.starts_with("https://") || token.starts_with("www.")
    }

    /// Send one mouse event to the daemon at `cell`.
    fn forward_mouse(&self, kind: MouseEventKind, cell: Cell, modifiers: Modifiers) {
        self.forward_mouse_with(kind, cell, keys::mouse_modifiers(modifiers));
    }

    /// Send one mouse event with wire modifiers already resolved.
    fn forward_mouse_with(&self, kind: MouseEventKind, cell: Cell, modifiers: KeyModifiers) {
        self.send_active(ClientMessage::Mouse(MouseEvent {
            kind,
            column: cell.column,
            row: cell.row,
            modifiers,
        }));
    }

    /// The seam of a split under `position`, and the border cell the daemon
    /// is handed to grab it by. Pointer and paint read the same geometry
    /// (`pane_chrome`), so the seam is where the gutter or the line is drawn,
    /// give or take the slop that makes a thin line possible to hit.
    fn divider_at(
        &mut self,
        window: &Window,
        position: Point<Pixels>,
    ) -> Option<(pane_frames::DividerAxis, Cell)> {
        if self.dividers.is_empty() {
            return None;
        }
        let layout = self.card_layout(window);
        let (skip_cols, skip_rows) = (self.skipped_cols(), self.skipped_rows());
        let slop = pane_chrome::divider_slop(self.settings.pane_gaps);
        let x = f32::from(position.x) - layout.origin_x;
        let y = f32::from(position.y) - layout.origin_y;
        let edge =
            |cell: u16, skip: u16, size: f32| ((f32::from(cell) - f32::from(skip)) * size).round();
        self.dividers.iter().find_map(|divider| {
            let (across, along, skip_across, skip_along, size_across, size_along) =
                match divider.axis {
                    pane_frames::DividerAxis::Vertical => {
                        (x, y, skip_cols, skip_rows, layout.cell_w, layout.cell_h)
                    }
                    pane_frames::DividerAxis::Horizontal => {
                        (y, x, skip_rows, skip_cols, layout.cell_h, layout.cell_w)
                    }
                };
            let on_seam = (across - edge(divider.at, skip_across, size_across)).abs() <= slop;
            let within = along >= edge(divider.from, skip_along, size_along)
                && along < edge(divider.to + 1, skip_along, size_along);
            if !(on_seam && within) {
                return None;
            }
            let pointer = skip_along.saturating_add((along / size_along).max(0.0) as u16);
            let (column, row) = divider.grab(pointer);
            Some((divider.axis, Cell { column, row }))
        })
    }

    /// Carry a held divider to the pointer. Returns whether a divider drag
    /// owns the move. A button released where the window never saw it ends
    /// the drag here, so the daemon is not left holding a seam.
    fn drag_divider(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.divider_drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.divider_drag = None;
            self.forward_mouse(
                MouseEventKind::Up(TerminalMouseButton::Left),
                drag.last,
                event.modifiers,
            );
            cx.notify();
            return true;
        }
        if let Some(cell) = self.cell_at(window, event.position)
            && cell != drag.last
        {
            self.forward_mouse(
                MouseEventKind::Drag(TerminalMouseButton::Left),
                cell,
                event.modifiers,
            );
            self.divider_drag = Some(DividerDrag { last: cell, ..drag });
        }
        true
    }

    fn begin_selection(&mut self, event: &MouseDownEvent, window: &Window, cx: &mut Context<Self>) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        if let Some((axis, grab)) = self.divider_at(window, event.position) {
            self.selection = None;
            self.divider_drag = Some(DividerDrag { axis, last: grab });
            self.forward_mouse(
                MouseEventKind::Down(TerminalMouseButton::Left),
                grab,
                event.modifiers,
            );
            cx.notify();
            return;
        }
        let Some(cell) = self.cell_at(window, event.position) else {
            return;
        };
        // Record the press, but paint nothing yet: a plain click must not
        // flash a one-cell highlight. The selection appears only once the
        // pointer actually leaves the anchor cell.
        self.dragging = true;
        self.drag_moved = false;
        self.press_anchor = Some(cell);
        self.hover_cell = Some(cell);
        cx.notify();
    }

    fn extend_selection(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_divider(event, window, cx) {
            return;
        }
        // A drag started on the grid may roam anywhere; anything else over
        // chrome belongs to the panels, not the daemon.
        if !self.dragging && self.hit_chrome(window, event.position) {
            let hovered = self.hover_cell.take().is_some();
            let parted = self.divider_hover.take().is_some();
            if hovered || parted {
                cx.notify();
            }
            return;
        }
        let over_divider = if self.dragging {
            None
        } else {
            self.divider_at(window, event.position)
                .map(|(axis, _)| axis)
        };
        if over_divider != self.divider_hover {
            self.divider_hover = over_divider;
            cx.notify();
        }
        if self.dragging && event.pressed_button == Some(MouseButton::Left) {
            let Some(cell) = self.cell_at(window, event.position) else {
                return;
            };
            let Some(anchor) = self.press_anchor else {
                return;
            };
            if cell == anchor {
                return;
            }
            match self.selection.as_mut() {
                Some(selection) if selection.cursor != cell => {
                    selection.cursor = cell;
                    self.drag_moved = true;
                    cx.notify();
                }
                None => {
                    let mut created = Selection::new(anchor);
                    created.cursor = cell;
                    self.selection = Some(created);
                    self.drag_moved = true;
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        // Hover: only tell the daemon when the pointer crosses into a new cell,
        // so links and scrollbars react without a message per pixel.
        let Some(cell) = self.cell_at(window, event.position) else {
            if self.hover_cell.take().is_some() {
                cx.notify();
            }
            return;
        };
        // A move carries the live modifier state (macOS only clears `Ctrl` on
        // the down/up it rewrites), so it repairs a stale flag after `Ctrl` was
        // released while the pointer was outside the window.
        self.control_held = event.modifiers.control;
        if self.hover_cell != Some(cell) {
            self.hover_cell = Some(cell);
            self.forward_mouse(MouseEventKind::Moved, cell, event.modifiers);
            // The pointer cursor follows the link under the cursor, so the
            // frame must repaint as the hovered cell changes.
            cx.notify();
        }
    }

    fn finish_selection(&mut self, event: &MouseUpEvent, window: &Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.divider_drag.take() {
            let cell = self.cell_at(window, event.position).unwrap_or(drag.last);
            self.forward_mouse(
                MouseEventKind::Up(TerminalMouseButton::Left),
                cell,
                event.modifiers,
            );
            cx.notify();
            return;
        }
        if !self.dragging {
            return;
        }
        // A release over native chrome (letting go on a sidebar button after
        // pressing it) ends the gesture without forwarding a click.
        if self.hit_chrome(window, event.position) {
            self.dragging = false;
            self.press_anchor = None;
            self.drag_moved = false;
            cx.notify();
            return;
        }
        self.dragging = false;
        self.press_anchor = None;
        let moved = std::mem::replace(&mut self.drag_moved, false);
        if moved {
            // A completed drag keeps its highlight; a release outside the grid
            // still keeps the text that was selected.
            if let Some(selection) = self.selection
                && !selection.is_collapsed()
            {
                cx.notify();
                return;
            }
        }
        // A plain click: hand press and release to the daemon so it focuses
        // whatever was clicked. Nothing was ever painted, so there is nothing
        // to clear.
        self.selection = None;
        let Some(cell) = self.cell_at(window, event.position) else {
            cx.notify();
            return;
        };
        if self.open_link_at(cell, event.modifiers, cx) {
            cx.notify();
            return;
        }
        self.forward_mouse(
            MouseEventKind::Down(TerminalMouseButton::Left),
            cell,
            event.modifiers,
        );
        self.forward_mouse(
            MouseEventKind::Up(TerminalMouseButton::Left),
            cell,
            event.modifiers,
        );
        cx.notify();
    }

    /// Open what a `Cmd`/`Ctrl` click landed on, if the window can.
    ///
    /// An `OSC 8` span's target is already in the frame, so it opens without a
    /// round trip to the daemon. A path in plain text opens in the Files tab
    /// once a worker has found the file. Anything else, or a scheme `link_at`
    /// refuses, returns false and goes to the daemon as before: it still finds
    /// plain URLs in the text.
    fn open_link_at(&self, cell: Cell, modifiers: Modifiers, cx: &mut Context<Self>) -> bool {
        if !keys::mouse_modifiers(modifiers).contains(KeyModifiers::CONTROL) {
            return false;
        }
        let Some(frame) = self.shown_frame() else {
            return false;
        };
        if let Some(url) = pilus_client::link_at(frame, cell.column, cell.row) {
            cx.open_url(url);
            return true;
        }
        // Plain text that reads like a path opens in the Files tab when the
        // file is there, else the click still goes to the daemon (see
        // `files_probe_path`).
        pilus_client::path_at(frame, cell.column, cell.row).is_some_and(|word| {
            let modifiers = keys::mouse_modifiers(modifiers) | KeyModifiers::CONTROL;
            self.files_probe_path(&word, cell, modifiers)
        })
    }

    fn press(&mut self, event: &MouseDownEvent, button: TerminalMouseButton, window: &Window) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        let Some(cell) = self.cell_at(window, event.position) else {
            return;
        };
        self.forward_button(MouseEventKind::Down(button), button, cell, event.modifiers);
    }

    fn release(&mut self, event: &MouseUpEvent, button: TerminalMouseButton, window: &Window) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        let Some(cell) = self.cell_at(window, event.position) else {
            return;
        };
        self.forward_button(MouseEventKind::Up(button), button, cell, event.modifiers);
    }

    /// Forward a button event, reinterpreting a `Ctrl`-held secondary click as
    /// the `Ctrl`+left-click the daemon uses to open a link.
    ///
    /// macOS rewrites `Ctrl`+left-click to a right-click in `gpui`, so the
    /// original chord is unrecoverable from the event alone; `control_held`
    /// supplies the missing half.
    fn forward_button(
        &self,
        kind: MouseEventKind,
        button: TerminalMouseButton,
        cell: Cell,
        modifiers: Modifiers,
    ) {
        if button == TerminalMouseButton::Right && self.control_held {
            let kind = match kind {
                MouseEventKind::Down(_) => MouseEventKind::Down(TerminalMouseButton::Left),
                MouseEventKind::Up(_) => MouseEventKind::Up(TerminalMouseButton::Left),
                other => other,
            };
            let modifiers = keys::mouse_modifiers(modifiers) | KeyModifiers::CONTROL;
            self.forward_mouse_with(kind, cell, modifiers);
            return;
        }
        self.forward_mouse_with(kind, cell, keys::mouse_modifiers(modifiers));
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &Window) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        let Some(cell) = self.cell_at(window, event.position) else {
            return;
        };
        let cell_height = self.cell_height();
        let (delta, lines) = match event.delta {
            ScrollDelta::Lines(point) => (point.y, point.y.abs().round().max(1.0) as u16),
            ScrollDelta::Pixels(point) => {
                let delta = f32::from(point.y);
                let lines = (delta.abs() / cell_height).round().max(1.0) as u16;
                (delta, lines)
            }
        };
        let kind = if delta > 0.0 {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        };
        for _ in 0..lines.min(10) {
            self.forward_mouse(kind, cell, event.modifiers);
        }
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(frame) = self.shown_frame() else {
            return;
        };
        let text = selection.text(frame);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Select the whole visible grid, so Cmd+A followed by Cmd+C copies it.
    fn select_all(&mut self, window: &Window, cx: &mut Context<Self>) {
        let layout = self.card_layout(window);
        let skip = self.skipped_cols();
        let skip_rows = self.skipped_rows();
        self.selection = Some(Selection {
            anchor: Cell {
                column: skip,
                row: skip_rows,
            },
            cursor: Cell {
                column: skip.saturating_add(layout.cols).saturating_sub(1),
                row: skip_rows.saturating_add(layout.rows).saturating_sub(1),
            },
        });
        cx.notify();
    }

    /// Resolve a control client for any endpoint, active or stashed.
    fn control_client_for_endpoint(&self, id: &EndpointId) -> Option<ControlClient> {
        match id {
            EndpointId::Local => ControlClient::connect(&self.api_socket()).ok(),
            EndpointId::Machine(machine_id) => {
                let target = self
                    .machines
                    .iter()
                    .find(|machine| &machine.id == machine_id)?
                    .ssh_target()?;
                ControlClient::connect_ssh(target).ok()
            }
        }
    }

    /// The endpoint an endpoint-key string names.
    fn endpoint_by_key(&self, key: &str) -> EndpointId {
        if key == "local" {
            EndpointId::Local
        } else {
            EndpointId::Machine(key.to_string())
        }
    }

    /// Open the right-click menu for a WORKSPACES or AGENTS row at `position`.
    fn open_context_menu(
        &mut self,
        target: SidebarMenuTarget,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.menu_prompt = None;
        let workspace_probe = match &target {
            SidebarMenuTarget::Workspace {
                endpoint,
                workspace_id,
                index,
                ..
            } => Some((
                endpoint.clone(),
                workspace_id.clone(),
                *index,
                target.clone(),
            )),
            SidebarMenuTarget::Agent { .. }
            | SidebarMenuTarget::Saved { .. }
            | SidebarMenuTarget::Terminal { .. }
            | SidebarMenuTarget::Github
            | SidebarMenuTarget::File { .. } => None,
        };
        self.context_menu = Some(SidebarMenu::new(
            target,
            f32::from(position.x),
            f32::from(position.y),
        ));
        if let Some((endpoint, workspace_id, index, target)) = workspace_probe {
            self.probe_worktree(&endpoint, &workspace_id, index, &target);
        }
        cx.notify();
    }

    /// Ask the daemon whether the row's checkout is a linked worktree, once
    /// per workspace, and only for a repo. Until an answer lands the row is
    /// absent; a successful probe inserts it a beat later. A failed probe is
    /// not cached, so the next right-click retries.
    fn probe_worktree(
        &mut self,
        endpoint_key: &str,
        workspace_id: &str,
        index: u16,
        target: &SidebarMenuTarget,
    ) {
        let key = (endpoint_key.to_string(), workspace_id.to_string());
        if self.worktree_linked.contains_key(&key) || self.worktree_probing.contains(&key) {
            return;
        }
        let Some(mut client) =
            self.control_client_for_endpoint(&self.endpoint_by_key(endpoint_key))
        else {
            return;
        };
        let expanded = match target {
            // A plain folder has no worktrees to list, and the daemon answers
            // `worktree.list` there with an error.
            SidebarMenuTarget::Workspace { branch: None, .. } => return,
            SidebarMenuTarget::Workspace { cwd, .. } => self.expand_cwd(endpoint_key, cwd),
            SidebarMenuTarget::Agent { .. }
            | SidebarMenuTarget::Saved { .. }
            | SidebarMenuTarget::Terminal { .. }
            | SidebarMenuTarget::Github
            | SidebarMenuTarget::File { .. } => return,
        };
        let workspace_id = workspace_id.to_string();
        self.worktree_probing.insert(key.clone());
        let tx = self.menu_tx.clone();
        std::thread::spawn(move || {
            let result = client
                .worktrees(index)
                .map(|worktrees| linked_worktree_path(&worktrees, &expanded))
                .map_err(|error| error.to_string());
            tx.send(MenuOpResult {
                ok: String::new(),
                error: None,
                renamed_workspace: None,
                renamed_agent: None,
                worktree_probe: Some((key.0, workspace_id, result)),
                worktree_cleared: None,
                worktree_escalated: None,
            })
            .ok();
        });
    }

    /// Close the context menu and any prompt it left behind.
    fn close_context_menu(&mut self) {
        self.context_menu = None;
    }

    /// The open menu's rows, resolved against the state they toggle.
    fn menu_rows(&self) -> Vec<SidebarItem> {
        let Some(menu) = self.context_menu.as_ref() else {
            return Vec::new();
        };
        let linked = match &menu.target {
            // Only a confirmed linked checkout offers Delete Worktree;
            // unknown and known-not-linked leave the row out.
            SidebarMenuTarget::Workspace {
                endpoint,
                workspace_id,
                ..
            } => self
                .worktree_linked
                .get(&(endpoint.clone(), workspace_id.clone()))
                .is_some_and(Option::is_some),
            SidebarMenuTarget::Agent { .. }
            | SidebarMenuTarget::Saved { .. }
            | SidebarMenuTarget::Terminal { .. }
            | SidebarMenuTarget::Github
            | SidebarMenuTarget::File { .. } => false,
        };
        menu.rows(linked)
    }

    /// Drive the open context menu from the keyboard.
    fn context_menu_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let rows = self.menu_rows();
        let (quick_open, quick_rows, in_quick) = self
            .context_menu
            .as_ref()
            .map(|menu| {
                (
                    menu.quick_open,
                    menu.quick_rows(),
                    menu.quick_selected.is_some(),
                )
            })
            .unwrap_or_default();
        match keystroke.key.as_str() {
            "escape" | "esc" => {
                if quick_open {
                    // Esc steps back out of the flyout before closing.
                    if let Some(menu) = self.context_menu.as_mut() {
                        menu.quick_open = false;
                        menu.quick_selected = None;
                    }
                } else {
                    self.close_context_menu();
                }
                cx.notify();
            }
            "up" | "down" => {
                let Some(menu) = self.context_menu.as_mut() else {
                    return;
                };
                let dir: isize = if keystroke.key == "up" { -1 } else { 1 };
                if let Some(current) = menu.quick_selected {
                    if let Some(next) = menu.next_selectable(&quick_rows, current, dir) {
                        menu.quick_selected = Some(next);
                    }
                } else {
                    let from = menu
                        .selected
                        .unwrap_or(if dir < 0 { 0 } else { usize::MAX });
                    if let Some(next) = menu.next_selectable(&rows, from, dir) {
                        menu.selected = Some(next);
                        // Moving off the parent row folds the flyout away.
                        menu.quick_open = false;
                    }
                }
                cx.notify();
            }
            // A right arrow opens the flyout and moves into it; a left arrow
            // climbs back to the parent row.
            "right" => {
                if !in_quick
                    && rows.contains(&SidebarItem::QuickActions)
                    && let Some(menu) = self.context_menu.as_mut()
                {
                    menu.quick_open = true;
                    menu.quick_selected = Some(0);
                    cx.notify();
                }
            }
            "left" => {
                if quick_open && let Some(menu) = self.context_menu.as_mut() {
                    menu.quick_open = false;
                    menu.quick_selected = None;
                    cx.notify();
                }
            }
            "enter" => {
                if in_quick {
                    let index = self
                        .context_menu
                        .as_ref()
                        .and_then(|menu| menu.quick_selected);
                    if let Some(item) = index.and_then(|index| quick_rows.get(index)) {
                        self.run_context_item(*item, cx);
                    }
                    return;
                }
                let row = self
                    .context_menu
                    .as_ref()
                    .and_then(|menu| menu.selected)
                    .filter(|index| rows.get(*index).is_some_and(|item| item.selectable()))
                    .or_else(|| rows.iter().position(|item| item.selectable()));
                if let Some(index) = row {
                    self.run_context_item(rows[index], cx);
                    // From the keyboard, opening the flyout also steps into it.
                    if rows[index] == SidebarItem::QuickActions
                        && let Some(menu) = self.context_menu.as_mut()
                    {
                        menu.quick_selected = Some(0);
                    }
                }
            }
            _ => {}
        }
    }

    /// Run one hit context-menu row. The menu is cloned so the Quick Actions
    /// parent row can pin its flyout open in place; everything else closes
    /// it and runs the action.
    fn run_context_item(&mut self, item: SidebarItem, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_ref() else {
            return;
        };
        let target = menu.target.clone();
        match (&target, item) {
            (SidebarMenuTarget::Workspace { .. }, SidebarItem::QuickActions) => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.quick_open = true;
                }
                cx.notify();
            }
            (SidebarMenuTarget::Workspace { .. }, SidebarItem::Close) => {
                self.close_context_menu();
                self.menu_close_workspace(&target, cx);
            }
            (SidebarMenuTarget::Workspace { .. }, SidebarItem::Rename) => {
                self.close_context_menu();
                self.menu_open_rename(&target, cx);
            }
            (SidebarMenuTarget::Workspace { .. }, SidebarItem::TogglePaths) => {
                self.close_context_menu();
                self.menu_toggle_workspace_paths(cx);
            }
            (SidebarMenuTarget::Workspace { cwd, .. }, SidebarItem::CopyPath) => {
                self.close_context_menu();
                self.menu_copy(cwd, cx);
            }
            (SidebarMenuTarget::Workspace { branch, .. }, SidebarItem::CopyBranch) => {
                self.close_context_menu();
                if let Some(branch) = branch {
                    self.menu_copy(branch, cx);
                }
            }
            (
                SidebarMenuTarget::Workspace {
                    endpoint, index, ..
                },
                SidebarItem::NewWorktree,
            ) => {
                self.close_context_menu();
                self.menu_new_worktree(
                    PromptAction::NewWorktree {
                        endpoint: self.endpoint_by_key(endpoint),
                        index: *index,
                    },
                    cx,
                );
            }
            (
                SidebarMenuTarget::Workspace {
                    endpoint,
                    workspace_id,
                    name,
                    ..
                },
                SidebarItem::DeleteWorktree,
            ) => {
                // Removal targets the root the probe confirmed, not the
                // workspace cwd, which can sit anywhere inside the checkout.
                let key = (endpoint.clone(), workspace_id.clone());
                let Some(path) = self.worktree_linked.get(&key).and_then(|path| path.clone())
                else {
                    self.close_context_menu();
                    self.toast(ToastKind::Info, "worktree status is still being checked");
                    cx.notify();
                    return;
                };
                self.close_context_menu();
                self.open_menu_prompt(
                    PromptAction::DeleteWorktree {
                        endpoint: self.endpoint_by_key(endpoint),
                        path,
                        workspace_id: workspace_id.clone(),
                        name: name.clone(),
                        dirty: false,
                        blockers: None,
                    },
                    "Delete Worktree",
                    "",
                    "",
                    cx,
                );
            }
            (
                SidebarMenuTarget::Workspace {
                    endpoint,
                    cwd,
                    name,
                    ..
                },
                SidebarItem::PinWorkspace | SidebarItem::UnpinWorkspace,
            ) => {
                self.close_context_menu();
                self.set_folder_pinned(endpoint, cwd, name, item == SidebarItem::PinWorkspace, cx);
            }
            (
                SidebarMenuTarget::Saved {
                    endpoint,
                    path,
                    name,
                    ..
                },
                SidebarItem::PinWorkspace | SidebarItem::UnpinWorkspace,
            ) => {
                self.close_context_menu();
                self.set_folder_pinned(endpoint, path, name, item == SidebarItem::PinWorkspace, cx);
            }
            (SidebarMenuTarget::Saved { path, .. }, SidebarItem::CopyPath) => {
                self.close_context_menu();
                self.menu_copy(path, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::Rename) => {
                self.close_context_menu();
                self.menu_open_agent_rename(&target, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::Close) => {
                self.close_context_menu();
                self.menu_close_agent(&target, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::TogglePaths) => {
                self.close_context_menu();
                self.menu_toggle_agent_paths(cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::ToggleScope) => {
                self.close_context_menu();
                self.agents_this_workspace = !self.agents_this_workspace;
                cx.notify();
            }
            (SidebarMenuTarget::Terminal { .. }, item) => {
                self.close_context_menu();
                self.run_terminal_item(item, cx);
            }
            (SidebarMenuTarget::Github, item) => {
                self.close_context_menu();
                self.github_menu_item(item, cx);
            }
            (SidebarMenuTarget::File { path, .. }, SidebarItem::CopyPath) => {
                self.close_context_menu();
                self.menu_copy(path, cx);
            }
            (SidebarMenuTarget::File { .. }, item) => {
                self.close_context_menu();
                self.files_menu_item(&target, item, cx);
            }
            (_, SidebarItem::Divider) => {}
            // Copy rows only exist on a WORKSPACES target; the arms above
            // cover every row the builders can emit.
            _ => {}
        }
    }

    /// Run one row of the terminal's menu. Every row is something the client
    /// already does from a chord, so the menu is only a second way in.
    fn run_terminal_item(&mut self, item: SidebarItem, cx: &mut Context<Self>) {
        match item {
            SidebarItem::CopySelection => self.copy_selection(cx),
            SidebarItem::Paste => self.paste_clipboard(cx),
            SidebarItem::SplitRight => self.split("right", cx),
            SidebarItem::SplitDown => self.split("down", cx),
            SidebarItem::ForkPane => self.fork_focused_pane(cx),
            SidebarItem::Rename => self.open_menu_prompt(
                PromptAction::RenamePane {
                    endpoint: self.active.clone(),
                },
                "Rename pane",
                "Alias ([a-z][a-z0-9_-]{0,31}); empty clears it",
                "",
                cx,
            ),
            SidebarItem::MoveToTab => self.open_menu_prompt(
                PromptAction::MovePane {
                    endpoint: self.active.clone(),
                },
                "Move to tab",
                "Tab number; empty for a new tab",
                "",
                cx,
            ),
            SidebarItem::NewTab => self.new_tab(cx),
            SidebarItem::CloseTab => self.close_active_tab(),
            _ => {}
        }
        cx.notify();
    }

    /// Run `op` against the focused pane on a background thread and toast the
    /// outcome. The pane is resolved there, not at click time, so the GUI
    /// thread never waits on the socket and the answer is the pane that has
    /// focus when the daemon gets the request.
    fn pane_op(
        &mut self,
        endpoint: &EndpointId,
        ok: String,
        op: impl FnOnce(&mut ControlClient, &str) -> pilus_client::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(mut client) = self.control_client_for_endpoint(endpoint) else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            cx.notify();
            return;
        };
        let tx = self.menu_tx.clone();
        std::thread::spawn(move || {
            let error = client
                .focused_pane()
                .and_then(|pane| op(&mut client, &pane))
                .err()
                .map(|error| error.to_string());
            tx.send(MenuOpResult {
                ok,
                error,
                renamed_workspace: None,
                renamed_agent: None,
                worktree_probe: None,
                worktree_cleared: None,
                worktree_escalated: None,
            })
            .ok();
        });
    }

    /// Fork the focused agent's session into a sibling pane. A pane that runs
    /// no agent has nothing to fork, which the daemon reports as an error.
    fn fork_focused_pane(&mut self, cx: &mut Context<Self>) {
        let endpoint = self.active.clone();
        self.pane_op(
            &endpoint,
            "forked into a new pane".to_string(),
            |client, pane| client.fork_agent(pane),
            cx,
        );
    }

    /// Open the terminal's own menu where the pointer is, in place of the
    /// one the daemon would draw into the grid.
    fn open_terminal_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        let selection = self
            .selection
            .is_some_and(|selection| !selection.is_collapsed());
        let agent = self.agents.iter().any(|agent| agent.focused);
        self.open_context_menu(
            SidebarMenuTarget::Terminal { selection, agent },
            event.position,
            cx,
        );
    }

    /// Close a workspace from its context menu, reporting through the menu
    /// channel so a remote round trip never blocks the window.
    fn menu_close_workspace(&mut self, target: &SidebarMenuTarget, cx: &mut Context<Self>) {
        let SidebarMenuTarget::Workspace {
            endpoint,
            workspace_id,
            name,
            ..
        } = target
        else {
            return;
        };
        let Some(mut client) = self.control_client_for_endpoint(&self.endpoint_by_key(endpoint))
        else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            cx.notify();
            return;
        };
        let workspace_id = workspace_id.clone();
        let name = name.clone();
        let tx = self.menu_tx.clone();
        std::thread::spawn(move || {
            let error = client
                .close_workspace(&workspace_id)
                .err()
                .map(|error| error.to_string());
            tx.send(MenuOpResult {
                ok: format!("closed {name}"),
                error,
                renamed_workspace: None,
                renamed_agent: None,
                worktree_probe: None,
                worktree_cleared: None,
                worktree_escalated: None,
            })
            .ok();
        });
        cx.notify();
    }

    /// Open the rename prompt for a workspace's label.
    fn menu_open_rename(&mut self, target: &SidebarMenuTarget, cx: &mut Context<Self>) {
        let SidebarMenuTarget::Workspace {
            endpoint,
            workspace_id,
            name,
            ..
        } = target
        else {
            return;
        };
        self.open_menu_prompt(
            PromptAction::RenameWorkspace {
                endpoint: self.endpoint_by_key(endpoint),
                workspace_id: workspace_id.clone(),
            },
            "Rename workspace",
            "Name",
            name,
            cx,
        );
    }

    /// Give a live agent's pane a new alias via its rename prompt.
    fn menu_open_agent_rename(&mut self, target: &SidebarMenuTarget, cx: &mut Context<Self>) {
        let SidebarMenuTarget::Agent {
            endpoint,
            pane,
            name,
            ..
        } = target
        else {
            return;
        };
        self.open_menu_prompt(
            PromptAction::RenameAgent {
                endpoint: self.endpoint_by_key(endpoint),
                pane: pane.clone(),
            },
            "Rename agent",
            "Alias ([a-z][a-z0-9_-]{0,31})",
            name,
            cx,
        );
    }

    /// Show or hide the focused pane's corner toolbar, persisting the choice.
    /// Flip energy mode, persisting the choice. `sync_energy` applies it on the
    /// next tick; the corners are published here so this frame already draws
    /// them square.
    fn toggle_energy_mode(&mut self, cx: &mut Context<Self>) {
        self.settings.energy_mode = !self.settings.energy_mode;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        style::sync_corner_radius(&self.settings);
        cx.notify();
    }

    fn toggle_pane_toolbar(&mut self, cx: &mut Context<Self>) {
        self.settings.pane_toolbar = !self.settings.pane_toolbar;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        cx.notify();
    }

    /// Flip whether the WORKSPACES rows draw their path line, persisting the
    /// choice and pushing it to the daemon's own dock.
    fn menu_toggle_workspace_paths(&mut self, cx: &mut Context<Self>) {
        self.sidebar_workspace_paths = !self.sidebar_workspace_paths;
        self.settings.sidebar_workspace_paths = self.sidebar_workspace_paths;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        self.push_sidebar_layout();
        cx.notify();
    }

    /// Flip whether the AGENTS rows draw their path line, persisting the
    /// choice locally since the daemon has no agents-path flag.
    fn menu_toggle_agent_paths(&mut self, cx: &mut Context<Self>) {
        self.sidebar_agent_paths = !self.sidebar_agent_paths;
        self.settings.sidebar_agent_paths = self.sidebar_agent_paths;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        cx.notify();
    }

    /// Copy a context-menu value to the system clipboard.
    fn menu_copy(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        cx.notify();
    }

    /// Close a live agent's pane from its context menu.
    fn menu_close_agent(&mut self, target: &SidebarMenuTarget, cx: &mut Context<Self>) {
        let SidebarMenuTarget::Agent {
            endpoint,
            pane,
            name,
            ..
        } = target
        else {
            return;
        };
        let Some(mut client) = self.control_client_for_endpoint(&self.endpoint_by_key(endpoint))
        else {
            self.toast(
                ToastKind::Error,
                "no control connection for the selected endpoint",
            );
            cx.notify();
            return;
        };
        let pane = pane.clone();
        let name = name.clone();
        self.agent_sel = None;
        let tx = self.menu_tx.clone();
        std::thread::spawn(move || {
            let error = client
                .close_pane(&pane)
                .err()
                .map(|error| error.to_string());
            tx.send(MenuOpResult {
                ok: format!("closed {name}"),
                error,
                renamed_workspace: None,
                renamed_agent: None,
                worktree_probe: None,
                worktree_cleared: None,
                worktree_escalated: None,
            })
            .ok();
        });
        cx.notify();
    }

    /// Open the branch prompt for a new worktree of the row's repo.
    fn menu_new_worktree(&mut self, action: PromptAction, cx: &mut Context<Self>) {
        self.open_menu_prompt(
            action,
            "New Git Worktree",
            "Branch",
            &generate_session_name(&[]),
            cx,
        );
    }

    /// Open the little prompt modal; the menu itself closes first.
    fn open_menu_prompt(
        &mut self,
        action: PromptAction,
        title: &'static str,
        placeholder: &'static str,
        initial: &str,
        cx: &mut Context<Self>,
    ) {
        self.menu_prompt_field.set_value(initial.to_string());
        self.menu_prompt_input.update(cx, |input, cx| {
            input.set_value(initial.to_string(), cx);
        });
        self.menu_prompt = Some(MenuPrompt {
            title,
            placeholder,
            action,
        });
        cx.notify();
    }

    /// Route a keystroke while the menu prompt is open: Enter submits,
    /// Escape cancels; everything else belongs to the field.
    fn menu_prompt_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" | "esc" => {
                self.menu_prompt = None;
                cx.notify();
            }
            "enter" => self.submit_menu_prompt(cx),
            _ => {}
        }
    }

    /// Run the prompt's action with whatever the field holds.
    fn submit_menu_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.menu_prompt.take() else {
            return;
        };
        let trimmed = self.menu_prompt_field.value().trim().to_string();
        match prompt.action {
            PromptAction::RenameTab { tab_id } => {
                if trimmed.chars().count() > TAB_NAME_MAX {
                    self.toast(
                        ToastKind::Error,
                        format!("tab names are capped at {TAB_NAME_MAX} characters"),
                    );
                    cx.notify();
                    return;
                }
                // An empty name clears it, which hands the label back to the cwd.
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.name = trimmed.clone();
                    if let Some(feed) = self.agent_feed.as_ref() {
                        feed.rename_tab(tab.index, trimmed);
                    }
                }
                cx.notify();
            }
            PromptAction::RenameWorkspace {
                endpoint,
                workspace_id,
            } => {
                if trimmed.is_empty() {
                    self.toast(ToastKind::Error, "a workspace name is required");
                    cx.notify();
                    return;
                }
                if trimmed.chars().count() > 40 {
                    self.toast(
                        ToastKind::Error,
                        "workspace names are capped at 40 characters",
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
                let ok = format!("renamed to {trimmed}");
                let renamed_workspace = Some((workspace_id.clone(), trimmed.clone()));
                let tx = self.menu_tx.clone();
                std::thread::spawn(move || {
                    let error = client
                        .rename_workspace(&workspace_id, &trimmed)
                        .err()
                        .map(|error| error.to_string());
                    tx.send(MenuOpResult {
                        ok,
                        error,
                        renamed_workspace,
                        renamed_agent: None,
                        worktree_probe: None,
                        worktree_cleared: None,
                        worktree_escalated: None,
                    })
                    .ok();
                });
            }
            PromptAction::RenameSession { name } => {
                if trimmed.chars().count() > 40 {
                    self.toast(
                        ToastKind::Error,
                        "session labels are capped at 40 characters",
                    );
                    cx.notify();
                    return;
                }
                self.settings.set_session_label(&name, &trimmed);
                self.save_settings();
                cx.notify();
            }
            PromptAction::RenameAgent { endpoint, pane } => {
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.toast(
                        ToastKind::Error,
                        "no control connection for the selected endpoint",
                    );
                    cx.notify();
                    return;
                };
                let endpoint_key = endpoint.key();
                let alias = trimmed.clone();
                let tx = self.menu_tx.clone();
                std::thread::spawn(move || {
                    let result = if trimmed.is_empty() {
                        client.clear_agent_name(&pane)
                    } else {
                        client.name_agent(&pane, &trimmed)
                    };
                    let renamed_agent = match result.err().map(|error| error.to_string()) {
                        Some(error) => {
                            tx.send(MenuOpResult {
                                ok: String::new(),
                                error: Some(error),
                                renamed_workspace: None,
                                renamed_agent: None,
                                worktree_probe: None,
                                worktree_cleared: None,
                                worktree_escalated: None,
                            })
                            .ok();
                            None
                        }
                        None => Some((endpoint_key, pane, alias.clone())),
                    };
                    if renamed_agent.is_some() {
                        tx.send(MenuOpResult {
                            ok: format!("renamed to {alias}"),
                            error: None,
                            renamed_workspace: None,
                            renamed_agent,
                            worktree_probe: None,
                            worktree_cleared: None,
                            worktree_escalated: None,
                        })
                        .ok();
                    }
                });
            }
            PromptAction::RenamePane { endpoint } => {
                let ok = if trimmed.is_empty() {
                    "cleared the pane's name".to_string()
                } else {
                    format!("renamed to {trimmed}")
                };
                self.pane_op(
                    &endpoint,
                    ok,
                    move |client, pane| {
                        if trimmed.is_empty() {
                            client.clear_agent_name(pane)
                        } else {
                            client.name_agent(pane, &trimmed)
                        }
                    },
                    cx,
                );
            }
            PromptAction::MovePane { endpoint } => {
                let tab = if trimmed.is_empty() {
                    None
                } else if let Ok(tab @ 1..) = trimmed.parse::<u16>() {
                    Some(tab)
                } else {
                    self.toast(ToastKind::Error, "a tab number starts at 1");
                    cx.notify();
                    return;
                };
                let ok = match tab {
                    Some(tab) => format!("moved to tab {tab}"),
                    None => "moved to a new tab".to_string(),
                };
                self.pane_op(
                    &endpoint,
                    ok,
                    move |client, pane| client.move_pane(pane, tab),
                    cx,
                );
            }
            PromptAction::CreateEntry {
                endpoint,
                dir,
                folder,
            } => self.files_create(&endpoint, dir, trimmed, folder, cx),
            PromptAction::RenameEntry { endpoint, path } => {
                self.files_rename(&endpoint, path, trimmed, cx)
            }
            PromptAction::DeleteEntry { endpoint, path, .. } => {
                self.files_delete(&endpoint, path, cx)
            }
            PromptAction::KillProcess {
                endpoint,
                pid,
                port,
                name,
                ..
            } => self.ports_kill(&endpoint, pid, port, name, cx),
            PromptAction::DiscardChanges {
                endpoint,
                repo,
                paths,
                kind,
                ..
            } => self.diff_discard(&endpoint, repo, paths, kind, cx),
            PromptAction::DeleteWorktree {
                endpoint,
                path,
                workspace_id,
                name,
                dirty,
                blockers,
            } => {
                let escalated = dirty || blockers.is_some();
                // The daemon's own TUI refuses when another open workspace or
                // pane lives inside the checkout before it force-removes; the
                // control API skips that guard under `force`, so this client
                // runs it first from the rows it already sees.
                let endpoint_key = endpoint.key();
                let endpoint_id = self.endpoint_by_key(&endpoint_key);
                // Both sides are canonicalized so a pane cwd spelled through a
                // symlink (`/tmp/..`) still counts as inside the checkout git
                // spells canonically (`/private/tmp/..`).
                let target = trims(&canonical(&path)).to_string();
                let mut blocker: Option<String> = None;
                for workspace in self.endpoint_workspaces(&endpoint_id) {
                    if workspace.id == workspace_id || blocker.is_some() {
                        continue;
                    }
                    let expanded = self.expand_cwd(&endpoint_key, &workspace.cwd);
                    let canonical_cwd = canonical(&expanded);
                    let candidate = trims(&canonical_cwd);
                    if inside(candidate, &target) {
                        blocker = Some(workspace.name.clone());
                    }
                }
                for agent in self.endpoint_agents(&endpoint_id) {
                    if agent.workspace_id == workspace_id || blocker.is_some() {
                        continue;
                    }
                    let expanded = self.expand_cwd(&endpoint_key, &agent.cwd);
                    let canonical_cwd = canonical(&expanded);
                    let candidate = trims(&canonical_cwd);
                    if inside(candidate, &target) {
                        blocker = Some(agent.name.clone());
                    }
                }
                if let Some(name) = blocker {
                    self.toast(
                        ToastKind::Error,
                        format!("close {name} first — it uses files inside this worktree"),
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
                let cleared = Some((endpoint_key.clone(), workspace_id.clone()));
                self.status = format!("deleting worktree {name}…");
                let ssh = match &endpoint {
                    EndpointId::Local => None,
                    EndpointId::Machine(id) => self
                        .machines
                        .iter()
                        .find(|machine| &machine.id == id)
                        .and_then(MachineInfo::ssh_target),
                };
                let tx = self.menu_tx.clone();
                std::thread::spawn(move || {
                    // The first confirm never forces: git is asked about
                    // uncommitted work (the daemon refuses a dirty checkout
                    // whatever `force` says) and the daemon about what still
                    // uses the worktree, and either finding holds the delete
                    // for a second, explicit yes. Only that second confirm
                    // discards work and forces past the guard. An unreadable
                    // state aborts instead of being taken for clean.
                    let outcome = (|| -> Result<Option<(bool, Option<String>)>, String> {
                        if escalated {
                            if dirty {
                                pilus_client::discard_worktree_changes(ssh.as_ref(), &path)?;
                            }
                            client
                                .remove_worktree(&path, true)
                                .map_err(|error| error.to_string())?;
                            return Ok(None);
                        }
                        let dirty_now = pilus_client::worktree_is_dirty(ssh.as_ref(), &path)?;
                        match client.remove_worktree(&path, false) {
                            Ok(()) => Ok(None),
                            Err(error) => match pilus_client::worktree_blockers(&error) {
                                Some(blockers) => Ok(Some((dirty_now, Some(blockers)))),
                                None if dirty_now => Ok(Some((true, None))),
                                None => Err(error.to_string()),
                            },
                        }
                    })();
                    let (held, error) = match outcome {
                        Ok(held) => (held, None),
                        Err(error) => (None, Some(error)),
                    };
                    let ok = match &held {
                        Some((_, Some(_))) => format!("{name} is still in use"),
                        Some((true, None)) => format!("{name} has uncommitted changes"),
                        _ => format!("deleted worktree {name}"),
                    };
                    let finished = held.is_none();
                    tx.send(MenuOpResult {
                        ok,
                        error,
                        renamed_workspace: None,
                        renamed_agent: None,
                        worktree_probe: None,
                        worktree_cleared: finished.then_some(cleared).flatten(),
                        worktree_escalated: held.map(|(dirty, blockers)| DeleteEscalation {
                            endpoint,
                            path,
                            workspace_id,
                            name,
                            dirty,
                            blockers,
                        }),
                    })
                    .ok();
                });
            }
            PromptAction::NewWorktree { endpoint, index } => {
                if trimmed.is_empty() {
                    self.toast(ToastKind::Error, "a branch name is required");
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
                self.status = format!("creating worktree {trimmed}…");
                let tx = self.menu_tx.clone();
                std::thread::spawn(move || {
                    let result = client.create_worktree_for(index, &trimmed);
                    let error = result.err().map(|error| error.to_string());
                    tx.send(MenuOpResult {
                        ok: format!("created a new worktree {trimmed}"),
                        error,
                        renamed_workspace: None,
                        renamed_agent: None,
                        worktree_probe: None,
                        worktree_cleared: None,
                        worktree_escalated: None,
                    })
                    .ok();
                });
            }
        }
        cx.notify();
    }

    /// Reflect a confirmed workspace rename in the projected labels, wherever
    /// the renamed endpoint's catalog lives (the active surface or a stashed
    /// endpoint's).
    fn rename_local_workspace_label(&mut self, workspace_id: &str, name: &str) {
        self.surface.rename_workspace(workspace_id, name);
        for endpoint in self.endpoints.values_mut() {
            endpoint.surface.rename_workspace(workspace_id, name);
        }
    }

    /// Reflect a confirmed agent alias in the AGENTS row, active or stashed.
    fn rename_local_agent(&mut self, endpoint_key: &str, pane: &str, alias: &str) {
        let endpoint = self.endpoint_by_key(endpoint_key);
        if endpoint == self.active {
            if let Some(agent) = self.agents.iter_mut().find(|agent| agent.pane == pane) {
                agent.name = alias.to_string();
            }
            return;
        }
        if let Some(endpoint) = self.endpoints.get_mut(&endpoint)
            && let Some(agent) = endpoint.agents.iter_mut().find(|agent| agent.pane == pane)
        {
            agent.name = alias.to_string();
        }
    }

    /// Build the native sidebar: nav, sessions, and workspaces.
    ///
    /// The daemon leaves the dock cells blank and reports their exact rectangle
    /// in `ShellDock`; this paints the same region from the `ShellWorkspaces`
    /// catalog, so the panel is native while staying column-aligned with the
    /// terminal grid behind it.
    fn sidebar(&self, cx: &mut Context<Self>, cell_width: f32, bottom: f32) -> Div {
        if !self.sidebar_visible && self.sidebar_anim.is_none() {
            return div();
        }
        let Some(dock) = self.sidebar_dock() else {
            return div();
        };
        let panel_w = f32::from(dock.width) * cell_width;

        // The scroll container stays a plain block. Making it a flex column
        // lets its children shrink to fit, which squeezes the nav rows; the
        // inner content carries the flex layout so it overflows and scrolls.
        // It insets by `SIDEBAR_ROW_EDGE` only, so every row is one hair off
        // the panel edge and insets its own content from there. It keeps the
        // full panel width while the panel tweens, so rows clip instead of
        // reflowing mid-slide.
        let mut content = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(panel_w))
            .px(px(SIDEBAR_ROW_EDGE))
            .pb(px(SIDEBAR_LIST_PAD));

        // One hover pill glides between rows, the way the tab strip's
        // selection does, instead of each row lighting up on its own.
        content = content.child(style::hover_glide(
            "pilus-sidebar-hover",
            &self.sidebar_hover,
            self.theme
                .sidebar_raise(self.glass_opacity(), SIDEBAR_HOVER_WEIGHT),
            style::RADIUS_MD,
            SIDEBAR_ROW_EDGE,
        ));
        // The selected row's raised fill is a second pill under the rows, so
        // changing the selection reads as motion, like the tab strip's.
        // Beneath the main pill: the active workspace's softer mark while an
        // agent row owns the selection.
        content = content.child(style::glide(
            "pilus-sidebar-context",
            &self.sidebar_context,
            self.theme
                .sidebar_raise(self.glass_opacity(), SIDEBAR_CONTEXT_WEIGHT),
            style::RADIUS_MD,
            SIDEBAR_ROW_EDGE,
        ));
        content = content.child(style::glide(
            "pilus-sidebar-selection",
            &self.sidebar_selection,
            self.theme.sidebar_raise(self.glass_opacity(), 1.0),
            style::RADIUS_MD,
            SIDEBAR_ROW_EDGE,
        ));
        content = content.child(self.open_workspace_row(cx));
        content = content.child(self.overview_nav_row(cx));
        content = content.child(self.search_nav_row(cx));
        content = content.child(self.stats_nav_row(cx));
        // sipil's own section leads the lists: the user's agents, people and
        // channels, with where the connection stands in its header.
        content = content.children(self.sipil_sections(cx));

        content = content.children(self.github_sidebar(cx));
        content = content.children(self.pinned_section(cx));
        content = content.child(self.workspaces_section(cx));
        content = content.child(self.agents_section(cx));

        let list = div()
            .id("pilus-sidebar-scroll")
            .flex_1()
            // Let the scroll area shrink below its content, so it scrolls
            // instead of pushing the panel taller than the window.
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll)
            .child(content);

        // The panel starts on row 0, covering the daemon's `« session Menu`
        // chrome so the sidebar shows only native sections, and runs the
        // full viewport height.
        let panel = div()
            .absolute()
            .left(px(f32::from(dock.x) * cell_width))
            .top(px(0.0))
            .w(px(panel_w * self.sidebar_open))
            .h(px(bottom))
            .flex()
            .flex_col()
            .bg(self.chrome_fill())
            .overflow_hidden()
            // Fixed clearance for the floating traffic lights; the sections
            // scroll under it instead of disappearing beneath the titlebar.
            // The session picker and collapse toggle pin to its right edge,
            // clear of the lights.
            .child(
                div()
                    .h(px(40.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(HEAD_CONTROLS_GAP))
                    .pr(px(SIDEBAR_HEAD_INSET))
                    .child(self.session_picker("pilus-session-button", cx))
                    .child(self.sidebar_toggle("pilus-sidebar-toggle", cx)),
            )
            .child(list);
        // The sidebar parts from the terminal with a hairline. It is an
        // overlay rather than a border: a border would take a pixel from the
        // panel's content box and leave the rows' right inset one short of
        // their left.
        // A split's tinted surface runs up to the sidebar, so it parts from
        // it by fill alone.
        panel.when(!self.is_split(), |panel| {
            panel.child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .right(px(0.0))
                    .w(px(1.0))
                    .bg(self.theme.border),
            )
        })
    }

    /// Focus a workspace tab.
    ///
    /// The pill updates immediately from the optimistic pick; `tab.list`
    /// confirms it a moment later, after the daemon has actually switched.
    /// A shortcut for a tab that does not exist (e.g. `Cmd+4` with three
    /// tabs) is ignored rather than selecting nothing.
    fn focus_tab(&mut self, index: u16, cx: &mut Context<Self>) {
        if !tab_exists(&self.tabs, index) {
            return;
        }
        // Moving to a tab leaves any page (and its split) covering the card.
        self.leave_settings_page();
        self.tab_sel = Some(index);
        self.tab_sel_at = Instant::now();
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.focus_tab(index);
        }
        cx.notify();
    }

    /// Whether a tab draws as active: an optimistic click wins briefly, then
    /// the daemon's own active flag. The window spans a remote click's SSH
    /// round trips; a local one confirms long inside it.
    fn tab_active(&self, tab: &Tab) -> bool {
        match self.tab_sel {
            Some(selected) if self.tab_sel_at.elapsed() < Duration::from_secs(4) => {
                tab.index == selected
            }
            _ => tab.active,
        }
    }

    /// The agent running on tab `index` in the active workspace, if any.
    ///
    /// `agent.list` tab positions repeat across workspaces, so a match must
    /// pin the workspace id too; a focused or working agent wins over another
    /// agent on the same tab.
    fn tab_agent(&self, index: u16) -> Option<&Agent> {
        let workspace_id = &self
            .surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)?
            .id;
        self.agents
            .iter()
            .filter(|agent| agent.workspace_id == *workspace_id && agent.tab == index)
            .max_by_key(|agent| (agent.focused, agent.status == "working"))
    }

    /// Open a new tab in the active workspace.
    fn new_tab(&mut self, cx: &mut Context<Self>) {
        // The new tab is the terminal surface, so a page covering it closes.
        self.leave_settings_page();
        cx.notify();
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.new_tab();
        }
    }

    /// Open a terminal on the local machine, wherever the surface is pointed.
    ///
    /// `tab.new` creates a neutral workspace and terminal at the user's home
    /// when none is open, so this is the local scratch shell even while a
    /// remote endpoint owns the visible surface.
    fn open_local_terminal(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.status = "opening a local terminal…".to_string();
        let socket = self.api_socket();
        let tx = self.picker_tx.clone();
        std::thread::spawn(move || {
            let result = ControlClient::connect(&socket)
                .and_then(|mut client| client.new_tab())
                .map_err(|error| error.to_string());
            tx.send(PickerEvent::LocalTerminalOpened(result)).ok();
        });
        cx.notify();
    }

    /// Split the focused pane; `right` is side by side, `down` is stacked.
    ///
    /// With Mission Control, Tasks, or Automations open there is no pane to
    /// split: the page shares the card with the terminal instead.
    fn split(&mut self, direction: &str, cx: &mut Context<Self>) {
        if let Some(axis) = SplitAxis::from_direction(direction)
            && self.split_page_open()
        {
            self.page_split = PageSplit::after(self.page_split(), axis);
            cx.notify();
            return;
        }
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.split(direction);
        }
    }

    /// Shot-harness entry point: split the open page like the chord does.
    #[cfg(feature = "visual-test")]
    pub(super) fn page_split_show(&mut self, direction: &str, cx: &mut Context<Self>) {
        self.split(direction, cx);
    }

    /// Close the active daemon tab, falling back to the last known one.
    fn close_active_tab(&mut self) {
        let index = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .or_else(|| self.tabs.last())
            .map(|tab| tab.index);
        if let Some(index) = index {
            // Closing the active tab hands focus to another, which a page
            // covering the card must not hide.
            if self.tabs.len() > 1 {
                self.leave_settings_page();
            }
            self.close_tab(index);
        }
    }

    /// Close a workspace tab.
    ///
    /// The daemon closes the whole workspace when its final tab closes, so the
    /// last tab is held back until explicit workspace close/delete exists.
    fn close_tab(&mut self, index: u16) {
        if self.tabs.len() <= 1 {
            self.toast(ToastKind::Info, "Can't close the last tab");
            return;
        }
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.close_tab(index);
        }
    }

    /// The native tab strip: terminal tabs and the new-tab
    /// action, with the sidebar toggle parked at its head while the sidebar
    /// is hidden.
    /// The sidebar collapse/expand button: pinned to the sidebar's top
    /// clearance while it is open, floating at the strip's head while it is
    /// hidden so the sidebar can always come back.
    fn sidebar_toggle(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let glass = self.glass_opacity();
        div()
            .id(id)
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT))
            .w(px(SIDEBAR_TOGGLE_W))
            .radius_md()
            .cursor_pointer()
            .hover_fill(
                theme.sidebar_raise(glass, SIDEBAR_HOVER_WEIGHT),
                style::RADIUS_MD,
            )
            .child(icon(
                include_bytes!("../../assets/icons/panel-left.svg"),
                14.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_sidebar(cx);
            }))
    }

    /// Open the rename prompt for the tab at `index`.
    fn open_tab_rename(&mut self, index: u16, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.index == index) else {
            return;
        };
        let (tab_id, name) = (tab.id.clone(), tab.name.clone());
        self.open_menu_prompt(
            PromptAction::RenameTab { tab_id },
            "Rename tab",
            "Name (empty shows the directory)",
            &name,
            cx,
        );
    }

    /// Move the tab at `from` to position `to`, as a drag-and-drop asks.
    ///
    /// The strip reorders at once and holds off the daemon's list until it
    /// reports the new order, so the dropped tab does not hop back for a
    /// refresh. Anything keyed by tab position (the selection, an agent's
    /// tab) follows the move.
    fn reorder_tab(&mut self, from: u16, to: u16, cx: &mut Context<Self>) {
        if from == to || !tab_exists(&self.tabs, from) || !tab_exists(&self.tabs, to) {
            return;
        }
        self.tabs_pick = Some(TabsPick {
            from: self.tabs.iter().map(|tab| tab.id.clone()).collect(),
            at: Instant::now(),
        });
        let now = Instant::now();
        let drawn: Option<Vec<_>> = self
            .tabs
            .iter()
            .map(|tab| {
                let bounds = self.tab_bounds.get(&tab.id)?;
                let offset = self.tab_offset(&tab.id, now);
                Some((
                    tab.id.clone(),
                    f32::from(bounds.origin.x),
                    f32::from(bounds.size.width),
                    offset,
                ))
            })
            .collect();
        for tab in &mut self.tabs {
            tab.index = moved_position(tab.index, from, to);
        }
        self.tabs.sort_by_key(|tab| tab.index);
        // A strip never drawn has nothing to slide from.
        self.tab_slide = drawn.map(|drawn| {
            let order: Vec<String> = self.tabs.iter().map(|tab| tab.id.clone()).collect();
            TabSlide {
                from: slide_starts(&drawn, &order),
                started: now,
            }
        });
        if let Some(selected) = self.tab_sel.as_mut() {
            *selected = moved_position(*selected, from, to);
        }
        if let Some(workspace_id) = self.active_workspace_id() {
            for agent in self
                .agents
                .iter_mut()
                .filter(|agent| agent.workspace_id == workspace_id)
            {
                agent.tab = moved_position(agent.tab, from, to);
            }
        }
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.move_tab(from, to);
        }
        cx.notify();
    }

    /// How far tab `id` is drawn from its layout slot right now; zero at rest.
    fn tab_offset(&self, id: &str, now: Instant) -> f32 {
        self.tab_slide
            .as_ref()
            .map_or(0.0, |slide| slide.offset(id, now))
    }

    fn top_chrome(
        &self,
        cx: &mut Context<Self>,
        cell_width: f32,
        right: f32,
        fullscreen: bool,
    ) -> Div {
        let theme = self.theme;

        // Nothing in the strip works without a daemon, so it leaves no bar
        // behind: the window's own base shows through where it would sit.
        if self.luvus_missing() {
            return div();
        }

        // Beside the sidebar when it is open; full width when it is hidden.
        // The left inset only clears the floating traffic lights, which
        // fullscreen hides, so the strip runs to the edge there.
        let left = self.sidebar_px(cell_width);
        let pad = self.strip_pad(fullscreen);
        let mut tabs = div()
            .id("pilus-tabs-scroll")
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_initial()
            .min_w_0()
            .overflow_x_scroll()
            .overflow_y_hidden()
            .track_scroll(&self.tab_scroll);
        // The raised fill is one pill that slides to the selected tab, so a
        // switch reads as motion from the old tab to the new one.
        let selection = style::GlideSlot::default();
        tabs = tabs.child(style::glide(
            "pilus-tab-glide",
            &selection,
            self.tab_raise(1.0),
            style::RADIUS_SOFT,
            0.0,
        ));
        for tab in &self.tabs {
            tabs = tabs.child(self.tab_item(tab, &selection, cx));
        }
        if self
            .tab_slide
            .as_ref()
            .is_some_and(|slide| slide.running(Instant::now()))
        {
            tabs = tabs.child(style::next_frame());
        }
        // Follow the active tab into view when it changes, not every frame,
        // so a manual scroll along the strip is left alone. Child 0 is the
        // glide pill, so a tab's child index is its position plus one.
        let active_tab = self
            .tabs
            .iter()
            .position(|tab| self.tab_active(tab))
            .map(|position| (position, self.tabs[position].index));
        if self.tab_scrolled.get() != active_tab.map(|(_, index)| index) {
            self.tab_scrolled.set(active_tab.map(|(_, index)| index));
            if let Some((position, _)) = active_tab {
                self.tab_scroll.scroll_to_item(position + 1);
            }
        }
        let new_tab = div()
            .id("pilus-new-tab")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT))
            .w(px(28.0))
            .radius_soft()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.muted)
            .hover_fill(self.tab_raise(SIDEBAR_HOVER_WEIGHT), style::RADIUS_SOFT)
            .hover(move |style| style.text_color(theme.panel_fg))
            .child(icon(
                include_bytes!("../../assets/icons/plus.svg"),
                13.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.new_tab(cx)));
        let mut tab_strip = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(TAB_STRIP_HEIGHT))
            .pl(px(pad))
            .pr(px(8.0));
        // The toggle lives in the sidebar while it is open. As the sidebar
        // leaves, a slot at the strip's head opens in step with it and the
        // toggle fades in, so the way back is there from the first frame and
        // the tabs glide aside instead of jumping when the tween settles.
        let parked = (1.0 - self.sidebar_open).clamp(0.0, 1.0);
        if parked > 0.0 {
            tab_strip = tab_strip.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .gap(px(HEAD_CONTROLS_GAP))
                    // The slot is the picker and toggle plus the strip's gap
                    // after them; the negative margin cancels the gap so a
                    // closed slot takes no room at all.
                    .w(px((HEAD_CONTROLS_W + 4.0) * parked))
                    .mr(px(-4.0))
                    .opacity(parked * parked)
                    .child(self.session_picker("pilus-session-button-parked", cx))
                    .child(self.sidebar_toggle("pilus-sidebar-toggle-parked", cx)),
            );
        }
        // The strip stays up under a page; any tab action closes the page.
        // The plus follows the tabs until overflow makes the scroll viewport
        // shrink.
        tab_strip = tab_strip.child(tabs).child(new_tab);
        // The right sidebar's toggle sits at the strip's far edge while the
        // column is away; open, the column's own close button replaces it.
        if self.right.openness() < 0.05 {
            tab_strip = tab_strip
                .child(div().flex_1())
                .child(self.right_toolbar(cx));
        }
        // An expanded right column covers the strip entirely.
        if self.right.expanded() && self.right.spread() >= 1.0 {
            return div();
        }
        div()
            .absolute()
            .left(px(left))
            .right(px(right))
            .top(px(0.0))
            .h(px(HEADER_HEIGHT))
            // Clip only while the column is stretching over the strip, so
            // the tab menus still reach outside it the rest of the time.
            .when(self.right.spread() > 0.0, |strip| strip.overflow_hidden())
            .flex()
            .flex_col()
            .justify_center()
            .bg(self.tab_strip_fill())
            .border_color(theme.border)
            .child(tab_strip)
    }

    fn tab_item(
        &self,
        tab: &Tab,
        selection: &style::GlideSlot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        // A lone tab has nothing to be told apart from, so it gets no raised
        // pill, but its label stays at full strength.
        let lone = self.tabs.len() == 1;
        let active = self.tab_active(tab) && !lone;
        let index = tab.index;
        let panes = tab.panes;
        let tab_id = tab.id.clone();
        let close_index = tab.index;
        let fg = if active || lone {
            theme.panel_fg
        } else {
            theme.muted
        };
        // The cwd is the label until the tab is named or its agent reports a
        // session title. Until `session.snapshot` enriches the tab, it is the
        // workspace's breadcrumb cwd.
        let cwd = if tab.cwd.is_empty() {
            self.crumb_path()
        } else {
            self.shorten_cwd(&self.active.key(), &tab.cwd)
        };
        let agent = self.tab_agent(index);
        let session_title = agent.and_then(|agent| agent.session_title.as_deref());
        let label = tab_label(&tab.name, session_title, &cwd, lone);
        let slide = if cx.reduce_motion() {
            0.0
        } else {
            self.tab_offset(&tab.id, Instant::now())
        };
        let drag = TabDrag {
            index,
            label: label.clone(),
            theme,
            font_size: self.footer_font(),
        };
        let process = if tab.process.is_empty() {
            "No terminal process"
        } else {
            tab.process.as_str()
        };
        let mut tip_lines = Vec::new();
        if !tab.name.trim().is_empty() {
            tip_lines.push((format!("Name: {}", tab.name.trim()), theme.muted));
        }
        tip_lines.push((format!("Directory: {cwd}"), theme.muted));
        tip_lines.push((format!("Process: {process}"), theme.muted));
        // An agent already has its own line below.
        if agent.is_none()
            && let Some(command) = running_command(&tab.title)
        {
            tip_lines.push((format!("Running: {command}"), theme.muted));
        }
        if let Some(agent) = agent {
            tip_lines.push((
                format!("Agent: {} · {}", agent.agent, status_label(&agent.status)),
                status_color(&agent.status, theme),
            ));
        }
        tip_lines.push((
            "Double-click to rename · drag to reorder".to_string(),
            theme.muted,
        ));
        let tip = row_tooltip(format!("Tab {index}"), fg, tip_lines, theme);
        let group = format!("pilus-tab-{index}");
        let drop_fill = self.tab_raise(0.6);
        let close = div()
            .id(format!("pilus-tab-close-{index}"))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_color(theme.muted)
            .opacity(0.0)
            .group_hover(group.clone(), |style| style.opacity(1.0))
            .hover(|style| style.text_color(theme.panel_fg))
            .child(icon(
                include_bytes!("../../assets/icons/x.svg"),
                10.0,
                theme.muted,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.close_tab(close_index);
                cx.notify();
            }));
        let tab = div()
            .id(format!("pilus-tab-{index}"))
            .group(group)
            .relative()
            .left(px(slide))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(TAB_HEIGHT))
            .flex_shrink_0()
            // A lone tab is as wide as its label; with company each keeps a
            // width of its own.
            .when(!lone, |tab| tab.min_w(px(140.0)).max_w(px(280.0)))
            // The tile sits near the tab's edge; the right side keeps room for
            // the close button.
            .pl(px(5.0))
            .pr_3()
            .radius_soft()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            // The selected tab's raised fill is the strip's glide pill;
            // hovering any other tab only hints at it.
            .hover_fill(
                self.tab_raise(if active { 0.0 } else { SIDEBAR_HOVER_WEIGHT }),
                style::RADIUS_SOFT,
            )
            .text_color(fg)
            .hover(move |style| style.text_color(theme.panel_fg))
            .tooltip(tip)
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            // The tab under a drag shows the pill it would take, unless it is
            // the one being dragged.
            .drag_over::<TabDrag>(move |style, drag, _, _| {
                if drag.index == index {
                    style
                } else {
                    style.bg(drop_fill)
                }
            })
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.reorder_tab(drag.index, index, cx);
            }));
        let mut content = div()
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .whitespace_nowrap();
        let (glyph, status) = match agent {
            Some(agent) => {
                // An idle agent keeps the plain tile, so the tab reads the
                // same as one with no agent.
                let wash = (agent.status != "idle").then(|| status_color(&agent.status, theme));
                (agent_icon(&agent.agent), wash)
            }
            None => (
                include_bytes!("../../assets/icons/terminal.svg").as_slice(),
                None,
            ),
        };
        content = content.child(style::glyph_tile(theme, glyph, panes, status));
        content = content.child(div().text_size(px(self.footer_font())).child(label));
        let tab = if active {
            tab.child(style::glide_target_shifted(selection, slide))
        } else {
            tab
        };
        let tab = tab.child(style::record_bounds(&self.tab_bounds, tab_id));
        tab.child(content).child(close).on_click(cx.listener(
            move |this, event: &ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.open_tab_rename(index, cx);
                } else {
                    this.focus_tab(index, cx);
                }
            },
        ))
    }

    /// Daemon columns hidden beneath our sidebar: the dock the daemon blanks
    /// for us. Read from the dock that matches the current frame, not the live
    /// `ShellDock` control message: the server updates the dock before sending
    /// the matching frame, so jumping the skip on the control message paints
    /// the old frame's blank dock cells as a black band mid-toggle.
    /// Daemon rows above the pane for the endpoint on screen. The local
    /// daemon and remote machines take separate settings, since they can run
    /// different Luvus versions.
    fn skipped_rows(&self) -> u16 {
        let top_spacing = match self.active {
            EndpointId::Local => self.settings.local_terminal_top_spacing,
            EndpointId::Machine(_) => self.settings.terminal_top_spacing,
        };
        skipped_rows(top_spacing)
    }

    fn skipped_cols(&self) -> u16 {
        if let Some((_, skip, _)) = &self.sidebar_freeze {
            return *skip;
        }
        match self.surface.frame_dock() {
            Some(dock) if dock.x == 0 => dock.width,
            _ => 0,
        }
    }

    /// Daemon columns the dock will hold once the toggle has landed. While
    /// the snapshot is held `skipped_cols` still reads the pre-toggle dock,
    /// so a size computed from it is off by the dock's width and the daemon
    /// reflows to it, then again when the live frame releases the freeze.
    fn settled_skip_cols(&self) -> u16 {
        if self.sidebar_freeze.is_none() {
            return self.skipped_cols();
        }
        if !self.sidebar_visible {
            return 0;
        }
        match self.sidebar_dock() {
            Some(dock) if dock.x == 0 => dock.width,
            _ => self.skipped_cols(),
        }
    }

    /// The frame the card paints: the sidebar's snapshot while it slides,
    /// otherwise the live one. Pointer mapping reads the same frame, so a
    /// click lands on what is drawn.
    fn shown_frame(&self) -> Option<&FrameData> {
        match &self.sidebar_freeze {
            Some((frame, _, _)) => Some(frame),
            None => self.surface.frame(),
        }
    }

    /// Hand the card back to the live grid once the slide has settled and
    /// the daemon has drawn a frame with the dock where the sidebar now is
    /// (or the wait ran long). Returns whether it swapped.
    fn release_sidebar_freeze(&mut self) -> bool {
        let Some((_, _, taken)) = &self.sidebar_freeze else {
            return false;
        };
        if self.sidebar_anim.is_some() {
            return false;
        }
        let live_skip = match self.surface.frame_dock() {
            Some(dock) if dock.x == 0 => dock.width,
            _ => 0,
        };
        let landed = (live_skip > 0) == self.sidebar_visible;
        if !landed && taken.elapsed() < SIDEBAR_FREEZE_MAX {
            return false;
        }
        self.sidebar_freeze = None;
        true
    }

    /// The dock the sidebar paints against: the live one while the daemon
    /// reports it, otherwise the last one it reported. Hiding the sidebar
    /// makes the daemon drop the dock, so a live-only rectangle would vanish
    /// mid-collapse and snap the panel and terminal shut.
    fn sidebar_dock(&self) -> Option<ShellDockRect> {
        self.surface.shell_dock().or(self.last_sidebar_dock)
    }

    /// The sidebar's width in cells, or zero when it is hidden.
    fn sidebar_cells(&self) -> u16 {
        // Full width regardless of visibility: `sidebar_factor` folds the
        // collapse tween in at each surface, so all of them glide together.
        if self.luvus_missing() {
            return 0;
        }
        self.sidebar_dock().map_or(0, |dock| dock.width)
    }

    /// The session picker, an icon alone in the header just before the
    /// sidebar toggle: the current session's mapped icon, opening the session
    /// menu below it. The name lives in the tooltip and the menu.
    fn session_picker(&self, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let glass = self.glass_opacity();
        let session = self.current_session_name();
        div()
            .id(id)
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
                if self.session_menu_open {
                    theme.sidebar_raise(glass, SIDEBAR_HOVER_WEIGHT)
                } else {
                    gpui::Rgba::default()
                },
                theme.sidebar_raise(glass, SIDEBAR_HOVER_WEIGHT),
                style::RADIUS_MD,
            )
            .child(icon(
                session_icon_glyph(self.settings.session_icon(session)),
                14.0,
                theme.muted,
            ))
            .tooltip(row_tooltip(
                format!("Session · {}", self.settings.session_label(session)),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.session_menu_open {
                    this.close_session_menu();
                } else {
                    this.session_menu_open = true;
                }
                cx.notify();
            }))
    }

    /// A session row's leading icon, which is also the button that unfolds
    /// the icon choices beneath the row. A stopped session's icon dims with
    /// its name.
    fn session_icon_button(
        &self,
        name: &str,
        running: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let target = name.to_string();
        div()
            .id(format!("pilus-menu-session-icon-{name}"))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(22.0))
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(icon(
                session_icon_glyph(self.settings.session_icon(name)),
                14.0,
                if running { theme.panel_fg } else { theme.muted },
            ))
            .tooltip(row_tooltip(
                format!("Change icon · {name}"),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.session_confirm = None;
                this.session_icon_editing = if this.session_icon_editing.as_deref() == Some(&target)
                {
                    None
                } else {
                    Some(target.clone())
                };
                cx.stop_propagation();
                cx.notify();
            }))
    }

    /// The strip of icons a session may take, unfolded under its menu row.
    /// The chosen one wears the panel fill, like a selected sidebar row.
    fn session_icon_choices(&self, name: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let chosen = self.settings.session_icon(name);
        let mut strip = div().flex().flex_row().flex_wrap().gap_1().px_2().py_1();
        for key in SESSION_ICONS {
            let target = name.to_string();
            let selected = key == chosen;
            strip = strip.child(
                div()
                    .id(format!("pilus-menu-session-icon-{name}-{key}"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(28.0))
                    .h(px(28.0))
                    .radius_md()
                    .cursor_pointer()
                    .state_fill(
                        if selected {
                            theme.panel
                        } else {
                            gpui::Rgba::default()
                        },
                        theme.panel,
                        style::RADIUS_MD,
                    )
                    .child(icon(
                        session_icon_glyph(key),
                        14.0,
                        if selected {
                            theme.panel_fg
                        } else {
                            theme.muted
                        },
                    ))
                    .tooltip(row_tooltip(
                        key.to_string(),
                        theme.panel_fg,
                        Vec::new(),
                        theme,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.set_session_icon(&target, key);
                        this.save_settings();
                        this.session_icon_editing = None;
                        cx.stop_propagation();
                        cx.notify();
                    })),
            );
        }
        strip
    }

    /// The pencil on a session row. The label is pilus's own, so it works on
    /// running and stopped sessions alike.
    fn session_rename_button(&self, name: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let target = name.to_string();
        div()
            .id(format!("pilus-menu-session-rename-{name}"))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(22.0))
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(icon(
                include_bytes!("../../assets/icons/pencil.svg"),
                13.0,
                theme.muted,
            ))
            .tooltip(row_tooltip(
                format!("Rename session (pilus only) · {name}"),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_session_rename(&target, cx);
                cx.stop_propagation();
            }))
    }

    /// Ask for a display label for `name`, closing the session menu first so
    /// the prompt is the only thing taking input.
    fn open_session_rename(&mut self, name: &str, cx: &mut Context<Self>) {
        self.close_session_menu();
        let current = self.settings.session_label(name).to_string();
        self.open_menu_prompt(
            PromptAction::RenameSession {
                name: name.to_string(),
            },
            "Rename session",
            "Shown in pilus only; the session keeps its name. Leave empty to restore it.",
            &current,
            cx,
        );
    }

    /// Persist the settings and share them with every window.
    fn save_settings(&mut self) {
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
    }

    /// The trailing end of a session row: its status, or the stop/delete
    /// control, which asks for a second press before it acts.
    fn session_row_tail(
        &self,
        session: &SessionInfo,
        action: Option<SessionVerb>,
        confirming: Option<SessionVerb>,
        working: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let label = |text: &'static str, color| {
            div()
                .text_size(px(self.footer_font()))
                .text_color(color)
                .child(text)
        };
        if working {
            return div().child(label("working…", theme.muted));
        }
        let Some(verb) = action else {
            return div().child(label(
                if session.running {
                    "running"
                } else {
                    "stopped"
                },
                theme.muted,
            ));
        };
        let name = session.name.clone();
        let run = cx.listener(move |this, _, _, cx| {
            this.run_session_verb(&name, verb);
            cx.stop_propagation();
            cx.notify();
        });
        if confirming == Some(verb) {
            return div().child(
                style::small_button(
                    format!("pilus-menu-session-confirm-{}", session.name),
                    match verb {
                        SessionVerb::Stop => "Stop?",
                        SessionVerb::Delete => "Delete?",
                    },
                    style::ButtonKind::Danger,
                    theme,
                    self.footer_font(),
                )
                .on_click(run),
            );
        }
        let name = session.name.clone();
        let (glyph, tip): (&'static [u8], &str) = match verb {
            SessionVerb::Stop => (
                include_bytes!("../../assets/icons/square-fill.svg"),
                "Stop session",
            ),
            SessionVerb::Delete => (
                include_bytes!("../../assets/icons/trash-2.svg"),
                "Delete session",
            ),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(label(
                if session.running {
                    "running"
                } else {
                    "stopped"
                },
                theme.muted,
            ))
            .child(
                div()
                    .id(format!(
                        "pilus-menu-session-{}-{}",
                        verb.verb(),
                        session.name
                    ))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(22.0))
                    .h(px(22.0))
                    .radius_md()
                    .cursor_pointer()
                    .hover_fill(theme.panel_dim, style::RADIUS_MD)
                    .child(icon(glyph, 13.0, theme.muted))
                    .tooltip(row_tooltip(
                        tip.to_string(),
                        theme.panel_fg,
                        Vec::new(),
                        theme,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session_confirm = Some((name.clone(), verb));
                        cx.stop_propagation();
                        cx.notify();
                    })),
            )
    }

    /// The session menu: switch sessions or start a fresh one.
    ///
    /// A transparent catcher covers the content area so a click anywhere
    /// outside dismisses; the panel stops propagation so its own clicks land.
    /// It opens below the session picker in the header, left-aligned with it,
    /// wherever the picker sits: the sidebar's head or the tab strip's.
    fn session_menu(
        &self,
        cx: &mut Context<Self>,
        cell_width: f32,
        fullscreen: bool,
    ) -> AnyElement {
        if !self.session_menu_open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let picker_left = if self.sidebar_visible {
            let sidebar_left = self
                .sidebar_dock()
                .map_or(0.0, |dock| f32::from(dock.x) * cell_width);
            sidebar_left + f32::from(self.sidebar_cells()) * cell_width
                - SIDEBAR_HEAD_INSET
                - HEAD_CONTROLS_W
        } else {
            self.strip_pad(fullscreen)
        };
        let menu_left = picker_left.max(SIDEBAR_HEAD_INSET);
        let menu_top = (HEADER_HEIGHT + TAB_HEIGHT) / 2.0 + SESSION_MENU_GAP;
        let mut items = div().flex().flex_col().gap_1();
        let current = self.current_session_name().to_string();
        for session in &self.sessions {
            let name = session.name.clone();
            let is_current = session.name == current;
            let working = self.session_working.contains(&session.name);
            let bg = if is_current {
                theme.panel
            } else {
                theme.sidebar_bg
            };
            let confirming = self
                .session_confirm
                .as_ref()
                .filter(|(pending, _)| *pending == session.name)
                .map(|(_, verb)| *verb);
            let action = session_ops::offered(session, &current, &self.sessions);
            let mut row = div()
                .id(format!("pilus-menu-session-{}", session.name))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(28.0))
                .px_2()
                .radius_md()
                .cursor_pointer()
                .state_fill(bg, theme.panel, style::RADIUS_MD)
                .text_size(px(self.chrome_font()))
                .child(self.session_icon_button(&session.name, session.running, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(if session.running {
                            theme.panel_fg
                        } else {
                            theme.muted
                        })
                        .child(self.settings.session_label(&session.name).to_string()),
                )
                .child(self.session_rename_button(&session.name, cx));
            row = row.child(
                div()
                    .flex_shrink_0()
                    .child(self.session_row_tail(session, action, confirming, working, cx)),
            );
            let editing_icon = self.session_icon_editing.as_deref() == Some(session.name.as_str());
            let session_name = session.name.as_str();
            items = items.child(row.on_click(cx.listener(move |this, _, _, cx| {
                // A press away from a pending confirmation only cancels it.
                if this.session_confirm.take().is_some() {
                    cx.notify();
                    return;
                }
                let running = this
                    .sessions
                    .iter()
                    .any(|session| session.name == name && session.running);
                this.close_session_menu();
                if running {
                    this.switch_session(&name);
                } else {
                    this.start_session(name.clone());
                }
                cx.notify();
            })));
            if editing_icon {
                items = items.child(self.session_icon_choices(session_name, cx));
            }
        }
        items = items.child(
            div()
                .id("pilus-menu-new-session")
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
                .text_color(theme.panel_fg)
                .child(icon(
                    include_bytes!("../../assets/icons/plus.svg"),
                    14.0,
                    theme.muted,
                ))
                .child("New session")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.start_new_session();
                    cx.notify();
                })),
        );
        style::enter(
            "pilus-session-menu-enter",
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
                        .w(px(264.0))
                        .radius_md()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .p_2()
                        .child(items)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                // Stop the press here: it would otherwise reach the picker
                // beneath, whose click handler reopens the menu this just
                // closed, so a second click on the picker could never close it.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_session_menu();
                        cx.stop_propagation();
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// The focused pane's corner buttons: split either way, zoom the pane
    /// (the daemon's `pane.zoom`, the same toggle as prefix + z), and close
    /// the pane. `render_grid` pins it to the pane.
    fn pane_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let size = style::scaled(&self.shared_settings, style::PANE_TOOLBAR_BUTTON);
        let button =
            |id: &'static str, glyph: &'static [u8], kind: style::ButtonKind, tip: &str| {
                style::icon_button(id, glyph, kind, theme, size).tooltip(row_tooltip(
                    tip.to_string(),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
            };
        // The frame says so itself, which also covers a remote machine and a
        // pane zoomed before we attached; the feed's event is the fallback.
        let zoomed =
            self.focused_zoomed || self.shown_frame().and_then(zoom_badge_column).is_some();
        let (zoom_icon, zoom_kind, zoom_tip): (&'static [u8], _, _) = if zoomed {
            (
                include_bytes!("../../assets/icons/minimize.svg"),
                style::ButtonKind::Primary,
                "Restore pane",
            )
        } else {
            (
                include_bytes!("../../assets/icons/maximize.svg"),
                style::ButtonKind::Ghost,
                "Zoom pane",
            )
        };
        style::pane_toolbar()
            .child(
                button(
                    "pilus-pane-split-right",
                    include_bytes!("../../assets/icons/columns-2.svg"),
                    style::ButtonKind::Ghost,
                    "Split right",
                )
                .on_click(cx.listener(|this, _, _, cx| this.split("right", cx))),
            )
            .child(
                button(
                    "pilus-pane-split-down",
                    include_bytes!("../../assets/icons/rows-2.svg"),
                    style::ButtonKind::Ghost,
                    "Split down",
                )
                .on_click(cx.listener(|this, _, _, cx| this.split("down", cx))),
            )
            .child(
                button("pilus-pane-zoom", zoom_icon, zoom_kind, zoom_tip).on_click(cx.listener(
                    |this, _, _, cx| {
                        let endpoint = this.active.clone();
                        // No toast: the pane filling the tab is the answer.
                        this.pane_op(
                            &endpoint,
                            String::new(),
                            |client, pane| client.zoom_pane(pane).map(|_| ()),
                            cx,
                        );
                    },
                )),
            )
            .child(
                button(
                    "pilus-pane-close",
                    include_bytes!("../../assets/icons/x.svg"),
                    style::ButtonKind::Danger,
                    "Close pane",
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let endpoint = this.active.clone();
                    this.pane_op(
                        &endpoint,
                        "pane closed".to_string(),
                        |client, pane| client.close_pane(pane),
                        cx,
                    );
                })),
            )
    }

    /// One selectable row of the context menu or its flyout. The caller adds
    /// the click and hover handlers.
    fn context_menu_row(
        &self,
        id: String,
        item: SidebarItem,
        active: bool,
        paths: bool,
        scoped: bool,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let color = if item.destructive() {
            theme.alert
        } else {
            theme.panel_fg
        };
        let mut label = item.label(paths, scoped).to_string();
        if item == SidebarItem::GithubAll {
            label.push_str(&format!(" · {}", self.github.repos.items.len()));
        }
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(26.0))
            .px_2()
            .radius_md()
            .cursor_pointer()
            .state_fill(
                if active {
                    theme.panel
                } else {
                    theme.window_bg.opacity(0.0)
                },
                theme.panel,
                style::RADIUS_MD,
            )
            .text_size(px(self.chrome_font()))
            .text_color(color)
            .child(icon(sidebar_menu_icon(item), 15.0, color))
            .child(div().flex_1().min_w_0().truncate().child(label))
    }

    /// The right-click context menu for a WORKSPACES or AGENTS row.
    ///
    /// A full-window catcher occludes the grid and sidebar while the menu is
    /// up; the panel stops propagation so its own rows land. The keyboard
    /// cursor (`selected`) and mouse hover share the panel pill, and the
    /// menu clamps to the viewport so a row opened at the window's edge never
    /// draws off-screen.
    fn context_menu_overlay(&self, cx: &mut Context<Self>, window: &Window) -> AnyElement {
        let Some(menu) = self.context_menu.as_ref() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let rows = self.menu_rows();
        let (paths, scoped) = match &menu.target {
            SidebarMenuTarget::Workspace { .. } => (self.sidebar_workspace_paths, false),
            SidebarMenuTarget::Agent { .. } => {
                (self.sidebar_agent_paths, self.agents_this_workspace)
            }
            SidebarMenuTarget::Saved { .. }
            | SidebarMenuTarget::Terminal { .. }
            | SidebarMenuTarget::Github
            | SidebarMenuTarget::File { .. } => (false, false),
        };

        let quick_rows = if menu.quick_open {
            menu.quick_rows()
        } else {
            Vec::new()
        };
        let mut items = div().flex().flex_col().gap_0();
        for (index, item) in rows.iter().copied().enumerate() {
            if item == SidebarItem::Divider {
                items = items.child(div().mx_2().my_1().h(px(1.0)).bg(theme.border.opacity(0.5)));
                continue;
            }
            let parent = item == SidebarItem::QuickActions;
            // The parent row keeps its pill while the flyout is out, so the
            // pointer can see which row the flyout belongs to.
            let active = menu.selected == Some(index)
                || (parent && menu.quick_open && menu.quick_selected.is_none());
            items = items.child(
                self.context_menu_row(
                    format!("pilus-sidebar-menu-{index}"),
                    item,
                    active,
                    paths,
                    scoped,
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if !*hovered {
                        return;
                    }
                    // Hovering the parent row slides the flyout out;
                    // hovering any other row folds it away.
                    let Some(menu) = this.context_menu.as_mut() else {
                        return;
                    };
                    if menu.quick_open != parent || menu.quick_selected.is_some() {
                        menu.quick_open = parent;
                        menu.quick_selected = None;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_context_item(item, cx);
                })),
            );
        }

        // Anchor at the cursor, flipping to the left and above when the panel
        // would cross the opposite edges.
        let width = 224.0f32;
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);
        let mut left = menu.column;
        if left + width + 8.0 > viewport_width {
            left = (viewport_width - width - 8.0).max(8.0);
        }
        let height = sidebar_menu::panel_height(&rows);
        let mut top = menu.row + 6.0;
        if top + height > viewport_height {
            top = (menu.row - height - 6.0).max(8.0);
        }

        let panel = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .radius_md()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(theme.border)
            .p_1()
            .child(items)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            );

        // The Quick Actions flyout sits beside the panel, level with its
        // parent row, and flips to the left edge when the right has no room.
        let flyout = rows
            .iter()
            .position(|item| *item == SidebarItem::QuickActions)
            .filter(|_| !quick_rows.is_empty())
            .map(|parent| {
                let fly_width = 168.0f32;
                let gap = 2.0f32;
                let mut fly_left = left + width + gap;
                if fly_left + fly_width + 8.0 > viewport_width {
                    fly_left = (left - fly_width - gap).max(8.0);
                }
                let fly_height = sidebar_menu::panel_height(&quick_rows);
                let fly_top = (top + sidebar_menu::row_offset(&rows, parent) - 5.0)
                    .min(viewport_height - fly_height - 8.0)
                    .max(8.0);
                let mut fly_items = div().flex().flex_col().gap_0();
                for (index, item) in quick_rows.iter().copied().enumerate() {
                    fly_items = fly_items.child(
                        self.context_menu_row(
                            format!("pilus-sidebar-quick-{index}"),
                            item,
                            menu.quick_selected == Some(index),
                            paths,
                            scoped,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run_context_item(item, cx);
                        })),
                    );
                }
                div()
                    .absolute()
                    .left(px(fly_left))
                    .top(px(fly_top))
                    .w(px(fly_width))
                    .radius_md()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .p_1()
                    .child(fly_items)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
            });

        style::enter(
            "pilus-context-menu-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .occlude()
                .child(panel)
                .children(flyout)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_context_menu();
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.close_context_menu();
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// The rename/alias/branch prompt a context-menu action opened.
    ///
    /// Same shape as the palette: a dimmed catcher over everything and a
    /// centered field panel that stops propagation so its own clicks land.
    fn menu_prompt_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(prompt) = self.menu_prompt.as_ref() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        // A destructive confirm swaps the field for the target path, a warning,
        // and the Enter/Esc hint, so nobody deletes by typing past it.
        let confirm = prompt.action.is_confirm();
        // What a Files-tab delete takes with it, since it has no worktree to
        // name: a folder takes everything inside.
        let entry_note = match &prompt.action {
            PromptAction::DeleteEntry { folder: true, .. } => {
                Some("Deletes the folder and everything in it. This can't be undone.")
            }
            PromptAction::DeleteEntry { .. } => Some("Deletes the file. This can't be undone."),
            PromptAction::KillProcess { .. } => {
                Some("Stops the process. Anything it hasn't saved is lost.")
            }
            PromptAction::DiscardChanges {
                kind: pilus_client::Discard::Untracked,
                ..
            } => Some("Deletes the file, which Git has no copy of. This can't be undone."),
            PromptAction::DiscardChanges { .. } => {
                Some("Reverts the file to its staged state. Your edits are lost for good.")
            }
            _ => None,
        };
        let confirm_escalated = prompt.action.escalated();
        let confirm_dirty = matches!(
            prompt.action,
            PromptAction::DeleteWorktree { dirty: true, .. }
        );
        let confirm_blockers = match &prompt.action {
            PromptAction::DeleteWorktree { blockers, .. } => blockers.clone(),
            _ => None,
        };
        let confirm_path = match &prompt.action {
            PromptAction::DeleteWorktree { path, .. } | PromptAction::DeleteEntry { path, .. } => {
                Some(path.clone())
            }
            PromptAction::KillProcess { label, .. }
            | PromptAction::DiscardChanges { label, .. } => Some(label.clone()),
            _ => None,
        };
        let input = self.menu_prompt_input.clone();
        input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(self.chrome_font()),
            );
        });
        let mut panel = div()
            .w(px(380.0))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .radius_md()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(theme.border)
            .child(style::title(
                theme,
                prompt.title,
                style::Fonts::resolve(&self.shared_settings).title,
            ))
            .children((!prompt.placeholder.is_empty()).then(|| {
                div()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(prompt.placeholder)
            }));
        if confirm {
            panel = panel
                .child(match confirm_path {
                    Some(path) => div()
                        .font_family(style::mono_family(&self.shared_settings))
                        .text_size(px(self.section_font()))
                        .text_color(theme.muted)
                        .truncate()
                        .child(path),
                    None => div(),
                })
                .children(confirm_blockers.map(|blockers| {
                    div()
                        .text_size(px(self.chrome_font()))
                        .text_color(theme.alert)
                        .child(format!("Still in use: {blockers}. Deleting stops it."))
                }))
                .children(confirm_dirty.then(|| {
                    div()
                        .text_size(px(self.chrome_font()))
                        .text_color(theme.alert)
                        .child("It has uncommitted or untracked changes that will be lost.")
                }))
                .child(
                    div()
                        .text_size(px(self.chrome_font()))
                        .text_color(theme.alert)
                        .child(match (entry_note, confirm_escalated) {
                            (Some(note), _) => note,
                            (None, true) => "This can't be undone.",
                            (None, false) => {
                                "Removes the checkout and its files. This can't be undone."
                            }
                        }),
                )
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .text_color(theme.muted)
                        .child(match (&prompt.action, confirm_escalated) {
                            (PromptAction::KillProcess { .. }, _) => "⏎ Kill · Esc Cancel",
                            (PromptAction::DiscardChanges { .. }, _) => "⏎ Discard · Esc Cancel",
                            (_, true) => "⏎ Delete anyway · Esc Cancel",
                            (_, false) => "⏎ Delete · Esc Cancel",
                        }),
                );
        } else {
            panel = panel.child(input);
        }
        let panel = panel
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            );

        style::enter(
            "pilus-menu-prompt-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .occlude()
                .bg(theme.window_bg.opacity(0.55))
                .flex()
                .items_center()
                .justify_center()
                .child(panel)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.menu_prompt = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.menu_prompt = None;
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// The ⌘⇧P overlay: a query field above a filtered command list.
    ///
    /// A transparent catcher covers the window so a click outside the panel
    /// dismisses it; the panel stops propagation so its own clicks land.
    /// The toasts, stacked newest-last at the window's bottom-right.
    ///
    /// The stack hugs its cards, so the margin around them stays clickable.
    /// Each card fades in through `style::enter` keyed by its id, and fades
    /// out through the queue's own exit opacity (the enter animation would
    /// otherwise pin the card's opacity), so the wrapper carries the exit.
    fn toast_stack(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self.toasts.items();
        if items.is_empty() {
            return div().into_any_element();
        }
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let now = Instant::now();
        let mut stack = div()
            .absolute()
            .right(px(12.0))
            .bottom(px(12.0))
            .flex()
            .flex_col()
            .items_end()
            .gap_2();
        for toast in items {
            let id = toast.id;
            let card = style::toast(
                SharedString::from(format!("pilus-toast-{id}")),
                toast.kind,
                toast.title.as_deref(),
                &toast.body,
                theme,
                fonts,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toasts.dismiss(id, Instant::now());
                cx.notify();
            }));
            stack = stack.child(
                div()
                    .opacity(toast.exit_opacity(now))
                    .child(style::enter(format!("pilus-toast-enter-{id}"), card)),
            );
        }
        stack.into_any_element()
    }

    fn palette_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.palette.is_open() {
            return div().into_any_element();
        }
        let theme = self.theme;
        let entries = self.palette_entries();
        let matches = self.palette.matches(&entries);
        let selected = self.palette.selected().min(matches.len().saturating_sub(1));

        let mut rows = div()
            .id("pilus-palette-list")
            .flex()
            .flex_col()
            .p_1()
            .max_h(px(360.0))
            .overflow_y_scroll()
            .track_scroll(&self.palette_scroll);
        if matches.is_empty() {
            rows = rows.child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No matching commands"),
            );
        }
        for (index, entry) in matches.iter().enumerate() {
            let active = index == selected;
            let command = entry.command.clone();
            // The highlight borrows the active theme's selection colors, so the
            // palette reads as part of the scheme rather than a fixed panel.
            let (bg, fg, dim) = if active {
                (
                    theme.selection_bg,
                    theme.selection_fg,
                    theme.selection_fg.opacity(0.75),
                )
            } else {
                (theme.sidebar_bg, theme.panel_fg, theme.muted)
            };
            rows = rows.child(
                div()
                    .id(format!("pilus-palette-{index}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .h(px(30.0))
                    .px_3()
                    .radius_md()
                    .state_fill(
                        bg,
                        if active {
                            theme.selection_bg
                        } else {
                            theme.panel
                        },
                        style::RADIUS_MD,
                    )
                    .cursor_pointer()
                    .text_size(px(self.chrome_font()))
                    .text_color(fg)
                    .child(div().flex_1().child(entry.title.clone()))
                    .child(
                        div()
                            .text_size(px(self.section_font()))
                            .text_color(dim)
                            .child(entry.group),
                    )
                    .child(match entry.shortcut {
                        Some(shortcut) => div()
                            .text_size(px(self.footer_font()))
                            .text_color(dim)
                            .child(shortcut.to_string()),
                        None => div(),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.palette.close();
                        this.run_command(command.clone(), window, cx);
                        cx.notify();
                    })),
            );
        }

        let palette_input = self.palette_input.clone();
        palette_input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(self.chrome_font()),
            );
        });
        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(44.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(self.chrome_font()))
            .child(palette_input);

        style::enter(
            "pilus-palette-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .occlude()
                .bg(theme.window_bg.opacity(0.55))
                .flex()
                .justify_center()
                .pt(px(HEADER_HEIGHT + 12.0))
                .child(
                    div()
                        .w(px(560.0))
                        .max_h(px(420.0))
                        .flex()
                        .flex_col()
                        .radius_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(field)
                        .child(rows)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.palette.close();
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
    }

    /// One workspace row: the name on the first line, the working directory on
    /// the second. The active workspace gets the pill.
    ///
    /// `host` is the machine name for a remote row, kept for the tooltip and
    /// the leading server glyph; the row itself stays unlabelled. `depth` is
    /// how far the row hangs below its machine: 0 is the machine's own row, 1 a
    /// workspace under it and 2 a worktree under that workspace. Anything
    /// deeper than the machine row swaps its icon for the elbow. `foldable`
    /// gives a machine's row the chevron that folds the workspaces below it.
    fn workspace_row(
        &self,
        endpoint: &EndpointId,
        workspace: &ShellWorkspace,
        host: Option<&str>,
        depth: usize,
        foldable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        // A folded machine keeps the pill when the workspace hidden below it
        // is the active one, so the selection never disappears.
        let hidden_active = foldable
            && self.machine_folded(endpoint)
            && self
                .endpoint_workspaces(endpoint)
                .iter()
                .any(|other| other.active);
        let active = self.endpoint_selected(endpoint) && (workspace.active || hidden_active);
        let remote = endpoint.machine_id().is_some();
        let selected = active;
        // A focused agent row takes the full pill; the workspace keeps a
        // subtler one rather than going unmarked.
        let pill = if self.agent_pill_shown() {
            &self.sidebar_context
        } else {
            &self.sidebar_selection
        };
        let cwd = self.shorten_cwd(&endpoint.key(), &workspace.cwd);
        // The home workspace is named for the machine it lives on, since its
        // own name is just the user's directory.
        let title = if cwd == "~" && !workspace.nested {
            self.endpoint_label(endpoint)
        } else {
            workspace.name.clone()
        };
        let mut lines = vec![(format!("Directory: {cwd}"), theme.muted)];
        if let Some(branch) = workspace.branch.clone() {
            lines.push((format!("Branch: {branch}"), theme.branch));
        }
        if let Some(host) = host {
            lines.push((format!("Host: {host}"), theme.muted));
        }
        let tip = row_tooltip(title.clone(), theme.panel_fg, lines, theme);

        let mut head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w_0()
            .text_size(px(self.chrome_font()));
        {
            // Every leading glyph wears the shared box, so the workspace name
            // lands on the same column as the nav and agent labels.
            let glyph = |bytes: &'static [u8], size: f32, color: gpui::Rgba| {
                div()
                    .w(px(SIDEBAR_GLYPH))
                    .flex_shrink_0()
                    .child(icon(bytes, size, color))
            };
            head = head.child(if depth > 0 {
                // A row below its machine (or a worktree below its workspace)
                // hangs off the row above it: an elbow ("|_") says so in place
                // of the indent a deeper row needs.
                div()
                    .w(px(SIDEBAR_GLYPH))
                    .h(px(SIDEBAR_GLYPH))
                    .flex_shrink_0()
                    .child(
                        div()
                            .ml(px(2.0))
                            .mt(px(-2.5))
                            .w(px(8.0))
                            .h(px(13.0))
                            .border_l_1()
                            .border_b_1()
                            .border_color(theme.muted.opacity(0.5))
                            .rounded_bl(style::Radius::Sm.pixels() * 0.75),
                    )
                    .into_any_element()
            } else if remote {
                glyph(
                    include_bytes!("../../assets/icons/server.svg"),
                    SIDEBAR_GLYPH,
                    theme.muted,
                )
                .into_any_element()
            } else {
                glyph(
                    include_bytes!("../../assets/icons/folder.svg"),
                    SIDEBAR_GLYPH,
                    theme.muted,
                )
                .into_any_element()
            });
        }
        // The name owns the first line. The machine name stays out of the
        // row (the tooltip names it), so nothing competes with the label.
        let name = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(theme.panel_fg);
        head = head.child(name.child(title));
        if foldable {
            head = head.child(self.machine_fold_toggle(endpoint, cx));
        }

        // Second line: the path grows and truncates; the branch rides at the
        // far right, where it competes with the path instead of the name.
        // A branch repeating the workspace's own name is dropped: it is the
        // label twice, and the path would pay for it. Nested rows skip it as
        // well — the tooltip still names it.
        let mut sub = div().flex().flex_row().items_center().gap_2().min_w_0();
        sub = sub.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(style::mono_family(&self.shared_settings))
                .text_size(px(self.section_font()))
                .text_color(theme.muted)
                .child(cwd),
        );
        if !workspace.nested
            && let Some(branch) = workspace.branch.clone()
            && branch != workspace.name
        {
            sub = sub.child(
                div()
                    .flex_shrink_0()
                    .font_family(style::mono_family(&self.shared_settings))
                    .text_size(px(self.section_font()))
                    .text_color(theme.branch)
                    .child(branch),
            );
        }
        let mut row = div()
            .id(format!(
                "pilus-workspace-{}-{}",
                endpoint.key(),
                workspace.index
            ))
            .flex()
            .flex_col()
            .justify_center()
            .gap_1()
            .pl(px(SIDEBAR_ROW_PAD))
            // The branch and host badges keep the row's content inset.
            .pr(px(SIDEBAR_ROW_PAD))
            .py_1()
            .radius_md()
            .cursor_pointer()
            .children(selected.then(|| style::glide_target(pill)))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            .child(head);
        // The path line follows the daemon's Show Path / Hide Path toggle.
        if self.sidebar_workspace_paths {
            row = row.child(sub);
        }
        if depth > 0 {
            row = row.pl(px(SIDEBAR_ROW_PAD + SIDEBAR_NEST_INDENT * depth as f32));
        }
        let workspace_id = workspace.id.clone();
        let workspace_name = workspace.name.clone();
        let workspace_cwd = workspace.cwd.clone();
        let workspace_branch = workspace.branch.clone();
        let workspace_index = workspace.index;
        let endpoint = endpoint.clone();
        let focus_endpoint = endpoint.clone();
        let focus_workspace = workspace_id.clone();
        let menu_endpoint = endpoint.clone();
        let menu_workspace = workspace_id.clone();
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.leave_settings_page();
                this.activate_endpoint(
                    focus_endpoint.clone(),
                    Some(focus_workspace.clone()),
                    None,
                    cx,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.leave_settings_page();
                this.open_context_menu(
                    SidebarMenuTarget::Workspace {
                        endpoint: menu_endpoint.key(),
                        workspace_id: menu_workspace.clone(),
                        index: workspace_index,
                        name: workspace_name.clone(),
                        cwd: workspace_cwd.clone(),
                        branch: workspace_branch.clone(),
                        pinned: this.folder_pinned(&menu_endpoint.key(), &workspace_cwd),
                    },
                    event.position,
                    cx,
                );
            }),
        )
    }

    /// Working directory for the tab label: the active workspace's cwd.
    fn crumb_path(&self) -> String {
        if let Some(active) = self.surface.workspaces().iter().find(|w| w.active) {
            return self.shorten_cwd(&self.active.key(), &active.cwd);
        }
        self.active_session()
    }

    /// An "Open Workspace" action above the nav rows.
    ///
    /// It opens the same folder picker as the Cmd+Shift+O chord. The picker is
    /// a modal overlay, so the row never takes the selection pill.
    fn open_workspace_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("pilus-open-workspace")
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
            .text_color(theme.panel_fg)
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/plus.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("Open Workspace"))
            .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx)))
    }

    /// The Home row below Open Workspace: what is working, waiting and new.
    fn overview_nav_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("pilus-overview")
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
            .text_color(theme.panel_fg)
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/house.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("Home"))
            .on_click(cx.listener(|this, _, _, cx| this.open_overview(cx)))
    }

    /// The Search row below Open Workspace: it opens the page that finds a
    /// saved agent session and resumes it.
    fn search_nav_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("pilus-search")
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
            .text_color(theme.panel_fg)
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/search.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("Search"))
            .on_click(cx.listener(|this, _, _, cx| this.open_search(cx)))
    }

    /// The Insights row below Search: usage by agent and model, and a year of
    /// activity.
    fn stats_nav_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("pilus-stats")
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
            .text_color(theme.panel_fg)
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/activity.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("Insights"))
            .on_click(cx.listener(|this, _, _, cx| this.open_stats(cx)))
    }

    /// Announce what the network side heard since the last tick: a message, as a
    /// toast and, while the window is not in front, as a system notification
    /// that opens the conversation when clicked; and a stranger knocking. A
    /// message in the conversation being read is neither.
    fn announce_sipil(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        for event in sipil::take_events() {
            match event {
                pilus_client::sipil::Event::Message {
                    thread,
                    name,
                    channel,
                    text,
                    ..
                } => {
                    let reading =
                        self.sipil_chat.open && self.sipil.watch == sipil::thread_watch(&thread);
                    if reading && self.window_active {
                        continue;
                    }
                    let body: String = text.chars().take(160).collect();
                    let title = match &channel {
                        Some(channel) => format!("#{channel}: {name}"),
                        None => name,
                    };
                    self.toasts
                        .push(ToastKind::Info, Some(title.clone()), body.clone());
                    changed = true;
                    if !self.window_active {
                        self.sipil_badged = true;
                        let tag = format!("sipil-{thread}");
                        notify::post(
                            cx,
                            notify::Alert {
                                tag,
                                title,
                                body,
                                badge: u32::try_from(self.sipil.unread() + 1).unwrap_or(u32::MAX),
                                target: ("sipil".to_string(), thread),
                            },
                        );
                    }
                }
                // Answered where it arrives, by `sipil_social::push_event`.
                pilus_client::sipil::Event::Mentioned { .. } => {}
                pilus_client::sipil::Event::Request { from } => {
                    let short: String = from.chars().take(8).collect();
                    self.toasts.push(
                        ToastKind::Info,
                        Some("Someone wants to talk".to_string()),
                        format!("Device {short}… asked to message you. Open sipil to add them."),
                    );
                    changed = true;
                }
            }
        }
        // Back in front with everything read: the badge we raised has done its job.
        if self.sipil_badged && self.window_active && self.sipil.unread() == 0 {
            self.sipil_badged = false;
            notify::set_badge(0);
        }
        changed
    }

    /// Open the conversation a system notification was about, by its thread: a
    /// contact's device ID or a channel's `#id`.
    pub(super) fn open_sipil_thread(&mut self, thread: &str, cx: &mut Context<Self>) {
        cx.dismiss_system_notification(&format!("sipil-{thread}"));
        self.open_sipil_chat(sipil::thread_watch(thread), cx);
    }

    /// SIPIL: the primary agents of every connected daemon,
    /// then the people this pilus talks to, the channels, and any agents the
    /// people share. People and
    /// channels are listed whether or not a daemon is connected, since they never
    /// needed one.
    fn sipil_sections(&self, cx: &mut Context<Self>) -> Vec<Div> {
        // Shown only for a pilus that is switched on and has something to list;
        // the Sipil tab, where a first agent or contact is added, opens from
        // the pilus mark in the right toolbar.
        if !self.sipil_listed() {
            return Vec::new();
        }
        vec![self.sipil_agents_section(cx)]
    }

    /// Whether sipil is set up and switched on: reachable, with something to
    /// list. The sidebar's section and the Home page's message both wait for it.
    fn sipil_listed(&self) -> bool {
        let me = &self.sipil.me;
        let listed = !self.sipil.daemons.is_empty()
            || !me.people.is_empty()
            || !me.channels.is_empty()
            || !me.requests.is_empty();
        me.reachable && listed
    }

    fn sipil_agents_section(&self, cx: &mut Context<Self>) -> Div {
        let section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(SIDEBAR_SECTION_GAP))
            .child(self.section_header(
                SectionHeader {
                    id: "pilus-sipil-agents-toggle",
                    label: "SIPIL",
                    collapsed: self.sipil_collapsed,
                    toggle: Box::new(|this, cx| {
                        this.sipil_collapsed = !this.sipil_collapsed;
                        cx.notify();
                    }),
                    menu: false,
                },
                cx,
            ));
        if self.sipil_collapsed {
            return section;
        }
        let mut rows = div().flex().flex_col().gap_1();
        let theme = self.theme;
        let me = &self.sipil.me;
        // A knock leads the people: it is the one row that asks for something,
        // and it stays where it is.
        for request in &me.requests {
            let short: String = request.id.chars().take(8).collect();
            rows = rows.child(self.sipil_row_item(
                SipilRow {
                    id: format!("pilus-sipil-request-{}", request.id),
                    key: None,
                    glyph: include_bytes!("../../assets/icons/circle-alert.svg"),
                    name: &format!("{short}… wants to talk"),
                    error: None,
                    nested: false,
                    strong: true,
                    open: sipil::Watch::Off,
                    badge: 0,
                    dot: None,
                    window: true,
                    logo: None,
                    note: None,
                },
                cx,
            ));
        }
        // Daemons, channels and people in the order they are made, then placed
        // by the order the person dragged them into.
        let mut placed: Vec<(String, AnyElement)> = Vec::new();
        for daemon in &self.sipil.daemons {
            let primaries: Vec<_> = daemon.agents.iter().collect();
            let key = format!("daemon:{}", daemon.id);
            if !primaries.is_empty() {
                // Its agent, up or not: a daemon that is away still has the
                // conversation it had, which opens, and a message written there
                // waits for it. The dot says which it is. The name the person
                // gave the daemon stands for its one agent; with several, each
                // keeps its own.
                let given = daemon.name();
                let single = primaries.len() == 1;
                for agent in primaries {
                    placed.push((
                        key.clone(),
                        self.sipil_row_item(
                            SipilRow {
                                id: format!("pilus-sipil-agent-{}-{}", daemon.id, agent.id),
                                key: Some(key.clone()),
                                glyph: include_bytes!("../../assets/icons/pilus-mark.svg"),
                                name: if single { &given } else { &agent.name },
                                error: agent.error.as_deref(),
                                nested: false,
                                strong: true,
                                open: sipil::Watch::On(daemon.id.clone(), agent.id.clone()),
                                badge: 0,
                                dot: Some(if daemon.connected() {
                                    theme.ok
                                } else {
                                    theme.muted
                                }),
                                window: false,
                                logo: daemon.own_logo(),
                                note: daemon.error.as_deref().filter(|_| !daemon.connected()),
                            },
                            cx,
                        )
                        .into_any_element(),
                    ));
                }
                continue;
            }
            // Never seen up: the daemon is still listed, with its own logo and
            // an offline dot at the right, and the reason when you hover it. A
            // click opens the Sipil tab, where it is managed.
            placed.push((
                key.clone(),
                self.sipil_row_item(
                    SipilRow {
                        id: format!("pilus-sipil-daemon-{}", daemon.id),
                        key: Some(key),
                        glyph: include_bytes!("../../assets/icons/pilus-mark.svg"),
                        name: &daemon.name(),
                        error: None,
                        nested: false,
                        strong: false,
                        open: sipil::Watch::Off,
                        badge: 0,
                        dot: Some(if daemon.connected() {
                            theme.ok
                        } else {
                            theme.muted
                        }),
                        window: true,
                        logo: daemon.own_logo(),
                        note: daemon.error.as_deref(),
                    },
                    cx,
                )
                .into_any_element(),
            ));
        }
        for row in &me.channels {
            let key = format!("channel:{}", row.channel.id);
            placed.push((
                key.clone(),
                self.sipil_row_item(
                    SipilRow {
                        id: format!("pilus-sipil-channel-{}", row.channel.id),
                        key: Some(key),
                        glyph: include_bytes!("../../assets/icons/hash.svg"),
                        name: &row.channel.name,
                        error: None,
                        nested: false,
                        strong: row.unread > 0,
                        open: sipil::Watch::Channel(row.channel.id.clone()),
                        badge: row.unread,
                        dot: None,
                        window: false,
                        logo: None,
                        note: None,
                    },
                    cx,
                )
                .into_any_element(),
            ));
        }
        for person in &me.people {
            let dot = match person.reach {
                sipil::Reach::Online => Some(theme.ok),
                sipil::Reach::Waiting | sipil::Reach::Offline | sipil::Reach::Unknown => None,
            };
            let key = format!("person:{}", person.contact.id);
            placed.push((
                key.clone(),
                self.sipil_row_item(
                    SipilRow {
                        id: format!("pilus-sipil-person-{}", person.contact.id),
                        key: Some(key),
                        glyph: include_bytes!("../../assets/icons/user.svg"),
                        name: &person.contact.name,
                        error: None,
                        nested: false,
                        strong: person.unread > 0,
                        open: sipil::Watch::Peer(person.contact.id.clone()),
                        badge: person.unread,
                        dot,
                        window: false,
                        logo: None,
                        note: None,
                    },
                    cx,
                )
                .into_any_element(),
            ));
        }
        // Stable, so rows the person has not placed keep the order above.
        placed.sort_by_key(|(key, _)| self.sipil_rank(key));
        rows = rows.children(placed.into_iter().map(|(_, row)| row));
        section.child(style::enter("pilus-sipil-agent-rows", rows))
    }

    /// One agent or session row: a glyph that becomes a spinner while it
    /// works, then its name. A click opens its conversation in the chat page.
    ///
    /// The open conversation's name is bold, the way a selected row's is.
    fn sipil_row_item(&self, row: SipilRow<'_>, cx: &mut Context<Self>) -> impl IntoElement {
        let SipilRow {
            id,
            key,
            glyph,
            name,
            error,
            nested,
            strong,
            open,
            badge,
            dot,
            window,
            logo,
            note,
        } = row;
        let theme = self.theme;
        let selected = self.sipil_chat.open && self.sipil.watch == open;
        let drag = key.as_ref().map(|key| SipilDrag {
            key: key.clone(),
            label: name.to_string(),
            theme,
            font_size: self.chrome_font(),
        });
        // Nothing spins here: who is writing is said in the conversation itself.
        let lead = if error.is_some() {
            icon(
                include_bytes!("../../assets/icons/circle-alert.svg"),
                SIDEBAR_GLYPH,
                theme.alert,
            )
            .into_any_element()
        } else if let Some(logo) = logo {
            // Larger than a glyph, centred on the glyph's column so the names
            // still line up.
            img(logo)
                .w(px(SIDEBAR_LOGO))
                .h(px(SIDEBAR_LOGO))
                .flex_shrink_0()
                .ml(px(-(SIDEBAR_LOGO - SIDEBAR_GLYPH) / 2.0))
                .into_any_element()
        } else {
            icon(glyph, SIDEBAR_GLYPH, theme.muted).into_any_element()
        };
        // The name truncates, so the tooltip carries it whole, and the reason
        // a turn failed, which is too long for a row.
        let tip = row_tooltip(
            name.to_string(),
            theme.panel_fg,
            error
                .or(note)
                .map(|error| vec![(error.to_string(), theme.alert)])
                .unwrap_or_default(),
            theme,
        );
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(
                SIDEBAR_ROW_PAD + if nested { SIDEBAR_GLYPH } else { 0.0 }
            ))
            .pr(px(SIDEBAR_ROW_PAD))
            .radius_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.panel_fg)
            .tooltip(tip)
            .child(style::hover_target(&self.sidebar_hover))
            .child(div().w(px(SIDEBAR_GLYPH)).flex_shrink_0().child(lead))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(strong || selected, |name| {
                        name.font_weight(FontWeight::BOLD)
                    })
                    .child(name.to_string()),
            )
            .when(badge > 0, |row| {
                row.child(style::chip(theme, badge.to_string(), self.section_font()))
            })
            .children(dot.map(|color| {
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_shrink_0()
                    .radius_full()
                    .bg(color)
            }))
            .when_some(drag.zip(key), |row, (drag, key)| {
                row.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                    // The row under a drag shows the pill it would take, unless
                    // it is the one being dragged.
                    .drag_over::<SipilDrag>({
                        let key = key.clone();
                        move |style, drag, _, _| {
                            if drag.key == key {
                                style
                            } else {
                                style.bg(theme.panel)
                            }
                        }
                    })
                    .on_drop(cx.listener(move |this, drag: &SipilDrag, _, cx| {
                        this.reorder_sipil(&drag.key, &key, cx);
                    }))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if window {
                    this.show_sipil(cx);
                } else {
                    this.open_sipil_chat(open.clone(), cx);
                }
            }))
    }

    /// Where the SIPIL row `key` stands: its place in the order the person
    /// dragged the rows into, with the rows not placed yet after those.
    fn sipil_rank(&self, key: &str) -> usize {
        self.settings
            .sipil_order
            .iter()
            .position(|placed| placed == key)
            .unwrap_or(usize::MAX)
    }

    /// The SIPIL rows as they are listed now, each once.
    fn sipil_keys(&self) -> Vec<String> {
        let me = &self.sipil.me;
        let mut keys: Vec<String> = self
            .sipil
            .daemons
            .iter()
            .map(|daemon| format!("daemon:{}", daemon.id))
            .chain(
                me.channels
                    .iter()
                    .map(|row| format!("channel:{}", row.channel.id)),
            )
            .chain(
                me.people
                    .iter()
                    .map(|person| format!("person:{}", person.contact.id)),
            )
            .collect();
        keys.sort_by_key(|key| self.sipil_rank(key));
        keys
    }

    /// Drop the SIPIL row `from` into the slot of `to`, and keep the order.
    fn reorder_sipil(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        if from == to {
            return;
        }
        let shown = self.sipil_keys();
        self.settings.move_sipil_row(&shown, from, to);
        self.save_settings();
        cx.notify();
    }

    /// Toggle the Home composer, bound to the Cmd+Shift+N chord.
    fn toggle_home(&mut self, cx: &mut Context<Self>) {
        if self.home.is_open() {
            self.home.close();
        } else {
            self.open_home(cx);
        }
        cx.notify();
    }

    /// One sidebar section header: collapse chevron, label, and hairline.
    ///
    /// Every section draws through this, so their labels share one column;
    /// the header is a band, not a control, so it takes `SIDEBAR_BAND_HEIGHT`.
    fn section_header(&self, spec: SectionHeader, cx: &mut Context<Self>) -> Div {
        let SectionHeader {
            id,
            label,
            collapsed,
            toggle,
            menu,
        } = spec;
        let theme = self.theme;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(SIDEBAR_BAND_HEIGHT))
            // The hairline runs to the panel edge, breaking out of the row
            // inset; the chevron stays on the row glyph column.
            .mr(px(-SIDEBAR_ROW_EDGE))
            .child(
                div()
                    .id(id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_GLYPH_GAP))
                    .pl(px(SIDEBAR_ROW_PAD))
                    .flex_1()
                    .min_w(px(0.0))
                    .cursor_pointer()
                    .text_size(px(self.section_font()))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted)
                    .hover(|style| style.text_color(theme.panel_fg))
                    // The chevron wears the shared glyph box, so it lines up
                    // with the rows' leading icons.
                    .child(
                        div()
                            .w(px(SIDEBAR_GLYPH))
                            .flex_shrink_0()
                            .child(style::chevron(
                                format!("{id}-chevron"),
                                !collapsed,
                                self.section_toggled.is_some_and(|(last, at)| {
                                    last == id && at.elapsed() < SECTION_TURN_WINDOW
                                }),
                                12.0,
                                theme.muted,
                            )),
                    )
                    .child(label)
                    .child(div().flex_1().h(px(1.0)).bg(theme.border))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section_toggled = Some((id, Instant::now()));
                        toggle(this, cx);
                    }))
                    .when(menu, |header| {
                        header.on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                this.open_context_menu(
                                    SidebarMenuTarget::Github,
                                    event.position,
                                    cx,
                                );
                            }),
                        )
                    }),
            )
    }

    /// Expand the daemon's `~`-shorthand dirs against the owning machine.
    fn expand_cwd(&self, endpoint_key: &str, cwd: &str) -> String {
        if endpoint_key == "local" {
            return expand_local(cwd);
        }
        match self.remote_home.get(endpoint_key) {
            Some(home) => expand_remote(cwd, home),
            None => cwd.to_string(),
        }
    }

    /// Shorten an absolute cwd with `~` against the owning machine's home.
    ///
    /// Pane cwds arrive absolute, but a remote one needs that machine's home:
    /// this machine's `$HOME` never matches it. When the home is not known yet,
    /// fall back to the `~`-shorthand the daemon uses for workspace cwds.
    fn shorten_cwd(&self, endpoint_key: &str, cwd: &str) -> String {
        if let Some(home) = self.remote_home.get(endpoint_key) {
            let shortened = shorten_against(cwd, home);
            if shortened != cwd {
                return shortened;
            }
        }
        let shortened = shorten_home(cwd);
        if shortened != cwd {
            return shortened;
        }
        self.shorten_by_shorthand(endpoint_key, cwd)
            .unwrap_or_else(|| cwd.to_string())
    }

    /// Recover a home prefix by aligning `cwd` with a workspace's `~`-shorthand.
    ///
    /// The daemon reports workspace cwds as `~`-shorthand but pane cwds
    /// absolute, so a matching tail names the prefix the machine's `$HOME`
    /// would, without probing it.
    fn shorten_by_shorthand(&self, endpoint_key: &str, cwd: &str) -> Option<String> {
        let endpoint = if endpoint_key == "local" {
            EndpointId::Local
        } else {
            EndpointId::Machine(endpoint_key.to_string())
        };
        for workspace in self.endpoint_workspaces(&endpoint) {
            if let Some(shortened) = shorten_like_shorthand(cwd, &workspace.cwd) {
                return Some(shortened);
            }
        }
        None
    }

    /// Resolve the `$HOME` of remote endpoints whose workspaces are shown.
    ///
    /// A remote machine's home lets the sidebar shorten absolute pane cwds;
    /// this machine's own `$HOME` never matches it. Homes arrive on
    /// `home_resolve_rx`.
    fn resolve_remote_homes(&mut self) {
        if self.home_resolve_pending {
            return;
        }
        let mut probes: Vec<(String, SshTarget, Vec<String>)> = Vec::new();
        for id in self.connected_endpoints() {
            let Some(machine_id) = id.machine_id() else {
                continue;
            };
            let key = id.key();
            if self.remote_home.contains_key(&key) {
                continue;
            }
            let Some(target) = self
                .machines
                .iter()
                .find(|machine| machine.id == machine_id)
                .and_then(|machine| machine.ssh_target())
            else {
                continue;
            };
            let cwds: Vec<String> = self
                .endpoint_workspaces(&id)
                .iter()
                .map(|workspace| workspace.cwd.clone())
                .filter(|cwd| !cwd.is_empty())
                .collect();
            if cwds.is_empty() {
                continue;
            }
            probes.push((key, target, cwds));
        }
        if probes.is_empty() {
            return;
        }
        self.home_resolve_pending = true;
        let tx = self.home_resolve_tx.clone();
        std::thread::spawn(move || {
            let mut homes = Vec::new();
            // Only the home is wanted here, so no git runs in each workspace.
            for (endpoint_key, target, _) in probes {
                let home = remote_home(&target);
                if !home.is_empty() {
                    homes.push((endpoint_key, home));
                }
            }
            tx.send(HomeResolve { homes }).ok();
        });
    }

    /// WORKSPACES: every connected endpoint as a machine row, with the
    /// workspaces opened on it hanging below.
    fn workspaces_section(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(SIDEBAR_SECTION_GAP))
            .child(self.section_header(
                SectionHeader {
                    id: "pilus-workspaces-toggle",
                    label: "WORKSPACES",
                    collapsed: self.workspaces_collapsed,
                    toggle: Box::new(|this, cx| {
                        this.workspaces_collapsed = !this.workspaces_collapsed;
                        cx.notify();
                    }),
                    menu: false,
                },
                cx,
            ));
        if self.workspaces_collapsed {
            return section;
        }
        let mut children = div().flex().flex_col().gap_1();
        let mut any = false;
        for id in self.connected_endpoints() {
            let host = id.machine_id().map(|_| self.endpoint_label(&id));
            let workspaces = self.endpoint_workspaces(&id);
            if workspaces.is_empty() {
                continue;
            }
            any = true;
            // The machine's home workspace is its row. Once a folder is opened
            // the daemon may no longer list one, so the row is drawn anyway:
            // the folder belongs under the machine, never in its place.
            let home = workspaces
                .iter()
                .position(|workspace| self.is_home_workspace(&id, workspace));
            let foldable = workspaces.len() > usize::from(home.is_some());
            children = match home {
                Some(index) => children.child(self.workspace_row(
                    &id,
                    &workspaces[index],
                    host.as_deref(),
                    0,
                    foldable,
                    cx,
                )),
                None => children.child(self.machine_row(&id, host.as_deref(), cx)),
            };
            if self.machine_folded(&id) {
                continue;
            }
            for (index, workspace) in workspaces.iter().enumerate() {
                if Some(index) == home {
                    continue;
                }
                let depth = if workspace.nested { 2 } else { 1 };
                children = children.child(self.workspace_row(
                    &id,
                    workspace,
                    host.as_deref(),
                    depth,
                    false,
                    cx,
                ));
            }
        }
        if !any {
            let loading = self
                .connected_endpoints()
                .iter()
                .any(|id| !self.endpoint_workspaces_loaded(id));
            children = children.child(if loading {
                self.sidebar_skeleton("pilus-workspaces-skeleton", &[0.62, 0.48, 0.7])
            } else {
                div()
                    .flex()
                    .items_center()
                    .h(px(SIDEBAR_BAND_HEIGHT))
                    .pl(px(SIDEBAR_ROW_PAD))
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No workspaces yet")
            });
        }
        // Collapsed sections drop their rows, so the fade replays each time
        // the section opens.
        section.child(style::enter("pilus-workspaces-rows", children))
    }

    /// Whether `workspace` is the machine's home directory, which the sidebar
    /// names for the machine rather than for the user's folder.
    fn is_home_workspace(&self, endpoint: &EndpointId, workspace: &ShellWorkspace) -> bool {
        !workspace.nested && self.shorten_cwd(&endpoint.key(), &workspace.cwd) == "~"
    }

    /// Whether the machine's workspaces are folded under its row.
    fn machine_folded(&self, endpoint: &EndpointId) -> bool {
        self.machines_collapsed.contains(&endpoint.key())
    }

    /// The chevron at the end of a machine's row. It folds and unfolds the
    /// workspaces below without selecting the machine, so the row's own click
    /// stays "go there".
    fn machine_fold_toggle(
        &self,
        endpoint: &EndpointId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = endpoint.key();
        let folded = self.machine_folded(endpoint);
        let turning = self
            .machine_toggled
            .as_ref()
            .is_some_and(|(last, at)| *last == key && at.elapsed() < SECTION_TURN_WINDOW);
        let chevron_id = format!("pilus-machine-fold-{key}");
        div()
            .id(chevron_id.clone())
            .w(px(SIDEBAR_GLYPH))
            .flex_shrink_0()
            .cursor_pointer()
            .child(style::chevron(
                chevron_id,
                !folded,
                turning,
                12.0,
                self.theme.muted,
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if !this.machines_collapsed.remove(&key) {
                        this.machines_collapsed.insert(key.clone());
                    }
                    this.machine_toggled = Some((key.clone(), Instant::now()));
                    cx.notify();
                }),
            )
    }

    /// A machine's row when it has no home workspace to stand for it. It takes
    /// the selection only while none of the machine's workspaces is active, so
    /// one row lights per endpoint.
    fn machine_row(
        &self,
        endpoint: &EndpointId,
        host: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let title = self.endpoint_label(endpoint);
        let selected = self.endpoint_selected(endpoint)
            && (self.machine_folded(endpoint)
                || !self
                    .endpoint_workspaces(endpoint)
                    .iter()
                    .any(|workspace| workspace.active));
        let pill = if self.agent_pill_shown() {
            &self.sidebar_context
        } else {
            &self.sidebar_selection
        };
        let icon_bytes: &'static [u8] = if endpoint.machine_id().is_some() {
            include_bytes!("../../assets/icons/server.svg")
        } else {
            include_bytes!("../../assets/icons/folder.svg")
        };
        let mut lines = Vec::new();
        if let Some(host) = host {
            lines.push((format!("Host: {host}"), theme.muted));
        }
        let tip = row_tooltip(title.clone(), theme.panel_fg, lines, theme);
        let target = endpoint.clone();
        div()
            .id(format!("pilus-machine-{}", endpoint.key()))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w_0()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .px(px(SIDEBAR_ROW_PAD))
            .radius_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .children(selected.then(|| style::glide_target(pill)))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            .child(div().w(px(SIDEBAR_GLYPH)).flex_shrink_0().child(icon(
                icon_bytes,
                SIDEBAR_GLYPH,
                theme.muted,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.panel_fg)
                    .child(title),
            )
            .child(self.machine_fold_toggle(endpoint, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.leave_settings_page();
                    this.activate_endpoint(target.clone(), None, None, cx);
                }),
            )
    }

    /// Placeholder rows for a sidebar list still in flight: a glyph dot and a
    /// label bar per row, in the geometry of a real row so nothing shifts
    /// when the content lands. `fractions` set each bar's share of the width.
    fn sidebar_skeleton(&self, id: &'static str, fractions: &[f32]) -> Div {
        let theme = self.theme;
        let mut rows = div().flex().flex_col().gap_1();
        for (index, fraction) in fractions.iter().enumerate() {
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_GLYPH_GAP))
                    .h(px(SIDEBAR_ROW_HEIGHT))
                    .px(px(SIDEBAR_ROW_PAD))
                    .child(
                        div()
                            .w(px(SIDEBAR_GLYPH))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(style::skeleton((id, index * 2), theme, px(10.0), 10.0)),
                    )
                    .child(div().flex_1().min_w_0().child(style::skeleton(
                        (id, index * 2 + 1),
                        theme,
                        relative(*fraction),
                        self.chrome_font() * 0.8,
                    ))),
            );
        }
        rows
    }

    /// The display label for an endpoint.
    fn endpoint_label(&self, id: &EndpointId) -> String {
        match id {
            EndpointId::Local => "Local".to_string(),
            EndpointId::Machine(machine_id) => self
                .machines
                .iter()
                .find(|machine| &machine.id == machine_id)
                .map(|machine| machine.display().to_string())
                .unwrap_or_else(|| machine_id.clone()),
        }
    }

    /// AGENTS: live agent sessions grouped by endpoint; clicking a row focuses
    /// its pane, activating the owning endpoint first when it is hidden. The
    /// machine name stays out of the rows and is named in the tooltip.
    fn agents_section(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(SIDEBAR_SECTION_GAP))
            .child(self.section_header(
                SectionHeader {
                    id: "pilus-agents-toggle",
                    label: "AGENTS",
                    collapsed: self.agents_collapsed,
                    toggle: Box::new(|this, cx| {
                        this.agents_collapsed = !this.agents_collapsed;
                        cx.notify();
                    }),
                    menu: false,
                },
                cx,
            ));
        if self.agents_collapsed {
            return section;
        }
        let mut children = div().flex().flex_col().gap_1();
        let mut any = false;
        // The AGENTS-scope menu row narrows the dock to the visible surface's
        // active workspace, mirroring the daemon's own scope filter.
        let scoped_workspace = if self.agents_this_workspace {
            self.endpoint_workspaces(&self.active)
                .iter()
                .find(|workspace| workspace.active)
                .map(|workspace| workspace.id.clone())
        } else {
            None
        };
        for id in self.connected_endpoints() {
            let mut agents: Vec<&Agent> = self.endpoint_agents(&id).iter().collect();
            if let Some(workspace_id) = &scoped_workspace {
                agents.retain(|agent| agent.workspace_id == *workspace_id);
            }
            if agents.is_empty() {
                continue;
            }
            any = true;
            for agent in agents {
                children = children.child(self.agent_row(&id, agent, cx));
            }
        }
        if !any {
            let loading = self
                .connected_endpoints()
                .iter()
                .any(|id| !self.endpoint_agents_loaded(id));
            children = children.child(if loading {
                self.sidebar_skeleton("pilus-agents-skeleton", &[0.55, 0.7])
            } else {
                div()
                    .flex()
                    .items_center()
                    .h(px(SIDEBAR_BAND_HEIGHT))
                    .pl(px(SIDEBAR_ROW_PAD))
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No agents")
            });
        }
        section.child(style::enter("pilus-agents-rows", children))
    }

    /// One agent row: the agent and its label on the first line, the working
    /// directory and branch on the second. The brand mark is tinted by status.
    /// Clicking focuses its pane on its owning endpoint.
    fn agent_row(
        &self,
        endpoint: &EndpointId,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let focused = self.agent_focused(endpoint, agent);
        // The focused row borrows the workspace/worktree pill, so a selected
        // agent reads like a selected workspace instead of a text selection.
        let selected = focused;
        let fg = theme.panel_fg;
        let dim = theme.muted;
        let status = status_color(&agent.status, theme);
        let branch = agent.branch.clone();
        let cwd = self.shorten_cwd(&endpoint.key(), &agent.cwd);
        let label = agent.label().to_string();
        let mut lines = vec![
            (format!("Agent: {}", agent.agent), dim),
            (format!("Status: {}", status_label(&agent.status)), status),
        ];
        if !agent.workspace_name.is_empty() {
            lines.push((format!("Workspace: {}", agent.workspace_name), dim));
        }
        if matches!(endpoint, EndpointId::Machine(_)) {
            lines.push((format!("Host: {}", self.endpoint_label(endpoint)), dim));
        }
        lines.push((format!("Directory: {cwd}"), dim));
        if let Some(branch) = branch.clone() {
            lines.push((format!("Branch: {branch}"), theme.ok));
        }
        let tip = row_tooltip(label.clone(), fg, lines, theme);

        let pane = agent.pane.clone();
        let endpoint = endpoint.clone();
        let focus_endpoint = endpoint.clone();
        let focus_pane = pane.clone();
        let menu_endpoint = endpoint.clone();
        let menu_pane = pane.clone();
        let menu_alias = agent.name.clone();
        let mut row = div()
            .id(format!("pilus-agent-{}-{}", endpoint.key(), agent.pane))
            .flex()
            .flex_col()
            .justify_center()
            .gap_1()
            .pl(px(SIDEBAR_ROW_PAD))
            // The status dot and branch keep the row's content inset.
            .pr(px(SIDEBAR_ROW_PAD))
            .py_1()
            .radius_md()
            .cursor_pointer()
            .children(selected.then(|| style::glide_target(&self.sidebar_selection)))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_GLYPH_GAP))
                    .min_w_0()
                    .text_size(px(self.chrome_font()))
                    // Status leads, in the shared glyph box, so the label
                    // column matches the other rows; the brand mark trails.
                    .child(
                        div()
                            .w(px(SIDEBAR_GLYPH))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(if agent.status == "working" {
                                // A working agent turns where the dot sits, so
                                // the row says "busy" without waiting on a
                                // status push.
                                style::spinner(
                                    format!(
                                        "pilus-agent-spinner-{}-{}",
                                        endpoint.key(),
                                        agent.pane
                                    ),
                                    10.0,
                                    status,
                                )
                                .into_any_element()
                            } else {
                                div()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .radius_full()
                                    .bg(status)
                                    .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(fg)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(icon(agent_icon(&agent.agent), 12.0, fg)),
                    ),
            );
        // The path line follows the daemon's Show Path / Hide Path toggle.
        if self.sidebar_agent_paths {
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_GLYPH_GAP))
                    .min_w_0()
                    .text_size(px(self.section_font()))
                    .text_color(dim)
                    // The path wins the line: workspace, path, and branch
                    // never fit together, and a lone ellipsis in the middle
                    // of the row reads as a glitch. The label above already
                    // names the agent; the workspace stays in the tooltip.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(style::mono_family(&self.shared_settings))
                            .child(cwd),
                    )
                    .child(match branch {
                        // A branch repeating the label above is the label
                        // twice; the path keeps the room instead.
                        Some(branch) if branch != agent.label() => div()
                            .flex_shrink_0()
                            .font_family(style::mono_family(&self.shared_settings))
                            .text_color(theme.ok)
                            .child(branch),
                        _ => div(),
                    }),
            );
        }
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.leave_settings_page();
                this.activate_endpoint(focus_endpoint.clone(), None, Some(focus_pane.clone()), cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.leave_settings_page();
                this.open_context_menu(
                    SidebarMenuTarget::Agent {
                        endpoint: menu_endpoint.key(),
                        pane: menu_pane.clone(),
                        // The daemon's rename modal pre-fills the current
                        // alias, not the derived label.
                        name: menu_alias.clone(),
                    },
                    event.position,
                    cx,
                );
            }),
        )
    }

    /// Measure the terminal card and the grid that fits inside it.
    ///
    /// The card runs from the sidebar to the right column and from the tab
    /// strip to the window's bottom edge, with no gap or border: the grid
    /// spends every pixel on cells.
    fn card_layout(&mut self, window: &Window) -> CardLayout {
        let cell_w = self.cell_width(window);
        let cell_h = self.cell_height();
        let viewport = window.viewport_size();
        let card_x = self.sidebar_px(cell_w);
        let right_px = self.right.px();
        let inner_w = f32::from(viewport.width) - card_x - right_px;
        let inner_h = f32::from(viewport.height) - HEADER_HEIGHT;
        // A split page leads the card; the grid fits what it leaves.
        let content_w = inner_w;
        let (page_w, page_h) = match self.page_split().map(|split| split.axis) {
            Some(SplitAxis::Right) => (page_extent(inner_w), 0.0),
            Some(SplitAxis::Down) => (0.0, page_extent(inner_h)),
            None => (0.0, 0.0),
        };
        let (inner_w, inner_h) = (inner_w - page_w, inner_h - page_h);
        let cols = (inner_w / cell_w).clamp(20.0, 500.0) as u16;
        let rows = (inner_h / cell_h).clamp(5.0, 300.0) as u16;
        // The grid pins to the top-left so the terminal truly reaches the
        // edges; the leftover fraction pools at the far edges, where the grid
        // extends its edge colors over it.
        let slack_x = (inner_w - f32::from(cols) * cell_w).max(0.0);
        let slack_y = (inner_h - f32::from(rows) * cell_h).max(0.0);
        // The left sidebar is whole cells wide, so the card's left edge is
        // cell-aligned. The right column is free-width, which would leave up
        // to a cell of remainder between the grid and it, a margin the left
        // never has. The column takes that remainder instead, so the grid
        // meets both edges. Only at rest: mid-tween the column would stair-step
        // a cell at a time. With the column away there is nothing to take it,
        // and trimming the card would leave a bare strip along the edge; the
        // grid's own fillers pool the remainder there instead.
        let absorbed = absorbed_remainder(right_px, self.right.tweening(), slack_x, cell_w);
        let (slack_x, content_w) = (slack_x - absorbed, content_w - absorbed);
        CardLayout {
            card_x,
            card_right: right_px + absorbed,
            origin_x: card_x + page_w,
            origin_y: HEADER_HEIGHT + page_h,
            cols,
            rows,
            cell_w,
            cell_h,
            slack_x,
            slack_y,
            page_w,
            page_h,
            content_w,
        }
    }

    /// The tab strip's background. A split always takes the pane surface, the
    /// tinted ground its cards sit on, opaque or not. One pane has no cards:
    /// opaque, its strip is the sidebar's own fill; translucent, it takes the
    /// pane surface so one pane and many look alike. A native page alone in the
    /// card sits in the chrome's fill, so the strip keeps to it.
    fn tab_strip_fill(&self) -> gpui::Rgba {
        let glass = self.glass_opacity();
        let surface = self.theme.pane_surface(glass).opacity(glass);
        if self.split_page_open() && self.page_split().is_none() {
            self.chrome_fill()
        } else if self.is_split() || (self.settings.pane_ui && glass < 1.0) {
            surface
        } else {
            self.chrome_fill()
        }
    }

    /// The fill of the selected (`weight` 1) and hovered tab. In a split the
    /// strip sits on the tinted surface, so a tab is raised in the card's own
    /// fill, which reads as the tab opening into the terminal card below it
    /// and takes no weight. Otherwise it is the sidebar's raised pill, so a
    /// tab and a sidebar row look alike at any window opacity.
    fn tab_raise(&self, weight: f32) -> gpui::Rgba {
        let glass = self.glass_opacity();
        if self.is_split() {
            self.theme.bg.opacity(glass * weight.min(1.0).ceil())
        } else {
            self.theme.sidebar_raise(glass, weight)
        }
    }

    /// Whether the shown frame is a split of floating cards, which is when the
    /// terminal sits on the tinted surface the tab strip matches. Flush panes
    /// cover that surface, so they chrome like a single pane.
    fn is_split(&self) -> bool {
        self.settings.pane_ui
            && self.settings.pane_gaps
            && self.shown_frame().is_some_and(|frame| {
                !pane_frames::PaneFrames::find(
                    frame,
                    frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS),
                )
                .panes()
                .is_empty()
            })
    }

    /// The fill behind the sidebar, the tab strip, and a native page: a
    /// shade denser than the terminal under translucency, so the chrome
    /// frames the grid and a page's text never sits on the bare desktop.
    fn chrome_fill(&self) -> gpui::Rgba {
        self.theme.chrome_glass(self.glass_opacity())
    }

    /// The window opacity in effect: the setting, except that a light scheme
    /// is always opaque, since the frost behind it is dark.
    fn glass_opacity(&self) -> f32 {
        if self.theme.is_light() {
            1.0
        } else {
            self.settings.drawn_opacity()
        }
    }

    /// Whether the window is fully opaque, so the blur and the glass coat
    /// would never show.
    fn opaque(&self) -> bool {
        self.glass_opacity() >= 1.0
    }

    /// Recompute the cell grid size and tell the daemon when it changes.
    ///
    /// The viewport is whatever fits inside the card (or the full-bleed
    /// terminal); planning with anything else lays out columns the card
    /// clips off.
    ///
    /// Nothing is sent while the sidebar slides. The daemon already moves
    /// its pane by the dock's width the moment the sidebar message lands,
    /// which is the final width; resizing toward the card's in-between
    /// widths made the TUI reflow narrower and back again, several times a
    /// slide — the flicker. The card clips or pads the grid until the tween
    /// settles, and the trailing render sends any size still owed.
    fn sync_metrics(&mut self, window: &Window) {
        // A column stretched over the terminal leaves it nothing to fit, and
        // sizing the daemon to that would reflow it for no one to see.
        if self.sidebar_anim.is_some() || self.right.moving() || self.right.expanded() {
            return;
        }
        let layout = self.card_layout(window);
        // The daemon lays out its whole chrome (docks included); we only show
        // the pane area, so the skipped rows and columns ride along.
        let cols = layout.cols.saturating_add(self.settled_skip_cols());
        let rows = layout
            .rows
            .saturating_add(self.skipped_rows())
            .saturating_add(SKIPPED_DAEMON_BOTTOM_ROWS);
        let cell = (
            layout.cell_w.round().clamp(1.0, 65535.0) as u16,
            layout.cell_h.round().clamp(1.0, 65535.0) as u16,
        );
        if cols == self.cols && rows == self.rows && cell == self.sent_cell {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.sent_cell = cell;
        let resize = ClientMessage::Resize {
            cols,
            rows,
            cell_width_px: cell.0,
            cell_height_px: cell.1,
        };
        self.send_active(resize.clone());
        // Suspended endpoints keep their panes and PTY sizes, so a window
        // resized while they are hidden would leave them reflowing content
        // the moment they are switched to. Push the new size down every
        // bridge; the prepare handoff then renders a candidate that already
        // matches, and committing it never visibly resizes the terminal.
        for endpoint in self.endpoints.values() {
            if let Some(connection) = endpoint.connection.as_ref() {
                let _ = connection.send(resize.clone());
            }
        }
    }
}

/// Daemon rows above the pane: the tab strip alone when the terminal takes
/// top spacing, the tab strip and its title row otherwise.
fn skipped_rows(top_spacing: bool) -> u16 {
    if top_spacing {
        SKIPPED_DAEMON_ROWS_UNTITLED
    } else {
        SKIPPED_DAEMON_ROWS
    }
}

/// Whether `tabs` contains the 1-based position `index`.
///
/// Guards `Cmd+N` shortcuts: a tab that does not exist must not be selected.
fn tab_exists(tabs: &[Tab], index: u16) -> bool {
    tabs.iter().any(|tab| tab.index == index)
}

impl Render for PilusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        perf::render();
        self.window_active = window.is_window_active();
        perf::window_active(self.window_active);
        // Remember the open dock so the collapse tween still has a width to
        // fold from once the daemon drops it in response to our hide message.
        if let Some(dock) = self.surface.shell_dock() {
            self.last_sidebar_dock = Some(dock);
        }
        // One tween sample per frame: every edge below reads `sidebar_open`,
        // so the panel, tab strip, and card stay flush while the sidebar flies.
        self.sidebar_open = self.sidebar_factor();
        // A split belongs to the page it opened beside; closing the page
        // ends it, so the next page starts whole.
        if self.page_split.is_some() && self.page_split().is_none() {
            self.page_split = None;
        }
        // What an expanded right column fills: the window beside the left
        // sidebar.
        let cell_width = self.cell_width(window);
        let beside = f32::from(window.viewport_size().width) - self.sidebar_px(cell_width);
        self.right.set_full(beside);
        self.sync_metrics(window);
        self.sync_home();
        self.browser_sync(window, cx);
        self.sync_focus(window, cx);

        let theme = self.theme;
        let glass = self.glass_opacity();
        // The blur slider drives the frosted backdrop view itself: full blur
        // is vibrant frost, weaker settings crossfade toward the sharp
        // desktop, and an explicit zero drops the view entirely. An opaque
        // window shows nothing through, so it keeps the view off.
        let blur = self.settings.blur.clamp(0.0, 1.0);
        let opaque = self.opaque();
        self.glass.apply(window, if opaque { 0.0 } else { blur });
        // A handler-less coat behind everything: it never intercepts clicks.
        // The root is transparent so a translucent window can show the
        // backdrop, which would otherwise leak the desktop through the inset
        // card's margins. Only the translucent window leans on the coat; an
        // opaque one paints its flat base.
        let coat = glass_coat(theme, blur, opaque);
        let cell_height = self.cell_height();
        let cell_width = self.cell_width(window);
        let layout = self.card_layout(window);
        let line_height = px(cell_height);
        let selection = self.selection;
        // The terminal shares the sidebar's background: one uniform base
        // under the grid, the card, and the chrome, whatever the running app
        // paints. Voting the dominant cell color made the padding track the
        // app instead, so a green TUI left a green frame around an ink grid.
        // With pane frames the grid takes the chrome's own fill for the cards
        // inside each pane's border, and `render_grid` lays a tinted surface
        // under them, so the gutters and margin read as pilus's container.
        let pane_ui = self.settings.pane_ui;
        let terminal_bg = if pane_ui {
            theme.chrome_cell(glass)
        } else {
            theme.window_bg
        };
        let grid_theme = Theme {
            bg: terminal_bg,
            remap_bg: pane_ui.then_some(theme.bg),
            ..theme
        };
        // One glass layer exactly: the grid has no padding and its edge
        // fillers seal every pixel, so when the frame seals the card the
        // cells carry the opacity and the card stays clear — otherwise two
        // stacked layers would read near-solid instead of the setting. Only
        // the row count can leave a hole the fillers do not cover: a lagging
        // frame's missing columns are padded by each row's right-edge filler,
        // so requiring them here would flip the card opaque mid-resize and
        // double-blend every cell for a frame (a visible blink). The card
        // still backs any short frame.
        let sealed = match self.shown_frame() {
            Some(frame) => {
                frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS)
                    >= self.skipped_rows().saturating_add(layout.rows)
            }
            None => false,
        };
        // A native page that owns the whole card has no cells to carry the
        // glass, so it takes the chrome's denser coat: under translucency
        // its text would otherwise sit straight on the desktop.
        let page_owns_card = self.home.is_open() || self.settings_open;
        let terminal_fill = if sealed {
            terminal_bg.opacity(0.0)
        } else {
            terminal_bg.opacity(glass)
        };
        // Tasks, Mission Control, and Automations own the card only while
        // unsplit; split, `page_card` coats each half itself.
        let page_alone = self.split_page_open() && self.page_split().is_none();
        let card_bg = if page_owns_card {
            self.chrome_fill()
        } else if page_alone || self.page_split().is_some() {
            terminal_bg.opacity(0.0)
        } else {
            terminal_fill
        };
        let pane_toolbar = (pane_ui && self.settings.pane_toolbar && self.toolbar_fade.shown())
            .then(|| {
                // The wrapper carries the fade; the bar inside keeps its
                // own `occlude`.
                div()
                    .opacity(self.toolbar_fade.alpha())
                    .child(self.pane_toolbar(cx))
            });
        let panes = match self.shown_frame() {
            Some(frame) if pane_ui => pane_frames::PaneFrames::find(
                frame,
                frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS),
            ),
            _ => pane_frames::PaneFrames::default(),
        };
        self.dividers = panes.dividers();
        let grid = match self.shown_frame() {
            Some(frame) => perf::grid_build(|| {
                render_grid(
                    frame,
                    grid_theme,
                    self.settings.terminal_font_size,
                    self.settings.terminal_font.as_str(),
                    line_height,
                    selection,
                    cell_width,
                    self.skipped_cols(),
                    self.skipped_rows(),
                    layout.cols,
                    layout.rows,
                    glass,
                    layout.slack_x,
                    layout.slack_y,
                    panes,
                    self.settings.pane_gaps,
                    pane_toolbar,
                    self.cursor_blink.alpha(),
                )
            }),
            None => {
                let mut empty = div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .size_full()
                    .px_6()
                    .text_center()
                    .text_color(theme.muted);
                if self.active_live {
                    empty = empty.child("Waiting for the daemon's first frame…");
                } else {
                    // No bridge: a stopped or protocol-incompatible daemon
                    // explains itself here while the reconnect loop retries,
                    // and the hint points at the one place that can update it.
                    empty = empty.child(self.status.clone()).child(
                        div()
                            .text_size(px(self.section_font()))
                            .text_color(theme.muted)
                            .child("Settings → Updates can update the Luvus daemon."),
                    );
                }
                empty
            }
        };
        // A link under the pointer reads as clickable, and a seam between
        // panes as something to drag.
        let grid = match self
            .divider_drag
            .map(|drag| drag.axis)
            .or(self.divider_hover)
        {
            Some(pane_frames::DividerAxis::Vertical) => grid.cursor_col_resize(),
            Some(pane_frames::DividerAxis::Horizontal) => grid.cursor_row_resize(),
            None if self.hovered_link() => grid.cursor_pointer(),
            None => grid,
        };
        let drop_edge = theme.border_focus;
        let grid = grid
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // A press on the terminal beside a split page takes the
                    // keyboard from the page.
                    if let Some(split) = this.page_split.as_mut() {
                        split.terminal_focused = true;
                    }
                    this.right.focused = false;
                    this.begin_selection(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // Ctrl+click is the daemon's link open, which macOS
                    // delivers as a right click; every other right click is
                    // this window's own menu, so the daemon never draws its
                    // own over the grid.
                    if this.control_held {
                        // macOS may have cleared `Ctrl` from the event it
                        // rewrote; `control_held` is the record of it.
                        let modifiers = Modifiers {
                            control: true,
                            ..event.modifiers
                        };
                        this.link_press = this
                            .cell_at(window, event.position)
                            .is_some_and(|cell| this.open_link_at(cell, modifiers, cx));
                        if !this.link_press {
                            this.press(event, TerminalMouseButton::Right, window);
                        }
                    } else {
                        this.open_terminal_menu(event, window, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    this.press(event, TerminalMouseButton::Middle, window);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.finish_selection(event, window, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, window, _| {
                    // The press never reached the daemon unless it was a
                    // Ctrl+click, so only that release does. A press the
                    // window opened as a link never reached it either.
                    if std::mem::take(&mut this.link_press) {
                        return;
                    }
                    if this.control_held {
                        this.release(event, TerminalMouseButton::Right, window);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, window, _| {
                    this.release(event, TerminalMouseButton::Middle, window);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.extend_selection(event, window, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, _| {
                this.scroll(event, window);
            }))
            // Images dropped on the terminal reach the pane as a path, on this
            // machine or the remote one it shows. The cue is an overlay, not a
            // fill on the grid, which its own rows would paint over; it is
            // absolute so the cell geometry the daemon sized never moves.
            .relative()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drop_on_terminal(paths.paths(), cx);
            }))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .border_2()
                    .border_color(drop_edge.opacity(0.0))
                    .drag_over::<ExternalPaths>(move |style, _, _, _| {
                        style.bg(drop_edge.opacity(0.10)).border_color(drop_edge)
                    }),
            );
        let luvus_missing = self.luvus_missing();
        let fullscreen = window.is_fullscreen() || window.is_simple_fullscreen();
        let sidebar = if luvus_missing {
            div()
        } else {
            self.sidebar(cx, cell_width, f32::from(window.viewport_size().height))
        };

        // A bare spacer: the sidebar, tab strip, and card each paint their
        // own background, so the root stays transparent and no chrome layer
        // ever double-darkens another.
        let header = div().h(px(HEADER_HEIGHT)).flex_shrink_0();

        let mut root = div()
            .id("pilus-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .text_color(theme.fg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .on_modifiers_changed(cx.listener(
                |this, event: &ModifiersChangedEvent, _window, _cx| {
                    this.control_held = event.modifiers.control;
                },
            ))
            // Every pilus shortcut lands on one of these, so leaving them
            // unregistered makes the whole set a no-op while Luvus is missing.
            .when(!luvus_missing, |root| {
                root.on_action(cx.listener(|this, _: &CopySelection, _window, cx| {
                    if !this.browser_edit(PageEdit::Copy, cx) {
                        this.copy_selection(cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &PasteClipboard, _window, cx| {
                    if !this.browser_edit(PageEdit::Paste, cx) {
                        this.paste_clipboard(cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &SelectAllGrid, window, cx| {
                    if !this.browser_edit(PageEdit::SelectAll, cx) {
                        this.select_all(window, cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &NewTab, _window, cx| this.new_tab(cx)))
                .on_action(cx.listener(|this, _: &OpenLocalTerminal, _window, cx| {
                    this.open_local_terminal(cx);
                }))
                .on_action(
                    cx.listener(|this, _: &SplitVertical, _window, cx| this.split("right", cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SplitHorizontal, _window, cx| this.split("down", cx)),
                )
                .on_action(cx.listener(|this, _: &CloseTab, _window, cx| {
                    this.close_active_tab();
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| {
                    this.toggle_sidebar(cx);
                }))
                .on_action(cx.listener(|this, _: &SaveDoc, _window, cx| {
                    this.files_save_doc(cx);
                }))
                .on_action(cx.listener(|this, _: &ToggleRightSidebar, _window, cx| {
                    this.toggle_right_sidebar(cx);
                }))
                .on_action(cx.listener(|this, _: &RightSidebarBack, _window, cx| {
                    if this.right.visible {
                        this.right_back(cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &RightSidebarForward, _window, cx| {
                    if this.right.visible {
                        this.right_forward(cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &OpenCommandPalette, _window, cx| {
                    // The chord toggles, so pressing it again dismisses.
                    if this.palette.is_open() {
                        this.palette.close();
                    } else {
                        this.quick_open.close();
                        this.palette.open();
                    }
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &OpenQuickOpen, _window, cx| {
                    // Like the palette, the chord toggles.
                    if this.quick_open.is_open() {
                        this.quick_open_close(cx);
                    } else {
                        this.quick_open_show(cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &FindInFile, window, cx| {
                    this.find_open(window, cx);
                }))
                .on_action(cx.listener(|this, _: &OpenWorkspacePicker, _window, cx| {
                    this.leave_settings_page();
                    this.open_picker(cx);
                }))
                .on_action(cx.listener(|this, _: &OpenSettings, _window, cx| {
                    this.open_settings(cx);
                }))
                .on_action(cx.listener(|this, _: &OpenHome, _window, cx| {
                    this.toggle_home(cx);
                }))
                .on_action(cx.listener(|this, _: &OpenSessionMenu, _window, cx| {
                    this.session_menu_open = true;
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &OpenAbout, _window, cx| {
                    about::open(this.theme, this.shared_settings.clone(), cx);
                }))
                .on_action(cx.listener(|this, _: &CheckForUpdates, _window, cx| {
                    this.check_for_updates(cx);
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &FocusTab1, _window, cx| this.focus_tab(1, cx)))
                .on_action(cx.listener(|this, _: &FocusTab2, _window, cx| this.focus_tab(2, cx)))
                .on_action(cx.listener(|this, _: &FocusTab3, _window, cx| this.focus_tab(3, cx)))
                .on_action(cx.listener(|this, _: &FocusTab4, _window, cx| this.focus_tab(4, cx)))
                .on_action(cx.listener(|this, _: &FocusTab5, _window, cx| this.focus_tab(5, cx)))
                .on_action(cx.listener(|this, _: &FocusTab6, _window, cx| this.focus_tab(6, cx)))
                .on_action(cx.listener(|this, _: &FocusTab7, _window, cx| this.focus_tab(7, cx)))
                .on_action(cx.listener(|this, _: &FocusTab8, _window, cx| this.focus_tab(8, cx)))
                .on_action(cx.listener(|this, _: &FocusTab9, _window, cx| {
                    if let Some(tab) = this.tabs.last() {
                        this.focus_tab(tab.index, cx);
                    }
                }))
            });

        // The chrome inherits the chosen UI family and size; the grid overrides
        // both.
        root = apply_chrome_font(root, &self.shared_settings);

        // Behind the sidebar and strip, so only their bare stretches reach it;
        // their controls occlude it.
        let header_drag = div()
            .id("pilus-header-drag")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .h(px(HEADER_HEIGHT))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    this.drag_window_from_header(event, window);
                }),
            );

        // Every drag (the right column's edge, the diff and git dividers) runs
        // on window-level listeners: those handles live inside occluding
        // panels, and a listener on the root only hears the pointer while
        // nothing occludes it, so the drag would freeze under the pointer.
        let view = cx.entity();
        let window_size = window.viewport_size();
        let drag_tracker = canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let (window_w, window_h) =
                    (f32::from(window_size.width), f32::from(window_size.height));
                let moving = view.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                    moving.update(cx, |this, cx| {
                        this.right_sidebar_drag_move(x, window_w, cx);
                        this.diff_divider_move(y, window_h, cx);
                        this.diff_side_divider_move(x, cx);
                        this.git_divider_move(y, window_h, cx);
                    });
                });
                let released = view.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                        return;
                    }
                    released.update(cx, |this, cx| {
                        this.right_sidebar_drag_end(cx);
                        this.diff_divider_end(cx);
                        this.git_divider_end(cx);
                    });
                });
            },
        )
        .absolute()
        .size_0();

        root.child(coat)
            .child(drag_tracker)
            .child(header_drag)
            .child(header)
            .child(
                div().relative().flex_1().child(
                    div()
                        .absolute()
                        .left(px(layout.card_x))
                        // Content already starts below the header spacer, so
                        // the top is flush (counting HEADER_HEIGHT twice
                        // pushes the whole grid down and clips its bottom).
                        .top(px(0.0))
                        .right(px(layout.card_right))
                        // Fixed bottom: any floor slack stays inside the card
                        // on the grid's own background, invisible.
                        .bottom(px(0.0))
                        .flex()
                        .flex_col()
                        .bg(card_bg)
                        .overflow_hidden()
                        .child(self.card_content(cx, grid, &layout, terminal_fill)),
                ),
            )
            // Chrome layers live at the root so they paint above the content
            // and the sidebar: the menu overlay must never slide beneath it.
            .child(sidebar)
            .child(self.top_chrome(cx, cell_width, layout.card_right, fullscreen))
            .children(
                (!luvus_missing && self.right.openness() > 0.0)
                    .then(|| self.right_sidebar_view(layout.card_right, fullscreen, cx)),
            )
            .when(!luvus_missing, |root| {
                root.child(self.session_menu(cx, cell_width, fullscreen))
                    .child(self.palette_overlay(cx))
                    .child(self.quick_open_overlay(cx))
                    .child(self.picker_overlay(window, cx))
            })
            .child(self.context_menu_overlay(cx, window))
            .child(self.menu_prompt_overlay(cx))
            .child(self.toast_stack(cx))
    }
}

/// Where to get Luvus, for the prompt shown when no `luvus` binary exists.
const LUVUS_INSTALL_URL: &str = "https://luvus.dev";

/// How often the window asks the local daemon which theme is selected.
const THEME_POLL_LOCAL: Duration = Duration::from_secs(2);
/// The same for a remote machine, whose every request crosses SSH.
const THEME_POLL_REMOTE: Duration = Duration::from_secs(5);

/// The tick loop's cadence while something is moving on screen: a tween, a
/// toast, the cursor's fade. One frame at 60 Hz.
const FRAME: Duration = Duration::from_millis(16);
/// The cadence when nothing is moving and the app is in front. State arriving
/// from worker threads (a git status, a search, a connect) is picked up within
/// this long, which is under what a person can tell; typing and daemon output do
/// not wait for it, since the daemon's frames wake the loop themselves.
const IDLE_FRONT: Duration = Duration::from_millis(50);
/// The cadence when the app is behind another one.
const IDLE_BEHIND: Duration = Duration::from_millis(250);
/// The same two cadences under energy mode: half as many wake-ups, still under
/// what a person notices for state arriving from a worker thread.
const IDLE_FRONT_ENERGY: Duration = Duration::from_millis(100);
const IDLE_BEHIND_ENERGY: Duration = Duration::from_millis(500);

/// How long the tick loop sleeps before its next pass: a frame while anything
/// is moving, and otherwise only often enough to notice what the worker threads
/// have left. A loop that runs at the frame rate forever keeps the CPU from ever
/// going idle, which is what an open window costs on a battery.
fn next_tick(moving: bool, front: bool, energy: bool) -> Duration {
    match (moving, front, energy) {
        (true, ..) => FRAME,
        (false, true, false) => IDLE_FRONT,
        (false, false, false) => IDLE_BEHIND,
        (false, true, true) => IDLE_FRONT_ENERGY,
        (false, false, true) => IDLE_BEHIND_ENERGY,
    }
}

/// Resolve on the first of the tick timer or a daemon burst landing, so a
/// frame is drained when it arrives rather than up to a tick later, and the
/// grid's cadence follows the daemon's instead of beating against the timer.
async fn tick_or_arrival(timer: impl Future<Output = ()>) {
    let mut timer = std::pin::pin!(timer);
    let mut arrival = std::pin::pin!(pilus_client::Arrival::global().wait());
    std::future::poll_fn(|cx| {
        if arrival.as_mut().poll(cx).is_ready() || timer.as_mut().poll(cx).is_ready() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Paint the visible window of the grid, clamped to the layout: the daemon
/// can lag a resize in either direction, and anything outside the card is
/// clipped anyway, so pointer and paint must agree on the same bounds.
#[allow(clippy::too_many_arguments)]
fn render_grid(
    frame: &FrameData,
    theme: Theme,
    font_size: f32,
    font_family: &str,
    line_height: Pixels,
    selection: Option<Selection>,
    cell_width: f32,
    skip_cols: u16,
    skip_rows: u16,
    max_cols: u16,
    max_rows: u16,
    glass: f32,
    slack_x: f32,
    slack_y: f32,
    panes: pane_frames::PaneFrames,
    pane_gaps: bool,
    toolbar: Option<Div>,
    cursor_alpha: f32,
) -> Div {
    let end_row = skip_rows
        .saturating_add(max_rows)
        .min(frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS));
    let mut column = div().relative().flex().flex_col().size_full();
    let surface_base = theme.pane_surface(glass);
    let surface = surface_base.opacity(glass);
    // What lies beyond the cell grid inside a split: the gutter when cards
    // float, but the card itself when panes are flush, since the cards then
    // grow over the remainder and any other color reads as an odd row.
    let remainder_base = if pane_gaps { surface_base } else { theme.bg };
    // Floating cards stroke in the surface's own color, so the ring reads as
    // gutter; flush panes outline themselves with the theme's border.
    let (border, focus_border) = if !pane_gaps {
        // Focus shows in the dimming of the other panes, not in the border.
        (theme.border, theme.border)
    } else if glass < 1.0 {
        let clear = gpui::Rgba { a: 0.0, ..surface };
        (clear, theme.border)
    } else {
        (surface, theme.border)
    };
    let tiled = panes.bounds();
    // A frame still short of the card (the daemon lags a resize) leaves its
    // missing columns to the row filler, which paints them as surface; the
    // outermost cards grow over them too, or a wide gutter reads as a column.
    let card_slack = (
        slack_x + unpainted_width(frame.width, skip_cols, max_cols, cell_width),
        slack_y,
    );
    let chromes: Vec<pane_chrome::PaneChrome> = panes
        .panes()
        .iter()
        .enumerate()
        .filter_map(|(index, pane)| {
            let tiled = tiled.as_ref()?;
            let focused = frame
                .cursor
                .is_some_and(|(cursor_column, cursor_row)| pane.holds(cursor_column, cursor_row));
            Some(pane_chrome::pane_chrome(
                index,
                pane,
                tiled,
                focused,
                pane_gaps,
                surface,
                border,
                focus_border,
                line_height,
                cell_width,
                card_slack,
                skip_cols,
                skip_rows,
            ))
        })
        .collect();
    // The surface and the cards sit under the rows. Cells the daemon paints
    // in its base color skip their fill inside the tiled box (`render_row`),
    // so the cards show through; the gutters keep the surface.
    if let Some(tiled) = panes.bounds() {
        column = column.child(pane_chrome::surface(
            &tiled,
            surface,
            line_height,
            cell_width,
            slack_x,
            skip_cols,
            skip_rows,
        ));
        for chrome in &chromes {
            column = column.child(chrome.card(theme.bg.opacity(glass)));
        }
    }
    for row in skip_rows..end_row.max(skip_rows) {
        column = column.child(render_row(
            frame,
            &panes,
            row,
            theme,
            font_size,
            font_family,
            line_height,
            selection,
            cell_width,
            skip_cols,
            max_cols,
            glass,
            slack_x,
            remainder_base,
            cursor_alpha,
        ));
    }
    // Full-bleed pools the sub-cell remainder below the last row; extend
    // that row's colors over it so no card-colored sliver shows. Always emit
    // it when a row painted: a zero or sub-pixel remainder still has to keep
    // the transparent card covered at fractional window heights.
    if end_row > skip_rows {
        column = column.child(bottom_fill(
            frame,
            end_row - 1,
            theme,
            selection,
            cell_width,
            skip_cols,
            max_cols,
            slack_x,
            slack_y,
            glass,
            panes.bounds(),
            remainder_base,
        ));
    }
    // Above the frames, so the focused pane's buttons take the pointer. An
    // unsplit tab has no frames: its one pane is the whole grid.
    let toolbar = toolbar.and_then(|bar| {
        if chromes.is_empty() {
            Some(pane_chrome::lone_toolbar(bar))
        } else {
            chromes
                .iter()
                .find(|chrome| chrome.focused())
                .map(|chrome| chrome.toolbar(bar))
        }
    });
    for chrome in chromes {
        column = column.child(chrome);
    }
    column.children(toolbar)
}

/// The sub-cell remainder the right column takes from the card, so the grid
/// meets both edges. None while the column is away or moving: a closed column
/// has nothing to take it, and the card would be left short of the window.
fn absorbed_remainder(column_px: f32, tweening: bool, slack_x: f32, cell_w: f32) -> f32 {
    if column_px > 0.0 && !tweening && slack_x < cell_w {
        slack_x
    } else {
        0.0
    }
}

/// Pixels of the card's width the frame has no columns for yet: the daemon
/// has not caught up with a window that grew.
fn unpainted_width(frame_width: u16, skip_cols: u16, max_cols: u16, cell_width: f32) -> f32 {
    f32::from(max_cols.saturating_sub(frame_width.saturating_sub(skip_cols))) * cell_width
}

/// The parts of the `cells` columns from `start` that fall outside
/// `left..=right`.
fn outside_columns(start: u16, cells: u16, left: u16, right: u16) -> Vec<(u16, u16)> {
    let end = start.saturating_add(cells);
    let mut parts = Vec::new();
    if start < left {
        parts.push((start, end.min(left) - start));
    }
    let after = right.saturating_add(1);
    if end > after {
        let from = start.max(after);
        parts.push((from, end - from));
    }
    parts
}

/// The column of the daemon's zoom badge, which is there exactly while the
/// focused pane is zoomed.
fn zoom_badge_column(frame: &FrameData) -> Option<usize> {
    let width = usize::from(frame.width);
    let row_start = usize::from(ZOOM_BADGE_ROW) * width;
    (width.saturating_sub(usize::from(ZOOM_BADGE_REACH))..width).find(|column| {
        frame
            .cells
            .get(row_start + column)
            .is_some_and(|cell| cell.symbol == ZOOM_BADGE_GLYPH)
    })
}

/// `cells` (the row's painted slice, starting `skip` columns in) with the
/// daemon's zoomed-pane chrome taken out, or `None` when the row has none.
///
/// Both pieces are painted over in the pane's own background, read from the
/// first plain pane row beneath them:
/// - the header row: its glyph goes blank and every cell sharing the strip's
///   background takes the pane's;
/// - the scroll chip under it: the filled run at the right edge, text and all.
fn without_zoom_chrome(
    frame: &FrameData,
    row: u16,
    skip: usize,
    cells: &[CellData],
) -> Option<Vec<CellData>> {
    if row != ZOOM_BADGE_ROW && row != SCROLL_CHIP_ROW {
        return None;
    }
    let badge = zoom_badge_column(frame)?;
    let width = usize::from(frame.width);
    let pane = frame
        .cells
        .get((usize::from(SCROLL_CHIP_ROW) + 1) * width + badge)?
        .bg;
    let local = badge
        .checked_sub(skip)
        .filter(|index| *index < cells.len())?;
    let mut cleared = cells.to_vec();
    if row == ZOOM_BADGE_ROW {
        let strip = cells[local].bg;
        for cell in &mut cleared {
            if cell.bg == strip {
                cell.bg = pane;
            }
        }
        cleared[local].symbol = " ".to_string();
        return Some(cleared);
    }
    let chip = cells[local].bg;
    if chip == pane {
        return None;
    }
    let run_start = (0..=local)
        .rev()
        .take_while(|index| cells[*index].bg == chip)
        .last()?;
    let run_end = (local..cells.len())
        .take_while(|index| cells[*index].bg == chip)
        .last()?;
    for cell in &mut cleared[run_start..=run_end] {
        cell.bg = pane;
        cell.symbol = " ".to_string();
    }
    Some(cleared)
}

/// Paint one grid row, skipping the dock columns our sidebar covers.
#[allow(clippy::too_many_arguments)]
fn render_row(
    frame: &FrameData,
    panes: &pane_frames::PaneFrames,
    row: u16,
    theme: Theme,
    font_size: f32,
    font_family: &str,
    line_height: Pixels,
    selection: Option<Selection>,
    cell_width: f32,
    skip_cols: u16,
    max_cols: u16,
    glass: f32,
    slack_x: f32,
    surface: gpui::Rgba,
    cursor_alpha: f32,
) -> Div {
    let width = usize::from(frame.width);
    let skip = usize::from(skip_cols).min(width);
    let paint_w = (width.saturating_sub(skip)).min(usize::from(max_cols));
    let start = usize::from(row) * width + skip;
    let end = (start + paint_w).min(frame.cells.len());
    let cells = frame.cells.get(start..end).unwrap_or(&[]);

    // Shared row width: every run pins its right edge to the same boundary the
    // next run starts on, so adjacent translucent backgrounds tile instead of
    // leaving a sub-pixel gap for the backdrop to show through as a hairline.
    let row_width = f32::from(frame.width.saturating_sub(skip_cols)) * cell_width;
    let mut element = div()
        .relative()
        .flex_shrink_0()
        .h(line_height)
        .w(px(row_width));

    // A double-width glyph covers two columns; the daemon blanks the trailing
    // continuation cell. Drop it here too so the run advances exactly one cell
    // width per column and the grid stays aligned with the daemon's geometry.
    let cleared = without_zoom_chrome(frame, row, skip, cells);
    let cells = cleared.as_deref().unwrap_or(cells);
    let mut rendered: Vec<(u16, &CellData, bool)> = Vec::with_capacity(cells.len());
    let mut column = 0usize;
    while column < cells.len() {
        let cell = &cells[column];
        let absolute = skip_cols.saturating_add(column as u16);
        rendered.push((absolute, cell, cursor_at(frame, absolute, row)));
        column += if pilus_protocol::symbol_width(symbol(cell)) >= 2 {
            2
        } else {
            1
        };
    }

    // Fill first, in color-only runs: a uniformly-filled row is then one rect
    // instead of one per fallback glyph, so the device-snapped edges cannot
    // leave backdrop-colored hairlines between them.
    let tiled = panes
        .bounds()
        .filter(|tiled| (tiled.top..=tiled.bottom).contains(&row));
    let fills = collect_background_runs(frame.width, &rendered, theme, row, selection)
        .into_iter()
        .flat_map(|(start, cells, bg)| match tiled {
            // The cards under the grid are the base inside the tiled box.
            Some(tiled) if bg == theme.bg => outside_columns(start, cells, tiled.left, tiled.right)
                .into_iter()
                .map(|(start, cells)| (start, cells, bg))
                .collect::<Vec<_>>(),
            _ => vec![(start, cells, bg)],
        });
    for (start_column, run_cells, bg) in fills {
        element = element.child(
            div()
                .absolute()
                .left(px(
                    f32::from(start_column.saturating_sub(skip_cols)) * cell_width
                ))
                .top(px(0.0))
                .right(px(row_width
                    - f32::from(
                        start_column
                            .saturating_add(run_cells)
                            .saturating_sub(skip_cols),
                    ) * cell_width))
                .h(line_height)
                .bg(bg.opacity(glass)),
        );
    }

    // Rounded by the corner setting, so zero corners keep a square block.
    if let Some(index) = rendered.iter().position(|(_, _, cursor)| *cursor) {
        let (column, cell, _) = rendered[index];
        let end_column = rendered
            .get(index + 1)
            .map_or(frame.width, |(next, _, _)| *next);
        let bg = style_of(
            cell,
            theme,
            true,
            selection.is_some_and(|s| s.contains(column, row)),
        )
        .1;
        let left = f32::from(column.saturating_sub(skip_cols)) * cell_width;
        let width = f32::from(end_column.saturating_sub(column).max(1)) * cell_width;
        element = element.child(
            div()
                .absolute()
                .left(px(left))
                .top(px(0.0))
                .w(px(width))
                .h(line_height)
                .rounded(style::RADIUS_SM.pixels())
                .bg(bg.opacity(glass * cursor_alpha)),
        );
    }

    let mut blocks = BlockBands::default();
    let mut box_rects = BoxRects::default();
    let mut box_curves: Vec<(box_drawing::Curve, gpui::Rgba)> = Vec::new();
    let light_stroke = box_drawing::light_stroke(font_size);
    for (start_column, run_cells, text, (fg, _bg, mods)) in
        collect_runs(frame.width, &rendered, theme, row, selection, cursor_alpha)
    {
        if let Some((rects, shade)) = block_element(&text) {
            let column = f32::from(start_column.saturating_sub(skip_cols));
            for rect in rects {
                blocks.add(column, rect, fg.opacity(shade));
            }
            continue;
        }
        if let Some(c) = box_drawing::box_char(&text) {
            let column = f32::from(start_column.saturating_sub(skip_cols));
            let left = (column * cell_width).round();
            let right = ((column + 1.0) * cell_width).round();
            // A pane's square daemon border is redrawn natively over the
            // grid (`pane_border`), so its glyphs are not painted here.
            if panes.is_border(start_column, row) {
                continue;
            }
            let color = fg;
            if let Some(strokes) =
                box_drawing::strokes(c, left, right, f32::from(line_height), light_stroke)
            {
                for rect in strokes.rects {
                    box_rects.add(rect, color);
                }
                box_curves.extend(strokes.curves.into_iter().map(|curve| (curve, color)));
            }
            continue;
        }
        // Runs position relative to the painted slice: the container starts
        // at the first visible column, so absolute daemon columns would
        // shove every run right by the skipped width (and clip the edge).
        //
        // The span reaches half a row past each edge with the line box still
        // centered, so the clip that keeps a fallback glyph out of its
        // neighbors' columns does not also shave glyphs taller than the row
        // (`⎿` reaching up to the line above, accents, emoji).
        let clip_cells = run_cells.saturating_add(u16::from(overflows_into_blank(
            &rendered,
            &text,
            start_column.saturating_add(run_cells),
        )));
        let mut span = div()
            .absolute()
            .left(px(
                f32::from(start_column.saturating_sub(skip_cols)) * cell_width
            ))
            .top(line_height * -0.5)
            .right(px((row_width
                - f32::from(
                    start_column
                        .saturating_add(clip_cells)
                        .saturating_sub(skip_cols),
                ) * cell_width)
                .max(0.0)))
            .h(line_height * 2.0)
            .flex()
            .items_center()
            .overflow_hidden()
            .font_family(font_family)
            .text_size(px(font_size))
            .line_height(line_height)
            .text_color(fg)
            .child(text);
        if mods & M_BOLD != 0 {
            span = span.font_weight(FontWeight::BOLD);
        }
        if mods & M_ITALIC != 0 {
            span = span.italic();
        }
        if mods & M_UNDERLINED != 0 {
            span = span.underline();
        }
        element = element.child(span);
    }
    // Whole-pixel edges: strips that end on one column then share the exact
    // same coordinate, whatever float sum produced it, and snap together.
    let height = f32::from(line_height);
    let x_edge = |x: f32| (x * cell_width).round();
    let y_edge = |y: f32| (y * height).round();
    for band in blocks.0 {
        element = element.child(
            div()
                .absolute()
                .left(px(x_edge(band.x0)))
                .w(px(x_edge(band.x1) - x_edge(band.x0)))
                .top(px(y_edge(band.y0)))
                .h(px(y_edge(band.y1) - y_edge(band.y0)))
                .bg(band.color),
        );
    }
    for (rect, color) in box_rects.0 {
        element = element.child(
            div()
                .absolute()
                .left(px(rect.x0))
                .w(px(rect.x1 - rect.x0))
                .top(px(rect.y0))
                .h(px(rect.y1 - rect.y0))
                .bg(color),
        );
    }
    if !box_curves.is_empty() {
        element = element.child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    for (curve, color) in box_curves {
                        if let Some(path) = curve.path(bounds.origin, light_stroke) {
                            window.paint_path(path, color);
                        }
                    }
                },
            )
            .absolute()
            .inset_0(),
        );
    }
    // Full-bleed pools the sub-cell remainder at the row's right edge, and a
    // lagging daemon leaves whole columns of it: extend the last background
    // over both, or the transparent card shows the backdrop as a black band
    // while the sidebar resizes.
    if slack_x > 0.5 || paint_w < usize::from(max_cols) {
        let edge_bg = match rendered.last() {
            Some((column, cell, cursor)) => {
                let selected = selection.is_some_and(|s| s.contains(*column, row));
                style_of(cell, theme, *cursor, selected).1
            }
            None => theme::resolve(0, theme.bg, &theme.ansi),
        };
        // Inside a split the base is the card, which stops at the cell grid;
        // the remainder beyond it belongs to the surface, or a strip of card
        // color shows along the right edge.
        let edge_bg = if tiled.is_some() && edge_bg == theme.bg {
            surface
        } else {
            edge_bg
        };
        element = element.child(
            div()
                .absolute()
                .left(px(paint_w as f32 * cell_width))
                .top(px(0.0))
                .w(px((f32::from(max_cols) - paint_w as f32).max(0.0)
                    * cell_width
                    + slack_x))
                .h(line_height)
                .bg(edge_bg.opacity(glass)),
        );
    }
    element
}

/// A background-only strip continuing one row's colors to the container's
/// bottom edge: that row's background runs, the last stretched over the
/// sub-cell remainder. No text paints: glyphs would clip mid-cell.
#[allow(clippy::too_many_arguments)]
fn bottom_fill(
    frame: &FrameData,
    row: u16,
    theme: Theme,
    selection: Option<Selection>,
    cell_width: f32,
    skip_cols: u16,
    max_cols: u16,
    slack_x: f32,
    slack_y: f32,
    glass: f32,
    tiled: Option<pane_frames::PaneFrame>,
    surface: gpui::Rgba,
) -> Div {
    let spans = bottom_fill_spans(frame, row, theme, selection, skip_cols, max_cols);
    // The same remainder rule as `render_row`: under a split, the base color
    // below the last row is the surface, not the card.
    let tiled = tiled.filter(|tiled| (tiled.top..=tiled.bottom).contains(&row));
    let strip_width = f32::from(max_cols) * cell_width + slack_x;
    let mut strip = div()
        .relative()
        .flex_shrink_0()
        .h(px(slack_y))
        .w(px(strip_width));
    for (index, (offset, run_cells, bg)) in spans.iter().enumerate() {
        // The last span absorbs the unpainted tail (a resize the daemon has
        // not caught up with) plus the remainder, so the strip always reaches
        // the container edge. Every other span pins its right edge to the next
        // span's shared boundary, so the translucent backgrounds tile instead
        // of leaving a backdrop-colored hairline between them.
        let right = if index + 1 == spans.len() {
            0.0
        } else {
            strip_width - f32::from(offset.saturating_add(*run_cells)) * cell_width
        };
        strip = strip.child(
            div()
                .absolute()
                .left(px(f32::from(*offset) * cell_width))
                .top(px(0.0))
                .right(px(right))
                .h_full()
                .bg(match tiled {
                    Some(tiled)
                        if *bg == theme.bg && skip_cols.saturating_add(*offset) >= tiled.left =>
                    {
                        surface
                    }
                    _ => *bg,
                }
                .opacity(glass)),
        );
    }
    strip
}

/// Group one row's painted cells into `(offset, width, background)` runs for
/// the bottom filler strip. Like [`collect_runs`] but background-only: glyph
/// flow is irrelevant when no text paints.
fn bottom_fill_spans(
    frame: &FrameData,
    row: u16,
    theme: Theme,
    selection: Option<Selection>,
    skip_cols: u16,
    max_cols: u16,
) -> Vec<(u16, u16, gpui::Rgba)> {
    let width = usize::from(frame.width);
    let skip = usize::from(skip_cols).min(width);
    let paint_w = (width.saturating_sub(skip)).min(usize::from(max_cols));
    let start = usize::from(row) * width + skip;
    let cells = frame
        .cells
        .get(start..start.saturating_add(paint_w))
        .unwrap_or(&[]);
    let mut spans = Vec::new();
    let mut index = 0usize;
    while index < cells.len() {
        let column = skip_cols.saturating_add(index as u16);
        let selected = selection.is_some_and(|s| s.contains(column, row));
        // The filler continues the row's *backgrounds*, never the caret: a
        // cursor cell's swapped color would otherwise stretch down the slack
        // below the last row and read as an oversized block cursor.
        let bg = style_of(&cells[index], theme, false, selected).1;
        let mut next = index + 1;
        while next < cells.len() {
            let column = skip_cols.saturating_add(next as u16);
            let selected = selection.is_some_and(|s| s.contains(column, row));
            if style_of(&cells[next], theme, false, selected).1 != bg {
                break;
            }
            next += 1;
        }
        spans.push((index as u16, (next - index) as u16, bg));
        index = next;
    }
    spans
}

/// Whether `symbol` can safely flow inside a multi-cell shaped run.
///
/// The grid is measured from the monospace base font's `M` advance, so an
/// ASCII run shapes to exactly `cells * cell_width`. Nerd-Font private-use
/// glyphs and emoji fall back to other fonts whose advances differ; shaping
/// them together with their neighbors shifts every later glyph in the run
/// and clips the tail (a starship prompt loses its branch's last letter and
/// the version's last digit). Isolate those cells in their own fixed-width
/// spans so the error cannot accumulate past one glyph.
///
/// Box-drawing and block elements are not shaped at all; each paints from
/// its own cell's geometry (see [`box_drawing`] and [`block_element`]).
fn is_flowable(symbol: &str) -> bool {
    pilus_protocol::symbol_width(symbol) <= 1 && symbol.is_ascii()
}

/// Whether a run's clip may widen by one cell into a blank `next_column`.
///
/// A fallback glyph is drawn at its own font's advance, which can be wider than
/// the Menlo cell: Claude Code's `⎿` loses its foot. A blank neighbor paints no
/// text, so letting the glyph finish over it cannot overlap anything; flowing
/// runs already shape to the grid and keep their exact clip.
fn overflows_into_blank(rendered: &[(u16, &CellData, bool)], text: &str, next_column: u16) -> bool {
    if is_flowable(text) {
        return false;
    }
    rendered
        .binary_search_by_key(&next_column, |(column, _, _)| *column)
        .ok()
        .is_some_and(|index| symbol(rendered[index].1) == " ")
}

/// One painted strip of block elements, in cell units within its row.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BlockBand {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    color: gpui::Rgba,
}

/// A row's block elements, merged into the fewest horizontal strips.
///
/// GPUI snaps each quad edge to the device grid on its own, so two rects
/// meeting at a fractional boundary can round apart and leave a one-pixel
/// seam (a dark line beside the eyes of the Claude Code logo). Cutting every
/// rect at the cell's half-height and joining touching strips of one color
/// leaves no vertical boundary inside a solid area for the rounding to open.
#[derive(Debug, Default)]
struct BlockBands(Vec<BlockBand>);

impl BlockBands {
    fn add(&mut self, column: f32, [x0, y0, x1, y1]: [f32; 4], color: gpui::Rgba) {
        let (x0, x1) = (column + x0, column + x1);
        if y0 < 0.5 && y1 > 0.5 {
            self.push(BlockBand {
                x0,
                x1,
                y0,
                y1: 0.5,
                color,
            });
            self.push(BlockBand {
                x0,
                x1,
                y0: 0.5,
                y1,
                color,
            });
        } else {
            self.push(BlockBand {
                x0,
                x1,
                y0,
                y1,
                color,
            });
        }
    }

    fn push(&mut self, band: BlockBand) {
        let joins = |prior: &BlockBand| {
            prior.x1 == band.x0
                && prior.y0 == band.y0
                && prior.y1 == band.y1
                && prior.color == band.color
        };
        match self.0.iter_mut().rev().find(|prior| joins(prior)) {
            Some(prior) => prior.x1 = band.x1,
            None => self.0.push(band),
        }
    }
}

/// A row's box-drawing rects, with collinear strokes of one color joined.
///
/// A border such as `────` then paints as one quad rather than one per cell,
/// and the overlapping arms of a junction (`┬` beside `─`) fold into it
/// instead of doubling a translucent color where they cross.
#[derive(Debug, Default)]
struct BoxRects(Vec<(box_drawing::Rect, gpui::Rgba)>);

impl BoxRects {
    fn add(&mut self, rect: box_drawing::Rect, color: gpui::Rgba) {
        let joins = |(prior, prior_color): &(box_drawing::Rect, gpui::Rgba)| {
            *prior_color == color
                && prior.y0 == rect.y0
                && prior.y1 == rect.y1
                && rect.x0 >= prior.x0
                && rect.x0 <= prior.x1
        };
        match self.0.iter_mut().rev().find(|prior| joins(prior)) {
            Some((prior, _)) => prior.x1 = prior.x1.max(rect.x1),
            None => self.0.push((rect, color)),
        }
    }
}

/// The cell-relative rects `[x0, y0, x1, y1]` and coverage of a block element
/// (U+2580–U+259F), or `None` for any other symbol.
///
/// A font draws these between its ascent and descent, which is shorter than
/// the row, and Menlo lacks the quadrants entirely; either way stacked blocks
/// leave gaps (the Claude Code logo splits into bands). Painting them as rects
/// fills the cell exactly, so they tile across rows as well as columns.
fn block_element(symbol: &str) -> Option<(Vec<[f32; 4]>, f32)> {
    const UL: [f32; 4] = [0.0, 0.0, 0.5, 0.5];
    const UR: [f32; 4] = [0.5, 0.0, 1.0, 0.5];
    const LL: [f32; 4] = [0.0, 0.5, 0.5, 1.0];
    const LR: [f32; 4] = [0.5, 0.5, 1.0, 1.0];
    const FULL: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
    let mut chars = symbol.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let eighths = |n: u32| n as f32 / 8.0;
    let block = match c {
        '\u{2580}' => (vec![[0.0, 0.0, 1.0, 0.5]], 1.0),
        '\u{2581}'..='\u{2588}' => {
            let n = c as u32 - 0x2580;
            (vec![[0.0, 1.0 - eighths(n), 1.0, 1.0]], 1.0)
        }
        '\u{2589}'..='\u{258f}' => {
            let n = 0x2590 - c as u32;
            (vec![[0.0, 0.0, eighths(n), 1.0]], 1.0)
        }
        '\u{2590}' => (vec![[0.5, 0.0, 1.0, 1.0]], 1.0),
        '\u{2591}' => (vec![FULL], 0.25),
        '\u{2592}' => (vec![FULL], 0.5),
        '\u{2593}' => (vec![FULL], 0.75),
        '\u{2594}' => (vec![[0.0, 0.0, 1.0, eighths(1)]], 1.0),
        '\u{2595}' => (vec![[1.0 - eighths(1), 0.0, 1.0, 1.0]], 1.0),
        '\u{2596}' => (vec![LL], 1.0),
        '\u{2597}' => (vec![LR], 1.0),
        '\u{2598}' => (vec![UL], 1.0),
        '\u{2599}' => (vec![UL, LL, LR], 1.0),
        '\u{259a}' => (vec![UL, LR], 1.0),
        '\u{259b}' => (vec![UL, UR, LL], 1.0),
        '\u{259c}' => (vec![UL, UR, LR], 1.0),
        '\u{259d}' => (vec![UR], 1.0),
        '\u{259e}' => (vec![UR, LL], 1.0),
        '\u{259f}' => (vec![UR, LL, LR], 1.0),
        _ => return None,
    };
    Some(block)
}

/// Group one row's cells into pinned runs sharing one style.
///
/// Runs split on style (so one background div paints them) and additionally
/// on [`is_flowable`], so ASCII flows while each fallback glyph gets its own
/// fixed-width span anchored to its daemon column. `frame_width` closes the
/// last run; `rendered` holds `(absolute column, cell, cursor)` with wide
/// continuations already dropped.
type PinnedRun = (u16, u16, String, (gpui::Rgba, gpui::Rgba, u16));

fn collect_runs(
    frame_width: u16,
    rendered: &[(u16, &CellData, bool)],
    theme: Theme,
    row: u16,
    selection: Option<Selection>,
    cursor_alpha: f32,
) -> Vec<PinnedRun> {
    let selected = |column: u16| selection.is_some_and(|s| s.contains(column, row));
    // The glyph under a fading cursor crosses from its own color to the
    // block's inverse, so it never pops when the block appears or leaves.
    let style_at = |cell: &CellData, cursor: bool, column: u16| {
        let mut style = style_of(cell, theme, cursor, selected(column));
        if cursor && cursor_alpha < 1.0 {
            let resting = style_of(cell, theme, false, selected(column)).0;
            style.0 = style::mix(resting, style.0, cursor_alpha);
        }
        style
    };
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < rendered.len() {
        let start_column = rendered[index].0;
        let style = style_at(rendered[index].1, rendered[index].2, start_column);
        let start_flowable = is_flowable(symbol(rendered[index].1));
        let mut text = String::new();
        let mut next = index;
        while next < rendered.len() {
            let (column, cell, cursor) = rendered[next];
            if style_at(cell, cursor, column) != style {
                break;
            }
            if next > index && (!start_flowable || !is_flowable(symbol(cell))) {
                break;
            }
            text.push_str(symbol(cell));
            next += 1;
        }
        // Pin the run to its first cell: flowing runs let a trailing space
        // trimmed by text layout shift every later column, which unaligns
        // box borders and can push the right edge off the window.
        let end_column = if next < rendered.len() {
            rendered[next].0
        } else {
            frame_width
        };
        let run_cells = end_column.saturating_sub(start_column).max(1);
        runs.push((start_column, run_cells, text, style));
        index = next;
    }
    runs
}

/// One row's background fill, grouped only by color so a uniformly filled row
/// paints as a single rect. [`collect_runs`] also splits on glyph flowability
/// to keep fallback glyphs aligned, but each extra split becomes another
/// translucent rect whose device-snapped edges can leave hairline gaps in a
/// full-bleed window — exactly where the backdrop shows through.
fn collect_background_runs(
    frame_width: u16,
    rendered: &[(u16, &CellData, bool)],
    theme: Theme,
    row: u16,
    selection: Option<Selection>,
) -> Vec<(u16, u16, gpui::Rgba)> {
    // The cursor is not a run: it paints on top as its own rounded block
    // (`cursor_block`), so the cell under its corners keeps the row's fill.
    let bg_at = |column: u16, cell: &CellData| {
        style_of(
            cell,
            theme,
            false,
            selection.is_some_and(|s| s.contains(column, row)),
        )
        .1
    };
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < rendered.len() {
        let (start_column, cell, _) = rendered[index];
        let bg = bg_at(start_column, cell);
        let mut next = index + 1;
        while next < rendered.len() {
            let (column, cell, _) = rendered[next];
            if bg_at(column, cell) != bg {
                break;
            }
            next += 1;
        }
        let end_column = if next < rendered.len() {
            rendered[next].0
        } else {
            frame_width
        };
        runs.push((
            start_column,
            end_column.saturating_sub(start_column).max(1),
            bg,
        ));
        index = next;
    }
    runs
}

/// How far (as an RGB distance, 0..=sqrt(3)) from the daemon's base surface a
/// background still counts as a blend toward it.
const BASE_BLEND_REACH: f32 = 0.12;

/// Draw `bg` against `to` instead of `base`, carrying near-base colors along.
///
/// The daemon's exact base surface becomes `to` outright. An app that fades
/// toward the terminal background (pi's logo halo, dimmed overlays) paints
/// colors a step off the base, which an equality test would leave on the old
/// surface: a visible box of the wrong tone around the fade. Shifting those by
/// the same offset, tapering to nothing at [`BASE_BLEND_REACH`], keeps the
/// fade seamless while leaving an app's own distinct backgrounds untouched.
fn remap_near(bg: gpui::Rgba, base: gpui::Rgba, to: gpui::Rgba) -> gpui::Rgba {
    let distance =
        ((bg.r - base.r).powi(2) + (bg.g - base.g).powi(2) + (bg.b - base.b).powi(2)).sqrt();
    let weight = (1.0 - distance / BASE_BLEND_REACH).clamp(0.0, 1.0);
    if weight >= 1.0 {
        return to;
    }
    gpui::Rgba {
        r: bg.r + (to.r - base.r) * weight,
        g: bg.g + (to.g - base.g) * weight,
        b: bg.b + (to.b - base.b) * weight,
        a: bg.a + (to.a - bg.a) * weight,
    }
}

fn style_of(
    cell: &CellData,
    theme: Theme,
    cursor: bool,
    selected: bool,
) -> (gpui::Rgba, gpui::Rgba, u16) {
    let mut fg = theme::resolve(cell.fg, theme.fg, &theme.ansi);
    let mut bg = theme::resolve(cell.bg, theme.bg, &theme.ansi);
    if let Some(base) = theme.remap_bg {
        bg = remap_near(bg, base, theme.bg);
    }
    let mut mods = cell.mods;
    if cursor {
        // A caret block is the cell's declared foreground painted as the
        // background. Apps that draw their own caret (a REVERSED cell) already
        // encode it here, so also applying REVERSED would invert twice and
        // erase the caret.
        std::mem::swap(&mut fg, &mut bg);
        if fg == bg {
            fg = theme.accent;
        }
    } else if mods & M_REVERSED != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    mods &= !M_REVERSED;
    if selected {
        fg = theme.selection_fg;
        bg = theme.selection_bg;
    }
    (fg, bg, mods)
}

fn cursor_at(frame: &FrameData, column: u16, row: u16) -> bool {
    frame.cursor_visible && frame.cursor == Some((column, row))
}

/// Tint alpha for `blur` (0.0..=1.0): the constant glass floor at full
/// blur, deepening toward [`FROST_MAX`] more as the backdrop sharpens — a
/// sharper desktop fights grid text more, so it asks for a deeper tint.
fn coat_tint(blur: f32) -> f32 {
    GLASS_TINT + (1.0 - blur.clamp(0.0, 1.0)) * FROST_MAX
}

/// The window's glass coat, painted behind every surface: a tint gradient
/// deepening toward the bottom plus a sheen band under the top edge, which
/// is what turns the frosted backdrop into a lit pane. The opaque window
/// only needs its flat base.
fn glass_coat(theme: Theme, blur: f32, opaque: bool) -> Div {
    if opaque {
        return div().absolute().inset_0().bg(theme.window_bg);
    }
    let tint = coat_tint(blur);
    div()
        .absolute()
        .inset_0()
        .child(div().absolute().inset_0().bg(linear_gradient(
            180.0,
            linear_color_stop(theme.window_bg.opacity(tint * TINT_TOP), 0.0),
            linear_color_stop(theme.window_bg.opacity(tint), 1.0),
        )))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(relative(SHEEN_HEIGHT))
                .bg(linear_gradient(
                    180.0,
                    linear_color_stop(theme.panel_fg.opacity(SHEEN_MAX), 0.0),
                    linear_color_stop(theme.panel_fg.opacity(0.0), 1.0),
                )),
        )
}

/// Apply the user's Interface font family and base size to a window root, so
/// every native surface starts from the same chrome type.
fn apply_chrome_font<T: Styled>(root: T, settings: &Rc<RefCell<Settings>>) -> T {
    let settings = settings.borrow();
    style::sync_corner_radius(&settings);
    let root = root.text_size(px(settings.ui_font_size));
    match settings.ui_font.as_ref() {
        Some(font) => root.font_family(font.clone()),
        None => root,
    }
}

fn symbol(cell: &CellData) -> &str {
    if cell.symbol.is_empty() {
        " "
    } else {
        cell.symbol.as_str()
    }
}

/// Run one `luvus` command for the Settings page and fold its combined
/// output into an outcome.
///
/// The daemon is restarted through its own CLI, which knows the selected
/// session; install is handled by [`crate::luvus_update`] instead.
fn run_luvus_command(root: &Path, session: Option<&str>, args: &[&str]) -> DaemonOutcome {
    let Some(binary) = luvus_binary() else {
        return DaemonOutcome::Message(Err("luvus binary not found on PATH".to_string()));
    };
    let mut command = std::process::Command::new(binary);
    if let Some(name) = session {
        command.arg("--session").arg(name);
    }
    let output = command.args(args).env(HOME_ENV, root).output();
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            if output.status.success() {
                DaemonOutcome::Message(Ok(if text.is_empty() {
                    "Done.".to_string()
                } else {
                    text
                }))
            } else {
                DaemonOutcome::Message(Err(if text.is_empty() {
                    "The command failed.".to_string()
                } else {
                    text
                }))
            }
        }
        Err(error) => DaemonOutcome::Message(Err(error.to_string())),
    }
}

/// Bounded exponential backoff with jitter for a machine reconnect.
///
/// `failures` is the count of consecutive failed attempts; the delay doubles
/// up to [`RECONNECT_MAX`] and lands in 75–125% of the nominal value so many
/// machines dropping together do not retry in lockstep.
fn reconnect_delay(failures: u32) -> Duration {
    let exponent = failures.min(5);
    let base = (RECONNECT_BASE.as_millis() * (1u128 << exponent)).min(RECONNECT_MAX.as_millis());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u128::from(elapsed.subsec_nanos()))
        .unwrap_or(0);
    let millis = (base * (75 + nanos % 51) / 100) as u64;
    Duration::from_millis(millis.max(1))
}

/// Compact byte count for the download badge (`12.4 MB`).
fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A baked lucide glyph at `size` logical pixels, tinted `color`.
///
/// GPUI paints SVGs as an alpha mask tinted with the element's own text
/// color, so every icon sets it explicitly instead of inheriting.
/// The glyph for a `Settings::session_icon` key. Unknown keys cannot reach
/// it, but fall back to the default so the match stays total.
fn session_icon_glyph(key: &str) -> &'static [u8] {
    match key {
        "blocks" => include_bytes!("../../assets/icons/blocks.svg"),
        "briefcase" => include_bytes!("../../assets/icons/briefcase.svg"),
        "monitor-cloud" => include_bytes!("../../assets/icons/monitor-cloud.svg"),
        "cloud" => include_bytes!("../../assets/icons/cloud.svg"),
        "user" => include_bytes!("../../assets/icons/user.svg"),
        "flask-conical" => include_bytes!("../../assets/icons/flask-conical.svg"),
        _ => include_bytes!("../../assets/icons/layers.svg"),
    }
}

fn icon(bytes: &'static [u8], size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg()
        .data(bytes)
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .text_color(color)
}

/// The compact face shared by composer picker chips: a leading glyph, the
/// value, and a chevron, highlighting while its dropdown is open.
///
/// `highlight` is the fill the chip takes while open or hovered: the panel
/// on Home (whose composer is transparent), the window background in the
/// session composer (whose panel already carries the fill).
#[allow(clippy::too_many_arguments)]
fn picker_face(
    id: impl Into<ElementId>,
    leading: AnyElement,
    value: String,
    value_max_w: f32,
    open: bool,
    highlight: gpui::Rgba,
    theme: Theme,
    font_size: f32,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let fg = if open { theme.panel_fg } else { theme.muted };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(PICKER_CHIP_H))
        .px_2()
        .radius_md()
        .text_color(fg)
        .cursor_pointer()
        .state_fill(
            if open {
                highlight
            } else {
                theme.panel.opacity(0.0)
            },
            highlight,
            style::RADIUS_MD,
        )
        .hover(move |style| style.text_color(theme.panel_fg))
        .on_click(on_toggle)
        .child(leading)
        .child(
            div()
                .max_w(px(value_max_w))
                .truncate()
                .text_size(px(font_size))
                .text_color(fg)
                .child(value),
        )
        .child(icon(
            include_bytes!("../../assets/icons/chevron-down.svg"),
            12.0,
            theme.muted,
        ))
}

/// The floated option list shared by composer pickers. The caller adds its
/// rows, a max height, and the deferred/anchored wrapper — below the chip on
/// Home, above it in the session composer.
fn picker_menu(id: impl Into<ElementId>, theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_1()
        .p_1()
        .w(px(PICKER_MENU_W))
        .overflow_y_scroll()
        .occlude()
        .radius_lg()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
}

/// One option row in a composer picker menu. The active row (Home's cursor,
/// or the session's current value) takes the panel fill and a bold label.
fn picker_row(
    id: impl Into<ElementId>,
    label: String,
    active: bool,
    theme: Theme,
    font_size: f32,
    on_choose: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    picker_option_row(id, label, None, active, theme, font_size, on_choose)
}

/// [`picker_row`] with a dim hint at the trailing edge — the provider that
/// owns a model, say. Display names repeat across providers, so the hint is
/// what tells two rows of a long catalog apart.
fn picker_option_row(
    id: impl Into<ElementId>,
    label: String,
    hint: Option<String>,
    active: bool,
    theme: Theme,
    font_size: f32,
    on_choose: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let hint_font = font_size;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(PICKER_ROW_H))
        .px_3()
        .radius_md()
        .text_color(theme.panel_fg)
        .font_weight(if active {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        })
        .text_size(px(font_size))
        .cursor_pointer()
        .state_fill(
            if active { theme.panel } else { theme.bg },
            theme.panel,
            style::RADIUS_MD,
        )
        // Long values (session prompts) truncate to one row.
        .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
        .when_some(hint, |row, hint| {
            row.child(
                div()
                    .flex_shrink_0()
                    .pl_2()
                    .truncate()
                    .text_size(px(hint_font))
                    .text_color(theme.muted)
                    .child(hint),
            )
        })
        .on_click(on_choose)
}

/// The leading glyph of one sidebar context-menu row.
fn sidebar_menu_icon(item: SidebarItem) -> &'static [u8] {
    match item {
        SidebarItem::Close => include_bytes!("../../assets/icons/x.svg"),
        SidebarItem::Rename => include_bytes!("../../assets/icons/pencil.svg"),
        SidebarItem::PinWorkspace => include_bytes!("../../assets/icons/pin.svg"),
        SidebarItem::UnpinWorkspace => include_bytes!("../../assets/icons/minus.svg"),
        SidebarItem::TogglePaths => include_bytes!("../../assets/icons/folder.svg"),
        SidebarItem::QuickActions => include_bytes!("../../assets/icons/chevron-right.svg"),
        SidebarItem::CopyPath | SidebarItem::CopyBranch => {
            include_bytes!("../../assets/icons/copy.svg")
        }
        SidebarItem::NewWorktree => include_bytes!("../../assets/icons/git-branch.svg"),
        SidebarItem::DeleteWorktree => include_bytes!("../../assets/icons/trash-2.svg"),
        SidebarItem::ToggleScope => include_bytes!("../../assets/icons/monitor.svg"),
        SidebarItem::NewFile => include_bytes!("../../assets/icons/file-plus.svg"),
        SidebarItem::NewFolder => include_bytes!("../../assets/icons/folder-plus.svg"),
        SidebarItem::Upload => include_bytes!("../../assets/icons/upload.svg"),
        SidebarItem::Download | SidebarItem::DownloadArchive => {
            include_bytes!("../../assets/icons/download.svg")
        }
        SidebarItem::Delete => include_bytes!("../../assets/icons/trash-2.svg"),
        SidebarItem::CopySelection => include_bytes!("../../assets/icons/copy.svg"),
        SidebarItem::Paste => include_bytes!("../../assets/icons/clipboard.svg"),
        SidebarItem::SplitRight => include_bytes!("../../assets/icons/columns-2.svg"),
        SidebarItem::SplitDown => include_bytes!("../../assets/icons/rows-2.svg"),
        SidebarItem::ForkPane => include_bytes!("../../assets/icons/git-fork.svg"),
        SidebarItem::MoveToTab => include_bytes!("../../assets/icons/corner-down-right.svg"),
        SidebarItem::NewTab => include_bytes!("../../assets/icons/plus.svg"),
        SidebarItem::CloseTab => include_bytes!("../../assets/icons/x.svg"),
        SidebarItem::GithubAll => include_bytes!("../../assets/icons/list.svg"),
        SidebarItem::GithubRefresh => include_bytes!("../../assets/icons/refresh-cw.svg"),
        SidebarItem::GithubHideSection => include_bytes!("../../assets/icons/minus.svg"),
        SidebarItem::GithubDisable => include_bytes!("../../assets/icons/x.svg"),
        SidebarItem::Divider => include_bytes!("../../assets/icons/plus.svg"),
    }
}

/// Resolve a path's physical spelling, falling back to the given text when
/// the path is not reachable from this machine.
///
/// Git canonicalizes the checkout paths `worktree.list` reports (`/tmp` is
/// `/private/tmp` on macOS), while a workspace's projected cwd and panes can
/// keep the symlink spelling. Matching one against the other by text alone
/// fails, so both sides are canonicalized before comparing; a remote
/// endpoint's path usually does not resolve locally, and the fallback keeps
/// that comparison a plain string match.
fn canonical(spelling: &str) -> String {
    match std::fs::canonicalize(spelling) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => spelling.to_string(),
    }
}

/// Trim trailing path separators (never the root), so `worktree.list`'s
/// absolute paths compare against an expanded cwd spelled with or without a
/// trailing slash.
fn trims(mut path: &str) -> &str {
    while path != "/" && path.ends_with('/') {
        path = &path[..path.len() - 1];
    }
    path
}

/// Whether `candidate` is `target` or a directory inside it.
fn inside(candidate: &str, target: &str) -> bool {
    candidate == target
        || (candidate.starts_with(target) && candidate[target.len()..].starts_with('/'))
}

/// The linked checkout root containing `cwd`, or `None` when the workspace
/// sits on the main checkout or outside every worktree.
///
/// A workspace's projected cwd can name a subdirectory of the checkout, so
/// matching by equality alone misses it and the Delete Worktree row never
/// appears; matching by `inside` finds the root `worktree.remove` actually
/// needs. The longest match wins so a worktree nested inside another binds
/// to its own root.
fn linked_worktree_path(worktrees: &[WorktreeInfo], cwd: &str) -> Option<String> {
    if let Some(tail) = cwd.trim().strip_prefix("~/") {
        return linked_worktree_by_tail(worktrees, trims(tail));
    }
    let cwd = trims(&canonical(cwd)).to_string();
    worktrees
        .iter()
        .filter(|worktree| !worktree.main)
        .filter_map(|worktree| {
            let root = trims(&canonical(&worktree.path)).to_string();
            inside(&cwd, &root).then(|| (root.len(), worktree.path.clone()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
}

/// Match a `~`-shorthand cwd whose machine home is not known yet.
///
/// A remote endpoint's `$HOME` resolves asynchronously, so until it lands the
/// cwd cannot be expanded and an absolute comparison finds nothing, which
/// silently drops the Delete Worktree row. The home-relative tail is still
/// enough: a worktree root matches when it ends with some leading part of
/// that tail, and the longest such part wins.
fn linked_worktree_by_tail(worktrees: &[WorktreeInfo], tail: &str) -> Option<String> {
    let mut ends: Vec<usize> = tail.match_indices('/').map(|(at, _)| at).collect();
    ends.push(tail.len());
    worktrees
        .iter()
        .filter(|worktree| !worktree.main)
        .filter_map(|worktree| {
            let root = trims(&worktree.path);
            ends.iter()
                .rev()
                .find(|end| {
                    let part = &tail[..**end];
                    !part.is_empty() && root.ends_with(&format!("/{part}"))
                })
                .map(|end| (*end, worktree.path.clone()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
}

/// Vendored brand mark for the daemon's agent kind.
///
/// The daemon's canonical ids are lowercase and stable, but aliases and
/// user-defined manifests reach us too, so each brand is probed by substring.
/// Unknown kinds fall back to a generic bot glyph so every row keeps a leading
/// icon. Probes are ordered so a longer name wins over one inside it: `copilot`
/// must beat `pi`.
fn agent_icon(kind: &str) -> &'static [u8] {
    let kind = kind.trim().to_ascii_lowercase();
    let probes: &[(&str, &'static [u8])] = &[
        (
            "claude",
            include_bytes!("../../assets/icons/agents/claude-code.svg"),
        ),
        (
            "copilot",
            include_bytes!("../../assets/icons/agents/github-copilot.svg"),
        ),
        (
            "codex",
            include_bytes!("../../assets/icons/agents/codex.svg"),
        ),
        (
            "openai",
            include_bytes!("../../assets/icons/agents/codex.svg"),
        ),
        (
            "antigravity",
            include_bytes!("../../assets/icons/agents/antigravity.svg"),
        ),
        (
            "opencode",
            include_bytes!("../../assets/icons/agents/opencode.svg"),
        ),
        ("kimi", include_bytes!("../../assets/icons/agents/kimi.svg")),
        ("grok", include_bytes!("../../assets/icons/agents/grok.svg")),
        (
            "hermes",
            include_bytes!("../../assets/icons/agents/hermes.svg"),
        ),
        (
            "cursor",
            include_bytes!("../../assets/icons/agents/cursor.svg"),
        ),
        (
            "kilo",
            include_bytes!("../../assets/icons/agents/kilo-code.svg"),
        ),
        (
            "gemini",
            include_bytes!("../../assets/icons/agents/gemini.svg"),
        ),
        ("qwen", include_bytes!("../../assets/icons/agents/qwen.svg")),
        // Devin's daemon id is the agent name; `cognition` is its vendor, which
        // manifests spell out instead.
        (
            "cognition",
            include_bytes!("../../assets/icons/agents/devin.svg"),
        ),
        (
            "devin",
            include_bytes!("../../assets/icons/agents/devin.svg"),
        ),
        ("arc", include_bytes!("../../assets/icons/agents/arc.svg")),
        ("omp", include_bytes!("../../assets/icons/agents/pi.svg")),
        ("pi", include_bytes!("../../assets/icons/agents/pi.svg")),
    ];
    for (needle, bytes) in probes {
        if kind.contains(needle) {
            return bytes;
        }
    }
    include_bytes!("../../assets/icons/agents/bot.svg")
}

/// A hover card that reveals a sidebar row's full, untruncated text.
struct SidebarTip {
    title: String,
    title_color: gpui::Rgba,
    lines: Vec<(String, gpui::Rgba)>,
    theme: Theme,
}

impl Render for SidebarTip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            // Paths wrap at `/`, so a deep cwd stays inside the card.
            .max_w(px(420.0))
            .px_3()
            .py_2()
            .radius_md()
            .bg(theme.panel)
            .text_size(px(SECTION_FONT))
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(self.title_color)
                    .child(self.title.clone()),
            );
        for (text, color) in &self.lines {
            card = card.child(div().text_color(*color).child(text.clone()));
        }
        // Each hover builds a fresh tip view, so the fade plays every time
        // one appears.
        style::enter("pilus-sidebar-tip-enter", card)
    }
}

/// The tooltip builder for a sidebar row, which truncates its text.
fn row_tooltip(
    title: String,
    title_color: gpui::Rgba,
    lines: Vec<(String, gpui::Rgba)>,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| SidebarTip {
            title: title.clone(),
            title_color,
            lines: lines.clone(),
            theme,
        })
        .into()
    }
}

/// A machine catalog tagged with the session it was read for, so a result
/// that lands after a session switch is not applied to the new session.
type MachineCatalog = (Option<String>, Vec<MachineInfo>);

/// Outcome of a background session start, stop, or delete.
enum SessionOpResult {
    Started(String),
    StartFailed(String, String),
    /// A stop or delete finished.
    Done(String, SessionVerb),
    /// A stop or delete failed, with the CLI's reason.
    Failed(String, SessionVerb, String),
}

/// Start a named session's server and wait for its socket, off-thread.
fn start_session_blocking(root: &Path, name: &str) -> SessionOpResult {
    match pilus_client::ensure_server_for(root, Some(name)) {
        Ok(()) => SessionOpResult::Started(name.to_string()),
        Err(pilus_client::Error::NotFound { .. }) => {
            SessionOpResult::StartFailed(name.to_string(), "luvus not found".to_string())
        }
        Err(error) => SessionOpResult::StartFailed(name.to_string(), error.to_string()),
    }
}

/// The control socket for `session` (`None` is default) under `root`.
fn api_socket_for(root: &Path, session: Option<&str>) -> PathBuf {
    match session {
        Some(name) => root.join("sessions").join(name).join("luvus.sock"),
        None => root.join("luvus.sock"),
    }
}

/// The control socket for a launch target, matching `PilusView`'s derivation so
/// the Appearances window talks to the same daemon session.
fn api_socket_for_target(target: &ConnectTarget) -> PathBuf {
    let (root, session) = match &target.socket {
        Some(path) => split_client_socket(path).unwrap_or_else(|| (config_dir(), None)),
        None => (config_dir(), target.session.clone()),
    };
    api_socket_for(&root, session.as_deref())
}

/// The status label and color for a saved machine, mirroring the manager.
fn machine_status_text(machine: &MachineInfo, theme: Theme) -> (String, gpui::Rgba) {
    if !machine.enabled {
        return ("disabled".to_string(), theme.muted);
    }
    match (machine.online, machine.latency_ms) {
        (Some(true), Some(ms)) => (format!("online · {ms} ms"), theme.ok),
        (Some(true), None) => ("online".to_string(), theme.ok),
        (Some(false), _) => ("offline".to_string(), theme.alert),
        (None, _) => ("probing…".to_string(), theme.muted),
    }
}

/// Recover a `~`-shortened path by aligning `path` with a shorthand cwd.
///
/// The daemon reports workspace cwds as `~`-shorthand but pane cwds absolute,
/// so a matching tail names the home prefix without probing it.
fn shorten_like_shorthand(path: &str, shorthand: &str) -> Option<String> {
    if path.starts_with('~') {
        return None;
    }
    let rest = shorthand.strip_prefix("~/")?;
    if rest.is_empty() {
        return None;
    }
    let needle = format!("/{rest}");
    let index = path.find(&needle)?;
    let after = index + needle.len();
    // The match must start at a path boundary and end one, or `repo` would
    // shorten a sibling named `repo2`.
    if index == 0 || (after < path.len() && path.as_bytes()[after] != b'/') {
        return None;
    }
    Some(format!("~{}", &path[index..]))
}

/// Shorten a path with `~` when it sits under `home`.
fn shorten_against(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

/// Shorten a workspace path with `~`, the way the mock's breadcrumb reads.
fn shorten_home(path: &str) -> String {
    match std::env::var_os("HOME") {
        Some(home) => shorten_against(path, &home.to_string_lossy()),
        None => path.to_string(),
    }
}

/// Cap a tab label at `max` chars so one long name cannot crowd the strip.
fn truncate_label(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_string();
    }
    let kept: String = label.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Collapse every directory but the last to its initial (`.s` for a dotted
/// one), so `~/.superset/projects/l3s` reads `~/.s/p/l3s` in a narrow tab.
fn abbreviate_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i == last || *part == "~" {
                return (*part).to_string();
            }
            let take = if part.starts_with('.') { 2 } else { 1 };
            part.chars().take(take).collect()
        })
        .collect::<Vec<String>>()
        .join("/")
}

/// A tab's label: the name the user gave it, else its agent's session title,
/// else its abbreviated cwd. A name the user typed outranks the title because
/// the title changes under them.
///
/// A `lone` tab is as wide as its label, so it needs none of that: the cwd
/// stays whole and nothing is capped.
fn tab_label(name: &str, session_title: Option<&str>, cwd: &str, lone: bool) -> String {
    let name = name.trim();
    let title = session_title.map(str::trim).unwrap_or_default();
    let text = if !name.is_empty() {
        name.to_string()
    } else if !title.is_empty() {
        title.to_string()
    } else {
        if lone {
            cwd.to_string()
        } else {
            abbreviate_path(cwd)
        }
    };
    if lone {
        text
    } else {
        truncate_label(&text, 24)
    }
}

/// The longest name a tab takes, so one title cannot crowd the strip.
const TAB_NAME_MAX: usize = 40;

/// Where the tab at position `at` sits once the tab at `from` has moved to
/// `to`: the moved tab lands on `to` and those between shift one step toward
/// the gap it left.
fn moved_position(at: u16, from: u16, to: u16) -> u16 {
    if at == from {
        to
    } else if from < to && at > from && at <= to {
        at - 1
    } else if from > to && at >= to && at < from {
        at + 1
    } else {
        at
    }
}

/// A tab being dragged along the strip, carrying the position it started at.
#[derive(Clone)]
struct TabDrag {
    index: u16,
    label: String,
    theme: Theme,
    font_size: f32,
}

/// What GPUI draws at the pointer for a tab drag: the tab's own pill, so the
/// drag reads as lifting it.
impl Render for TabDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(TAB_HEIGHT))
            .px_3()
            .radius_soft()
            .bg(self.theme.panel)
            .text_color(self.theme.panel_fg)
            .text_size(px(self.font_size))
            .child(self.label.clone())
    }
}

/// A SIPIL sidebar row being dragged to a new place.
#[derive(Clone)]
struct SipilDrag {
    key: String,
    label: String,
    theme: Theme,
    font_size: f32,
}

/// What GPUI draws at the pointer for a row drag: the selected row's pill.
impl Render for SipilDrag {
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
            .child(self.label.clone())
    }
}

/// A color for an agent status dot.
fn status_color(status: &str, theme: Theme) -> gpui::Rgba {
    match status {
        "working" => theme.accent,
        "blocked" => theme.alert,
        "done" => theme.ok,
        _ => theme.muted,
    }
}

/// How long a menu bar pick waits for its endpoint before giving up.
const MENU_PICK_PATIENCE: Duration = Duration::from_secs(20);

/// An agent as the menu bar draws it: the label, its status as a colored dot,
/// and the working directory for the tooltip. `path` is already
/// shortened by the caller, which knows whose home it is relative to.
fn agent_menu_row(
    endpoint: &str,
    group: &str,
    agent: &Agent,
    path: String,
    theme: Theme,
) -> menubar::Row {
    let color = status_color(&agent.status, theme);
    menubar::Row {
        endpoint: endpoint.to_string(),
        group: group.to_string(),
        pane: agent.pane.clone(),
        title: agent.label().to_string(),
        status: agent.status.clone(),
        path,
        color: [color.r, color.g, color.b].map(|c| (c * 255.0).round() as u8),
    }
}

/// Title-case the daemon's lowercase status (`working` -> `Working`).
fn status_label(status: &str) -> String {
    let mut chars = status.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Whether a daemon's reported sidebars match the window's layout, ignoring
/// the `agents` dock the window strips before sending.
fn adopts_layout(
    sidebars: &ShellSidebars,
    layout: Option<&pilus_protocol::SidebarsConfig>,
) -> bool {
    let mut reported = sidebars.layout.clone();
    remove_dock(&mut reported.left, "agents");
    remove_dock(&mut reported.right, "agents");
    Some(&reported) == layout
}

/// Remove one dock id and its parallel weight from a sidebar config.
fn remove_dock(side: &mut pilus_protocol::SideConfig, name: &str) {
    if let Some(index) = side.docks.iter().position(|dock| dock == name) {
        side.docks.remove(index);
        if index < side.dock_weights.len() {
            side.dock_weights.remove(index);
        }
    }
}

/// Open the workspace at `location`, then start and prompt the chosen agent
/// there.
///
/// `agent.start` spawns beside the focused pane, so the workspace open must
/// land first; both run in one background thread in the caller. `args` are the
/// agent's own launch flags — pi's model and thinking level, which the catalog
/// marks default — so a fresh session opens where the user left off.
fn launch_home_agent(
    client: &mut ControlClient,
    location: &str,
    name: &str,
    agent: &str,
    args: &[String],
    prompt: &str,
) -> Result<(String, Option<String>), String> {
    client
        .open_workspace(location)
        .map_err(|error| error.to_string())?;
    // A fresh workspace opens with one shell pane. Reuse it for the agent so
    // the launch does not split a second, empty terminal beside Pi.
    let pane = client.idle_pane().ok().flatten();
    client
        .start_agent(name, agent, pane.as_deref(), args)
        .map_err(|error| error.to_string())?;
    // Go straight into the agent's pane rather than whatever pane the daemon
    // left focused, so the prompt lands in front of the user.
    if let Some(pane) = pane.as_deref() {
        client.focus_pane(pane).ok();
    }
    // Target the pane directly: a fresh agent's alias may not be registered
    // yet, but the pane id always resolves.
    let target = pane.as_deref().unwrap_or(name);
    client
        .prompt_agent(target, prompt)
        .map_err(|error| error.to_string())?;
    Ok((format!("started {agent} as {name}"), pane))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tick_loop_runs_at_the_frame_rate_only_while_something_moves() {
        assert_eq!(next_tick(true, true, false), FRAME);
        assert_eq!(next_tick(true, false, true), FRAME);
        assert_eq!(next_tick(false, true, false), IDLE_FRONT);
        assert_eq!(next_tick(false, false, false), IDLE_BEHIND);
        assert_eq!(next_tick(false, true, true), IDLE_FRONT_ENERGY);
        assert_eq!(next_tick(false, false, true), IDLE_BEHIND_ENERGY);
        assert!(FRAME < IDLE_FRONT && IDLE_FRONT < IDLE_BEHIND);
    }

    /// A 12-wide frame under a 2-column skip: row 1 is the header strip
    /// (bg 5) with the badge at column 9, row 2 and 3 the pane (bg 9).
    fn zoomed_frame() -> FrameData {
        let mut frame = FrameData::blank(12, 4, 0, 9);
        for column in 0..12 {
            frame.cells[12 + column] = CellData::blank(0, 5);
        }
        frame.cells[12 + 9] = CellData {
            symbol: ZOOM_BADGE_GLYPH.to_string(),
            fg: 7,
            bg: 5,
            mods: 1,
        };
        // The border column keeps its own background.
        frame.cells[12 + 11] = CellData::blank(0, 3);
        frame
    }

    fn row_slice(frame: &FrameData, row: usize) -> &[CellData] {
        &frame.cells[row * 12 + 2..(row + 1) * 12]
    }

    #[test]
    fn the_zoom_header_takes_the_pane_background() {
        let mut frame = zoomed_frame();
        assert_eq!(zoom_badge_column(&frame), Some(9));
        let cleared =
            without_zoom_chrome(&frame, ZOOM_BADGE_ROW, 2, row_slice(&frame, 1)).expect("header");
        assert_eq!(cleared[7].symbol, " ");
        assert!(cleared[..9].iter().all(|cell| cell.bg == 9));
        assert_eq!(cleared[9].bg, 3);
        // An unzoomed row, or a glyph away from the right edge, is a TUI's own.
        assert!(without_zoom_chrome(&frame, 3, 2, row_slice(&frame, 3)).is_none());
        frame.cells[12 + 9] = CellData::blank(0, 5);
        frame.cells[12 + 3].symbol = ZOOM_BADGE_GLYPH.to_string();
        assert_eq!(zoom_badge_column(&frame), None);
        assert!(without_zoom_chrome(&frame, 1, 2, row_slice(&frame, 1)).is_none());
    }

    #[test]
    fn the_scroll_chip_in_a_zoomed_pane_is_painted_out() {
        let mut frame = zoomed_frame();
        // Row 2 is plain until a scroll puts a chip in columns 7..=10.
        let plain = row_slice(&frame, 2).to_vec();
        assert!(without_zoom_chrome(&frame, SCROLL_CHIP_ROW, 2, &plain).is_none());
        for (column, symbol) in [(7, "\u{2191}"), (8, "2"), (9, "4"), (10, " ")] {
            frame.cells[2 * 12 + column] = CellData {
                symbol: symbol.to_string(),
                fg: 7,
                bg: 6,
                mods: 0,
            };
        }
        // Text outside the chip keeps its own look.
        frame.cells[2 * 12 + 5].symbol = "x".to_string();
        let cleared =
            without_zoom_chrome(&frame, SCROLL_CHIP_ROW, 2, row_slice(&frame, 2)).expect("chip");
        assert_eq!(cleared[3].symbol, "x");
        assert!(
            cleared[5..=8]
                .iter()
                .all(|cell| cell.symbol == " " && cell.bg == 9)
        );
        // The strip above still resolves to the pane, not to the chip.
        let header =
            without_zoom_chrome(&frame, ZOOM_BADGE_ROW, 2, row_slice(&frame, 1)).expect("header");
        assert_eq!(header[0].bg, 9);
    }

    #[test]
    fn a_closed_column_takes_no_remainder() {
        assert_eq!(absorbed_remainder(0.0, false, 5.0, 8.0), 0.0);
        assert_eq!(absorbed_remainder(482.0, false, 5.0, 8.0), 5.0);
        assert_eq!(absorbed_remainder(482.0, true, 5.0, 8.0), 0.0);
    }

    #[test]
    fn a_lagging_frame_leaves_its_missing_columns_unpainted() {
        assert_eq!(unpainted_width(100, 10, 90, 8.0), 0.0);
        assert_eq!(unpainted_width(100, 10, 93, 8.0), 24.0);
        // A frame wider than the card has nothing missing.
        assert_eq!(unpainted_width(120, 10, 90, 8.0), 0.0);
    }

    fn strip(ids: &[&str]) -> Vec<Tab> {
        ids.iter()
            .enumerate()
            .map(|(position, id)| Tab {
                index: position as u16 + 1,
                id: (*id).to_string(),
                ..Tab::default()
            })
            .collect()
    }

    #[test]
    fn tabs_pick_holds_off_only_the_list_it_left() {
        let pick = TabsPick {
            from: vec!["a".into(), "b".into()],
            at: Instant::now(),
        };
        assert!(pick.is_stale(&strip(&["a", "b"])));
        assert!(!pick.is_stale(&strip(&["c", "d"])));
        assert!(!pick.is_stale(&strip(&["a"])));
    }

    use super::*;

    /// A blank cell carrying only attribute bits.
    fn cell(mods: u16) -> CellData {
        CellData {
            mods,
            ..CellData::default()
        }
    }

    #[test]
    fn top_spacing_keeps_the_row_under_the_daemon_tab_strip() {
        assert_eq!(skipped_rows(true), 1);
        assert_eq!(skipped_rows(false), SKIPPED_DAEMON_ROWS);
    }

    #[test]
    fn canonical_matches_git_spelling_through_a_symlink() {
        // git reports checkout paths canonicalized; a workspace cwd can keep
        // the symlink spelling. macOS `/tmp` is `/private/tmp`; the same gap
        // is reproduced with a link under the (already canonical) temp dir.
        let root = std::env::temp_dir().join(format!("pilus-canon-{}", std::process::id()));
        let real = root.join("real");
        std::fs::create_dir_all(&real).expect("real dir");
        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let spelled = link.to_string_lossy().into_owned();
        let git_spelling = canonical(&real.to_string_lossy());
        assert_eq!(canonical(&spelled), git_spelling);
        assert_ne!(spelled, git_spelling);
        // Unresolvable spellings fall back to their own text.
        let missing = root.join("missing").to_string_lossy().into_owned();
        assert_eq!(canonical(&missing), missing);
    }

    #[test]
    fn linked_worktree_path_matches_the_exact_root() {
        let worktrees = [
            WorktreeInfo {
                path: "/repo".to_string(),
                main: true,
            },
            WorktreeInfo {
                path: "/repo/feature".to_string(),
                main: false,
            },
        ];
        assert_eq!(
            linked_worktree_path(&worktrees, "/repo/feature"),
            Some("/repo/feature".to_string())
        );
    }

    #[test]
    fn linked_worktree_path_climbs_a_nested_cwd() {
        // Workspaces can open inside a linked checkout rather than at its
        // root; the root is still what `worktree.remove` must delete.
        let worktrees = [
            WorktreeInfo {
                path: "/repo".to_string(),
                main: true,
            },
            WorktreeInfo {
                path: "/repo/feature".to_string(),
                main: false,
            },
        ];
        assert_eq!(
            linked_worktree_path(&worktrees, "/repo/feature/crates/app"),
            Some("/repo/feature".to_string())
        );
    }

    #[test]
    fn linked_worktree_path_never_returns_the_main_checkout() {
        let worktrees = [WorktreeInfo {
            path: "/repo".to_string(),
            main: true,
        }];
        assert_eq!(linked_worktree_path(&worktrees, "/repo"), None);
        assert_eq!(linked_worktree_path(&worktrees, "/repo/crates/app"), None);
    }

    #[test]
    fn linked_worktree_path_matches_an_unexpanded_home_shorthand() {
        let worktrees = [
            WorktreeInfo {
                path: "/home/deploy/repo".to_string(),
                main: true,
            },
            WorktreeInfo {
                path: "/home/deploy/.luvus/worktrees/repo/feature".to_string(),
                main: false,
            },
        ];
        assert_eq!(
            linked_worktree_path(&worktrees, "~/.luvus/worktrees/repo/feature"),
            Some("/home/deploy/.luvus/worktrees/repo/feature".to_string())
        );
        assert_eq!(
            linked_worktree_path(&worktrees, "~/.luvus/worktrees/repo/feature/src"),
            Some("/home/deploy/.luvus/worktrees/repo/feature".to_string())
        );
        assert_eq!(linked_worktree_path(&worktrees, "~/repo"), None);
        assert_eq!(
            linked_worktree_path(&worktrees, "~/.luvus/worktrees/repo/feature-two"),
            None
        );
    }

    #[test]
    fn spring_departs_overshoots_and_settles() {
        assert_eq!(style::ease_spring(0.0), 0.0);
        assert_eq!(style::ease_spring(1.0), 1.0);
        // Quick off the mark, unlike an ease-in that idles at the start.
        assert!(style::ease_spring(0.15) > 0.25);
        // A gentle overshoot: elastic, never a bounce.
        let peak = (0..=100)
            .map(|step| style::ease_spring(step as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.0 && peak < 1.1, "peak {peak}");
        // Settled well before the duration ends.
        assert!((style::ease_spring(0.95) - 1.0).abs() < 0.01);
    }

    #[test]
    fn coat_tint_deepens_as_blur_drops() {
        assert_eq!(coat_tint(1.0), GLASS_TINT);
        assert_eq!(coat_tint(0.0), GLASS_TINT + FROST_MAX);
        assert_eq!(coat_tint(0.5), GLASS_TINT + FROST_MAX / 2.0);
        assert_eq!(coat_tint(2.0), GLASS_TINT);
        assert_eq!(coat_tint(-1.0), GLASS_TINT + FROST_MAX);
    }

    #[test]
    fn bottom_fill_spans_group_by_background() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 4, 0, 0);
        // First visible row: two default cells, then two red ones.
        frame.cells[2 * 4 + 2].bg = 0x0100_0001;
        frame.cells[2 * 4 + 3].bg = 0x0100_0001;
        let spans = bottom_fill_spans(&frame, 2, theme, None, 0, 4);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, 0);
        assert_eq!(spans[0].1, 2);
        assert_eq!(spans[0].2, theme.bg);
        assert_eq!(spans[1].0, 2);
        assert_eq!(spans[1].1, 2);
        assert_eq!(
            spans[1].2,
            theme::resolve(0x0100_0001, theme.bg, &theme.ansi)
        );
    }

    #[test]
    fn bottom_fill_spans_never_extend_the_caret() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 4, 0, 0);
        // The cursor cell's swapped color would be `ansi[1]`; the filler must
        // keep the row's plain background so the caret cannot grow tall.
        frame.cells[2 * 4 + 1].fg = 0x0100_0001;
        frame.cursor = Some((1, 2));
        frame.cursor_visible = true;
        let spans = bottom_fill_spans(&frame, 2, theme, None, 0, 4);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].2, theme.bg);
    }

    #[test]
    fn background_runs_merge_across_fallback_glyphs() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 1, 0, 0);
        // A Nerd-Font private-use glyph: each cell stays its own text span for
        // advance pinning, so the fill must collapse them on its own or its
        // snapped edges leave hairline gaps.
        for cell in frame.cells.iter_mut() {
            cell.symbol = "\u{e0b0}".to_string();
        }
        let rendered: Vec<(u16, &CellData, bool)> = frame
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| (index as u16, cell, false))
            .collect();
        assert_eq!(
            collect_runs(frame.width, &rendered, theme, 0, None, 1.0).len(),
            4
        );
        let background = collect_background_runs(frame.width, &rendered, theme, 0, None);
        assert_eq!(background.len(), 1);
        assert_eq!(background[0], (0, 4, theme.bg));
    }

    #[test]
    fn box_drawing_glyphs_paint_cell_by_cell() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 1, 0, 0);
        for cell in frame.cells.iter_mut() {
            cell.symbol = "\u{2500}".to_string();
        }
        let rendered: Vec<(u16, &CellData, bool)> = frame
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| (index as u16, cell, false))
            .collect();
        // Each stroke paints from its own cell's geometry; `BoxRects` joins
        // the border back into one quad.
        let runs = collect_runs(frame.width, &rendered, theme, 0, None, 1.0);
        assert_eq!(runs.len(), 4);
        assert!(
            runs.iter()
                .all(|(_, cells, text, _)| *cells == 1 && box_drawing::box_char(text).is_some())
        );
    }

    #[test]
    fn linked_worktree_path_ignores_sibling_prefixes() {
        let worktrees = [
            WorktreeInfo {
                path: "/repo".to_string(),
                main: true,
            },
            WorktreeInfo {
                path: "/repo/feature".to_string(),
                main: false,
            },
        ];
        assert_eq!(linked_worktree_path(&worktrees, "/repo/feature-two"), None);
    }

    #[test]
    fn shorthand_alignment_recovers_the_home_prefix() {
        // A pane cwd under a `~`-shorthand workspace shortens without knowing
        // the machine's `$HOME`.
        assert_eq!(
            shorten_like_shorthand("/home/deploy/repo/api", "~/repo"),
            Some("~/repo/api".to_string())
        );
        // The shorthand itself is already short; nothing to align.
        assert_eq!(shorten_like_shorthand("~/repo", "~/repo"), None);
        // A tail that never appears leaves the path alone.
        assert_eq!(shorten_like_shorthand("/home/deploy/other", "~/repo"), None);
        // A sibling sharing the prefix must not align.
        assert_eq!(shorten_like_shorthand("/home/deploy/repo2", "~/repo"), None);
    }

    #[test]
    fn shorten_against_uses_the_given_home() {
        assert_eq!(shorten_against("/home/deploy", "/home/deploy"), "~");
        assert_eq!(
            shorten_against("/home/deploy/repo/api", "/home/deploy"),
            "~/repo/api"
        );
        // A sibling with the same prefix must not shorten.
        assert_eq!(
            shorten_against("/home/deployer/repo", "/home/deploy"),
            "/home/deployer/repo"
        );
        // An unknown home leaves the path alone.
        assert_eq!(shorten_against("/repo/api", ""), "/repo/api");
    }

    #[test]
    fn menu_rows_carry_the_path_for_hover_and_a_dot_for_status() {
        let agent = Agent {
            pane: "p1".to_string(),
            agent: "claude".to_string(),
            status: "blocked".to_string(),
            cwd: "/work/api".to_string(),
            ..Agent::default()
        };
        let row = agent_menu_row(
            "local",
            "Local",
            &agent,
            "~/work/api".to_string(),
            Theme::default(),
        );
        let alert = Theme::default().alert;
        assert_eq!(
            row.color,
            [alert.r, alert.g, alert.b].map(|c| (c * 255.0).round() as u8)
        );
        assert_eq!(row.path, "~/work/api");
        assert_eq!((row.endpoint.as_str(), row.pane.as_str()), ("local", "p1"));
    }

    #[test]
    fn tab_label_falls_back_to_cwd() {
        assert_eq!(tab_label("", None, "~/work/pilus", false), "~/w/pilus");
        assert_eq!(tab_label("  ", Some(" "), "~", false), "~");
        let long = tab_label("", None, "~/work/a-directory-name-that-will-not-fit", false);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_lone_tab_label_is_whole() {
        let cwd = "~/work/a-directory-name-that-will-not-fit";
        assert_eq!(tab_label("", None, cwd, true), cwd);
        let name = "n".repeat(40);
        assert_eq!(tab_label(&name, None, cwd, true), name);
    }

    #[test]
    fn abbreviate_path_keeps_last_component_and_dotted_prefix() {
        assert_eq!(abbreviate_path("~/.superset/projects/l3s"), "~/.s/p/l3s");
        assert_eq!(
            abbreviate_path("~/Developer/App/ClaudeCode"),
            "~/D/A/ClaudeCode"
        );
        assert_eq!(
            abbreviate_path("/Developer/App/ClaudeCode"),
            "/D/A/ClaudeCode"
        );
    }

    #[test]
    fn restored_frames_must_stay_reachable() {
        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let frame = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(800.), px(600.)));
        // Inside, edge-clipped, or mostly off-screen but still grabbable.
        assert!(frame_reachable(&frame(100., 100.), display));
        assert!(frame_reachable(&frame(-650., -450.), display));
        // Gone with the display it was recorded on.
        assert!(!frame_reachable(&frame(2400., 0.), display));
    }

    #[test]
    fn overlays_own_the_keyboard_over_content() {
        // Each content page owns the keyboard on its own...
        assert_eq!(top_layer(false, false, true, false, None), InputLayer::Home);
        assert_eq!(
            top_layer(false, false, false, true, None),
            InputLayer::Settings
        );
        // ...but an overlay opened above one takes over.
        assert_eq!(
            top_layer(false, true, true, false, None),
            InputLayer::Palette
        );
        assert_eq!(
            top_layer(false, true, false, true, None),
            InputLayer::Palette
        );
        // The folder picker is the topmost overlay.
        assert_eq!(top_layer(true, true, true, true, None), InputLayer::Picker);
        assert_eq!(
            top_layer(false, false, false, false, Some(InputLayer::Mission)),
            InputLayer::Mission
        );
        assert_eq!(
            top_layer(false, false, false, false, Some(InputLayer::Automations)),
            InputLayer::Automations
        );
        // With nothing open, the grid has it.
        assert_eq!(
            top_layer(false, false, false, false, None),
            InputLayer::Terminal
        );
    }

    #[test]
    fn tab_label_prefers_the_name_over_the_cwd() {
        assert_eq!(tab_label("api", None, "~/work/pilus", false), "api");
        assert_eq!(tab_label("api", None, "~/work/pilus", true), "api");
        assert_eq!(
            tab_label("api", Some("Fix the parser"), "~/work/pilus", false),
            "api"
        );
        let long = tab_label(&"n".repeat(40), None, "~/work/pilus", false);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn tab_label_takes_the_agent_session_title_before_the_cwd() {
        assert_eq!(
            tab_label("", Some(" Fix the parser "), "~/work/pilus", false),
            "Fix the parser"
        );
    }

    #[test]
    fn slide_starts_fly_each_tab_from_where_it_was_drawn() {
        let drawn = |id: &str, x: f32, offset: f32| (id.to_string(), x, 100.0, offset);
        let drawn = [
            drawn("a", 10.0, 0.0),
            drawn("b", 114.0, 0.0),
            drawn("c", 218.0, 0.0),
        ];
        let order = |ids: [&str; 3]| ids.map(String::from).to_vec();
        // `a` moves to the end: `b` and `c` start a slot to the right of
        // where they now sit, and `a` starts two slots to the left.
        let starts = slide_starts(&drawn, &order(["b", "c", "a"]));
        assert_eq!(starts["b"], 104.0);
        assert_eq!(starts["c"], 104.0);
        assert_eq!(starts["a"], -208.0);
        // A tab that stays in its slot has nothing to travel.
        assert!(!slide_starts(&drawn, &order(["a", "c", "b"])).contains_key("a"));
    }

    #[test]
    fn slide_starts_measure_from_the_slot_not_the_drawn_spot() {
        // Mid-slide, `a` is drawn 50px right of its slot; the slots themselves
        // are unmoved, so the next reorder starts from where it is drawn.
        let drawn = [
            ("a".to_string(), 60.0, 100.0, 50.0),
            ("b".to_string(), 114.0, 100.0, 0.0),
        ];
        let starts = slide_starts(&drawn, &["b".to_string(), "a".to_string()]);
        assert_eq!(starts["b"], 104.0);
        assert_eq!(starts["a"], 60.0 - 114.0);
    }

    #[test]
    fn a_slide_springs_home() {
        let slide = TabSlide {
            from: HashMap::from([("a".to_string(), 80.0)]),
            started: Instant::now(),
        };
        let start = slide.started;
        assert_eq!(slide.offset("a", start), 80.0);
        assert_eq!(slide.offset("a", start + TAB_SLIDE), 0.0);
        assert_eq!(slide.offset("other", start), 0.0);
        assert!(slide.running(start) && !slide.running(start + TAB_SLIDE));
    }

    #[test]
    fn moved_position_shifts_the_tabs_between() {
        // 2 -> 4: tabs 3 and 4 slide left into the gap.
        let after: Vec<u16> = (1..=5).map(|at| moved_position(at, 2, 4)).collect();
        assert_eq!(after, [1, 4, 2, 3, 5]);
        // 4 -> 2: tabs 2 and 3 slide right.
        let after: Vec<u16> = (1..=5).map(|at| moved_position(at, 4, 2)).collect();
        assert_eq!(after, [1, 3, 4, 2, 5]);
    }

    #[test]
    fn caret_over_an_app_drawn_reverse_cell_stays_visible() {
        let theme = Theme::default();
        // Apps like pi paint their own caret as a REVERSED cell. The caret
        // highlight must not invert it again, which would cancel the block.
        let block = (theme.bg, theme.fg, 0);
        assert_eq!(style_of(&cell(0), theme, true, false), block);
        assert_eq!(style_of(&cell(M_REVERSED), theme, true, false), block);
    }

    #[test]
    fn reversed_cell_without_the_caret_still_inverts() {
        let theme = Theme::default();
        assert_eq!(
            style_of(&cell(M_REVERSED), theme, false, false),
            (theme.bg, theme.fg, 0)
        );
    }

    #[test]
    fn the_daemons_base_surface_draws_as_the_chrome_only_when_remapped() {
        let theme = Theme::default();
        let mut base = cell(0);
        base.bg = (2 << 24) | 0x0011_1116; // `Theme::default().bg`, as the daemon sends it
        let chrome = theme.window_bg;
        let remapped = Theme {
            bg: chrome,
            remap_bg: Some(theme.bg),
            ..theme
        };
        assert_eq!(style_of(&base, remapped, false, false).1, chrome);
        assert_eq!(style_of(&base, theme, false, false).1, theme.bg);
        let mut other = cell(0);
        other.bg = (2 << 24) | 0x00ff_0000;
        assert_eq!(
            style_of(&other, remapped, false, false).1,
            gpui::rgb(0xff0000),
            "an app's own background is left alone"
        );
    }

    #[test]
    fn a_fade_toward_the_base_surface_stays_seamless_when_remapped() {
        // A light theme: the daemon's base is `mantle`, drawn as the lighter
        // `crust`. pi fades its logo halo toward that base, so a halo cell is
        // a step off it and must move with it rather than keep the old tone.
        let theme =
            Theme::from_palette(scheme::builtin("rose-pine-dawn").expect("vendored scheme"));
        let remapped = Theme {
            bg: theme.window_bg,
            remap_bg: Some(theme.bg),
            ..theme
        };
        let packed = |c: gpui::Rgba| {
            (2 << 24)
                | (((c.r * 255.0).round() as u32) << 16)
                | (((c.g * 255.0).round() as u32) << 8)
                | (c.b * 255.0).round() as u32
        };
        let mut halo = cell(0);
        halo.bg = packed(gpui::rgb(0x00f8_f1e8));
        let drawn = style_of(&halo, remapped, false, false).1;
        let raw = gpui::rgb(0x00f8_f1e8);
        let surface = theme.window_bg;
        // The cell keeps its offset from the base, so it lands near the new
        // surface, not on the old tone.
        assert!(
            (drawn.b - surface.b).abs() < (raw.b - surface.b).abs(),
            "halo drawn {drawn:?} should track the surface {surface:?}"
        );
        // A color well away from the base is an app's own and stays put.
        let mut far = cell(0);
        far.bg = packed(gpui::rgb(0x00d7_827e));
        assert_eq!(
            style_of(&far, remapped, false, false).1,
            gpui::rgb(0x00d7_827e)
        );
    }

    #[test]
    fn ansi_cells_resolve_through_the_active_theme() {
        // The same ANSI red cell must paint differently under two themes, so
        // terminal app output follows the appearance instead of fixed xterm.
        let mocha = Theme::from_palette(scheme::builtin("catppuccin-mocha").expect("vendored"));
        let latte = Theme::from_palette(scheme::builtin("catppuccin-latte").expect("vendored"));
        let mut red = cell(0);
        red.fg = 0x0100_0001; // ANSI 1
        let (mocha_fg, _, _) = style_of(&red, mocha, false, false);
        let (latte_fg, _, _) = style_of(&red, latte, false, false);
        assert_eq!(mocha_fg, mocha.ansi[1]);
        assert_eq!(latte_fg, latte.ansi[1]);
        assert_ne!(mocha_fg, latte_fg);
    }

    #[test]
    fn builtin_theme_supplies_the_pane_background() {
        // Catppuccin Mocha's pane/terminal background is its `mantle` role.
        let theme = Theme::from_palette(scheme::builtin("catppuccin-mocha").expect("vendored"));
        assert_eq!(theme.bg, gpui::rgb(0x181825));
        // Luvus aliases resolve to the same palette.
        assert_eq!(
            scheme::builtin("mocha").map(|palette| palette.mantle),
            Some(gpui::rgb(0x181825))
        );
        assert!(scheme::builtin("no-such-theme").is_none());
    }

    #[test]
    fn app_key_bindings_are_unique() {
        // `app_key_bindings` parses every chord as it is built; a duplicate
        // would shadow an earlier shortcut, so catch that here too. The same
        // chord under two contexts is deliberate: the deeper one wins.
        let mut seen = HashSet::new();
        for binding in app_key_bindings() {
            let chord = format!("{:?} in {:?}", binding.keystrokes(), binding.predicate());
            assert!(seen.insert(chord.clone()), "duplicate shortcut: {chord}");
        }
    }

    #[test]
    fn focus_shortcut_ignores_a_missing_tab() {
        let tabs: Vec<Tab> = (1..=3)
            .map(|index| Tab {
                index,
                ..Tab::default()
            })
            .collect();
        assert!(tab_exists(&tabs, 3));
        // `Cmd+4` with three tabs must not select anything.
        assert!(!tab_exists(&tabs, 4));
        assert!(!tab_exists(&[], 1));
    }

    #[test]
    fn fallback_glyphs_do_not_flow_with_neighbors() {
        assert!(is_flowable("a"));
        assert!(is_flowable(" "));
        // Nerd-Font branch glyph and emoji fall back past Menlo, so their
        // advances differ from the measured cell width.
        assert!(!is_flowable("\u{e0a0}"));
        assert!(!is_flowable("\u{1f4e6}"));
    }

    #[test]
    fn fallback_glyphs_overflow_only_into_a_blank_neighbor() {
        let cells: Vec<CellData> = ["\u{23bf}", " ", "\u{23bf}", "R", "a"]
            .iter()
            .map(|symbol| CellData {
                symbol: (*symbol).to_string(),
                ..CellData::blank(0, 0)
            })
            .collect();
        let rendered: Vec<(u16, &CellData, bool)> = cells
            .iter()
            .enumerate()
            .map(|(column, cell)| (column as u16, cell, false))
            .collect();
        assert!(overflows_into_blank(&rendered, "\u{23bf}", 1));
        assert!(!overflows_into_blank(&rendered, "\u{23bf}", 3));
        // Past the row's last cell there is nothing to borrow.
        assert!(!overflows_into_blank(&rendered, "\u{23bf}", 5));
        // Shaped runs already match the grid.
        assert!(!overflows_into_blank(&rendered, "a", 1));
    }

    #[test]
    fn logo_blocks_merge_into_seamless_bands() {
        // `▛███▜`: one top strip across all five cells; the bottom strips
        // break only at the two eyes.
        let color = gpui::rgb(0xd77757);
        let mut bands = BlockBands::default();
        for (column, symbol) in ["\u{259b}", "\u{2588}", "\u{2588}", "\u{2588}", "\u{259c}"]
            .iter()
            .enumerate()
        {
            let (rects, _) = block_element(symbol).expect("block");
            for rect in rects {
                bands.add(column as f32, rect, color);
            }
        }
        let spans: Vec<(f32, f32, f32, f32)> = bands
            .0
            .iter()
            .map(|band| (band.x0, band.x1, band.y0, band.y1))
            .collect();
        assert_eq!(
            spans,
            vec![
                (0.0, 5.0, 0.0, 0.5),
                (0.0, 0.5, 0.5, 1.0),
                (1.0, 4.0, 0.5, 1.0),
                (4.5, 5.0, 0.5, 1.0),
            ]
        );
    }

    #[test]
    fn block_elements_fill_their_cell_geometry() {
        assert_eq!(
            block_element("\u{2588}"),
            Some((vec![[0.0, 0.0, 1.0, 1.0]], 1.0))
        );
        assert_eq!(
            block_element("\u{2581}"),
            Some((vec![[0.0, 0.875, 1.0, 1.0]], 1.0))
        );
        assert_eq!(
            block_element("\u{258f}"),
            Some((vec![[0.0, 0.0, 0.125, 1.0]], 1.0))
        );
        assert_eq!(
            block_element("\u{2589}"),
            Some((vec![[0.0, 0.0, 0.875, 1.0]], 1.0))
        );
        // The Claude Code logo's eyes: `▛` leaves the lower-right quadrant open.
        assert_eq!(
            block_element("\u{259b}").map(|(rects, _)| rects.len()),
            Some(3)
        );
        assert_eq!(block_element("\u{2592}").map(|(_, shade)| shade), Some(0.5));
        assert_eq!(block_element("\u{2500}"), None);
        assert_eq!(block_element("a"), None);
        // Each block paints on its own rather than shaping with neighbors.
        assert!(!is_flowable("\u{2588}"));
        assert!(!is_flowable("\u{2500}"));
    }

    #[test]
    fn starship_prompt_isolates_fallback_glyphs() {
        let theme = Theme::default();
        let text = "a \u{e0a0} b \u{1f4e6} c";
        // The emoji occupies two daemon columns with a blank continuation.
        let width: u16 = 10;
        let mut cells = Vec::new();
        let mut columns: Vec<u16> = Vec::new();
        let mut column: u16 = 0;
        for grapheme in [
            "a",
            " ",
            "\u{e0a0}",
            " ",
            "b",
            " ",
            "\u{1f4e6}",
            "",
            " ",
            "c",
        ] {
            if grapheme.is_empty() {
                column += 1;
                continue;
            }
            cells.push(CellData {
                symbol: grapheme.to_string(),
                fg: 0,
                bg: 0,
                mods: 0,
            });
            columns.push(column);
            column += if pilus_protocol::symbol_width(grapheme) >= 2 {
                2
            } else {
                1
            };
        }
        assert_eq!(text, "a \u{e0a0} b \u{1f4e6} c");
        let rendered: Vec<(u16, &CellData, bool)> = columns
            .iter()
            .zip(cells.iter())
            .map(|(c, cell)| (*c, cell, false))
            .collect();
        let runs = collect_runs(width, &rendered, theme, 0, None, 1.0);
        let texts: Vec<&str> = runs.iter().map(|(_, _, text, _)| text.as_str()).collect();
        assert_eq!(texts, vec!["a ", "\u{e0a0}", " b ", "\u{1f4e6}", " c"]);
        // Every run stays pinned: widths cover the daemon columns exactly,
        // so later ASCII never shifts into the clipped tail.
        let mut covered = 0u16;
        for (start, cells, _, _) in &runs {
            assert_eq!(*start, covered);
            covered = covered.saturating_add(*cells);
        }
        assert_eq!(covered, width);
    }

    #[test]
    fn reconnect_delay_grows_and_stays_bounded() {
        let nominal = |failures: u32| {
            let exponent = failures.min(5);
            (RECONNECT_BASE.as_millis() * (1u128 << exponent)).min(RECONNECT_MAX.as_millis())
        };
        // Jitter makes an exact value unpredictable; check the 75-125% envelope
        // and that the ceiling holds for arbitrarily many failures.
        for failures in 0..10u32 {
            let delay = reconnect_delay(failures).as_millis();
            let base = nominal(failures);
            assert!(
                delay >= base * 75 / 100 && delay <= base * 125 / 100,
                "attempt {failures}: {delay} outside envelope of {base}"
            );
        }
        assert!(reconnect_delay(40).as_millis() <= RECONNECT_MAX.as_millis() * 125 / 100);
    }
}
