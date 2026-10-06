//! The GPUI desktop window.
//!
//! The window is a thin, native shell over one daemon connection: it forwards
//! input and resizes and paints the server's rendered cell grid. The daemon
//! still owns layout, but this client claims the workspaces dock so it can draw
//! that one panel natively, and selects text over the grid itself.

mod about;
mod app_icon;
mod appearances;
mod box_drawing;
mod connector;
mod glass;
mod home;
mod keys;
mod machines;
mod markdown;
mod palette;
mod pi_extension;
mod picker;
mod scheme;
mod selection;
mod session;
#[cfg(feature = "visual-test")]
pub(crate) mod shots;
mod sidebar_menu;
pub(crate) mod sound;
mod style;
mod task_compose;
mod tasks;
mod text_input;
mod theme;
mod ui_playground;
mod update_window;
mod updater;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::settings::{CHROME_FONT, Settings, WindowGeometry};
use crate::update::{UpdateChannel, UpdateRequest, describe_source};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton as TerminalMouseButton, MouseEvent,
    MouseEventKind,
};
use glass::Glass;
use gpui::{
    Action, Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, DisplayId, Div,
    ElementId, Entity, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding, KeyDownEvent, Menu,
    MenuItem, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString,
    Stateful, SystemNotification, TextRun, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, anchored, canvas, deferred, div, font, img, linear_color_stop,
    linear_gradient, point, prelude::*, px, relative, size, svg,
};
use gpui_platform::application;
use gpui_updater_core::{Release, UpdateStatus, Version};
use home::{HomeField, HomeFocus, HomePanel};
use palette::{Command, Entry, Palette};
use picker::{DirSource, MenuChoice, PickerEvent, WorkspacePicker};
use pilus_client::{
    Agent, AgentConfig, AgentFeed, Appearance, AutomationSummary, BarWidget, ClientEvent,
    ConnectTarget, Connection, ConnectorStatus, ControlClient, HOME_ENV, MachineInfo, PaneFrame,
    SessionInfo, SshTarget, Surface, SurfaceChange, Tab, TaskSummary, WorktreeInfo, agent_binary,
    common_git_dirs_remote, config_dir, connect_machine, expand_local, expand_remote,
    generate_session_name, install_agent, list_machines, list_session_infos, luvus_binary,
    pilus_config_dir, refresh_machines, split_client_socket,
};
use pilus_protocol::{
    CellData, ClientMessage, FrameData, ShellDockRect, ShellSidebars, ShellWorkspace,
    SurfaceInterest,
};
use selection::{Cell, Selection};
use session::SessionPanel;
use sidebar_menu::{Item as SidebarItem, MenuTarget as SidebarMenuTarget, SidebarMenu};
use style::FillExt;
use tasks::{TasksInputs, TasksPanel};
use text_input::{TextField, TextInput};
use theme::Theme;
use updater::Updater;

/// Line height as a fraction of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Height of our native tab strip; the grid starts below it.
const TAB_STRIP_HEIGHT: f32 = 36.0;
/// Metrics shared by composer picker chips and dropdowns (Home + session).
const PICKER_CHIP_H: f32 = 24.0;
/// Dropdown rows, gaps, and width: Home and session menus read as one.
const PICKER_MENU_W: f32 = 260.0;
const PICKER_MENU_MAX_H: f32 = 240.0;
const PICKER_ROW_H: f32 = 30.0;
/// A menu's own padding (`p_1`) and the gap between its rows (`gap_1`). The
/// menus know their exact height from these, so an anchored overlay can
/// offset by it and open without shifting the composer.
const PICKER_PAD: f32 = 4.0;
const PICKER_GAP: f32 = 4.0;
/// A picker's type-to-filter field, drawn once a list outgrows the rows
/// that fit.
const PICKER_FIELD_H: f32 = 28.0;
/// Grid offset from the window top: tab strip plus its top spacing.
const HEADER_HEIGHT: f32 = 40.0;
/// Daemon rows skipped at the top of the grid. The daemon draws its own tab
/// strip plus a cwd breadcrumb there; the grid starts below them so only
/// native chrome shows. Selection and click coordinates stay in daemon rows,
/// so copy and gear clicks keep working.
const SKIPPED_DAEMON_ROWS: u16 = 2;
/// Daemon rows skipped at the grid bottom: its one-row status line, which the
/// native shell omits. Assumes a single-row status line.
const SKIPPED_DAEMON_BOTTOM_ROWS: u16 = 1;
/// Gap around the floating terminal card: left, right, and bottom.
const CARD_GAP: f32 = 8.0;
/// Gap between the tab strip and the card. The card hugs the header tighter
/// than it does the other three sides, which stay at `CARD_GAP`.
const CARD_GAP_TOP: f32 = 2.0;
/// Slides the sidebar's first row down so it lands on the top edge of the
/// inset terminal card: the header spacer ends at `HEADER_HEIGHT` and the
/// card starts `CARD_GAP_TOP` below it.
const SIDEBAR_TOP_INSET: f32 = CARD_GAP_TOP;
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
/// Box every leading sidebar glyph sits in, so labels share one column
/// whatever the glyph is: chevron, folder, brand mark, or status dot.
const SIDEBAR_GLYPH: f32 = 14.0;
/// Gap between a row's leading glyph and its label.
const SIDEBAR_GLYPH_GAP: f32 = 8.0;
/// Height of a single-line sidebar row (nav, new workspace, section header).
const SIDEBAR_ROW_HEIGHT: f32 = 28.0;
/// Height of a one-line band that carries no control: a section header, an
/// endpoint group label, or an empty-state placeholder.
const SIDEBAR_BAND_HEIGHT: f32 = 24.0;
/// How long after a sidebar section header is clicked its chevron may still
/// turn: past the turn itself, so a late frame still catches it.
const SECTION_TURN_WINDOW: Duration = Duration::from_millis(400);
/// Space above a section header, on top of the list's row gap.
const SIDEBAR_SECTION_GAP: f32 = 12.0;
/// Breathing room between the terminal grid and the card's inset border.
/// The card snaps to whole cells, so all four gaps stay exactly this.
const TERMINAL_PAD: f32 = 6.0;
/// How long the sidebar takes to collapse or expand: long enough for its
/// spring to overshoot and settle, short enough to stay out of the way.
const SIDEBAR_ANIM_DURATION: Duration = Duration::from_millis(460);
/// How long terminal text takes to fade back in after the grid reflows to a
/// new size, so a resize reads as a settle rather than a jump.
const REFLOW_FADE: Duration = Duration::from_millis(180);
/// The opacity text fades in from after a resize reflows it: a light dip,
/// enough to soften the jump without reading as a flicker.
const REFLOW_FLOOR: f32 = 0.6;
/// The opacity text fades in from when the terminal's content is swapped
/// (another tab, another machine, or the terminal coming back from Home,
/// Settings, or the session panel): deep enough to read as a crossfade.
const SWAP_FLOOR: f32 = 0.15;
/// Size changes closer together than this belong to one resize (a window
/// drag, a sidebar tween), which fades once instead of strobing per step.
const REFLOW_BURST: Duration = Duration::from_millis(250);
/// Longest the terminal holds its pre-toggle snapshot waiting for a frame
/// drawn with the new dock: a slow link must not leave it stale.
const SIDEBAR_FREEZE_MAX: Duration = Duration::from_secs(2);

/// What the terminal card is showing, compared frame to frame: a change means
/// its content was swapped rather than updated.
#[derive(Clone, PartialEq, Eq)]
struct SurfaceKey {
    endpoint: String,
    tab: Option<u16>,
    shown: bool,
}

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
/// Right margin on the card. Matches the other sides so the terminal sits
/// centered between the sidebar and the window edge.
const CARD_GAP_RIGHT: f32 = CARD_GAP;
/// Width of the sidebar collapse/expand button.
const SIDEBAR_TOGGLE_W: f32 = 28.0;
/// Height of a tab pill in the top chrome.
const TAB_HEIGHT: f32 = 26.0;
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
/// Section label size (SESSIONS / WORKSPACES).
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

/// Native sidebar destinations: live daemon data and bound views.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavKind {
    Automations,
    Tasks,
    Mission,
    Orchestration,
}

impl NavKind {
    /// All rows in sidebar order.
    const ALL: [NavKind; 4] = [
        NavKind::Automations,
        NavKind::Tasks,
        NavKind::Mission,
        NavKind::Orchestration,
    ];

    fn label(self) -> &'static str {
        match self {
            NavKind::Automations => "Automations",
            NavKind::Tasks => "Tasks",
            NavKind::Mission => "Mission Control",
            NavKind::Orchestration => "Orchestration",
        }
    }

    /// Vendored lucide glyph; tinted by the caller's text color.
    fn icon(self) -> &'static [u8] {
        match self {
            NavKind::Automations => include_bytes!("../../assets/icons/clock.svg"),
            NavKind::Tasks => include_bytes!("../../assets/icons/list-checks.svg"),
            NavKind::Mission => include_bytes!("../../assets/icons/layout-panel-left.svg"),
            NavKind::Orchestration => include_bytes!("../../assets/icons/workflow.svg"),
        }
    }

    /// The `<bind>` chord that opens this destination in the daemon.
    ///
    /// Automations surface in Mission Control and tasks on the task board,
    /// so the data rows deep-link to the view that shows them.
    fn chord(self) -> char {
        match self {
            NavKind::Automations | NavKind::Mission => 'm',
            NavKind::Tasks | NavKind::Orchestration => 'o',
        }
    }
}

// Ratatui `Modifier` bits, as packed by the daemon.
const M_BOLD: u16 = 1 << 0;
const M_ITALIC: u16 = 1 << 2;
const M_UNDERLINED: u16 = 1 << 3;
const M_REVERSED: u16 = 1 << 6;

gpui::actions!([
    /// Open the native Appearances window.
    OpenAppearances,
    /// Open the native remote-machine manager window.
    OpenMachineManager,
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
    /// Check for updates (not wired up yet).
    CheckForUpdates,
    /// Grow the terminal font size.
    IncreaseFontSize,
    /// Shrink the terminal font size.
    DecreaseFontSize,
    /// Restore the default terminal font size.
    ResetFontSize,
    /// Show or hide the sidebar.
    ToggleSidebar,
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
pub fn run(target: ConnectTarget) -> anyhow::Result<()> {
    run_inner(target, None)
}

/// Launch the main window with the Pi Session panel already open for `spec`.
///
/// The CLI entry for `--session-ui`; the session is read inside the main
/// window, never a separate one.
pub fn run_session(spec: String) -> anyhow::Result<()> {
    run_inner(ConnectTarget::local(), Some(spec))
}

/// Open the desktop window, optionally opening the Pi Session panel at launch.
fn run_inner(target: ConnectTarget, initial_session: Option<String>) -> anyhow::Result<()> {
    // The window opens before the daemon is attached, so a stopped or
    // protocol-incompatible daemon cannot stop `pilus` from launching. The
    // view's local reconnect loop owns the first attach and every retry, and
    // the empty grid area names the failure. The real viewport is not known
    // until a window exists; the first resize after the window opens corrects
    // the provisional dimensions.
    let appearances_socket = api_socket_for_target(&target);
    let config_root = config_dir();
    let pilus_root = pilus_config_dir();
    migrate_legacy_stores(&config_root, &pilus_root);
    let new_window_target = target.clone();
    let settings = Rc::new(RefCell::new(Settings::load(&pilus_root)));
    let machine_signal = Rc::new(RefCell::new(MachineSignal::default()));
    let shared_theme = Rc::new(RefCell::new(Theme::default()));

    application().run(move |cx: &mut App| {
        app_icon::install();
        // Name the process for notification and taskbar identity. Read the
        // running bundle so a packaged app reports its `Info.plist` value;
        // this is a no-op on macOS, where the bundle already carries it.
        cx.set_app_identity(&app_icon::bundle_identifier(), "pilus");
        // Clicking a notification brings the window forward instead of only
        // activating the process.
        cx.on_system_notification_response(|_, cx| {
            cx.activate(true);
            if let Some(window) = cx.windows().into_iter().next() {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            }
        });
        // A persisted "this node" choice starts the connector with the app, so
        // it survives closing the Remote Machines window. A failed start clears
        // the flag rather than failing on every launch.
        if settings.borrow().connector {
            match connector::start(&connector::default_options()) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("pilus: remote access did not start: {error}");
                    settings.borrow_mut().connector = false;
                    settings.borrow().save(&pilus_root);
                }
            }
        }
        cx.on_action({
            let api = appearances_socket.clone();
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &OpenAppearances, cx: &mut App| {
                appearances::open(api.clone(), settings.clone(), root.clone(), cx)
            }
        });
        cx.on_action({
            let root = config_root.clone();
            let signal = machine_signal.clone();
            let theme = shared_theme.clone();
            let settings = settings.clone();
            move |_: &OpenMachineManager, cx: &mut App| {
                machines::open(
                    root.clone(),
                    signal.clone(),
                    theme.clone(),
                    settings.clone(),
                    cx,
                );
            }
        });
        cx.on_action(|_: &QuitApp, cx: &mut App| {
            connector::stop();
            cx.quit();
        });
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
            let target = new_window_target.clone();
            let settings = settings.clone();
            let signal = machine_signal.clone();
            let theme = shared_theme.clone();
            move |_: &NewWindow, cx: &mut App| {
                open_main_window(
                    target.clone(),
                    settings.clone(),
                    signal.clone(),
                    theme.clone(),
                    None,
                    cx,
                )
            }
        });
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
            move |_: &IncreaseFontSize, cx: &mut App| bump_font(&settings, &root, 1.0, cx)
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &DecreaseFontSize, cx: &mut App| bump_font(&settings, &root, -1.0, cx)
        });
        cx.on_action({
            let settings = settings.clone();
            let root = pilus_root.clone();
            move |_: &ResetFontSize, cx: &mut App| bump_font(&settings, &root, 0.0, cx)
        });
        cx.bind_keys(app_key_bindings());
        cx.set_menus([
            Menu::new("pilus").items([
                MenuItem::action("About pilus", OpenAbout),
                MenuItem::action("Check for Updates", CheckForUpdates),
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
            ]),
            Menu::new("View").items([
                MenuItem::action("Command Palette", OpenCommandPalette),
                MenuItem::separator(),
                MenuItem::action("Increase Font Size", IncreaseFontSize),
                MenuItem::action("Decrease Font Size", DecreaseFontSize),
                MenuItem::action("Reset Font Size", ResetFontSize),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
            ]),
            Menu::new("Window").items([
                MenuItem::action("Minimize", MinimizeWindow),
                MenuItem::action("Zoom", ZoomWindow),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ]),
        ]);
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
                ..Default::default()
            },
            move |window, cx| {
                let signal = machine_signal.clone();
                let theme = shared_theme.clone();
                let initial_session = initial_session.clone();
                cx.new(move |cx| {
                    PilusView::new(
                        target,
                        None,
                        settings.clone(),
                        signal,
                        theme,
                        initial_session,
                        window,
                        cx,
                    )
                })
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
    initial_session: Option<String>,
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
            ..Default::default()
        },
        move |window, cx| {
            let signal = machine_signal.clone();
            let theme = shared_theme.clone();
            cx.new(move |cx| {
                PilusView::new(
                    target,
                    None,
                    settings,
                    signal,
                    theme,
                    initial_session,
                    window,
                    cx,
                )
            })
        },
    );
    if let Err(error) = opened {
        eprintln!("pilus: could not open another window: {error}");
    }
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

/// A process-unique tag so a native notification is not replaced by the next
/// one the daemon pushes.
fn notification_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("pilus-notify-{}", NEXT.fetch_add(1, Ordering::Relaxed))
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

/// Geometry of the inset terminal card: grid origin, visible cells, and the
/// measurements behind them, so painting, pointer mapping, and the daemon
/// viewport stay pixel-true to one another.
struct CardLayout {
    /// Card's left edge in window pixels (sidebar plus gap, or just gap).
    card_x: f32,
    /// Card's top, right, and bottom offsets: the inset gaps when framed,
    /// zero when the terminal runs full-bleed.
    card_top: f32,
    card_right: f32,
    card_bottom: f32,
    /// Inset of the first cell from the border, after centering the
    /// whole-cell grid in the card's inner box.
    pad_x: f32,
    pad_y: f32,
    /// First visible cell's top-left corner in window pixels.
    origin_x: f32,
    origin_y: f32,
    /// Visible grid size in cells.
    cols: u16,
    rows: u16,
    /// Measured cell size in pixels.
    cell_w: f32,
    cell_h: f32,
    /// Sub-cell remainder pooled at the right and bottom edges in full-bleed
    /// (zero when framed, where centering absorbs it). The grid paints its
    /// edge colors over this strip so no card-colored sliver shows.
    slack_x: f32,
    slack_y: f32,
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
    /// The Home composer page.
    Home,
    /// The Settings page (machines, updates).
    Settings,
    /// The native Tasks page.
    Tasks,
    /// The native Pi Session page.
    Session,
    /// The live terminal grid.
    Terminal,
}

/// Resolve the keyboard owner from which surfaces are open.
///
/// Overlays sit above content, so the picker beats the palette and both beat
/// the Home composer, Settings, and the session page.
fn top_layer(
    picker: bool,
    palette: bool,
    home: bool,
    settings: bool,
    tasks: bool,
    session: bool,
) -> InputLayer {
    if picker {
        InputLayer::Picker
    } else if palette {
        InputLayer::Palette
    } else if home {
        InputLayer::Home
    } else if settings {
        InputLayer::Settings
    } else if tasks {
        InputLayer::Tasks
    } else if session {
        InputLayer::Session
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
}

/// The focused pi pane the session panel currently stands in for, when
/// `pi_gui_auto` replaced its terminal with the native conversation.
/// `session` is the daemon's latest report; `None` means pi has not
/// reported yet and the panel waits on the extension socket instead of
/// flashing the terminal first.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PiView {
    endpoint: EndpointId,
    pane: String,
    session: Option<String>,
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
    /// Create a linked worktree of the right-clicked workspace's repo.
    NewWorktree { endpoint: EndpointId, index: u16 },
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

/// One background context-menu operation's outcome, folded into the status
/// line so a slow SSH round trip never blocks the window.
struct MenuOpResult {
    /// Status-line text when the operation finished.
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

/// The little modal a context-menu action opens: rename, alias, or branch.
struct MenuPrompt {
    /// Modal heading, e.g. `Rename workspace`.
    title: &'static str,
    placeholder: &'static str,
    action: PromptAction,
}

/// How long a pilus-initiated pi pin waits for `agent.list` to confirm
/// before falling back to the snapshot. Shorter than the optimistic picks'
/// four seconds: a wrong panel covers more than a wrong row highlight,
/// and a confirming snapshot lands well inside this window.
const PI_OPTIMISTIC_WINDOW: Duration = Duration::from_secs(2);
/// Panels kept alive after their pi pane leaves the screen, newest first.
/// Each holds a reader and a socket, so the list stays short.
const PI_PARKED_MAX: usize = 3;
/// How long a parked panel keeps following its session before it is shut
/// down; a return after that reads the session afresh.
const PI_PARK_TTL: Duration = Duration::from_secs(300);
/// How often the on-screen pi pane's place in its tab is re-read: a split,
/// a resize, or a zoom moves it without any event the window hears.
const PI_FRAME_POLL_LOCAL: Duration = Duration::from_millis(500);
/// The same over SSH, where every read is a proxied round trip.
const PI_FRAME_POLL_REMOTE: Duration = Duration::from_secs(2);
/// How long a click beside a split pi panel counts as moving focus off it,
/// ahead of the `agent.list` that confirms the daemon followed the click.
const PI_BLUR_WINDOW: Duration = Duration::from_secs(4);

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
    /// Collapse/expand tween in flight, if any; the panel, tab strip, and
    /// terminal read its openness so they glide together.
    sidebar_anim: Option<SidebarAnim>,
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
    /// The painted frame's size and when it last changed, so a burst of
    /// resizes fades the text once.
    grid_size: (u16, u16),
    grid_resized_at: Option<Instant>,
    /// What the terminal is showing — endpoint, tab, and whether it is on
    /// screen at all — so a swap fades the new content in.
    surface_key: Option<SurfaceKey>,
    /// The current text fade: when it began and the opacity it rises from;
    /// `None` once the text has settled.
    reflow_fade: Option<(Instant, f32)>,
    /// Whether the remembered frame changed since the last settings write.
    window_dirty: bool,
    /// When the frame last changed, so a drag settles before the write.
    window_moved_at: Instant,
    cols: u16,
    rows: u16,
    status: String,
    /// Whether this window is frontmost, tracked each render so a daemon push
    /// can skip a banner the user is already looking at.
    window_active: bool,
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
    /// Whether `Ctrl` is held, tracked from modifier changes. macOS turns a
    /// `Ctrl`+left-click into a right-click before the window sees it, so the
    /// flag is what lets a secondary click still reach the daemon as the
    /// `Ctrl`+left-click that opens a link.
    control_held: bool,
    /// Background poller for the current session's tabs and widgets.
    agent_feed: Option<AgentFeed>,
    /// State root the SESSIONS section lists, derived from the launch target.
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
    machine_rx: Receiver<Vec<MachineInfo>>,
    /// Sender half for the background machine refresh.
    machine_tx: Sender<Vec<MachineInfo>>,
    /// Cheap catalog reloads (no probe) from the background `list_machines`.
    machine_list_rx: Receiver<Vec<MachineInfo>>,
    /// Sender half for the cheap catalog reload.
    machine_list_tx: Sender<Vec<MachineInfo>>,
    /// Status lines from background machine mutations (enable/disable/remove).
    machine_op_rx: Receiver<String>,
    /// Sender half for background machine mutation outcomes.
    machine_op_tx: Sender<String>,
    /// The machine id whose CLI mutation is running, if any.
    machine_busy: Option<String>,
    /// The host connector's latest observed state, polled each tick.
    connector_status: ConnectorStatus,
    /// A machine remove awaiting its second click, if any.
    machine_pending_remove: Option<String>,
    /// Registered appearance themes and the selected id, from `theme.list`.
    appearance: Appearance,
    /// Last theme poll; the registry only changes on user action.
    last_theme_poll: Instant,
    /// A theme refresh is running off-thread; no overlapping spawns.
    theme_refreshing: bool,
    /// Fresh appearance snapshots from the background refresh.
    theme_rx: Receiver<pilus_client::Result<Appearance>>,
    /// Sender half for the background theme refresh.
    theme_tx: Sender<pilus_client::Result<Appearance>>,
    /// Optimistic nav pick: the row a click chose, before the daemon confirms.
    nav_sel: Option<NavKind>,
    /// When the optimistic pick was made, so stale picks age out.
    nav_sel_at: Instant,
    /// Optimistic tab pick from a click, until `tab.list` confirms it.
    tab_sel: Option<u16>,
    /// When the optimistic tab pick was made, so stale picks age out.
    tab_sel_at: Instant,
    /// Keeps the tab list scrolled when tabs overflow the strip.
    tab_scroll: ScrollHandle,
    /// The rows the sidebar's shared hover pill can land on.
    sidebar_hover: style::HoverSlot,
    /// The row the sidebar's selection pill is gliding to.
    sidebar_selection: style::GlideSlot,
    /// The tab index the strip last scrolled to, so it only follows changes.
    tab_scrolled: std::cell::Cell<Option<u16>>,
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
    /// Workspace ids pinned to the top of WORKSPACES, per endpoint. The
    /// daemon's own dock tracks these client-side too, so the label and the
    /// action pair without a protocol field.
    pinned_workspaces: HashSet<(String, String)>,
    /// Live agent panes pinned to the top of AGENTS, per endpoint.
    pinned_agents: HashSet<(String, String)>,
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
    /// Whether the WORKSPACES section is collapsed to its header.
    workspaces_collapsed: bool,
    /// Whether the AGENTS section is collapsed to its header.
    agents_collapsed: bool,
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
    /// The native Pi Session panel, read live from a session file.
    session: SessionPanel,
    /// The pi pane the session panel is pinned to, if auto-switch put one
    /// up. `None` means the card shows the terminal again.
    pi_view: Option<PiView>,
    /// A pi pane whose panel the user closed or browsed away from, keyed by
    /// endpoint, pane, and the session that was showing: the terminal stays
    /// up until the pane reports a session (for a waiting panel) or loses
    /// focus, so a closed panel is not reopened every tick.
    pi_released: Option<(EndpointId, String, Option<String>)>,
    /// A pi pin awaiting `agent.list` confirmation, set when pilus itself
    /// moved focus to a pane the snapshot already knew was pi — a sidebar
    /// row or a tab holding one. The snapshot's own record stands in so the
    /// panel opens with the click instead of a reconcile behind it.
    pi_optimistic: Option<(PiView, Agent, Instant)>,
    /// Panels whose pane left the screen, still following their sessions,
    /// with when they were parked: returning to the pane swaps its panel
    /// back in as it stands instead of reloading the conversation.
    pi_parked: Vec<(PiView, SessionPanel, Instant)>,
    /// The pi pane on screen (focused, or sharing the active tab through a
    /// split), whose frame the window keeps reading.
    pi_candidate: Option<(EndpointId, String)>,
    /// Where `pi_candidate` sits in its tab, as last read.
    pi_frame: Option<(EndpointId, String, PaneFrame)>,
    pi_frame_rx: Receiver<(EndpointId, String, Option<PaneFrame>)>,
    pi_frame_tx: Sender<(EndpointId, String, Option<PaneFrame>)>,
    /// A frame read is in flight; one at a time.
    pi_frame_pending: bool,
    /// When the last frame read went out.
    pi_frame_at: Option<Instant>,
    /// Whether the pi view's pane holds the daemon's focus. In a split with
    /// another pane focused, the panel shows but the keyboard stays with
    /// the terminal.
    pi_focused: bool,
    /// A click on the grid beside a split panel, moving focus to the other
    /// pane before `agent.list` says so.
    pi_blurred_at: Option<Instant>,
    /// Agents pilus launched moments ago, before `agent.list` has listed
    /// them; `focus_agent` checks here so a launched pi pins immediately.
    /// Pruned on age — the snapshot takes over within a few reconciles.
    recent_launches: Vec<(EndpointId, Agent, Instant)>,
    /// The last focused agent per endpoint and workspace. Switching to a
    /// workspace restores its remembered pane, so the memory predicts
    /// which agent holds focus a reconcile before `agent.list` says so.
    workspace_focus: HashMap<(EndpointId, String), Agent>,
    /// The same memory per tab: a tab restores the pane it last focused,
    /// so `(endpoint, workspace, tab)` predicts a `focus_tab` outcome.
    tab_focus: HashMap<(EndpointId, String, u16), Agent>,
    /// Machine ids whose pi extension install already ran this window, so
    /// the SSH write happens once per machine per launch.
    pi_ext_machines: HashSet<String>,
    /// The composer's prompt, an IME-capable field over `home`'s buffer.
    home_prompt: Entity<TextInput>,
    /// The Tasks page's own state; the tasks come from `tasks` (the feed).
    tasks_panel: TasksPanel,
    /// The Tasks composer's text fields.
    tasks_inputs: TasksInputs,
    /// Daemon answers to Tasks page mutations, run off the GUI thread.
    tasks_op_rx: Receiver<task_compose::TasksMsg>,
    tasks_op_tx: Sender<task_compose::TasksMsg>,
    /// The Pi Session composer's field.
    session_prompt: Entity<TextInput>,
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
    /// A workspace opened from the composer's picker, awaiting its catalog row
    /// so the Workspace picker can select it.
    home_pending_open: Option<String>,
    /// Whether the Settings page covers the terminal card.
    settings_open: bool,
    /// The Settings page's one-line status, above the page footer.
    settings_status: String,
}

impl PilusView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        target: ConnectTarget,
        connection: Option<Connection>,
        shared_settings: Rc<RefCell<Settings>>,
        machine_signal: Rc<RefCell<MachineSignal>>,
        shared_theme: Rc<RefCell<Theme>>,
        initial_session: Option<String>,
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
            connector::stop();
            view.settings.save(&view.pilus_root);
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
        let (tasks_op_tx, tasks_op_rx) = std::sync::mpsc::channel();
        let (home_resolve_tx, home_resolve_rx) = std::sync::mpsc::channel();
        let (agent_config_tx, agent_config_rx) = std::sync::mpsc::channel();
        let (pi_frame_tx, pi_frame_rx) = std::sync::mpsc::channel();
        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let (machine_connect_tx, machine_connect_rx) = std::sync::mpsc::channel();
        let (picker_tx, picker_rx) = std::sync::mpsc::channel();
        let (menu_tx, menu_rx) = std::sync::mpsc::channel();
        let (theme_tx, theme_rx) = std::sync::mpsc::channel();
        let (daemon_tx, daemon_rx) = std::sync::mpsc::channel();
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
        let session = SessionPanel::default();
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
        let tasks_inputs = TasksInputs::new(
            style::chrome_font(&shared_settings),
            initial_fonts.body,
            initial_theme,
            cx,
        );
        let session_prompt = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Message pi…",
                style::chrome_font(&shared_settings),
                px(initial_fonts.body),
                true,
                initial_theme,
                cx,
            )
            .submit_on_enter()
        });
        let session_filter = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Filter models",
                style::chrome_font(&shared_settings),
                px(initial_fonts.label),
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
            sidebar_anim: None,
            last_sidebar_dock: None,
            sidebar_freeze: None,
            sidebar_open: 1.0,
            section_toggled: None,
            grid_size: (0, 0),
            grid_resized_at: None,
            surface_key: None,
            reflow_fade: None,
            window_dirty: false,
            window_moved_at: Instant::now(),
            cols: 120,
            rows: 40,
            status: "connecting…".to_string(),
            window_active: false,
            cell_width: None,
            sent_cell: (0, 0),
            selection: None,
            dragging: false,
            drag_moved: false,
            press_anchor: None,
            hover_cell: None,
            control_held: false,
            agent_feed,
            session_root,
            pilus_root,
            session_name,
            sessions,
            last_session_poll: Instant::now(),
            session_menu_open: false,
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
            connector_status: ConnectorStatus::default(),
            machine_pending_remove: None,
            appearance: Appearance::default(),
            last_theme_poll: Instant::now() - Duration::from_secs(11),
            theme_refreshing: false,
            theme_rx,
            theme_tx,
            nav_sel: None,
            nav_sel_at: Instant::now(),
            tab_sel: None,
            tab_sel_at: Instant::now(),
            tab_scroll: ScrollHandle::new(),
            sidebar_hover: style::HoverSlot::default(),
            sidebar_selection: style::GlideSlot::default(),
            tab_scrolled: std::cell::Cell::new(None),
            endpoint_sel: None,
            agent_sel: None,
            agent_sel_at: Instant::now(),
            sidebar_layout: None,
            sidebar_workspace_paths,
            sidebar_agent_paths,
            pinned_workspaces: HashSet::new(),
            pinned_agents: HashSet::new(),
            agents_this_workspace: false,
            context_menu: None,
            menu_prompt: None,
            menu_prompt_field,
            menu_prompt_input,
            worktree_linked: HashMap::new(),
            worktree_probing: HashSet::new(),
            sidebar_revision: 0,
            sidebar_visible: true,
            workspaces_collapsed: false,
            agents_collapsed: false,
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
            active_live,
            // Force the first probe, which also seeds the Local row; the cheap
            // reload only supplies the saved profiles.
            catalog_dirty: true,
            updater: updater.clone(),
            last_update_status: UpdateStatus::Idle,
            update_feed,
            update_feed_input,
            update_url_editing: false,
            daemon_rx,
            daemon_tx,
            daemon_busy: false,
            daemon_status: String::new(),
            daemon_installed: None,
            daemon_latest: None,
            daemon_restart_pending: false,
            machine_signal,
            seen_machine_revision: 0,
            home_resolve_rx,
            home_resolve_tx,
            home_resolve_pending: false,
            remote_home: HashMap::new(),
            home,
            session,
            pi_view: None,
            pi_released: None,
            pi_optimistic: None,
            pi_parked: Vec::new(),
            pi_candidate: None,
            pi_frame: None,
            pi_frame_rx,
            pi_frame_tx,
            pi_frame_pending: false,
            pi_frame_at: None,
            pi_focused: false,
            pi_blurred_at: None,
            recent_launches: Vec::new(),
            workspace_focus: HashMap::new(),
            tab_focus: HashMap::new(),
            pi_ext_machines: HashSet::new(),
            home_prompt,
            tasks_panel: TasksPanel::default(),
            tasks_inputs,
            tasks_op_rx,
            tasks_op_tx,
            session_prompt,
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
        };
        // The session panel filters its model picker through this field; it
        // owns the entity the way the view owns the composer prompt, so a
        // query never lands in the draft the user is about to send.
        view.session.set_picker_field(session_filter);
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
        // Drop the pi session-report extension into this machine's agent
        // dir; remote machines get it when they become the active endpoint.
        std::thread::spawn(|| {
            let _ = pi_extension::install_local();
        });

        cx.spawn(async move |this, cx| {
            loop {
                let updated = this.update(cx, |view, cx| {
                    view.persist_window_bounds();
                    let changed = view.sync_settings(cx);
                    // The sidebar tween rides this loop: it ticks and
                    // repaints here, so no second driver can stall unseen.
                    // A settling tween repaints once more so a trailing
                    // resize still lands when the daemon is otherwise idle:
                    // `was_animating` covers the tick that returns settled.
                    let was_animating = view.sidebar_anim.is_some();
                    let settled = view.tick_sidebar_anim();
                    let released = view.release_sidebar_freeze(cx.reduce_motion());
                    // The reflow fade rides the same loop for the same reason.
                    let reflowing = view.reflow_fade.is_some();
                    // The transcript glide behind the jump buttons.
                    let gliding = view.session.tick_scroll();
                    if view.poll(cx)
                        || gliding
                        || changed
                        || !settled
                        || was_animating
                        || reflowing
                        || released
                    {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
            }
        })
        .detach();

        // `--session-ui` opens the panel immediately; a bare `pilus` waits
        // for the palette's Open Pi Session.
        if let Some(spec) = initial_session {
            let api_socket = view.api_socket();
            view.session.open_local(api_socket, spec, None, cx);
        }

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

    /// Tell the active bridge how much surface we want, when one exists.
    fn set_active_interest(&self, interest: SurfaceInterest) {
        if let Some(connection) = self.connection.as_ref() {
            let _ = connection.set_interest(interest);
        }
    }

    /// Drain pending connection events. Returns whether a repaint is needed.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        // The session panel owns a background reader, so this only ever
        // clones an already-folded transcript.
        let session_prompt = self.session_prompt.clone();
        changed |= self.session.poll(&session_prompt, cx);
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
        let machine_revision = self.machine_signal.borrow().revision;
        if machine_revision != self.seen_machine_revision {
            self.seen_machine_revision = machine_revision;
            self.catalog_dirty = true;
            changed = true;
        }
        let connector_status = connector::status();
        if connector_status != self.connector_status {
            self.connector_status = connector_status;
            changed = true;
        }
        changed |= self.poll_active(cx);
        changed |= self.poll_endpoints();
        changed |= self.poll_machine_connects();
        self.pump_reconnects();
        self.drive_pending_switch();
        self.expire_pending_switch();
        let mut tabs_changed = false;
        if let Some(feed) = self.agent_feed.as_ref() {
            while let Some(update) = feed.try_recv() {
                self.bar_widgets = update.bar;
                self.tabs = update.tabs;
                self.agents = update.agents;
                self.agents_loaded = true;
                self.automations = update.automations;
                self.tasks = update.tasks;
                tabs_changed = true;
                changed = true;
            }
        }
        if tabs_changed {
            self.reconcile_nav();
        }
        changed |= self.poll_pi_frame(tabs_changed);
        changed |= self.sync_pi_view(cx);
        self.poll_parked_sessions(cx);
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
                    self.sessions = list_session_infos(&self.session_root);
                    self.switch_session(&name);
                }
                SessionOpResult::StartFailed(name, error) => {
                    self.status = format!("couldn't start {name}: {error}");
                }
            }
            changed = true;
        }
        // Sessions change rarely; re-read the state root on a slow tick so a
        // created or stopped session appears without rescanning every frame.
        if self.last_session_poll.elapsed() >= Duration::from_secs(2) {
            self.last_session_poll = Instant::now();
            let fresh = list_session_infos(&self.session_root);
            if fresh != self.sessions {
                self.sessions = fresh;
                changed = true;
            }
        }
        let needs_probe = self.machines.iter().any(|machine| {
            machine.id != "local"
                && machine.enabled
                && !self.reconnects.contains_key(&machine.id)
                && !self.machine_connected(&machine.id)
        });
        if (self.catalog_dirty
            || (needs_probe && self.last_machine_poll.elapsed() >= Duration::from_secs(10))
            // Reachability drifts, and a connected or disabled machine never
            // sets `needs_probe`, so a slow fallback keeps every status
            // honest instead of stuck on `probing…`.
            || self.last_machine_poll.elapsed() >= Duration::from_secs(30))
            && !self.machine_refreshing
        {
            self.catalog_dirty = false;
            self.probe_machines();
        }
        while let Ok(machines) = self.machine_rx.try_recv() {
            self.machine_refreshing = false;
            changed |= self.apply_machine_catalog(machines);
        }
        while let Ok(mut machines) = self.machine_list_rx.try_recv() {
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
        while let Ok(message) = self.machine_op_rx.try_recv() {
            self.machine_busy = None;
            // Tell every open main window and the manager to reload.
            self.machine_signal.borrow_mut().revision += 1;
            if self.settings_open {
                self.settings_status = message;
            } else {
                self.status = message;
            }
            changed = true;
        }
        if self.tasks_panel.is_open() {
            self.tasks_tick();
        }
        while let Ok(message) = self.tasks_op_rx.try_recv() {
            self.tasks_receive(message);
            changed = true;
        }
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
        // poll keeps every pilus window on the same appearance.
        if !self.theme_refreshing && self.last_theme_poll.elapsed() >= Duration::from_secs(2) {
            self.theme_refreshing = true;
            self.last_theme_poll = Instant::now();
            let api = api_socket_for(&self.session_root, self.session_name.as_deref());
            let tx = self.theme_tx.clone();
            std::thread::spawn(move || {
                let result =
                    ControlClient::connect(&api).and_then(|mut client| client.appearance());
                tx.send(result).ok();
            });
        }
        while let Ok(result) = self.theme_rx.try_recv() {
            self.theme_refreshing = false;
            if let Ok(appearance) = result
                && appearance != self.appearance
            {
                self.appearance = appearance;
                self.apply_appearance();
                changed = true;
            }
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
                        Ok(()) => self.status = "opened the workspace".to_string(),
                        Err(error) => self.status = format!("couldn't open workspace: {error}"),
                    }
                    changed = true;
                }
                PickerEvent::WorktreeOpened(result) => {
                    match result {
                        Ok(()) => self.status = "created a new worktree".to_string(),
                        Err(error) => self.status = format!("couldn't create worktree: {error}"),
                    }
                    changed = true;
                }
                PickerEvent::LocalTerminalOpened(result) => {
                    match result {
                        Ok(()) => {
                            self.status = "opened a local terminal".to_string();
                            self.activate_opened_endpoint(None, None, cx);
                        }
                        Err(error) => {
                            self.status = format!("couldn't open a local terminal: {error}");
                        }
                    }
                    changed = true;
                }
                PickerEvent::HomeLaunched { machine, result } => {
                    match result {
                        Ok((message, launched)) => {
                            // The agent is up and focused; only now leave the
                            // composer, so no empty terminal flashes first.
                            self.home_launching = false;
                            self.home.close();
                            self.status = message;
                            // `agent.list` will not list the pane for a
                            // reconcile or two; the launch's own record
                            // lets `focus_agent` pin a pi right away.
                            let target = match &machine {
                                None => EndpointId::Local,
                                Some(id) => EndpointId::Machine(id.clone()),
                            };
                            if let Some(agent) = launched {
                                self.recent_launches
                                    .push((target, agent.clone(), Instant::now()));
                                self.activate_opened_endpoint(machine, Some(agent.pane), cx);
                            } else {
                                self.activate_opened_endpoint(machine, None, cx);
                            }
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
                Some(error) => self.status = error,
                None => self.status = op.ok,
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
                            !menu.quick_open
                                && matches!(
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
                        self.status = format!("couldn't check worktree: {error}");
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
                    if self.surface.apply(&message) != SurfaceChange::None {
                        changed = true;
                    }
                    if let Some(text) = self.surface.take_notify() {
                        self.notify_desktop(&text, cx);
                        self.status = text;
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

    /// Raise a native notification for a daemon push.
    ///
    /// Skipped when the user turned notifications off or is already looking at
    /// this window, where the status line carries the same text. The tag is
    /// unique so the Notification Center keeps every push rather than
    /// replacing the last one.
    fn notify_desktop(&self, text: &str, cx: &mut Context<Self>) {
        if !self.settings.notifications || self.window_active {
            return;
        }
        let session = self.active_session();
        let title = match session.as_str() {
            "" => "pilus".to_string(),
            session => format!("pilus · {session}"),
        };
        cx.show_system_notification(SystemNotification {
            tag: SharedString::from(notification_tag()),
            title: SharedString::from(title),
            body: SharedString::from(text.to_string()),
            actions: Vec::new(),
        });
    }

    /// Re-read the machine catalog without probing, off the GUI thread.
    ///
    /// `list_machines` shells out to the `luvus` CLI, so it must never run on
    /// the GUI thread. The result lands through the same channel the probe
    /// uses, and the next probe tick fills in reachability.
    fn reload_machines(&mut self) {
        let root = self.session_root.clone();
        let tx = self.machine_list_tx.clone();
        std::thread::spawn(move || {
            tx.send(list_machines(&root)).ok();
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
        let tx = self.machine_tx.clone();
        std::thread::spawn(move || {
            tx.send(refresh_machines(&root)).ok();
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
                        if endpoint.surface.apply(&message) != SurfaceChange::None {
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
                self.status = format!("{} · lost connection to {label}", self.active_session());
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
                    self.status = if entry.failures >= RECONNECT_ATTEMPTS {
                        format!("{id}: {error} (still retrying)")
                    } else {
                        format!("{id}: {error}")
                    };
                    changed = true;
                }
                MachineConnect::LocalFailed { error } => {
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
        self.status = format!("timed out switching to {label}");
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
        self.nav_sel = None;
        self.tab_sel = None;
        self.endpoint_sel = None;
        self.close_picker();
        // The swapped-in tabs carry their own active kind; follow it so the
        // nav rows highlight the surface that just took the window.
        self.reconcile_nav();
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
            self.status = format!("unknown machine `{machine_id}`");
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
        self.status = "reconnected".to_string();
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
        self.status = "reconnected".to_string();
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

    /// Pull shared settings the Appearances window or Settings page changed.
    ///
    /// A change to the update channel or feed URL rebuilds the updater engine
    /// in place; turning auto-check on checks right away, like a launch would.
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
        match self.input_layer() {
            InputLayer::Picker => self.picker_key(event, cx),
            InputLayer::Palette => self.palette_key(event, window, cx),
            InputLayer::Home => self.home_key(event, cx),
            InputLayer::Settings => self.settings_key(event, cx),
            InputLayer::Tasks => self.tasks_key(event, cx),
            InputLayer::Session => {
                let input = self.session_prompt.clone();
                if self.session.key(&event.keystroke, &input, cx) {
                    // A consumed key must not also reach the composer's
                    // input method, which would turn Ctrl+T into a
                    // transpose or Shift+Tab into a backtab.
                    cx.stop_propagation();
                    cx.notify();
                }
            }
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
        top_layer(
            self.picker.is_open(),
            self.palette.is_open(),
            self.home.is_open(),
            self.settings_open,
            self.tasks_panel.is_open(),
            self.session_owns_keys(),
        )
    }

    /// Whether the session panel takes the keyboard: always when it owns
    /// the card, and in a split only while its pane holds focus — keys for
    /// the pane beside it belong to the terminal.
    fn session_owns_keys(&self) -> bool {
        self.session.is_open() && (self.pi_split().is_none() || self.pi_focused)
    }

    /// The pi panel's place in the card when its pane shares the tab with a
    /// split: the panel covers only that pane and the terminal shows the
    /// rest. `None` while the panel owns the whole card.
    fn pi_split(&self) -> Option<PaneFrame> {
        if !self.session.is_open() {
            return None;
        }
        let view = self.pi_view.as_ref()?;
        let (endpoint, pane, frame) = self.pi_frame.as_ref()?;
        (*endpoint == view.endpoint && *pane == view.pane && !frame.is_full()).then_some(*frame)
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
            self.session_menu_open = false;
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
        ];
        // The native Pi Session panel reads whichever agent the active
        // workspace is running, on any endpoint. Auto mode keeps pi panes
        // covered itself, so the command only exists while the setting
        // is off.
        if !self.settings.pi_gui_auto && self.surface.workspaces().iter().any(|w| w.active) {
            entries.push(entry(
                Command::OpenSession,
                "Open Pi Session",
                "Workspace",
                None,
            ));
        }
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
                &format!("Switch to Session {}", session.name),
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
            Command::OpenUiPlayground => {
                ui_playground::open(self.theme, self.shared_settings.clone(), cx);
            }
            Command::OpenSession => self.open_session(cx),
            Command::FocusTab(index) => self.focus_tab(index, cx),
            Command::SwitchSession(name) => self.switch_session(&name),
            Command::NewSession => self.start_new_session(),
            Command::FocusWorkspace(id) => self.focus_workspace(&id),
            Command::FocusAgent(pane) => self.focus_agent(&pane),
        }
    }

    /// Send the system clipboard to the daemon, which applies the pane's own
    /// bracketed-paste rules.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if !text.is_empty() {
            self.send_active(ClientMessage::Paste(text));
        }
    }

    /// Focus an agent's pane, switching workspace and tab as needed.
    ///
    /// The AGENTS row highlights immediately from the optimistic pick; the
    /// feed's next `agent.list` confirms it once the daemon has focused it.
    /// A pi row gets the same optimism for its panel: the snapshot already
    /// says the pane is pi, so the pin need not wait on the daemon's move.
    fn focus_agent(&mut self, pane: &str) {
        self.agent_sel = Some((self.active.clone(), pane.to_string()));
        self.agent_sel_at = Instant::now();
        let picked = self
            .agents
            .iter()
            .find(|agent| agent.pane == pane && agent.agent == "pi")
            .or_else(|| {
                // A pane pilus just launched outruns `agent.list`: its own
                // record stands in until the snapshot knows the pane.
                self.recent_launches
                    .iter()
                    .find_map(|(endpoint, agent, _)| {
                        (*endpoint == self.active && agent.pane == pane && agent.agent == "pi")
                            .then_some(agent)
                    })
            });
        self.pi_optimistic = picked.map(|agent| {
            (
                PiView {
                    endpoint: self.active.clone(),
                    pane: agent.pane.clone(),
                    session: agent.session.clone(),
                },
                agent.clone(),
                Instant::now(),
            )
        });
        if self.pi_optimistic.is_some() {
            self.pi_released = None;
        }
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

    /// Focus a workspace by id, switching the daemon's active workspace.
    fn focus_workspace(&mut self, workspace_id: &str) {
        // The workspace will restore the pane it last focused — the
        // memory names it, so a pi there pins ahead of the snapshot like
        // any other pilus-initiated move.
        let remembered = self
            .workspace_focus
            .get(&(self.active.clone(), workspace_id.to_string()))
            .filter(|agent| agent.agent == "pi")
            .and_then(|remembered| {
                self.agents
                    .iter()
                    .find(|agent| agent.pane == remembered.pane && agent.agent == "pi")
                    .cloned()
            });
        self.pi_optimistic = remembered.map(|agent| {
            (
                PiView {
                    endpoint: self.active.clone(),
                    pane: agent.pane.clone(),
                    session: agent.session.clone(),
                },
                agent,
                Instant::now(),
            )
        });
        if self.pi_optimistic.is_some() {
            self.pi_released = None;
        }
        self.send_active(ClientMessage::ShellWorkspaceFocus {
            workspace_id: workspace_id.to_string(),
        });
    }

    /// Leave the full-card pages (Home and Settings): sidebar actions address
    /// the terminal surface they cover, so acting on one dismisses the other.
    fn leave_settings_page(&mut self) {
        self.settings_open = false;
        self.tasks_panel.close();
        self.home.close();
        self.update_url_editing = false;
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
            self.status = "no control connection for the selected endpoint".to_string();
            return;
        };
        self.close_picker();
        // Opening from the Home composer's picker should leave the composer
        // targeting the workspace it just opened, once the catalog catches up.
        if self.home.is_open() {
            self.home_pending_open = Some(path.clone());
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
            self.status = "no control connection for the selected endpoint".to_string();
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
        if key == ">"
            || event.keystroke.key_char.as_deref() == Some(">")
            || (event.keystroke.modifiers.shift && key == ".")
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
    /// terminal grid. Sharing the card keeps the page inset exactly like
    /// the terminal, with the sidebar still showing beside it.
    ///
    /// Each page fades in under its own id, so switching pages replays it.
    /// The terminal itself takes no opacity fade, which would show the
    /// desktop through a translucent grid; its text fades instead (see
    /// [`Self::reflow_alpha`]).
    fn card_content(
        &mut self,
        cx: &mut Context<Self>,
        grid: Div,
        layout: &CardLayout,
    ) -> AnyElement {
        let (pad_x, pad_y) = (layout.pad_x, layout.pad_y);
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
            return style::enter(
                "pilus-page-tasks",
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.tasks_page(cx)),
            )
            .into_any_element();
        }
        if self.settings_open {
            return style::enter("pilus-page-settings", self.settings_page(cx)).into_any_element();
        }
        if let Some(frame) = self.pi_split() {
            return self.split_session(cx, grid, layout, frame);
        }
        if self.session.is_open() {
            return style::enter(
                "pilus-page-session",
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.session_render(cx)),
            )
            .into_any_element();
        }
        div()
            .flex_1()
            .overflow_hidden()
            .pl(px(pad_x))
            .pt(px(pad_y))
            .child(grid)
            .into_any_element()
    }

    /// The terminal with the pi panel laid over its own pane only, so the
    /// panes it was split with stay live beside it.
    ///
    /// A press on the panel focuses its pane (the daemon would have, had
    /// the press reached the grid); the panel's own controls still get it.
    fn split_session(
        &mut self,
        cx: &mut Context<Self>,
        grid: Div,
        layout: &CardLayout,
        frame: PaneFrame,
    ) -> AnyElement {
        let theme = self.theme;
        let place = pane_place(frame, layout);
        let mut panel = div()
            .id("pilus-split-session")
            .absolute()
            .left(px(place.left))
            .top(px(place.top))
            .flex()
            .flex_col()
            .overflow_hidden()
            .occlude()
            .bg(self.chrome_fill())
            .border_1()
            .rounded_md()
            // Focus is the one edge accent earns here: it says which side
            // of the split the keyboard is on.
            .border_color(if self.pi_focused {
                theme.accent.opacity(0.55)
            } else {
                terminal_inset_border()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.pi_blurred_at = None;
                    if !this.pi_focused
                        && let Some(view) = this.pi_view.clone()
                    {
                        this.pi_focused = true;
                        this.focus_agent(&view.pane);
                        cx.notify();
                    }
                }),
            );
        panel = match place.width {
            Some(width) => panel.w(px(width)),
            None => panel.right(px(0.0)),
        };
        panel = match place.height {
            Some(height) => panel.h(px(height)),
            None => panel.bottom(px(0.0)),
        };
        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .child(
                div()
                    .size_full()
                    .pl(px(layout.pad_x))
                    .pt(px(layout.pad_y))
                    .child(grid),
            )
            .child(style::enter(
                "pilus-split-session-enter",
                panel.child(self.session_render(cx)),
            ))
            .into_any_element()
    }

    /// Paint the Pi Session panel for the active workspace.
    fn session_render(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let settings = self.shared_settings.clone();
        let input = self.session_prompt.clone();
        // While a picker is open, up and down walk its rows rather than
        // moving the draft's caret; every other editing key still belongs
        // to the prompt.
        let picker_open = self.session.picker_open();
        input.update(cx, |input, _| input.surrender_vertical_keys(picker_open));
        self.session.render(theme, &settings, &input, cx)
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

    /// The pi pane the session panel should stand in for right now.
    ///
    /// Auto mode only: the focused agent, when it is pi. The daemon's
    /// report is carried along as a seed but no longer gates the pin — a
    /// pane waiting on it still gets the waiting panel instead of a beat
    /// of terminal pi's extension will replace anyway.
    /// Pure so tests can drive it with fabricated agents.
    ///
    /// With no pi focused, a pi sharing the visible tab (`visible`: the
    /// active workspace id and tab index) through a split still counts —
    /// its panel covers just its own pane beside the focused one. The
    /// caller only shows it once the pane's frame confirms the split.
    fn pi_view_for(
        agents: &[Agent],
        auto: bool,
        endpoint: &EndpointId,
        visible: Option<(&str, u16)>,
    ) -> Option<(PiView, Agent)> {
        if !auto {
            return None;
        }
        let agent = match agents.iter().find(|agent| agent.focused) {
            Some(agent) if agent.agent == "pi" => agent,
            _ => {
                let (workspace, tab) = visible?;
                agents.iter().find(|agent| {
                    agent.agent == "pi" && agent.workspace_id == workspace && agent.tab == tab
                })?
            }
        };
        Some((
            PiView {
                endpoint: endpoint.clone(),
                pane: agent.pane.clone(),
                session: agent.session.clone(),
            },
            agent.clone(),
        ))
    }

    /// Follow the focused pi pane: pin its conversation over the terminal,
    /// or close the panel when nothing pi-shaped holds focus.
    ///
    /// A focused pi pins even before its extension reports — the panel
    /// waits on the socket rather than show a beat of terminal the report
    /// then replaces. A pilus-initiated focus (an agent row or a tab
    /// holding a pi) pins earlier still, on the snapshot's own record,
    /// since the daemon's move is only reported a reconcile later. While
    /// the socket is live its session id outranks the daemon's:
    /// `pane.report_session` produces no event, so `agent.list` can lag a
    /// `/new` by a reconcile tick, and reopening on that stale value would
    /// blank the session the socket already followed.
    ///
    /// Also installs the session-report extension on a machine the first
    /// time it becomes the active endpoint — the daemon's `session` field
    /// only appears once the extension is running.
    fn sync_pi_view(&mut self, cx: &mut Context<Self>) -> bool {
        if let EndpointId::Machine(id) = &self.active
            && self.pi_ext_machines.insert(id.clone())
            && let Some(target) = self.machine_target_by_key(id)
        {
            std::thread::spawn(move || {
                let _ = pi_extension::install_remote(&target);
            });
        }
        remember_focus(
            &self.agents,
            self.surface.workspaces(),
            &self.tabs,
            &self.active,
            &mut self.workspace_focus,
            &mut self.tab_focus,
        );
        // A launch the snapshot has adopted (or never will) leaves the list.
        self.recent_launches.retain(|(_, agent, at)| {
            at.elapsed() < Duration::from_secs(8)
                && !self.agents.iter().any(|known| known.pane == agent.pane)
        });
        let visible_workspace = self
            .surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)
            .map(|workspace| workspace.id.clone());
        let visible_tab = self.tabs.iter().find(|tab| tab.active).map(|tab| tab.index);
        let visible = visible_workspace.as_deref().zip(visible_tab);
        let mut desired = Self::pi_view_for(
            &self.agents,
            self.settings.pi_gui_auto,
            &self.active,
            visible,
        );
        let mut focused = desired.as_ref().is_some_and(|(_, agent)| agent.focused);
        // A pilus-initiated focus pins ahead of the snapshot: the daemon
        // has not reported the move yet, so `pi_view_for` still names the
        // pane we came from — or nothing. A snapshot showing the picked
        // pane focused takes over (it produces the same `PiView`, so the
        // panel never reopens); the window bounds a pick the daemon
        // never confirms.
        if self.settings.pi_gui_auto
            && let Some((view, agent)) =
                optimistic_pick(&self.agents, &self.active, &mut self.pi_optimistic)
        {
            desired = Some((view, agent));
            focused = true;
        }
        // The frame reader follows the pi on screen whether or not its panel
        // shows yet: an unfocused one waits on it below.
        self.pi_candidate = desired
            .as_ref()
            .map(|(view, _)| (view.endpoint.clone(), view.pane.clone()));
        // A click beside the panel moved focus off it; the snapshot catches
        // up within a reconcile or two.
        if let Some(at) = self.pi_blurred_at {
            if !focused || at.elapsed() >= PI_BLUR_WINDOW {
                self.pi_blurred_at = None;
            } else {
                focused = false;
            }
        }
        // An unfocused pi shows only beside a split it is known to share;
        // until its frame says so, the focused pane's terminal owns the card.
        if !focused && let Some((view, _)) = &desired {
            let split = self
                .pi_frame
                .as_ref()
                .is_some_and(|(endpoint, pane, frame)| {
                    *endpoint == view.endpoint && *pane == view.pane && !frame.is_full()
                });
            if !split {
                desired = None;
            }
        }
        let focus_changed = self.pi_focused != focused;
        self.pi_focused = focused;
        // A panel the user closed or browsed away from stays released until
        // the pane reports a session (a waiting panel that finally got one)
        // or loses focus; reopening it every tick would be the terminal
        // flash, or the history browse, all over again.
        if let Some((endpoint, pane, released_session)) = self.pi_released.clone() {
            match &desired {
                Some((view, _)) if view.endpoint == endpoint && view.pane == pane => {
                    if released_session.is_none() && view.session.is_some() {
                        self.pi_released = None;
                    } else {
                        return false;
                    }
                }
                _ => self.pi_released = None,
            }
        }
        let view = desired.as_ref().map(|(view, _)| view.clone());
        if view == self.pi_view {
            if focus_changed {
                return true;
            }
            // A panel that got closed by hand is a release, not a state
            // change: remember it so the next tick does not reopen.
            if let Some(view) = &self.pi_view
                && !self.session.is_open()
            {
                let session = view.session.clone();
                self.pi_released = Some((view.endpoint.clone(), view.pane.clone(), session));
            }
            return false;
        }
        match desired {
            Some((view, agent)) => {
                let mut same_pane = self
                    .pi_view
                    .as_ref()
                    .is_some_and(|old| old.endpoint == view.endpoint && old.pane == view.pane);
                // Coming back to a pane whose panel was parked swaps it in
                // as it stands — no reload, no skeleton, same scroll.
                if !same_pane {
                    self.park_session();
                    same_pane = self.unpark_session(&view);
                }
                // The extension socket learns the pane's session before
                // the daemon's report catches up; while it is live its
                // word stands.
                let live = (same_pane && self.session.rpc_live())
                    .then(|| self.session.live_session())
                    .flatten();
                let want = live.or_else(|| view.session.clone());
                let already = same_pane && want == self.session.live_session();
                if !already {
                    match &self.active {
                        EndpointId::Local => self.session.open_local_session(
                            self.api_socket(),
                            expand_local(&agent.cwd),
                            want.clone(),
                            view.pane.clone(),
                            cx,
                        ),
                        EndpointId::Machine(id) => {
                            let Some(target) = self.machine_target_by_key(id) else {
                                // The pane is real but the machine has no
                                // SSH profile: nothing can read its
                                // session, so the terminal stays up.
                                let was_open = self.pi_view.take().is_some();
                                if was_open {
                                    self.session.close();
                                }
                                return was_open;
                            };
                            self.session.open_remote_session(
                                target,
                                self.expand_cwd(id, &agent.cwd),
                                want.clone(),
                                view.pane.clone(),
                                cx,
                            );
                        }
                    }
                }
                // Only a known session pins: a waiting panel stays
                // escapable so a pi that never reports is not a locked
                // card.
                self.session.set_pinned(want.is_some());
                self.pi_view = Some(PiView {
                    session: want,
                    ..view
                });
            }
            None => {
                self.park_session();
                self.session.set_pinned(false);
            }
        }
        true
    }

    /// Set the auto-pinned panel aside when its pane leaves the screen: it
    /// keeps following the session, and the card gets an empty stand-in.
    /// A panel the user already closed has nothing worth keeping.
    fn park_session(&mut self) {
        let Some(view) = self.pi_view.take() else {
            self.session.close();
            return;
        };
        if !self.session.is_open() {
            self.session.close();
            return;
        }
        self.session.suspend();
        let stand_in = self.session.sibling();
        let panel = std::mem::replace(&mut self.session, stand_in);
        self.pi_parked.retain(|(parked, _, _)| {
            !(parked.endpoint == view.endpoint && parked.pane == view.pane)
        });
        self.pi_parked.insert(0, (view, panel, Instant::now()));
        while self.pi_parked.len() > PI_PARKED_MAX {
            if let Some((_, mut evicted, _)) = self.pi_parked.pop() {
                evicted.close();
            }
        }
    }

    /// Swap the parked panel for `view`'s pane back in. Returns whether one
    /// was waiting.
    fn unpark_session(&mut self, view: &PiView) -> bool {
        let Some(index) = self.pi_parked.iter().position(|(parked, _, _)| {
            parked.endpoint == view.endpoint && parked.pane == view.pane
        }) else {
            return false;
        };
        let (_, mut panel, _) = self.pi_parked.remove(index);
        panel.resume();
        let mut stand_in = std::mem::replace(&mut self.session, panel);
        stand_in.close();
        true
    }

    /// Keep parked panels current and retire the ones left too long.
    fn poll_parked_sessions(&mut self, cx: &mut Context<Self>) {
        let input = self.session_prompt.clone();
        self.pi_parked.retain_mut(|(_, panel, parked_at)| {
            if parked_at.elapsed() >= PI_PARK_TTL {
                panel.close();
                return false;
            }
            panel.poll(&input, cx);
            true
        });
    }

    /// Read where the on-screen pi pane sits in its tab, off the GUI
    /// thread, and fold the answers in. Returns whether the panel's frame
    /// changed.
    fn poll_pi_frame(&mut self, refresh: bool) -> bool {
        let mut changed = false;
        while let Ok((endpoint, pane, frame)) = self.pi_frame_rx.try_recv() {
            self.pi_frame_pending = false;
            if self.pi_candidate.as_ref() != Some(&(endpoint.clone(), pane.clone())) {
                continue;
            }
            let next = frame.map(|frame| (endpoint, pane, frame));
            if next != self.pi_frame {
                self.pi_frame = next;
                changed = true;
            }
        }
        let Some((endpoint, pane)) = self.pi_candidate.clone() else {
            if self.pi_frame.take().is_some() {
                changed = true;
            }
            return changed;
        };
        if self.pi_frame_pending {
            return changed;
        }
        let known = self
            .pi_frame
            .as_ref()
            .is_some_and(|(known_endpoint, known_pane, _)| {
                *known_endpoint == endpoint && *known_pane == pane
            });
        let interval = match endpoint {
            EndpointId::Local => PI_FRAME_POLL_LOCAL,
            EndpointId::Machine(_) => PI_FRAME_POLL_REMOTE,
        };
        let due = !known || refresh || self.pi_frame_at.is_none_or(|at| at.elapsed() >= interval);
        if !due {
            return changed;
        }
        let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
            return changed;
        };
        self.pi_frame_pending = true;
        self.pi_frame_at = Some(Instant::now());
        let tx = self.pi_frame_tx.clone();
        std::thread::spawn(move || {
            let frame = client.pane_layout(&pane).ok();
            let _ = tx.send((endpoint, pane, frame));
        });
        changed
    }

    /// Remember that the auto-pinned panel was dismissed, so the next sync
    /// does not reopen it while the same pane stays focused. A manual panel
    /// (`--session-ui`, no pane) records nothing.
    fn record_pi_release(&mut self) {
        if let Some(view) = &self.pi_view {
            let session = self.session.live_session().or_else(|| view.session.clone());
            self.pi_released = Some((view.endpoint.clone(), view.pane.clone(), session));
        }
    }

    /// Close the Pi Session panel from its header button.
    fn release_pi_session(&mut self, cx: &mut Context<Self>) {
        self.record_pi_release();
        self.session.close();
        self.session.set_pinned(false);
        cx.notify();
    }

    /// Detach the panel from its live pane before browsing the history.
    /// The chosen session is read-only, so the auto-pin must not retarget it
    /// back to the pane's current session next tick.
    fn release_pi_for_history(&mut self) {
        self.record_pi_release();
        self.session.set_pinned(false);
    }

    /// Open the Pi Session panel for the active workspace's agent.
    ///
    /// Manual open, used when `pi_gui_auto` is off. Only the pane's own
    /// reported session id resolves — without the extension's report there
    /// is no reliable link from a pane to a file, so we ask the user to get
    /// one rather than paint the wrong conversation.
    fn open_session(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self
            .surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)
            .cloned()
        else {
            self.status = "no active workspace to read a session from".to_string();
            cx.notify();
            return;
        };
        let Some(agent) = self
            .agents
            .iter()
            .find(|agent| agent.agent == "pi" && agent.focused)
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|agent| agent.agent == "pi" && agent.workspace_id == active.id)
            })
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|agent| agent.agent == "pi" && agent.cwd == active.cwd)
            })
            .cloned()
        else {
            self.status = "no pi agent in this workspace to read a session from".to_string();
            cx.notify();
            return;
        };
        let Some(session_id) = agent.session.clone() else {
            self.status =
                "pi hasn't reported its session yet — restart pi or run /reload in it".to_string();
            cx.notify();
            return;
        };
        self.home.close();
        self.settings_open = false;
        match self.active.clone() {
            EndpointId::Local => {
                self.session.open_local_session(
                    self.api_socket(),
                    expand_local(&agent.cwd),
                    Some(session_id),
                    agent.pane.clone(),
                    cx,
                );
            }
            EndpointId::Machine(id) => {
                let Some(target) = self.machine_target_by_key(&id) else {
                    self.status = format!("no SSH target for `{id}`");
                    cx.notify();
                    return;
                };
                let cwd = self.expand_cwd(&id, &agent.cwd);
                self.session.open_remote_session(
                    target,
                    cwd,
                    Some(session_id),
                    agent.pane.clone(),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Show the Settings page.
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.home.close();
        self.settings_open = true;
        self.settings_status.clear();
        self.probe_machines();
        self.ensure_agent_configs_for("local");
        cx.notify();
    }

    /// Show the Home composer.
    ///
    /// The composer picks the machine, model, and effort for a launch, so it
    /// probes the machine catalog and resolves the local agent list first.
    fn open_home(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.picker.close();
        self.home.open();
        self.home_sync_key.clear();
        self.probe_machines();
        self.ensure_home_agent_configs();
        self.sync_home();
        self.home_focus_active_workspace();
        cx.notify();
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
    fn home_workspaces(&self) -> Vec<(EndpointId, &ShellWorkspace)> {
        let mut rows = Vec::new();
        for id in self.connected_endpoints() {
            for workspace in self.endpoint_workspaces(&id) {
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

        // A workspace opened from the composer's own picker lands in the
        // catalog a tick later; select it then so Send targets it.
        let pending = self.home_pending_open.clone();
        let pending_index = pending.as_deref().and_then(|pending| {
            self.home_workspaces().iter().position(|(id, workspace)| {
                let cwd = self.expand_cwd(&id.key(), &workspace.cwd);
                cwd == pending || canonical(&cwd) == canonical(pending)
            })
        });
        if let Some(index) = pending_index {
            self.home.choice_mut(HomeField::Workspace).choose(index);
            self.home_pending_open = None;
        }

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

    /// Route a keystroke to the Home composer.
    fn home_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;

        // An open dropdown owns the keyboard: arrows move, Enter commits.
        if self.home.dropdown().is_some() {
            match key {
                "escape" => self.home.close_dropdown(),
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
            self.home.close();
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
        let location = workspace.cwd.clone();
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
            && !matches!(prompt.action, PromptAction::DeleteWorktree { .. })
        {
            let handle = self.menu_prompt_input.focus_handle(cx);
            if !handle.is_focused(window) {
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
            InputLayer::Home => {
                if self.home.focus() == HomeFocus::Prompt {
                    Some(self.home_prompt.focus_handle(cx))
                } else {
                    None
                }
            }
            InputLayer::Settings => {
                if self.update_url_editing {
                    Some(self.update_feed_input.focus_handle(cx))
                } else {
                    None
                }
            }
            // The composer's text fields hold focus; its pickers leave it
            // with the window, which reads their keys.
            InputLayer::Tasks if self.tasks_panel.composing() => self
                .tasks_inputs
                .focus_handle(self.tasks_panel.compose_focus(), cx),
            InputLayer::Tasks => None,
            // An open session picker takes the keyboard, so a query reaches
            // its filter field instead of the prompt the user is typing.
            InputLayer::Session => Some(
                self.session
                    .picker_filter_field()
                    .unwrap_or_else(|| self.session_prompt.clone())
                    .focus_handle(cx),
            ),
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

    /// The Home screen: the launch composer that fills the inset card.
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
            .rounded_lg()
            .bg(theme.panel.opacity(0.0))
            .border_t_1()
            .border_l_1()
            .border_r_1()
            .border_b_1()
            .border_color(terminal_inset_border())
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
                .rounded_full()
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
                    .rounded_md()
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
    /// It fills the same inset card the grid lives in — same gaps, border,
    /// and rounding — so it reads as a native surface with the sidebar still
    /// showing. A Back button at the top left closes it; there is no close
    /// button by design.
    fn settings_page(&self, cx: &mut Context<Self>) -> Div {
        if !self.settings_open {
            return div();
        }
        let theme = self.theme;
        let body = div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.remote_access_section_block(cx))
            .child(self.machines_section_block(cx))
            .child(self.agent_config_section_block(cx))
            .child(self.notifications_section_block(cx))
            .child(self.pi_gui_section_block(cx))
            .child(self.updates_section_block(cx));
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(
                div().flex().flex_row().flex_shrink_0().px_4().pt_3().child(
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

    /// The AGENT CONFIG section of the page: the coding agents installed on
    /// this machine, their config source, and the model catalog each exposes.
    fn agent_config_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let loading = self.agent_config_pending.contains("local");
        let configs = self.agent_config_cache.get("local");
        let mut section = div().flex().flex_col().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_3()
                .child(style::section_label(
                    theme,
                    "AGENT CONFIG",
                    self.section_font(),
                ))
                .child(
                    style::small_button(
                        "pilus-agents-refresh",
                        "Refresh",
                        style::ButtonKind::Secondary,
                        theme,
                        self.section_font(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.agent_config_cache.remove("local");
                        this.agent_config_pending.remove("local");
                        this.ensure_agent_configs_for("local");
                        cx.notify();
                    })),
                ),
        );
        match configs {
            Some(configs) if !configs.is_empty() => {
                for config in configs {
                    section = section.child(self.agent_config_row(config));
                }
            }
            Some(_) | None if loading => {
                section = section.child(agent_config_empty(
                    theme,
                    "Detecting agents…",
                    self.footer_font(),
                ));
            }
            Some(_) | None => {
                section = section.child(agent_config_empty(
                    theme,
                    "No coding agents found. Install one, then Refresh.",
                    self.footer_font(),
                ));
            }
        }
        section
    }

    /// One installed agent row: name, config source, default, and model count.
    fn agent_config_row(&self, config: &AgentConfig) -> Div {
        let theme = self.theme;
        let default = config
            .default_model
            .clone()
            .unwrap_or_else(|| "—".to_string());
        let catalog = if config.models.is_empty() {
            "no catalog".to_string()
        } else if config.discovered {
            format!("{} models · live", config.models.len())
        } else {
            format!("{} models", config.models.len())
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .mx_3()
            .px_2()
            .py_2()
            .rounded_md()
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
                                    .text_size(px(self.chrome_font()))
                                    .text_color(theme.fg)
                                    .child(config.label.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(self.section_font()))
                                    .text_color(theme.muted)
                                    .child(config.kind.clone()),
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
                            .child(config.source.clone())
                            .child(format!("· default: {default}")),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(catalog),
            )
    }

    /// The REMOTE MACHINES section of the page: every saved machine profile
    /// with the same status and enable/remove actions as the manager window.
    /// Adding and provisioning stay in the full manager window.
    /// The REMOTE ACCESS section: reach this machine from another device.
    fn remote_access_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let running = connector::running();
        let status = &self.connector_status;
        let toggle = style::toggle("pilus-remote-access-toggle", running, theme).on_click(
            cx.listener(|this, _, _, cx| {
                this.toggle_connector();
                cx.notify();
            }),
        );
        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .child(style::section_label(
                        theme,
                        "REMOTE ACCESS",
                        self.section_font(),
                    ))
                    .child(toggle),
            )
            // The instruction stays put; the connection details appear below.
            .child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("Enable to reach this machine from another device"),
            );
        if let Some(error) = &status.error {
            section = section.child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.alert)
                    .child(error.clone()),
            );
        } else if running {
            let mut output = div()
                .mx_3()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .rounded_md()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border);
            match (&status.endpoint_id, &status.ticket) {
                (Some(id), Some(ticket)) => {
                    output = output.child(self.copy_row(
                        "endpoint",
                        id.clone(),
                        "pilus-copy-endpoint",
                        cx,
                    ));
                    if let Some(enrollment) = &status.enrollment {
                        output = output.child(self.copy_row(
                            "enroll",
                            enrollment.clone(),
                            "pilus-copy-enroll",
                            cx,
                        ));
                    }
                    output = output.child(self.copy_row(
                        "ticket",
                        ticket.clone(),
                        "pilus-copy-ticket",
                        cx,
                    ));
                }
                _ => {
                    output = output.child(
                        div()
                            .text_size(px(self.footer_font()))
                            .text_color(theme.muted)
                            .child("Starting…"),
                    );
                }
            }
            section = section.child(output);
        } else if agent_binary().is_none() {
            section = section.child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("pilus-agent is not installed next to pilus."),
            );
        }
        section
    }

    /// A muted read-only value with a Copy button beside it.
    fn copy_row(
        &self,
        label: &'static str,
        value: String,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let size = self.section_font();
        let copied = value.clone();
        let button = style::small_button(id, "Copy", style::ButtonKind::Ghost, theme, size)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
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
                    .child(format!("{label}: {value}")),
            )
            .child(button)
    }

    /// Turn this node's connector on or off from the settings page.
    fn toggle_connector(&mut self) {
        if connector::running() {
            connector::stop();
            self.connector_status = ConnectorStatus::default();
            self.set_connector(false);
            self.settings_status = "Remote access disabled.".to_string();
        } else {
            match connector::start(&connector::default_options()) {
                Ok(()) => self.set_connector(true),
                Err(error) => self.settings_status = error,
            }
        }
    }

    /// Persist the connector choice so it restarts on the next launch.
    fn set_connector(&mut self, on: bool) {
        self.shared_settings.borrow_mut().connector = on;
        self.shared_settings.borrow().save(&pilus_config_dir());
    }

    fn machines_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_3()
                .child(style::section_label(
                    theme,
                    "REMOTE MACHINES",
                    self.section_font(),
                ))
                .child(
                    style::small_button(
                        "pilus-machines-manage",
                        "Manage Machines",
                        style::ButtonKind::Primary,
                        theme,
                        self.footer_font(),
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(OpenMachineManager.boxed_clone(), cx);
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
        let confirming = self.machine_pending_remove.as_deref() == Some(machine.id.as_str());
        let (status, status_color) = machine_status_text(machine, theme);
        let toggle_id = id.clone();
        let toggle_label = if machine.enabled { "Disable" } else { "Enable" };
        let install_id = id.clone();
        let remove_id = id.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .mx_3()
            .px_2()
            .py_2()
            .rounded_md()
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
                            .child(machine.destination.clone())
                            .child(div().text_color(status_color).child(status)),
                    ),
            )
            .child(
                style::small_button(
                    format!("pilus-machine-page-toggle-{id}"),
                    if busy { "…" } else { toggle_label },
                    style::ButtonKind::Ghost,
                    theme,
                    self.section_font(),
                )
                .text_color(if busy { theme.muted } else { theme.fg })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_machine_page(toggle_id.clone());
                    cx.notify();
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-machine-page-agent-{id}"),
                    "Install agent",
                    style::ButtonKind::Ghost,
                    theme,
                    self.section_font(),
                )
                .text_color(theme.muted)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.install_machine_page(install_id.clone());
                    cx.notify();
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-machine-page-remove-{id}"),
                    if confirming { "Confirm" } else { "Remove" },
                    style::ButtonKind::Ghost,
                    theme,
                    self.section_font(),
                )
                .text_color(if confirming { theme.alert } else { theme.muted })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_machine_page(remove_id.clone());
                    cx.notify();
                })),
            )
    }

    /// Enable a disabled machine, or disable an enabled one, off-thread.
    fn toggle_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        let Some(machine) = self
            .machines
            .iter()
            .find(|machine| machine.id == id)
            .cloned()
        else {
            return;
        };
        self.machine_pending_remove = None;
        if machine.enabled {
            self.run_machine_page_op(
                &id,
                vec!["disable".to_string()],
                format!("Disabled `{id}`."),
            );
        } else {
            self.run_machine_page_op(&id, vec!["enable".to_string()], format!("Enabled `{id}`."));
        }
    }

    /// Remove a machine after a second confirming click.
    fn remove_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        if self.machine_pending_remove.as_deref() != Some(id.as_str()) {
            self.machine_pending_remove = Some(id.clone());
            self.settings_status = format!("Remove `{id}`? Click Confirm to delete the profile.");
            return;
        }
        self.machine_pending_remove = None;
        self.run_machine_page_op(&id, vec!["remove".to_string()], format!("Removed `{id}`."));
    }

    /// Run one `luvus machine …` mutation off-thread, then report back.
    ///
    /// The outcome and a fresh catalog travel through the existing channels,
    /// so the window reloads the section and tells other windows, exactly
    /// like the manager window's own mutations do.
    fn run_machine_page_op(&mut self, id: &str, args: Vec<String>, success: String) {
        self.machine_busy = Some(id.to_string());
        self.settings_status = "Running…".to_string();
        let root = self.session_root.clone();
        let op_tx = self.machine_op_tx.clone();
        let machine_tx = self.machine_tx.clone();
        std::thread::spawn(move || {
            let message = match run_machine_cli(&root, args) {
                Ok(()) => success,
                Err(error) => error,
            };
            op_tx.send(message).ok();
            machine_tx.send(list_machines(&root)).ok();
        });
    }

    /// Install the connector on `machine` over SSH, off-thread.
    fn install_machine_page(&mut self, id: String) {
        if self.machine_busy.is_some() {
            return;
        }
        let Some(machine) = self
            .machines
            .iter()
            .find(|machine| machine.id == id)
            .cloned()
        else {
            return;
        };
        self.machine_pending_remove = None;
        let Some(agent) = agent_binary() else {
            self.settings_status =
                "pilus-agent not found; set PILUS_AGENT_BIN or install it beside pilus".to_string();
            return;
        };
        self.machine_busy = Some(id);
        self.settings_status = "Installing agent…".to_string();
        let op_tx = self.machine_op_tx.clone();
        let machine_tx = self.machine_tx.clone();
        let root = self.session_root.clone();
        std::thread::spawn(move || {
            let message = match install_agent(&machine, &agent) {
                Ok(message) => message,
                Err(error) => error.to_string(),
            };
            op_tx.send(message).ok();
            machine_tx.send(list_machines(&root)).ok();
        });
    }

    /// The NOTIFICATIONS section of the page: a single toggle for native
    /// desktop notifications, persisted immediately.
    fn notifications_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let on = self.settings.notifications;
        let toggle = style::toggle("pilus-notifications-toggle", on, theme)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_notifications(cx)));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .child(style::section_label(
                        theme,
                        "NOTIFICATIONS",
                        self.section_font(),
                    ))
                    .child(toggle),
            )
            .child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("Show a desktop notification when the daemon pushes one"),
            )
    }

    /// Toggle native notifications and persist the choice at once.
    fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        self.settings.notifications = !self.settings.notifications;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        cx.notify();
    }

    /// The PI SESSIONS section of the page: whether a focused pi pane is
    /// painted as the native conversation instead of its terminal.
    fn pi_gui_section_block(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let on = self.settings.pi_gui_auto;
        let toggle = style::toggle("pilus-pi-gui-toggle", on, theme)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_pi_gui(cx)));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .child(style::section_label(
                        theme,
                        "PI SESSIONS",
                        self.section_font(),
                    ))
                    .child(toggle),
            )
            .child(
                div()
                    .px_3()
                    .text_size(px(self.footer_font()))
                    .text_color(theme.muted)
                    .child("Show pi panes as a native conversation instead of the terminal"),
            )
    }

    /// Toggle the native pi view and persist the choice at once.
    fn toggle_pi_gui(&mut self, cx: &mut Context<Self>) {
        self.settings.pi_gui_auto = !self.settings.pi_gui_auto;
        self.settings.save(&self.pilus_root);
        *self.shared_settings.borrow_mut() = self.settings.clone();
        cx.notify();
    }

    /// The UPDATES section of the page: channel, feed override, auto-check,
    /// and a manual check. Committing a change rebuilds the updater engine.
    fn updates_section_block(&self, cx: &mut Context<Self>) -> Div {
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

        let mut line = div().flex().flex_row().items_center().min_h(px(18.0));
        if editing {
            let input = self.update_feed_input.clone();
            input.update(cx, |input, _| {
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(self.section_font()),
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
            .rounded_md()
            .bg(theme.panel_dim)
            .border_1()
            .border_color(if editing { theme.accent } else { theme.border })
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

        // `pilus` checks the public manifest and installs the daemon archive
        // itself; only the restart goes through the CLI, which knows the
        // selected session. A restart is two-click because it stops every pane.
        let daemon_busy = self.daemon_busy;
        let mut check_daemon = style::small_button(
            "pilus-check-daemon",
            "Check Luvus",
            style::ButtonKind::Ghost,
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
            style::ButtonKind::Secondary,
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
            style::ButtonKind::Ghost,
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
        let daemon_version = match (&self.daemon_installed, &self.daemon_latest) {
            (Some(installed), Some(latest)) => {
                Some(format!("installed {installed} · latest {latest}"))
            }
            (Some(installed), None) => Some(format!("installed {installed}")),
            (None, Some(latest)) => Some(format!("latest {latest}")),
            (None, None) => None,
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(style::section_label(theme, "UPDATES", self.section_font()).px_3())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .mx_3()
                    .px_3()
                    .py_3()
                    .rounded_md()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
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
                                    .child("Channel"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(self.update_channel_pill(UpdateChannel::Stable, cx))
                                    .child(self.update_channel_pill(UpdateChannel::Preview, cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(self.section_font()))
                                    .text_color(theme.muted)
                                    .child("Feed URL override"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(field)
                                    .child(feed_action),
                            ),
                    )
                    .child(checkbox)
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
                                    .child("Luvus daemon"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(check_daemon)
                                    .child(update_daemon)
                                    .child(restart_daemon),
                            ),
                    )
                    .children(daemon_version.map(|version| {
                        div()
                            .text_size(px(self.footer_font()))
                            .text_color(theme.muted)
                            .child(version)
                    }))
                    .children((!daemon_status.is_empty()).then(|| {
                        div()
                            .text_size(px(self.footer_font()))
                            .text_color(theme.muted)
                            .child(daemon_status)
                    }))
                    .child(
                        div().flex().flex_row().items_center().justify_end().child(
                            style::small_button(
                                "pilus-update-check-now",
                                "Check Now",
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

    /// One channel pill; the active one uses the accent color.
    fn update_channel_pill(&self, value: UpdateChannel, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let active = UpdateChannel::parse(&self.settings.update_channel) == value;
        div().child(
            div()
                .id(format!("pilus-update-channel-{}", value.label()))
                .flex()
                .items_center()
                .justify_center()
                .h(px(22.0))
                .px_3()
                .rounded_md()
                .border_1()
                .border_color(if active { theme.accent } else { theme.border })
                .text_color(if active { theme.window_bg } else { theme.fg })
                .text_size(px(self.section_font()))
                .font_weight(if active {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                })
                .cursor_pointer()
                .state_fill(
                    if active {
                        theme.accent
                    } else {
                        theme.panel_dim
                    },
                    theme.border,
                    style::RADIUS_MD,
                )
                .child(value.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_update_channel(value, cx);
                })),
        )
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
            .border_color(terminal_inset_border())
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
            .rounded_md()
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
            .border_color(terminal_inset_border())
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
                        .rounded_md()
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
                    .rounded_md()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(terminal_inset_border())
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
                    .rounded_md()
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
            .border_color(terminal_inset_border())
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
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(terminal_inset_border())
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
        self.session_name = session;
        self.target = ConnectTarget::socket(dir.join("luvus-client.sock"));
        match pilus_client::connect(&self.target, self.cols, self.rows) {
            Ok(connection) => {
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
            }
            Err(error) => {
                self.status = format!("couldn't attach to {name}: {error}");
            }
        }
    }

    /// Create a fresh session and attach to it once its socket appears.
    ///
    /// The name is generated docker-style; creation runs off the GUI thread
    /// and reports back through the session channel the poll loop drains.
    fn start_new_session(&mut self) {
        let existing: Vec<String> = self.sessions.iter().map(|info| info.name.clone()).collect();
        let name = generate_session_name(&existing);
        self.status = format!("starting {name}…");
        self.session_menu_open = false;
        let root = self.session_root.clone();
        let tx = self.session_tx.clone();
        std::thread::spawn(move || {
            tx.send(start_session_blocking(&root, &name)).ok();
        });
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
        self.save_update_settings(cx);
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
        self.sidebar_anim = Some(SidebarAnim {
            started: Instant::now(),
            from,
            to,
        });
        cx.notify();
    }

    /// The terminal text's opacity this frame: dipped and easing back to
    /// full while the grid settles after a size change or a content swap.
    ///
    /// Only the text fades. Cell backgrounds stay put, so a translucent
    /// window never flashes the desktop through the grid, and the layout
    /// never moves. The first size change of a burst starts the fade; the
    /// rest of a drag rides it out rather than restarting it. A sidebar
    /// slide starts none: the daemon reflows once when the dock moves, and
    /// a dip on top of that would read as the flicker it is meant to hide.
    fn reflow_alpha(&mut self, reduce_motion: bool) -> f32 {
        let now = Instant::now();
        let key = SurfaceKey {
            endpoint: self.active.key(),
            tab: self
                .tabs
                .iter()
                .find(|tab| self.tab_active(tab))
                .map(|tab| tab.index),
            shown: !self.home.is_open()
                && !self.settings_open
                && (!self.session.is_open() || self.pi_split().is_some()),
        };
        if self.surface_key.as_ref() != Some(&key) {
            // The first key is the launch: the first frame fades in too.
            if !reduce_motion {
                self.reflow_fade = Some((now, SWAP_FLOOR));
            }
            self.surface_key = Some(key);
        }
        if let Some(size) = self.shown_frame().map(|frame| (frame.width, frame.height))
            && size != self.grid_size
        {
            let fresh = self
                .grid_resized_at
                .is_none_or(|at| now.duration_since(at) >= REFLOW_BURST);
            if fresh && !reduce_motion && self.sidebar_anim.is_none() && self.reflow_fade.is_none()
            {
                self.reflow_fade = Some((now, REFLOW_FLOOR));
            }
            self.grid_size = size;
            self.grid_resized_at = Some(now);
        }
        let Some((started, floor)) = self.reflow_fade else {
            return 1.0;
        };
        let step = now.duration_since(started).as_secs_f32() / REFLOW_FADE.as_secs_f32();
        if step >= 1.0 {
            self.reflow_fade = None;
            return 1.0;
        }
        reflow_opacity(floor, step)
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
    fn hit_chrome(&mut self, window: &Window, position: Point<Pixels>) -> bool {
        if self.session_menu_open
            || self.palette.is_open()
            || self.picker.is_open()
            || self.context_menu.is_some()
            || self.menu_prompt.is_some()
        {
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
        // The gap ring around the floating card belongs to no pane.
        // Full-bleed has no ring: every point in the card is grid.
        let layout = self.card_layout(window, self.framed());
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
        let layout = self.card_layout(window, self.framed());
        let skip = u32::from(self.skipped_cols());
        let paint_w = u32::from(layout.cols).min(u32::from(width).saturating_sub(skip));
        let paint_h = u32::from(layout.rows).min(u32::from(height).saturating_sub(
            u32::from(SKIPPED_DAEMON_ROWS) + u32::from(SKIPPED_DAEMON_BOTTOM_ROWS),
        ));
        let y = (f32::from(position.y) - layout.origin_y) / layout.cell_h;
        let x = (f32::from(position.x) - layout.origin_x) / layout.cell_w;
        if y < 0.0 || x < 0.0 {
            return None;
        }
        let column = skip.saturating_add((x as u32).min(paint_w.saturating_sub(1)));
        let row = u32::from(SKIPPED_DAEMON_ROWS)
            .saturating_add((y as u32).min(paint_h.saturating_sub(1)));
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

    fn begin_selection(&mut self, event: &MouseDownEvent, window: &Window, cx: &mut Context<Self>) {
        if self.hit_chrome(window, event.position) {
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
        // A drag started on the grid may roam anywhere; anything else over
        // chrome belongs to the panels, not the daemon.
        if !self.dragging && self.hit_chrome(window, event.position) {
            if self.hover_cell.take().is_some() {
                cx.notify();
            }
            return;
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
        let layout = self.card_layout(window, self.framed());
        let skip = self.skipped_cols();
        self.selection = Some(Selection {
            anchor: Cell {
                column: skip,
                row: SKIPPED_DAEMON_ROWS,
            },
            cursor: Cell {
                column: skip.saturating_add(layout.cols).saturating_sub(1),
                row: SKIPPED_DAEMON_ROWS
                    .saturating_add(layout.rows)
                    .saturating_sub(1),
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
            SidebarMenuTarget::Agent { .. } => None,
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
    /// per workspace. Until an answer lands the row is absent; a successful
    /// probe inserts it a beat later. A failed probe is not cached, so the
    /// next right-click retries.
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
            SidebarMenuTarget::Workspace { cwd, .. } => self.expand_cwd(endpoint_key, cwd),
            SidebarMenuTarget::Agent { .. } => return,
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
        let (pinned, linked) = match &menu.target {
            SidebarMenuTarget::Workspace {
                endpoint,
                workspace_id,
                ..
            } => (
                self.pinned_workspaces
                    .contains(&(endpoint.clone(), workspace_id.clone())),
                // Only a confirmed linked checkout offers Delete Worktree;
                // unknown and known-not-linked leave the row out.
                self.worktree_linked
                    .get(&(endpoint.clone(), workspace_id.clone()))
                    .is_some_and(Option::is_some),
            ),
            SidebarMenuTarget::Agent { endpoint, pane, .. } => (
                self.pinned_agents
                    .contains(&(endpoint.clone(), pane.clone())),
                false,
            ),
        };
        menu.rows(pinned, linked)
    }

    /// Drive the open context menu from the keyboard.
    fn context_menu_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let rows = self.menu_rows();
        let quick_open = self
            .context_menu
            .as_ref()
            .is_some_and(|menu| menu.quick_open);
        match keystroke.key.as_str() {
            "escape" | "esc" => {
                if quick_open {
                    // Esc steps back out of the copy submenu before closing.
                    if let Some(menu) = self.context_menu.as_mut() {
                        menu.quick_open = false;
                        menu.selected = None;
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
                let from = menu
                    .selected
                    .unwrap_or(if dir < 0 { 0 } else { usize::MAX });
                if let Some(next) = menu.next_selectable(&rows, from, dir) {
                    menu.selected = Some(next);
                }
                cx.notify();
            }
            // A right arrow opens the copy submenu; a left arrow climbs back.
            "right" => {
                if !quick_open
                    && rows.contains(&SidebarItem::QuickActions)
                    && let Some(menu) = self.context_menu.as_mut()
                {
                    menu.quick_open = true;
                    menu.selected = None;
                    cx.notify();
                }
            }
            "left" => {
                if quick_open && let Some(menu) = self.context_menu.as_mut() {
                    menu.quick_open = false;
                    menu.selected = None;
                    cx.notify();
                }
            }
            "enter" => {
                let row = self
                    .context_menu
                    .as_ref()
                    .and_then(|menu| menu.selected)
                    .filter(|index| rows.get(*index).is_some_and(|item| item.selectable()))
                    .or_else(|| rows.iter().position(|item| item.selectable()));
                if let Some(index) = row {
                    self.run_context_item(rows[index], cx);
                }
            }
            _ => {}
        }
    }

    /// Run one hit context-menu row. The menu is cloned so the Quick Actions
    /// parent row can swap the submenu open in place; everything else closes
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
                    menu.selected = None;
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
            (SidebarMenuTarget::Workspace { .. }, SidebarItem::Pin | SidebarItem::Unpin) => {
                self.close_context_menu();
                self.menu_pin_workspace(&target, item == SidebarItem::Pin, cx);
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
                    self.status = "worktree status is still being checked".to_string();
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
                SidebarMenuTarget::Workspace { .. },
                SidebarItem::OpenGit | SidebarItem::OpenBoard | SidebarItem::OpenMission,
            ) => {
                self.close_context_menu();
                self.menu_open_board(&target, item, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::Rename) => {
                self.close_context_menu();
                self.menu_open_agent_rename(&target, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::Close) => {
                self.close_context_menu();
                self.menu_close_agent(&target, cx);
            }
            (SidebarMenuTarget::Agent { .. }, SidebarItem::Pin | SidebarItem::Unpin) => {
                self.close_context_menu();
                self.menu_pin_agent(&target, item == SidebarItem::Pin, cx);
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
            (_, SidebarItem::Divider) => {}
            // Copy rows only exist on a WORKSPACES target; the arms above
            // cover every row the builders can emit.
            _ => {}
        }
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
            self.status = "no control connection for the selected endpoint".to_string();
            cx.notify();
            return;
        };
        let workspace_id = workspace_id.clone();
        let name = name.clone();
        self.pinned_workspaces
            .remove(&(endpoint.clone(), workspace_id.clone()));
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

    /// Pin or unpin a workspace; the daemon floats its group first on the
    /// next catalog projection.
    fn menu_pin_workspace(
        &mut self,
        target: &SidebarMenuTarget,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        let SidebarMenuTarget::Workspace {
            endpoint,
            workspace_id,
            name,
            ..
        } = target
        else {
            return;
        };
        let key = (endpoint.clone(), workspace_id.clone());
        if pinned {
            self.pinned_workspaces.insert(key.clone());
        } else {
            self.pinned_workspaces.remove(&key);
        }
        let Some(mut client) = self.control_client_for_endpoint(&self.endpoint_by_key(endpoint))
        else {
            self.status = "no control connection for the selected endpoint".to_string();
            cx.notify();
            return;
        };
        let workspace_id = workspace_id.clone();
        let name = name.clone();
        let tx = self.menu_tx.clone();
        std::thread::spawn(move || {
            let error = client
                .pin_workspace(&workspace_id, pinned)
                .err()
                .map(|error| error.to_string());
            tx.send(MenuOpResult {
                ok: format!("{} {name}", if pinned { "pinned" } else { "unpinned" }),
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
            self.status = "no control connection for the selected endpoint".to_string();
            cx.notify();
            return;
        };
        let pane = pane.clone();
        let name = name.clone();
        self.pinned_agents.remove(&(endpoint.clone(), pane.clone()));
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

    /// Pin or unpin a live agent; pinned rows float to the top of AGENTS.
    fn menu_pin_agent(&mut self, target: &SidebarMenuTarget, pinned: bool, cx: &mut Context<Self>) {
        let SidebarMenuTarget::Agent {
            endpoint,
            pane,
            name,
            ..
        } = target
        else {
            return;
        };
        let key = (endpoint.clone(), pane.clone());
        if pinned {
            self.pinned_agents.insert(key);
        } else {
            self.pinned_agents.remove(&key);
        }
        self.status = format!("{} {}", if pinned { "pinned" } else { "unpinned" }, name);
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

    /// Switch to the workspace and open the daemon board its menu row names.
    fn menu_open_board(
        &mut self,
        target: &SidebarMenuTarget,
        item: SidebarItem,
        cx: &mut Context<Self>,
    ) {
        let SidebarMenuTarget::Workspace {
            endpoint,
            workspace_id,
            name,
            ..
        } = target
        else {
            return;
        };
        let (bind, nav) = match item {
            SidebarItem::OpenGit => ('g', None),
            SidebarItem::OpenBoard => ('o', Some(NavKind::Orchestration)),
            SidebarItem::OpenMission => ('m', Some(NavKind::Mission)),
            _ => return,
        };
        if let Some(kind) = nav {
            self.nav_sel = Some(kind);
            self.nav_sel_at = Instant::now();
        }
        self.status = format!("opening {name}…");
        self.activate_endpoint(
            self.endpoint_by_key(endpoint),
            Some(workspace_id.clone()),
            None,
            cx,
        );
        self.send_bind(bind);
        cx.notify();
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
            PromptAction::RenameWorkspace {
                endpoint,
                workspace_id,
            } => {
                if trimmed.is_empty() {
                    self.status = "a workspace name is required".to_string();
                    cx.notify();
                    return;
                }
                if trimmed.chars().count() > 40 {
                    self.status = "workspace names are capped at 40 characters".to_string();
                    cx.notify();
                    return;
                }
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.status = "no control connection for the selected endpoint".to_string();
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
            PromptAction::RenameAgent { endpoint, pane } => {
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.status = "no control connection for the selected endpoint".to_string();
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
                    self.status =
                        format!("close {name} first — it uses files inside this worktree");
                    cx.notify();
                    return;
                }
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.status = "no control connection for the selected endpoint".to_string();
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
                    self.status = "a branch name is required".to_string();
                    cx.notify();
                    return;
                }
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.status = "no control connection for the selected endpoint".to_string();
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
            .pt(px(SIDEBAR_TOP_INSET))
            .pb(px(SIDEBAR_LIST_PAD));

        // One hover pill glides between rows, the way the tab strip's
        // selection does, instead of each row lighting up on its own.
        content = content.child(style::hover_glide(
            "pilus-sidebar-hover",
            &self.sidebar_hover,
            self.theme
                .sidebar_raise(self.settings.glass_opacity(), SIDEBAR_HOVER_WEIGHT),
            style::RADIUS_MD,
        ));
        // The selected row's raised fill is a second pill under the rows, so
        // changing the selection reads as motion, like the tab strip's.
        content = content.child(style::glide(
            "pilus-sidebar-selection",
            &self.sidebar_selection,
            self.theme.sidebar_raise(self.settings.glass_opacity(), 1.0),
            style::RADIUS_MD,
        ));
        content = content.child(self.new_workspace_row(cx));
        // Auto mode replaces focused pi panes itself, so the manual toggle
        // row only exists while the setting is off.
        if !self.settings.pi_gui_auto {
            content = content.child(self.session_row(cx));
        }
        for kind in NavKind::ALL {
            content = content.child(self.nav_row(kind, cx));
        }

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
        let mut panel = div()
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
            // The collapse toggle pins to its right edge, clear of the lights.
            .child(
                div()
                    .h(px(40.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .pr(px(8.0))
                    .child(self.sidebar_toggle("pilus-sidebar-toggle", cx)),
            )
            .child(list);
        // A translucent sidebar parts from the terminal with the inset
        // hairline; the opaque one meets it seamlessly.
        // It is an overlay rather than a border: a border would take a pixel
        // from the panel's content box and leave the rows' right inset one
        // short of their left, but only while the window is translucent.
        if !self.framed() {
            panel = panel.child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .right(px(0.0))
                    .w(px(1.0))
                    .bg(terminal_inset_border()),
            );
        }
        panel
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
        self.tab_sel = Some(index);
        self.tab_sel_at = Instant::now();
        // The tab restores the pane it last focused — the memory names it,
        // so a pi there pins ahead of the snapshot like an agent row does.
        // The live `agents` list re-validates it, so a pi that exited
        // since cannot pin.
        let workspace = self
            .surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)
            .map(|workspace| workspace.id.clone());
        self.pi_optimistic = workspace
            .and_then(|workspace| {
                self.tab_focus
                    .get(&(self.active.clone(), workspace, index))
                    .filter(|agent| agent.agent == "pi")
                    .and_then(|remembered| {
                        self.agents
                            .iter()
                            .find(|agent| agent.pane == remembered.pane && agent.agent == "pi")
                            .cloned()
                    })
            })
            .map(|agent| {
                (
                    PiView {
                        endpoint: self.active.clone(),
                        pane: agent.pane.clone(),
                        session: agent.session.clone(),
                    },
                    agent,
                    Instant::now(),
                )
            });
        if self.pi_optimistic.is_some() {
            self.pi_released = None;
        }
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
    fn new_tab(&self) {
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
    fn split(&self, direction: &str) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.split(direction);
        }
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
            self.close_tab(index);
        }
    }

    /// Close a workspace tab.
    ///
    /// The daemon closes the whole workspace when its final tab closes, so the
    /// last tab is held back until explicit workspace close/delete exists.
    fn close_tab(&mut self, index: u16) {
        if self.tabs.len() <= 1 {
            self.status = "Can't close the last tab".to_string();
            return;
        }
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.close_tab(index);
        }
    }

    /// The native tab strip: session picker, terminal tabs, and the new-tab
    /// action, with the sidebar toggle parked at its head while the sidebar
    /// is hidden.
    /// The sidebar collapse/expand button: pinned to the sidebar's top
    /// clearance while it is open, floating at the strip's head while it is
    /// hidden so the sidebar can always come back.
    fn sidebar_toggle(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let glass = self.settings.glass_opacity();
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT))
            .w(px(SIDEBAR_TOGGLE_W))
            .rounded_md()
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

    fn top_chrome(&self, cx: &mut Context<Self>, cell_width: f32, fullscreen: bool) -> Div {
        let theme = self.theme;
        let glass = self.settings.glass_opacity();
        // Beside the sidebar when it is open; full width when it is hidden.
        // The left inset only clears the floating traffic lights, which
        // fullscreen hides, so the strip runs to the edge there.
        let left = self.sidebar_px(cell_width);
        let pad = self.strip_pad(fullscreen);
        let mut tabs = div()
            .id("pilus-tabs-scroll")
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
            theme.panel.opacity(glass),
            style::RADIUS_MD,
        ));
        for tab in &self.tabs {
            tabs = tabs.child(self.tab_item(tab, &selection, cx));
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
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT))
            .w(px(28.0))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.muted)
            .hover_fill(theme.panel.opacity(glass), style::RADIUS_MD)
            .hover(move |style| style.text_color(theme.panel_fg))
            .child(icon(
                include_bytes!("../../assets/icons/plus.svg"),
                13.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, _| this.new_tab()));
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
                    // The slot is the toggle plus the strip's gap after it;
                    // the negative margin cancels the gap so a closed slot
                    // takes no room at all.
                    .w(px((SIDEBAR_TOGGLE_W + 4.0) * parked))
                    .mr(px(-4.0))
                    .opacity(parked * parked)
                    .child(self.sidebar_toggle("pilus-sidebar-toggle-parked", cx)),
            );
        }
        tab_strip = tab_strip.child(self.session_picker(cx).flex_shrink_0());
        // Tabs belong to the terminal surface, so they hide with it while a
        // page owns the card; the session picker stays on every page. The plus
        // follows the tabs until overflow makes the scroll viewport shrink.
        if !self.settings_open && !self.home.is_open() {
            tab_strip = tab_strip.child(tabs).child(new_tab);
        }
        div()
            .absolute()
            .left(px(left))
            .right(px(0.0))
            .top(px(0.0))
            .h(px(HEADER_HEIGHT))
            .flex()
            .flex_col()
            .justify_center()
            .bg(self.chrome_fill())
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
        let glass = self.settings.glass_opacity();
        let active = self.tab_active(tab);
        let index = tab.index;
        let close_index = tab.index;
        let fg = if active { theme.panel_fg } else { theme.muted };
        // Until `session.snapshot` enriches the tab, show the workspace's
        // breadcrumb cwd.
        let cwd = if tab.cwd.is_empty() {
            self.crumb_path()
        } else {
            self.shorten_cwd(&self.active.key(), &tab.cwd)
        };
        let label = tab_label(&cwd, index);
        let process = if tab.process.is_empty() {
            "No terminal process"
        } else {
            tab.process.as_str()
        };
        let agent = self.tab_agent(index);
        let mut tip_lines = vec![
            (format!("Directory: {cwd}"), theme.muted),
            (format!("Process: {process}"), theme.muted),
        ];
        if let Some(agent) = agent {
            tip_lines.push((
                format!("Agent: {} · {}", agent.agent, status_label(&agent.status)),
                status_color(&agent.status, theme),
            ));
        }
        let tip = row_tooltip(format!("Tab {index}"), fg, tip_lines, theme);
        let group = format!("pilus-tab-{index}");
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
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT))
            .min_w(px(140.0))
            .max_w(px(280.0))
            .px_3()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            // The selected tab's raised fill is the strip's glide pill;
            // hovering any other tab only hints at it.
            .hover_fill(
                if active {
                    theme.panel.opacity(0.0)
                } else {
                    theme.panel.opacity(glass * 0.5)
                },
                style::RADIUS_MD,
            )
            .text_color(fg)
            .hover(move |style| style.text_color(theme.panel_fg))
            .tooltip(tip);
        let mut content = div()
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .whitespace_nowrap();
        if let Some(agent) = agent {
            let mut tile = div()
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .w(px(14.0))
                .h(px(14.0))
                .flex_shrink_0()
                .child(icon(agent_icon(&agent.agent), 12.0, fg));
            // An idle agent needs no badge; the icon alone is quieter and the
            // tab reads the same as one with no agent.
            if agent.status != "idle" {
                let status = status_color(&agent.status, theme);
                let indicator = if agent.status == "working" {
                    style::spinner(format!("pilus-tab-agent-spinner-{index}"), 7.0, status)
                        .into_any_element()
                } else {
                    div()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded_full()
                        .bg(status)
                        .into_any_element()
                };
                tile = tile.child(
                    div()
                        .absolute()
                        .top(px(-2.0))
                        .right(px(-2.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded_full()
                        .bg(theme.panel.opacity(0.0))
                        .child(indicator),
                );
            }
            content = content.child(tile);
        } else {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(14.0))
                    .h(px(14.0))
                    .flex_shrink_0()
                    .child(icon(
                        include_bytes!("../../assets/icons/terminal.svg"),
                        12.0,
                        fg,
                    )),
            );
        }
        content = content.child(
            div()
                .text_size(px(self.footer_font()))
                .font_family(style::mono_family(&self.shared_settings))
                .child(label),
        );
        let tab = if active {
            tab.child(style::glide_target(selection))
        } else {
            tab
        };
        tab.child(content)
            .child(close)
            .on_click(cx.listener(move |this, _, _, cx| this.focus_tab(index, cx)))
    }

    /// Daemon columns hidden beneath our sidebar: the dock the daemon blanks
    /// for us. Read from the dock that matches the current frame, not the live
    /// `ShellDock` control message: the server updates the dock before sending
    /// the matching frame, so jumping the skip on the control message paints
    /// the old frame's blank dock cells as a black band mid-toggle.
    fn skipped_cols(&self) -> u16 {
        if let Some((_, skip, _)) = &self.sidebar_freeze {
            return *skip;
        }
        match self.surface.frame_dock() {
            Some(dock) if dock.x == 0 => dock.width,
            _ => 0,
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
    fn release_sidebar_freeze(&mut self, reduce_motion: bool) -> bool {
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
        // The panes may have reflowed into their new width; a short text
        // fade softens the swap the way any resize settles.
        if !reduce_motion {
            self.reflow_fade = Some((Instant::now(), REFLOW_FLOOR));
        }
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
        self.sidebar_dock().map_or(0, |dock| dock.width)
    }

    /// The session picker in the top tab strip: current session name plus a
    /// chevron, opening the session menu below it.
    fn session_picker(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let glass = self.settings.glass_opacity();
        div()
            .id("pilus-session-button")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(TAB_HEIGHT))
            .pl(px(6.0))
            .pr_2()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .hover_fill(theme.panel.opacity(glass), style::RADIUS_MD)
            .child(
                div()
                    .text_color(theme.panel_fg)
                    .child(self.current_session_name().to_string()),
            )
            .child(icon(
                include_bytes!("../../assets/icons/chevron-down.svg"),
                12.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.session_menu_open = !this.session_menu_open;
                cx.notify();
            }))
    }

    /// The session menu: switch sessions or start a fresh one.
    ///
    /// A transparent catcher covers the content area so a click anywhere
    /// outside dismisses; the panel stops propagation so its own clicks land.
    /// It opens below the session picker in the top strip, offset by the same
    /// sidebar/fullscreen-dependent left inset the strip uses.
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
        let strip_left = self.sidebar_px(cell_width);
        let pad = self.strip_pad(fullscreen);
        let menu_left = strip_left + pad + 28.0 + 4.0;
        let mut items = div().flex().flex_col().gap_1();
        for session in &self.sessions {
            let name = session.name.clone();
            let is_current = session.name == self.current_session_name();
            let bg = if is_current {
                theme.panel
            } else {
                theme.sidebar_bg
            };
            items = items.child(
                div()
                    .id(format!("pilus-menu-session-{}", session.name))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.0))
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .state_fill(bg, theme.panel, style::RADIUS_MD)
                    .text_size(px(self.chrome_font()))
                    .child(icon(
                        if session.running {
                            include_bytes!("../../assets/icons/cloud.svg")
                        } else {
                            include_bytes!("../../assets/icons/circle.svg")
                        },
                        14.0,
                        theme.muted,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_color(if session.running {
                                theme.panel_fg
                            } else {
                                theme.muted
                            })
                            .child(session.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(self.footer_font()))
                            .text_color(theme.muted)
                            .child(if session.running {
                                "running"
                            } else {
                                "stopped"
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session_menu_open = false;
                        this.switch_session(&name);
                        cx.notify();
                    })),
            );
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
                .rounded_md()
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
                        .top(px(HEADER_HEIGHT))
                        .w(px(264.0))
                        .rounded_md()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(terminal_inset_border())
                        .p_2()
                        .child(items)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.session_menu_open = false;
                        cx.notify();
                    }),
                ),
        )
        .into_any_element()
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
        };

        let mut items = div().flex().flex_col().gap_0();
        for (index, item) in rows.iter().copied().enumerate() {
            if item == SidebarItem::Divider {
                items = items.child(div().mx_2().my_1().h(px(1.0)).bg(theme.border.opacity(0.5)));
                continue;
            }
            let active = menu.selected == Some(index);
            let color = if item.destructive() {
                theme.alert
            } else {
                theme.panel_fg
            };
            let label = item.label(paths, scoped).to_string();
            items = items.child(
                div()
                    .id(format!("pilus-sidebar-menu-{index}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.0))
                    .px_2()
                    .rounded_md()
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
        let height = 8.0 + rows.len() as f32 * 30.0;
        let mut top = menu.row + 6.0;
        if top + height > viewport_height {
            top = (menu.row - height - 6.0).max(8.0);
        }

        let panel = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .rounded_md()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(terminal_inset_border())
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
        let confirm = matches!(prompt.action, PromptAction::DeleteWorktree { .. });
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
            PromptAction::DeleteWorktree { path, .. } => Some(path.clone()),
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
            .rounded_md()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(terminal_inset_border())
            .child(style::title(
                theme,
                prompt.title,
                style::Fonts::resolve(&self.shared_settings).title,
            ))
            .child(
                div()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(prompt.placeholder),
            );
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
                        .child(if confirm_escalated {
                            "This can't be undone."
                        } else {
                            "Removes the checkout and its files. This can't be undone."
                        }),
                )
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .text_color(theme.muted)
                        .child(if confirm_escalated {
                            "⏎ Delete anyway · Esc Cancel"
                        } else {
                            "⏎ Delete · Esc Cancel"
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
                    .rounded_md()
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
            .border_color(terminal_inset_border())
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
                        .rounded_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(terminal_inset_border())
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
    /// the leading server glyph; the row itself stays unlabelled. `compact`
    /// drops the leading icon for rows nested under another row, which
    /// already carries one.
    fn workspace_row(
        &self,
        endpoint: &EndpointId,
        workspace: &ShellWorkspace,
        host: Option<&str>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let active = self.endpoint_selected(endpoint) && workspace.active;
        let remote = endpoint.machine_id().is_some();
        let selected = active;
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
        if !compact {
            // Every leading glyph wears the shared box, so the workspace name
            // lands on the same column as the nav and agent labels.
            let glyph = |bytes: &'static [u8], size: f32, color: gpui::Rgba| {
                div()
                    .w(px(SIDEBAR_GLYPH))
                    .flex_shrink_0()
                    .child(icon(bytes, size, color))
            };
            head = head.child(if workspace.nested {
                // A worktree hangs off the workspace above it: an elbow
                // ("|_") says so in place of the indent a deeper row needs.
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
                            .rounded_bl(px(3.0)),
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
                    theme.panel_fg,
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
        if !compact
            && !workspace.nested
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
            .rounded_md()
            .cursor_pointer()
            .children(selected.then(|| style::glide_target(&self.sidebar_selection)))
            .child(style::hover_target(&self.sidebar_hover))
            .tooltip(tip)
            .child(head);
        // The path line follows the daemon's Show Path / Hide Path toggle.
        if self.sidebar_workspace_paths {
            row = row.child(sub);
        }
        if workspace.nested {
            row = row.pl(px(SIDEBAR_ROW_PAD + SIDEBAR_NEST_INDENT));
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

    /// The bound view holding focus, if any, from the daemon's tab kinds.
    fn tab_view(&self) -> Option<NavKind> {
        match self.tabs.iter().find(|tab| tab.active)?.kind.as_str() {
            "mission" => Some(NavKind::Mission),
            "orch" => Some(NavKind::Orchestration),
            _ => None,
        }
    }

    /// The view a nav row opens: data rows deep-link to their board.
    fn nav_view(kind: NavKind) -> NavKind {
        match kind {
            NavKind::Automations | NavKind::Mission => NavKind::Mission,
            NavKind::Tasks | NavKind::Orchestration => NavKind::Orchestration,
        }
    }

    /// Whether a nav row reads active: the optimistic pick wins while fresh,
    /// then yields to whatever the daemon actually focused. Exactly one row
    /// ever lights, so a click never sprays the highlight across the menu.
    fn nav_active(&self, kind: NavKind) -> bool {
        if self.tasks_panel.is_open() {
            return kind == NavKind::Tasks;
        }
        if self.nav_sel != Some(kind) {
            return false;
        }
        self.nav_sel_at.elapsed() < Duration::from_secs(3)
            || self.tab_view() == Some(Self::nav_view(kind))
    }

    /// Follow the daemon when it navigates on its own (chords typed in a pane).
    fn reconcile_nav(&mut self) {
        if let Some(view) = self.tab_view()
            && self.nav_sel.is_none_or(|sel| Self::nav_view(sel) != view)
        {
            self.nav_sel = Some(view);
            self.nav_sel_at = Instant::now();
        }
    }

    /// A "New Workspace" action above the nav rows.
    ///
    /// The Home composer is where a new workspace and its first agent are
    /// chosen, so this row toggles the same composer as the Cmd+Shift+N chord.
    fn new_workspace_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let selected = self.home.is_open();
        div()
            .id("pilus-new-workspace")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(SIDEBAR_ROW_PAD))
            .pr(px(SIDEBAR_ROW_PAD))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.panel_fg)
            .children(selected.then(|| style::glide_target(&self.sidebar_selection)))
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/plus.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("New Workspace"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_home(cx)))
    }

    /// A "Pi Session" row: toggles the native session panel for the active
    /// workspace.
    fn session_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let selected = self.session.is_open();
        div()
            .id("pilus-pi-session")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(SIDEBAR_ROW_PAD))
            .pr(px(SIDEBAR_ROW_PAD))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.panel_fg)
            .children(selected.then(|| style::glide_target(&self.sidebar_selection)))
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(
                include_bytes!("../../assets/icons/agents/pi.svg"),
                SIDEBAR_GLYPH,
                theme.muted,
            ))
            .child(div().flex_1().child("Pi Session"))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.session.is_open() {
                    this.session.close();
                    cx.notify();
                } else {
                    this.open_session(cx);
                }
            }))
    }

    /// Toggle the Home composer, shared by the Cmd+Shift+N chord and the
    /// sidebar's New Workspace row.
    fn toggle_home(&mut self, cx: &mut Context<Self>) {
        if self.home.is_open() {
            self.home.close();
        } else {
            self.open_home(cx);
        }
        cx.notify();
    }

    /// One sidebar destination: lucide glyph, label, live badge, chord click.
    fn nav_row(&self, kind: NavKind, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = self.nav_active(kind);
        let selected = active;
        let badge = match kind {
            NavKind::Automations => Some(self.automations.enabled.to_string()),
            NavKind::Tasks => Some(self.tasks.open.to_string()),
            NavKind::Mission | NavKind::Orchestration => None,
        };
        let badge = match badge {
            Some(text) => div()
                .text_size(px(self.section_font()))
                .text_color(theme.muted)
                .child(text),
            None => div(),
        };
        let label = div().flex_1();
        let chord = kind.chord();
        div()
            .id(format!("pilus-nav-{}", kind.label()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_GLYPH_GAP))
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(SIDEBAR_ROW_PAD))
            // The badge keeps the row's content inset, so it reads as a
            // column rather than a number touching the panel edge.
            .pr(px(SIDEBAR_ROW_PAD))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .text_color(theme.panel_fg)
            .children(selected.then(|| style::glide_target(&self.sidebar_selection)))
            .child(style::hover_target(&self.sidebar_hover))
            .child(icon(kind.icon(), SIDEBAR_GLYPH, theme.muted))
            .child(label.child(kind.label().to_string()))
            .child(badge)
            .on_click(cx.listener(move |this, _, _, cx| {
                if kind == NavKind::Tasks {
                    this.open_tasks(cx);
                    return;
                }
                this.leave_settings_page();
                this.nav_sel = Some(kind);
                this.nav_sel_at = Instant::now();
                this.send_bind(chord);
                cx.notify();
            }))
    }

    /// Send `<bind> + key`: the daemon's prefix chord followed by `key`.
    ///
    /// The daemon's Keys tab confirms these: `m` opens Mission Control and
    /// `o` opens the task board.
    fn send_bind(&self, key: char) {
        let press = |code: KeyCode, mods: KeyModifiers| {
            ClientMessage::Key(KeyEvent::new_with_kind(code, mods, KeyEventKind::Press))
        };
        self.send_active(press(KeyCode::Char(' '), KeyModifiers::CONTROL));
        self.send_active(press(KeyCode::Char(key), KeyModifiers::NONE));
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
        } = spec;
        let theme = self.theme;
        let header = div()
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
                    .child(div().flex_1().h(px(1.0)).bg(terminal_inset_border()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section_toggled = Some((id, Instant::now()));
                        toggle(this, cx);
                    })),
            );
        header
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
            for (endpoint_key, target, cwds) in probes {
                let home = common_git_dirs_remote(&target, &cwds).home;
                if !home.is_empty() {
                    homes.push((endpoint_key, home));
                }
            }
            tx.send(HomeResolve { homes }).ok();
        });
    }

    /// WORKSPACES: every open workspace on every connected endpoint.
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
            for workspace in workspaces {
                any = true;
                children =
                    children.child(self.workspace_row(&id, workspace, host.as_deref(), false, cx));
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
            // Pinned agents float to the top; a stable sort keeps the pinned
            // and unpinned blocks in the order the daemon reported them.
            agents
                .sort_by_key(|agent| !self.pinned_agents.contains(&(id.key(), agent.pane.clone())));
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
            .rounded_md()
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
                                    .rounded_full()
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

    /// Measure the inset card and the grid that fits inside it.
    ///
    /// The card floats below the tab strip with a gap on top, right, and
    /// bottom, starting past the sidebar. The grid keeps a border on every
    /// side.
    fn card_layout(&mut self, window: &Window, framed: bool) -> CardLayout {
        let cell_w = self.cell_width(window);
        let cell_h = self.cell_height();
        let viewport = window.viewport_size();
        // A translucent terminal drops the inset card and runs to the window
        // edges; the opaque one keeps its gaps, hairline, and rounding.
        let gap_left = if framed { CARD_GAP } else { 0.0 };
        let gap_top = if framed { CARD_GAP_TOP } else { 0.0 };
        let gap_right = if framed { CARD_GAP_RIGHT } else { 0.0 };
        let gap_bottom = if framed { CARD_GAP } else { 0.0 };
        let border = if framed { 1.0 } else { 0.0 };
        let card_x = self.sidebar_px(cell_w) + gap_left;
        // The card's inner box: inside the hairline border, before the grid's
        // own breathing room.
        let inner_w = f32::from(viewport.width) - card_x - gap_right - 2.0 * border;
        let inner_h =
            f32::from(viewport.height) - HEADER_HEIGHT - gap_top - gap_bottom - 2.0 * border;
        // The framed card reserves breathing room around the grid; full-bleed
        // spends every pixel on cells instead.
        let breathing = if framed { TERMINAL_PAD } else { 0.0 };
        let cols = ((inner_w - 2.0 * breathing) / cell_w).clamp(20.0, 500.0) as u16;
        let rows = ((inner_h - 2.0 * breathing) / cell_h).clamp(5.0, 300.0) as u16;
        // The framed card snaps to the cell grid: the sub-cell remainder
        // leaves the card (widening its right and bottom gaps to the window)
        // instead of padding the grid, so all four insets stay exactly
        // `TERMINAL_PAD`. Full-bleed pins the grid to the top-left instead,
        // so the terminal truly reaches the edges.
        let (pad_x, pad_y) = if framed {
            (TERMINAL_PAD, TERMINAL_PAD)
        } else {
            (0.0, 0.0)
        };
        let (shrink_x, shrink_y) = if framed {
            (
                (inner_w - 2.0 * breathing - f32::from(cols) * cell_w).max(0.0),
                (inner_h - 2.0 * breathing - f32::from(rows) * cell_h).max(0.0),
            )
        } else {
            (0.0, 0.0)
        };
        // Full-bleed pools the leftover fraction at the far edges; the grid
        // extends its edge colors over it below.
        let slack_x = if framed {
            0.0
        } else {
            (inner_w - f32::from(cols) * cell_w).max(0.0)
        };
        let slack_y = if framed {
            0.0
        } else {
            (inner_h - f32::from(rows) * cell_h).max(0.0)
        };
        CardLayout {
            card_x,
            card_top: gap_top,
            card_right: gap_right + shrink_x,
            card_bottom: gap_bottom + shrink_y,
            pad_x,
            pad_y,
            origin_x: card_x + border + pad_x,
            origin_y: HEADER_HEIGHT + gap_top + border + pad_y,
            cols,
            rows,
            cell_w,
            cell_h,
            slack_x,
            slack_y,
        }
    }

    /// The fill behind the sidebar, the tab strip, and a native page: a
    /// shade denser than the terminal under translucency, so the chrome
    /// frames the grid and a page's text never sits on the bare desktop.
    fn chrome_fill(&self) -> gpui::Rgba {
        self.theme.chrome_glass(self.settings.glass_opacity())
    }

    /// Whether the terminal keeps its floating inset card: only the opaque
    /// window frames the grid, so any translucency runs full-bleed.
    fn framed(&self) -> bool {
        self.settings.glass_opacity() >= 1.0
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
        if self.sidebar_anim.is_some() {
            return;
        }
        let layout = self.card_layout(window, self.framed());
        // The daemon lays out its whole chrome (docks included); we only show
        // the pane area, so the skipped rows and columns ride along.
        let cols = layout.cols.saturating_add(self.skipped_cols());
        let rows = layout
            .rows
            .saturating_add(SKIPPED_DAEMON_ROWS)
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

/// Where a split pane's panel sits inside the card, in card pixels. An
/// edge on the card's own edge runs to it (`None` size), so the panel meets
/// the card flush instead of stopping at the last whole cell.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PanePlace {
    left: f32,
    top: f32,
    width: Option<f32>,
    height: Option<f32>,
}

/// A pane's cell span on the visible grid: `[col0, col1)` by `[row0, row1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneCells {
    col0: u16,
    col1: u16,
    row0: u16,
    row1: u16,
}

/// Round a pane's fractional frame to the whole cells of a `cols` by
/// `rows` grid.
fn pane_cells(frame: PaneFrame, cols: u16, rows: u16) -> PaneCells {
    let span = |start: f32, size: f32, cells: u16| {
        let count = f32::from(cells);
        let first = (start * count).round().clamp(0.0, count);
        let last = ((start + size) * count).round().clamp(first, count);
        (first as u16, last as u16)
    };
    let (col0, col1) = span(frame.x, frame.width, cols);
    let (row0, row1) = span(frame.y, frame.height, rows);
    PaneCells {
        col0,
        col1,
        row0,
        row1,
    }
}

/// A copy of `frame` with the split pi pane's cells blanked.
///
/// The panel over the pane is glass under translucency, so whatever the
/// cells beneath it hold — pi's own TUI — would read through it. Blank
/// cells keep the grid's glass there and nothing else.
fn mask_pane(frame: &FrameData, cells: PaneCells, skip_cols: u16) -> FrameData {
    let mut masked = frame.clone();
    let width = usize::from(frame.width);
    let col0 = usize::from(skip_cols.saturating_add(cells.col0)).min(width);
    let col1 = usize::from(skip_cols.saturating_add(cells.col1)).min(width);
    for row in cells.row0..cells.row1 {
        let row = usize::from(SKIPPED_DAEMON_ROWS.saturating_add(row));
        if row >= usize::from(frame.height) {
            break;
        }
        let start = row * width;
        if let Some(span) = masked.cells.get_mut(start + col0..start + col1) {
            span.fill(CellData::default());
        }
    }
    masked
}

/// Map a pane's fractional frame onto the card's whole-cell grid.
fn pane_place(frame: PaneFrame, layout: &CardLayout) -> PanePlace {
    let cells = pane_cells(frame, layout.cols, layout.rows);
    let (col0, col1) = (f32::from(cells.col0), f32::from(cells.col1));
    let (row0, row1) = (f32::from(cells.row0), f32::from(cells.row1));
    let (to_right, to_bottom) = (cells.col1 >= layout.cols, cells.row1 >= layout.rows);
    let left = if col0 <= 0.0 {
        0.0
    } else {
        layout.pad_x + col0 * layout.cell_w
    };
    let top = if row0 <= 0.0 {
        0.0
    } else {
        layout.pad_y + row0 * layout.cell_h
    };
    PanePlace {
        left,
        top,
        width: (!to_right).then_some(layout.pad_x + col1 * layout.cell_w - left),
        height: (!to_bottom).then_some(layout.pad_y + row1 * layout.cell_h - top),
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
        self.window_active = window.is_window_active();
        // Remember the open dock so the collapse tween still has a width to
        // fold from once the daemon drops it in response to our hide message.
        if let Some(dock) = self.surface.shell_dock() {
            self.last_sidebar_dock = Some(dock);
        }
        // One tween sample per frame: every edge below reads `sidebar_open`,
        // so the panel, tab strip, and card stay flush while the sidebar flies.
        self.sidebar_open = self.sidebar_factor();
        self.sync_metrics(window);
        self.sync_home();
        self.sync_focus(window, cx);

        let theme = self.theme;
        let glass = self.settings.glass_opacity();
        let framed = self.framed();
        // The blur slider drives the frosted backdrop view itself: full blur
        // is vibrant frost, weaker settings crossfade toward the sharp
        // desktop, and an explicit zero drops the view entirely. A framed
        // window is opaque — nothing would show — so it keeps it off.
        let blur = self.settings.blur.clamp(0.0, 1.0);
        self.glass.apply(window, if framed { 0.0 } else { blur });
        // A handler-less coat behind everything: it never intercepts clicks.
        // The framed card floats with a gap on every side, and that gap must
        // read as window base — the root is transparent so a translucent
        // window can show the backdrop, which would otherwise leak the
        // desktop through the margins. Only the translucent window leans on
        // the coat.
        let coat = glass_coat(theme, blur, framed);
        let cell_height = self.cell_height();
        let cell_width = self.cell_width(window);
        let layout = self.card_layout(window, framed);
        let line_height = px(cell_height);
        let selection = self.selection;
        // The terminal shares the sidebar's background: one uniform base
        // under the grid, the card, and the chrome, whatever the running app
        // paints. Voting the dominant cell color made the padding track the
        // app instead, so a green TUI left a green frame around an ink grid.
        let terminal_bg = theme.window_bg;
        let grid_theme = Theme {
            bg: terminal_bg,
            ..theme
        };
        // One glass layer exactly: full-bleed has no padding and its edge
        // fillers seal every pixel, so when the frame seals the card the
        // cells carry the opacity and the card stays clear — otherwise two
        // stacked layers would read near-solid instead of the setting. Only
        // the row count can leave a hole the fillers do not cover: a lagging
        // frame's missing columns are padded by each row's right-edge filler,
        // so requiring them here would flip the card opaque mid-resize and
        // double-blend every cell for a frame (a visible blink). The card
        // still backs the framed pad ring and any short frame.
        let sealed = match self.shown_frame() {
            Some(frame) => {
                frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS)
                    >= SKIPPED_DAEMON_ROWS.saturating_add(layout.rows)
            }
            None => false,
        };
        // A native page that owns the whole card has no cells to carry the
        // glass, so it takes the chrome's denser coat: under translucency
        // its text would otherwise sit straight on the desktop.
        let page_owns_card = self.home.is_open()
            || self.settings_open
            || (self.session.is_open() && self.pi_split().is_none());
        let card_bg = if page_owns_card {
            self.chrome_fill()
        } else if sealed && !framed {
            terminal_bg.opacity(0.0)
        } else {
            terminal_bg.opacity(glass)
        };
        let text_alpha = self.reflow_alpha(cx.reduce_motion());
        // Beside a split pi panel, the pane under it paints blank.
        let masked = self
            .pi_split()
            .zip(self.shown_frame())
            .map(|(pane, frame)| {
                mask_pane(
                    frame,
                    pane_cells(pane, layout.cols, layout.rows),
                    self.skipped_cols(),
                )
            });
        let grid = match masked.as_ref().or(self.shown_frame()) {
            Some(frame) => render_grid(
                frame,
                grid_theme,
                self.settings.terminal_font_size,
                self.settings.terminal_font.as_str(),
                line_height,
                selection,
                cell_width,
                self.skipped_cols(),
                layout.cols,
                layout.rows,
                glass,
                layout.slack_x,
                layout.slack_y,
                text_alpha,
            ),
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
        // A link under the pointer reads as clickable.
        let grid = if self.hovered_link() {
            grid.cursor_pointer()
        } else {
            grid
        };
        let grid = grid
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // A press beside a split pi panel focuses the pane under
                    // it in the daemon; hand the keyboard back to the
                    // terminal now rather than a reconcile later.
                    if this.pi_focused && this.pi_split().is_some() {
                        this.pi_focused = false;
                        this.pi_blurred_at = Some(Instant::now());
                    }
                    this.begin_selection(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    this.press(event, TerminalMouseButton::Right, window);
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
                    this.release(event, TerminalMouseButton::Right, window);
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
            }));
        let sidebar = self.sidebar(cx, cell_width, f32::from(window.viewport_size().height));
        let fullscreen = window.is_fullscreen() || window.is_simple_fullscreen();

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
            .on_action(cx.listener(|this, _: &CopySelection, _window, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &PasteClipboard, _window, cx| {
                this.paste_clipboard(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllGrid, window, cx| {
                this.select_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, _window, _cx| this.new_tab()))
            .on_action(cx.listener(|this, _: &OpenLocalTerminal, _window, cx| {
                this.open_local_terminal(cx);
            }))
            .on_action(cx.listener(|this, _: &SplitVertical, _window, _cx| this.split("right")))
            .on_action(cx.listener(|this, _: &SplitHorizontal, _window, _cx| this.split("down")))
            .on_action(cx.listener(|this, _: &CloseTab, _window, cx| {
                this.close_active_tab();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| {
                this.toggle_sidebar(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, _window, cx| {
                // The chord toggles, so pressing it again dismisses.
                if this.palette.is_open() {
                    this.palette.close();
                } else {
                    this.palette.open();
                }
                cx.notify();
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
            }));

        // The chrome inherits the chosen UI family and size; the grid overrides
        // both.
        root = apply_chrome_font(root, &self.shared_settings);

        root.child(coat)
            .child(header)
            .child(div().relative().flex_1().child({
                let mut card = div()
                    .absolute()
                    .left(px(layout.card_x))
                    // Content already starts below the header spacer, so the
                    // top is just the gap (counting HEADER_HEIGHT twice
                    // pushes the whole grid down and clips its bottom).
                    .top(px(layout.card_top))
                    .right(px(layout.card_right))
                    // Fixed bottom: any floor slack stays inside the card
                    // on the grid's own background, invisible.
                    .bottom(px(layout.card_bottom))
                    .flex()
                    .flex_col()
                    .bg(card_bg)
                    .overflow_hidden();
                // The opaque window keeps the floating card's hairline
                // and rounding; a translucent terminal runs frameless to
                // the window edges.
                if framed {
                    card = card
                        .border_1()
                        .border_color(terminal_inset_border())
                        .rounded_md();
                }
                card.child(self.card_content(cx, grid, &layout))
            }))
            // Chrome layers live at the root so they paint above the content
            // and the sidebar: the menu overlay must never slide beneath it.
            .child(sidebar)
            .child(self.top_chrome(cx, cell_width, fullscreen))
            .child(self.session_menu(cx, cell_width, fullscreen))
            .child(self.palette_overlay(cx))
            .child(self.picker_overlay(window, cx))
            .child(self.context_menu_overlay(cx, window))
            .child(self.menu_prompt_overlay(cx))
    }
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
    max_cols: u16,
    max_rows: u16,
    glass: f32,
    slack_x: f32,
    slack_y: f32,
    text_alpha: f32,
) -> Div {
    let end_row = SKIPPED_DAEMON_ROWS
        .saturating_add(max_rows)
        .min(frame.height.saturating_sub(SKIPPED_DAEMON_BOTTOM_ROWS));
    let mut column = div().flex().flex_col().size_full();
    for row in SKIPPED_DAEMON_ROWS..end_row.max(SKIPPED_DAEMON_ROWS) {
        column = column.child(render_row(
            frame,
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
            text_alpha,
        ));
    }
    // Full-bleed pools the sub-cell remainder below the last row; extend
    // that row's colors over it so no card-colored sliver shows. Always emit
    // it when a row painted: a zero or sub-pixel remainder still has to keep
    // the transparent card covered at fractional window heights.
    if end_row > SKIPPED_DAEMON_ROWS {
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
        ));
    }
    column
}

/// Paint one grid row, skipping the dock columns our sidebar covers.
#[allow(clippy::too_many_arguments)]
fn render_row(
    frame: &FrameData,
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
    text_alpha: f32,
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
    for (start_column, run_cells, bg) in
        collect_background_runs(frame.width, &rendered, theme, row, selection)
    {
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

    let mut blocks = BlockBands::default();
    let mut box_rects = BoxRects::default();
    let mut box_curves: Vec<(box_drawing::Curve, gpui::Rgba)> = Vec::new();
    let light_stroke = box_drawing::light_stroke(font_size);
    for (start_column, run_cells, text, (fg, _bg, mods)) in
        collect_runs(frame.width, &rendered, theme, row, selection)
    {
        if let Some((rects, shade)) = block_element(&text) {
            let column = f32::from(start_column.saturating_sub(skip_cols));
            for rect in rects {
                blocks.add(column, rect, fg.opacity(text_alpha * shade));
            }
            continue;
        }
        if let Some(c) = box_drawing::box_char(&text) {
            let column = f32::from(start_column.saturating_sub(skip_cols));
            let left = (column * cell_width).round();
            let right = ((column + 1.0) * cell_width).round();
            let color = fg.opacity(text_alpha);
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
            .text_color(fg.opacity(text_alpha))
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
) -> Div {
    let spans = bottom_fill_spans(frame, row, theme, selection, skip_cols, max_cols);
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
                .bg(bg.opacity(glass)),
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

/// Text opacity `step` of the way through a fade up from `floor`: quick off
/// the floor, gentle into full.
fn reflow_opacity(floor: f32, step: f32) -> f32 {
    let eased = 1.0 - (1.0 - step.clamp(0.0, 1.0)).powi(3);
    floor + (1.0 - floor) * eased
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
) -> Vec<PinnedRun> {
    let selected = |column: u16| selection.is_some_and(|s| s.contains(column, row));
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < rendered.len() {
        let start_column = rendered[index].0;
        let style = style_of(
            rendered[index].1,
            theme,
            rendered[index].2,
            selected(start_column),
        );
        let start_flowable = is_flowable(symbol(rendered[index].1));
        let mut text = String::new();
        let mut next = index;
        while next < rendered.len() {
            let (column, cell, cursor) = rendered[next];
            if style_of(cell, theme, cursor, selected(column)) != style {
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
    let bg_at = |column: u16, cell: &CellData, cursor: bool| {
        style_of(
            cell,
            theme,
            cursor,
            selection.is_some_and(|s| s.contains(column, row)),
        )
        .1
    };
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < rendered.len() {
        let (start_column, cell, cursor) = rendered[index];
        let bg = bg_at(start_column, cell, cursor);
        let mut next = index + 1;
        while next < rendered.len() {
            let (column, cell, cursor) = rendered[next];
            if bg_at(column, cell, cursor) != bg {
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

fn style_of(
    cell: &CellData,
    theme: Theme,
    cursor: bool,
    selected: bool,
) -> (gpui::Rgba, gpui::Rgba, u16) {
    let mut fg = theme::resolve(cell.fg, theme.fg, &theme.ansi);
    let mut bg = theme::resolve(cell.bg, theme.bg, &theme.ansi);
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

/// The terminal card's frame, independent of the color scheme: a low-contrast
/// hairline so the inset reads subtly against any theme.
fn terminal_inset_border() -> gpui::Rgba {
    gpui::rgb(0x24242c)
}

/// Tint alpha for `blur` (0.0..=1.0): the constant glass floor at full
/// blur, deepening toward [`FROST_MAX`] more as the backdrop sharpens — a
/// sharper desktop fights grid text more, so it asks for a deeper tint.
fn coat_tint(blur: f32) -> f32 {
    GLASS_TINT + (1.0 - blur.clamp(0.0, 1.0)) * FROST_MAX
}

/// The window's glass coat, painted behind every surface: a tint gradient
/// deepening toward the bottom plus a sheen band under the top edge, which
/// is what turns the frosted backdrop into a lit pane. The framed (opaque)
/// window only needs its flat base under the card margins.
fn glass_coat(theme: Theme, blur: f32, framed: bool) -> Div {
    if framed {
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
        .rounded_md()
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
        .rounded_lg()
        .bg(theme.bg)
        .border_1()
        .border_color(terminal_inset_border())
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
        .rounded_md()
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

/// A picker's type-to-filter field: the query input beside the count of
/// what it keeps. The field is interactive content, so it takes
/// `theme.border` rather than the floating panel's hairline.
fn picker_filter_field(
    theme: Theme,
    font_size: f32,
    count: String,
    input: Entity<TextInput>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(PICKER_FIELD_H))
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(input)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(font_size))
                .text_color(theme.muted)
                .child(count),
        )
}

/// The exact pixel height of `rows` menu rows, so a menu that opens upward
/// can offset by its full height.
fn picker_rows_h(rows: usize) -> f32 {
    // `p_1` top and bottom, `PICKER_ROW_H` rows, `gap_1` between them.
    PICKER_PAD * 2.0 + rows as f32 * PICKER_ROW_H + rows.saturating_sub(1) as f32 * PICKER_GAP
}

/// The exact pixel height of a picker's virtualized option list.
///
/// Its rows pack without the gap a stacked menu puts between them — a
/// virtualized list has no stacking to gap — which buys back the space a
/// menu spends on the rows above.
fn picker_body_h(rows: usize) -> f32 {
    rows as f32 * PICKER_ROW_H
}

/// The exact pixel height of a composer picker menu: its padding, the
/// filter field when the list is long enough to need one, and `rows`
/// option rows.
fn picker_menu_h(rows: usize, filtered: bool) -> f32 {
    let field = if filtered {
        PICKER_FIELD_H + PICKER_GAP
    } else {
        0.0
    };
    PICKER_PAD * 2.0 + field + picker_body_h(rows)
}

/// The leading glyph of one sidebar context-menu row.
fn sidebar_menu_icon(item: SidebarItem) -> &'static [u8] {
    match item {
        SidebarItem::Close => include_bytes!("../../assets/icons/x.svg"),
        SidebarItem::Rename => include_bytes!("../../assets/icons/pencil.svg"),
        SidebarItem::Pin | SidebarItem::Unpin => include_bytes!("../../assets/icons/pin.svg"),
        SidebarItem::TogglePaths => include_bytes!("../../assets/icons/folder.svg"),
        SidebarItem::QuickActions => include_bytes!("../../assets/icons/chevron-right.svg"),
        SidebarItem::CopyPath | SidebarItem::CopyBranch => {
            include_bytes!("../../assets/icons/copy.svg")
        }
        SidebarItem::NewWorktree => include_bytes!("../../assets/icons/git-branch.svg"),
        SidebarItem::DeleteWorktree => include_bytes!("../../assets/icons/trash-2.svg"),
        SidebarItem::OpenGit => include_bytes!("../../assets/icons/folder-git.svg"),
        SidebarItem::OpenBoard => include_bytes!("../../assets/icons/list-checks.svg"),
        SidebarItem::OpenMission => include_bytes!("../../assets/icons/gauge.svg"),
        SidebarItem::ToggleScope => include_bytes!("../../assets/icons/monitor.svg"),
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
        ("amp", include_bytes!("../../assets/icons/agents/amp.svg")),
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
            .rounded_md()
            .border_1()
            .border_color(terminal_inset_border())
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

/// Outcome of a background `luvus server start` for a fresh session.
enum SessionOpResult {
    Started(String),
    StartFailed(String, String),
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

/// Run one `luvus machine …` mutation, returning the error to show.
fn run_machine_cli(root: &Path, args: Vec<String>) -> Result<(), String> {
    let Some(binary) = luvus_binary() else {
        return Err("luvus binary not found on PATH".to_string());
    };
    match std::process::Command::new(binary)
        .arg("machine")
        .args(&args)
        .env(HOME_ENV, root)
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                "The command failed.".to_string()
            } else {
                stderr
            })
        }
        Err(error) => Err(error.to_string()),
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

/// A tab's label: its abbreviated workspace cwd plus the `⌘N` shortcut. The
/// cwd is capped before the shortcut is appended, so `⌘N` is never truncated
/// away.
fn tab_label(cwd: &str, index: u16) -> String {
    format!("{} ⌘{index}", truncate_label(&abbreviate_path(cwd), 24))
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

/// A muted placeholder row for the AGENT CONFIG section.
fn agent_config_empty(theme: Theme, message: &str, text_size: f32) -> Div {
    div()
        .mx_3()
        .px_3()
        .py_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_size(px(text_size))
        .text_color(theme.muted)
        .child(message.to_string())
}

/// Title-case the daemon's lowercase status (`working` -> `Working`).
fn status_label(status: &str) -> String {
    let mut chars = status.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Record the snapshot's focused agent under its workspace and tab so a
/// pilus-initiated `focus_workspace`/`focus_tab` can predict the pane the
/// daemon restores, a reconcile before `agent.list` reports it. A group
/// whose focused pane is a plain shell flags no agent, so its memory
/// clears — the daemon restores that shell, not a stale pick.
fn remember_focus(
    agents: &[Agent],
    workspaces: &[ShellWorkspace],
    tabs: &[Tab],
    active: &EndpointId,
    workspace_focus: &mut HashMap<(EndpointId, String), Agent>,
    tab_focus: &mut HashMap<(EndpointId, String, u16), Agent>,
) {
    if let Some(agent) = agents.iter().find(|agent| agent.focused) {
        workspace_focus.insert((active.clone(), agent.workspace_id.clone()), agent.clone());
        tab_focus.insert(
            (active.clone(), agent.workspace_id.clone(), agent.tab),
            agent.clone(),
        );
        return;
    }
    let Some(workspace) = workspaces
        .iter()
        .find(|workspace| workspace.active)
        .map(|workspace| workspace.id.clone())
    else {
        return;
    };
    workspace_focus.remove(&(active.clone(), workspace.clone()));
    if let Some(tab) = tabs.iter().find(|tab| tab.active) {
        tab_focus.remove(&(active.clone(), workspace, tab.index));
    }
}

/// The pending pilus-initiated pi pick, while it still stands: dropped
/// once the snapshot shows its pane focused, once the active endpoint
/// moves away (its pane id could collide there), or once its window
/// lapses — the daemon may never confirm a guess that was wrong.
fn optimistic_pick(
    agents: &[Agent],
    active: &EndpointId,
    optimistic: &mut Option<(PiView, Agent, Instant)>,
) -> Option<(PiView, Agent)> {
    let (view, agent, at) = optimistic.as_ref()?;
    let confirmed = view.endpoint == *active
        && agents
            .iter()
            .find(|agent| agent.focused)
            .is_some_and(|agent| agent.pane == view.pane);
    if confirmed || view.endpoint != *active || at.elapsed() >= PI_OPTIMISTIC_WINDOW {
        *optimistic = None;
        return None;
    }
    Some((view.clone(), agent.clone()))
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
) -> Result<(String, Option<Agent>), String> {
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
    // The pane is known before `agent.list` lists the agent: a record made
    // now lets the pin follow the launch instead of waiting a reconcile.
    let launched = pane.map(|pane| Agent {
        pane,
        agent: agent.to_string(),
        name: name.to_string(),
        cwd: location.to_string(),
        ..Agent::default()
    });
    Ok((format!("started {agent} as {name}"), launched))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blank cell carrying only attribute bits.
    fn cell(mods: u16) -> CellData {
        CellData {
            mods,
            ..CellData::default()
        }
    }

    fn agent(agent: &str, focused: bool, session: Option<&str>) -> Agent {
        Agent {
            pane: "7".to_string(),
            agent: agent.to_string(),
            focused,
            session: session.map(str::to_string),
            ..Agent::default()
        }
    }

    #[test]
    fn pi_view_needs_auto_mode_and_a_focused_pi() {
        let endpoint = EndpointId::Local;
        let focused = vec![agent("pi", true, Some("s1"))];
        // Auto off always declines, even with a live report in front.
        assert_eq!(
            PilusView::pi_view_for(&focused, false, &endpoint, None),
            None
        );
        // A non-pi pane stays terminal.
        let non_pi = vec![agent("claude", true, Some("s1"))];
        assert_eq!(PilusView::pi_view_for(&non_pi, true, &endpoint, None), None);
        // An unfocused pi is nobody's foreground.
        let background = vec![agent("pi", false, Some("s1"))];
        assert_eq!(
            PilusView::pi_view_for(&background, true, &endpoint, None),
            None
        );
    }

    #[test]
    fn a_split_pane_maps_onto_whole_cells_and_meets_the_card_edges() {
        let layout = CardLayout {
            card_x: 0.0,
            card_top: 0.0,
            card_right: 0.0,
            card_bottom: 0.0,
            pad_x: 6.0,
            pad_y: 4.0,
            origin_x: 0.0,
            origin_y: 0.0,
            cols: 80,
            rows: 24,
            cell_w: 8.0,
            cell_h: 16.0,
            slack_x: 0.0,
            slack_y: 0.0,
        };
        let right_half = PaneFrame {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let place = pane_place(right_half, &layout);
        assert_eq!(place.left, 6.0 + 40.0 * 8.0);
        assert_eq!(place.top, 0.0);
        // Its right and bottom edges are the card's.
        assert_eq!((place.width, place.height), (None, None));
        let top_left = PaneFrame {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        let place = pane_place(top_left, &layout);
        assert_eq!((place.left, place.top), (0.0, 0.0));
        assert_eq!(place.width, Some(6.0 + 40.0 * 8.0));
        assert_eq!(place.height, Some(4.0 + 12.0 * 16.0));
    }

    #[test]
    fn a_split_pi_pane_is_blanked_under_its_panel() {
        let mut frame = FrameData::blank(6, SKIPPED_DAEMON_ROWS + 2, 0x00ff_ffff, 0);
        for cell in &mut frame.cells {
            cell.symbol = "x".to_string();
        }
        // Right half of a 4-column visible grid behind a 2-column dock.
        let cells = pane_cells(
            PaneFrame {
                x: 0.5,
                y: 0.0,
                width: 0.5,
                height: 1.0,
            },
            4,
            2,
        );
        let masked = mask_pane(&frame, cells, 2);
        let row = usize::from(SKIPPED_DAEMON_ROWS) * 6;
        let symbols: Vec<&str> = masked.cells[row..row + 6].iter().map(symbol).collect();
        assert_eq!(symbols, ["x", "x", "x", "x", " ", " "]);
    }

    #[test]
    fn pi_view_falls_back_to_a_pi_sharing_the_visible_tab() {
        let endpoint = EndpointId::Local;
        let mut split = agent("pi", false, Some("s1"));
        split.workspace_id = "w1".to_string();
        split.tab = 2;
        let agents = vec![split];
        // Beside the focused shell in the visible tab, the pi is a split.
        let Some((view, agent)) = PilusView::pi_view_for(&agents, true, &endpoint, Some(("w1", 2)))
        else {
            panic!("a pi in the visible tab is on screen");
        };
        assert_eq!(view.pane, "7");
        assert!(!agent.focused);
        // Another tab or workspace hides it.
        assert_eq!(
            PilusView::pi_view_for(&agents, true, &endpoint, Some(("w1", 1))),
            None
        );
        assert_eq!(
            PilusView::pi_view_for(&agents, true, &endpoint, Some(("w2", 2))),
            None
        );
    }

    #[test]
    fn pi_view_reports_the_focused_pane() {
        let endpoint = EndpointId::Local;
        let agents = vec![agent("pi", true, Some("s1"))];
        let Some((view, _)) = PilusView::pi_view_for(&agents, true, &endpoint, None) else {
            panic!("a focused, reporting pi must be chosen");
        };
        assert_eq!(view.pane, "7");
        assert_eq!(view.session.as_deref(), Some("s1"));
        assert_eq!(view.endpoint, endpoint);
    }

    #[test]
    fn pi_view_pins_a_pi_that_has_not_reported() {
        // A focused pi without a report still pins: the waiting panel is
        // what keeps the terminal from flashing before the report lands.
        let endpoint = EndpointId::Local;
        let agents = vec![agent("pi", true, None)];
        let Some((view, _)) = PilusView::pi_view_for(&agents, true, &endpoint, None) else {
            panic!("a focused pi pins even without a session");
        };
        assert_eq!(view.pane, "7");
        assert_eq!(view.session, None);
    }

    fn workspace(id: &str, active: bool) -> ShellWorkspace {
        ShellWorkspace {
            dot: String::new(),
            dot_color: 0,
            id: id.to_string(),
            index: 0,
            name: String::new(),
            cwd: String::new(),
            branch: None,
            active,
            selected: false,
            nested: false,
        }
    }

    #[test]
    fn focus_memory_records_workspace_and_tab() {
        let endpoint = EndpointId::Local;
        let mut workspaces = HashMap::new();
        let mut tabs = HashMap::new();
        let mut pi = agent("pi", true, Some("s1"));
        pi.workspace_id = "w1".to_string();
        pi.tab = 2;
        remember_focus(
            &[pi.clone()],
            &[],
            &[],
            &endpoint,
            &mut workspaces,
            &mut tabs,
        );
        assert_eq!(
            workspaces.get(&(endpoint.clone(), "w1".to_string())),
            Some(&pi)
        );
        assert_eq!(tabs.get(&(endpoint, "w1".to_string(), 2)), Some(&pi));
    }

    #[test]
    fn focus_memory_clears_a_group_focused_on_a_shell() {
        let endpoint = EndpointId::Local;
        let mut workspaces = HashMap::new();
        let mut tabs = HashMap::new();
        let mut pi = agent("pi", false, None);
        pi.workspace_id = "w1".to_string();
        pi.tab = 2;
        workspaces.insert((endpoint.clone(), "w1".to_string()), pi.clone());
        tabs.insert((endpoint.clone(), "w1".to_string(), 2), pi.clone());
        // An unfocused agent means the active group's focused pane is a
        // shell; both memories drop their entries for it.
        remember_focus(
            &[pi],
            &[workspace("w1", true)],
            &[Tab {
                index: 2,
                active: true,
                ..Tab::default()
            }],
            &endpoint,
            &mut workspaces,
            &mut tabs,
        );
        assert!(workspaces.is_empty());
        assert!(tabs.is_empty());
    }

    #[test]
    fn optimistic_pick_stands_until_the_snapshot_confirms() {
        let endpoint = EndpointId::Local;
        let mut pi = agent("pi", false, Some("s1"));
        pi.workspace_id = "w1".to_string();
        let view = PiView {
            endpoint: endpoint.clone(),
            pane: pi.pane.clone(),
            session: pi.session.clone(),
        };
        let mut optimistic = Some((view.clone(), pi.clone(), Instant::now()));
        // The daemon still shows another pane focused: the pick stands.
        let mut other = agent("claude", true, None);
        other.pane = "9".to_string();
        let pending = vec![pi.clone(), other.clone()];
        assert_eq!(
            optimistic_pick(&pending, &endpoint, &mut optimistic).map(|(v, _)| v.pane),
            Some("7".to_string())
        );
        // The next snapshot focuses the pick: the pin resolves and drops.
        let confirmed = vec![agent("pi", true, Some("s1")), other];
        assert_eq!(
            optimistic_pick(&confirmed, &endpoint, &mut optimistic),
            None
        );
        assert!(optimistic.is_none());
    }

    #[test]
    fn optimistic_pick_drops_on_endpoint_drift_and_age() {
        let local = EndpointId::Local;
        let machine = EndpointId::Machine("m1".to_string());
        let pi = agent("pi", false, None);
        let view = PiView {
            endpoint: local.clone(),
            pane: pi.pane.clone(),
            session: None,
        };
        // A pick made for one endpoint cannot survive a switch to another.
        let mut optimistic = Some((view.clone(), pi.clone(), Instant::now()));
        assert_eq!(optimistic_pick(&[], &machine, &mut optimistic), None);
        assert!(optimistic.is_none());
        // A pick the daemon never confirms expires with its window.
        let mut optimistic = Some((
            view,
            pi,
            Instant::now() - PI_OPTIMISTIC_WINDOW - Duration::from_secs(1),
        ));
        assert_eq!(optimistic_pick(&[], &local, &mut optimistic), None);
        assert!(optimistic.is_none());
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
    fn reflowed_text_fades_up_from_the_floor() {
        assert!((reflow_opacity(REFLOW_FLOOR, 0.0) - REFLOW_FLOOR).abs() < f32::EPSILON);
        assert!((reflow_opacity(SWAP_FLOOR, 1.0) - 1.0).abs() < f32::EPSILON);
        let mut last = reflow_opacity(SWAP_FLOOR, 0.0);
        for step in 1..=10 {
            let next = reflow_opacity(SWAP_FLOOR, step as f32 / 10.0);
            assert!(next >= last, "the fade never dips back");
            last = next;
        }
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
            collect_runs(frame.width, &rendered, theme, 0, None).len(),
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
        let runs = collect_runs(frame.width, &rendered, theme, 0, None);
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
    fn tab_label_keeps_cwd_and_shortcut() {
        assert_eq!(tab_label("~/work/pilus", 3), "~/w/pilus ⌘3");
        assert_eq!(tab_label("~", 1), "~ ⌘1");
        let long = tab_label("~/work/a-directory-name-that-will-not-fit", 7);
        assert!(long.ends_with("… ⌘7"));
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
        assert_eq!(
            top_layer(false, false, true, false, false, false),
            InputLayer::Home
        );
        assert_eq!(
            top_layer(false, false, false, true, false, false),
            InputLayer::Settings
        );
        assert_eq!(
            top_layer(false, false, false, false, false, true),
            InputLayer::Session
        );
        // ...but an overlay opened above one takes over.
        assert_eq!(
            top_layer(false, true, true, false, false, false),
            InputLayer::Palette
        );
        assert_eq!(
            top_layer(false, true, false, true, false, false),
            InputLayer::Palette
        );
        // The folder picker is the topmost overlay.
        assert_eq!(
            top_layer(true, true, true, true, false, true),
            InputLayer::Picker
        );
        // With nothing open, the grid has it.
        assert_eq!(
            top_layer(false, false, false, false, false, false),
            InputLayer::Terminal
        );
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
        // would shadow an earlier shortcut, so catch that here too.
        let mut seen = HashSet::new();
        for binding in app_key_bindings() {
            let chord = format!("{:?}", binding.keystrokes());
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
        let runs = collect_runs(width, &rendered, theme, 0, None);
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
