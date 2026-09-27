//! The GPUI desktop window.
//!
//! The window is a thin, native shell over one daemon connection: it forwards
//! input and resizes and paints the server's rendered cell grid. The daemon
//! still owns layout, but this client claims the workspaces dock so it can draw
//! that one panel natively, and selects text over the grid itself.

mod about;
mod app_icon;
mod appearances;
mod keys;
mod scheme;
mod selection;
mod settings;
mod theme;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton as TerminalMouseButton, MouseEvent,
    MouseEventKind,
};
use gpui::{
    App, Bounds, ClipboardItem, Context, Div, FocusHandle, FontWeight, Hsla, KeyBinding,
    KeyDownEvent, Menu, MenuItem, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, TextRun, TitlebarOptions,
    Window, WindowBounds, WindowOptions, div, font, point, prelude::*, px, size, svg,
};
use gpui_platform::application;
use l3s_client::{
    Agent, AgentFeed, Appearance, AutomationSummary, BarWidget, ClientEvent, ConnectTarget,
    Connection, ControlClient, MachineInfo, SessionInfo, Surface, SurfaceChange, Tab, TaskSummary,
    config_dir, generate_session_name, list_machines, list_session_infos, refresh_machines,
    split_client_socket,
};
use l3s_protocol::{CellData, ClientMessage, FrameData, ShellSidebars, ShellWorkspace};
use selection::{Cell, Selection};
use settings::Settings;
use theme::Theme;

/// Monospace family used for the cell grid.
const FONT_FAMILY: &str = "Menlo";
/// Base font size in logical pixels.
const FONT_SIZE: f32 = 13.0;
/// Line height as a fraction of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Height of our native tab strip; the grid starts below it.
const TAB_STRIP_HEIGHT: f32 = 36.0;
/// Grid offset from the window top: tab strip plus its top spacing.
const HEADER_HEIGHT: f32 = 40.0;
/// Daemon rows skipped at the top of the grid. The daemon draws its own tab
/// strip plus a cwd breadcrumb there; the grid starts below them so only
/// native chrome shows. (Our footer already carries the cwd.) Selection and
/// click coordinates stay in daemon rows, so copy and gear clicks keep
/// working.
const SKIPPED_DAEMON_ROWS: u16 = 2;
/// Daemon rows skipped at the grid bottom: its one-row status line, which our
/// footer widgets already mirror.
const SKIPPED_DAEMON_BOTTOM_ROWS: u16 = 1;
/// Daemon rows skipped at the bottom: its status line, mirrored by our own
/// footer widgets. Assumes a single-row status line.
/// Height of the native footer bar.
const FOOTER_HEIGHT: f32 = 28.0;
/// Gap around the floating terminal card, on top/left/bottom.
const CARD_GAP: f32 = 8.0;
/// Slides the sidebar's first row down so it lines up with the top of the
/// inset terminal card. The list's own `gap_1` already covers the rest of the
/// `CARD_GAP` between the header and the card.
const SIDEBAR_TOP_INSET: f32 = 4.0;
/// Breathing room below the last row is intentionally zero: the grid runs
/// flush to the card's bottom border.
/// Breathing room above the first row, mirroring the bottom.
const TERMINAL_TOP_AIR: f32 = 6.0;
/// Wider right margin on the card, for breathing room at the window edge.
const CARD_GAP_RIGHT: f32 = 16.0;
/// Height of a tab pill in the top chrome.
const TAB_HEIGHT: f32 = 26.0;
/// Left inset for the collapsed tab strip: clears the floating traffic lights
/// (the third light ends near 64px) with a comfortable gap before the toggle.
const TRAFFIC_LIGHT_CLEARANCE: f32 = 92.0;
/// Chrome text size; the cell grid keeps its own monospace size.
const CHROME_FONT: f32 = 13.0;
/// Section label size (SESSIONS / PROJECTS).
const SECTION_FONT: f32 = 11.0;

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
    /// Quit the application.
    QuitApp,
    /// Copy the current grid selection.
    CopySelection,
    /// Paste the system clipboard into the pane.
    PasteClipboard,
    /// Minimize the active window.
    MinimizeWindow,
    /// Zoom (maximize) the active window.
    ZoomWindow,
    /// Close the active window.
    CloseWindow,
    /// Select the whole visible grid.
    SelectAllGrid,
    /// Open another l3s window attached to the same daemon.
    NewWindow,
    /// Open a new daemon tab.
    NewTab,
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

/// Open the desktop window and attach to `target`.
pub fn run(target: ConnectTarget) -> anyhow::Result<()> {
    // The real viewport is not known until a window exists; the first resize
    // after the window opens corrects these provisional dimensions.
    let connection = l3s_client::connect(&target, 120, 40)?;
    // Claim the workspaces dock before the window paints, so the daemon blanks
    // its own copy and starts projecting the catalog to us.
    connection.request_workspace_dock()?;
    let appearances_socket = api_socket_for_target(&target);
    let config_root = config_dir();
    let new_window_target = target.clone();
    let settings = Rc::new(RefCell::new(Settings::load(&config_root)));

    application().run(move |cx: &mut App| {
        app_icon::install();
        cx.on_action({
            let api = appearances_socket.clone();
            let settings = settings.clone();
            let config_root = config_root.clone();
            move |_: &OpenAppearances, cx: &mut App| {
                appearances::open(api.clone(), settings.clone(), config_root.clone(), cx)
            }
        });
        cx.on_action(|_: &QuitApp, cx: &mut App| cx.quit());
        // Enables the menu item; the update flow is not wired up yet.
        cx.on_action(|_: &CheckForUpdates, _cx: &mut App| {});
        cx.on_action(|_: &MinimizeWindow, cx: &mut App| {
            if let Some(window) = cx.active_window() {
                window
                    .update(cx, |_, window, _| window.minimize_window())
                    .ok();
            }
        });
        cx.on_action(|_: &ZoomWindow, cx: &mut App| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, _| window.zoom_window()).ok();
            }
        });
        cx.on_action(|_: &CloseWindow, cx: &mut App| {
            if let Some(window) = cx.active_window() {
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        });
        cx.on_action({
            let target = new_window_target.clone();
            let settings = settings.clone();
            move |_: &NewWindow, cx: &mut App| {
                open_main_window(target.clone(), settings.clone(), cx)
            }
        });
        cx.on_action(|_: &CloseAllWindows, cx: &mut App| {
            for window in cx.windows() {
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        });
        cx.on_action({
            let settings = settings.clone();
            let root = config_root.clone();
            move |_: &IncreaseFontSize, cx: &mut App| bump_font(&settings, &root, 1.0, cx)
        });
        cx.on_action({
            let settings = settings.clone();
            let root = config_root.clone();
            move |_: &DecreaseFontSize, cx: &mut App| bump_font(&settings, &root, -1.0, cx)
        });
        cx.on_action({
            let settings = settings.clone();
            let root = config_root.clone();
            move |_: &ResetFontSize, cx: &mut App| bump_font(&settings, &root, 0.0, cx)
        });
        cx.bind_keys(app_key_bindings());
        cx.set_menus([
            Menu::new("l3s").items([
                MenuItem::action("About l3s", OpenAbout),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Appearances…", OpenAppearances),
                MenuItem::separator(),
                MenuItem::action("Quit l3s", QuitApp),
            ]),
            Menu::new("File").items([
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("New Tab", NewTab),
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
        let bounds = Bounds::centered(None, size(px(1240.0), px(780.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(640.0), px(400.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("l3s".into()),
                    appears_transparent: true,
                    // Centred on the sidebar toggle icon box (22px from the
                    // window top) and left-aligned with the sidebar nav glyphs
                    // (12px panel padding + 8px row padding).
                    traffic_light_position: Some(point(px(20.0), px(15.0))),
                }),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(move |cx| L3sView::new(target, connection, settings.clone(), window, cx))
            },
        );
        if let Err(error) = opened {
            eprintln!("l3s: could not open a window: {error}");
            return;
        }
        cx.activate(true);
    });

    Ok(())
}

/// Every application-level keybinding, in one place so a duplicate chord is a
/// test failure rather than a shadowed shortcut.
fn app_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-,", OpenAppearances, None),
        KeyBinding::new("cmd-q", QuitApp, None),
        KeyBinding::new("cmd-c", CopySelection, None),
        KeyBinding::new("cmd-v", PasteClipboard, None),
        KeyBinding::new("cmd-a", SelectAllGrid, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-n", NewWindow, None),
        KeyBinding::new("cmd-t", NewTab, None),
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

/// Open another l3s window on the same target.
///
/// Connection setup includes a handshake, so it briefly blocks; this only runs
/// on an explicit File → New Window.
fn open_main_window(target: ConnectTarget, settings: Rc<RefCell<Settings>>, cx: &mut App) {
    let connection = match l3s_client::connect(&target, 120, 40) {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("l3s: could not open another window: {error}");
            return;
        }
    };
    let _ = connection.request_workspace_dock();
    let bounds = Bounds::centered(None, size(px(1240.0), px(780.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(640.0), px(400.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("l3s".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(20.0), px(15.0))),
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(move |cx| L3sView::new(target, connection, settings, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("l3s: could not open another window: {error}");
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

/// Geometry of the inset terminal card: grid origin, visible cells, and the
/// measurements behind them, so painting, pointer mapping, and the daemon
/// viewport stay pixel-true to one another.
struct CardLayout {
    /// Card's left edge in window pixels (sidebar plus gap, or just gap).
    card_x: f32,
    /// First visible cell's top-left corner in window pixels.
    origin_x: f32,
    origin_y: f32,
    /// Visible grid size in cells.
    cols: u16,
    rows: u16,
    /// Measured cell size in pixels.
    cell_w: f32,
    cell_h: f32,
}

struct L3sView {
    target: ConnectTarget,
    connection: Connection,
    surface: Surface,
    theme: Theme,
    focus_handle: FocusHandle,
    /// Font choices, shared with the Appearances window.
    settings: Settings,
    shared_settings: Rc<RefCell<Settings>>,
    cols: u16,
    rows: u16,
    status: String,
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
    /// Background poller for the current session's tabs and widgets.
    agent_feed: Option<AgentFeed>,
    /// State root the SESSIONS section lists, derived from the launch target.
    session_root: PathBuf,
    /// Current session; `None` is the default session.
    session_name: Option<String>,
    /// Visible sessions under `session_root`.
    sessions: Vec<SessionInfo>,
    /// Last session-directory poll, so the disk isn't hit every frame.
    last_session_poll: Instant,
    /// Whether the footer session menu is open.
    session_menu_open: bool,
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
    /// Registered appearance themes and the selected id, from `theme.list`.
    appearance: Appearance,
    /// Last theme poll; the registry only changes on user action.
    last_theme_poll: Instant,
    /// A theme refresh is running off-thread; no overlapping spawns.
    theme_refreshing: bool,
    /// Fresh appearance snapshots from the background refresh.
    theme_rx: Receiver<l3s_client::Result<Appearance>>,
    /// Sender half for the background theme refresh.
    theme_tx: Sender<l3s_client::Result<Appearance>>,
    /// Optimistic nav pick: the row a click chose, before the daemon confirms.
    nav_sel: Option<NavKind>,
    /// When the optimistic pick was made, so stale picks age out.
    nav_sel_at: Instant,
    /// The sidebar layout we superseded the daemon's with, once known.
    sidebar_layout: Option<l3s_protocol::SidebarsConfig>,
    /// Whether workspace paths are shown in that layout.
    sidebar_workspace_paths: bool,
    /// Revision we last sent, so the daemon accepts each update.
    sidebar_revision: u64,
    /// Whether the native sidebar is expanded.
    sidebar_visible: bool,
}

impl L3sView {
    fn new(
        target: ConnectTarget,
        connection: Connection,
        shared_settings: Rc<RefCell<Settings>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // Tabs and widgets come from UHP, which is addressed by session. Find
        // the state root from the launch target, including explicit sockets,
        // so `--socket` builds still list sessions and feed the right tabs.
        let (session_root, session_name) = match &target.socket {
            Some(path) => split_client_socket(path).unwrap_or_else(|| (config_dir(), None)),
            None => (config_dir(), target.session.clone()),
        };
        let agent_feed = Some(AgentFeed::spawn(api_socket_for(
            &session_root,
            session_name.as_deref(),
        )));
        let sessions = list_session_infos(&session_root);
        let machines = list_machines(&session_root);
        let (machine_tx, machine_rx) = std::sync::mpsc::channel();
        let (session_tx, session_rx) = std::sync::mpsc::channel();
        let (theme_tx, theme_rx) = std::sync::mpsc::channel();
        let settings = shared_settings.borrow().clone();

        let view = Self {
            target,
            connection,
            surface: Surface::new(),
            theme: Theme::default(),
            focus_handle,
            settings,
            shared_settings,
            cols: 120,
            rows: 40,
            status: "connecting…".to_string(),
            cell_width: None,
            sent_cell: (0, 0),
            selection: None,
            dragging: false,
            drag_moved: false,
            press_anchor: None,
            hover_cell: None,
            agent_feed,
            session_root,
            session_name,
            sessions,
            last_session_poll: Instant::now(),
            session_menu_open: false,
            session_rx,
            session_tx,
            bar_widgets: Vec::new(),
            tabs: Vec::new(),
            agents: Vec::new(),
            automations: AutomationSummary::default(),
            tasks: TaskSummary::default(),
            machines,
            last_machine_poll: Instant::now() - Duration::from_secs(11),
            machine_refreshing: false,
            machine_rx,
            machine_tx,
            appearance: Appearance::default(),
            last_theme_poll: Instant::now() - Duration::from_secs(11),
            theme_refreshing: false,
            theme_rx,
            theme_tx,
            nav_sel: None,
            nav_sel_at: Instant::now(),
            sidebar_layout: None,
            sidebar_workspace_paths: false,
            sidebar_revision: 0,
            sidebar_visible: true,
        };

        cx.spawn(async move |this, cx| {
            loop {
                let updated = this.update(cx, |view, cx| {
                    let changed = view.sync_settings();
                    if view.poll() || changed {
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

        view
    }

    /// Drain pending connection events. Returns whether a repaint is needed.
    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.connection.try_recv() {
            match event {
                ClientEvent::Ready { .. } => {
                    self.status = self.connection.identity().session.clone();
                    changed = true;
                }
                ClientEvent::Message(message) => {
                    // Temporary: full-frame dimensions vs our viewport.
                    if let l3s_client::protocol::ServerMessage::Frame(frame) = &message {
                        eprintln!(
                            "l3s-frame {}x{} want {}x{} skip {}",
                            frame.width,
                            frame.height,
                            self.cols,
                            self.rows,
                            self.skipped_cols()
                        );
                    }
                    if self.surface.apply(&message) != SurfaceChange::None {
                        changed = true;
                    }
                    if let Some(text) = self.surface.take_notify() {
                        self.status = text;
                        changed = true;
                    }
                }
                ClientEvent::Disconnected { reason } => {
                    self.status = format!("disconnected · {reason}");
                    changed = true;
                }
            }
        }
        let mut tabs_changed = false;
        if let Some(feed) = self.agent_feed.as_ref() {
            while let Some(update) = feed.try_recv() {
                self.bar_widgets = update.bar;
                self.tabs = update.tabs;
                self.agents = update.agents;
                self.automations = update.automations;
                self.tasks = update.tasks;
                tabs_changed = true;
                changed = true;
            }
        }
        if tabs_changed {
            self.reconcile_nav();
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
            self.sidebar_workspace_paths = sidebars.workspace_paths;
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
        if !self.machine_refreshing && self.last_machine_poll.elapsed() >= Duration::from_secs(10) {
            self.machine_refreshing = true;
            self.last_machine_poll = Instant::now();
            let root = self.session_root.clone();
            let tx = self.machine_tx.clone();
            std::thread::spawn(move || {
                tx.send(refresh_machines(&root)).ok();
            });
        }
        while let Ok(machines) = self.machine_rx.try_recv() {
            self.machine_refreshing = false;
            if machines != self.machines {
                self.machines = machines;
                changed = true;
            }
        }
        // The theme registry changes only when the user selects one, and the
        // selection can also come from another window or the CLI, so a slow
        // poll keeps every l3s window on the same appearance.
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
        changed
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
    }

    /// Pull the shared font settings the Appearances window may have changed.
    fn sync_settings(&mut self) -> bool {
        let latest = self.shared_settings.borrow().clone();
        if latest == self.settings {
            return false;
        }
        self.settings = latest;
        // The cell advance depends on the terminal font, so re-measure and let
        // `sync_metrics` push the new size to the daemon.
        self.cell_width = None;
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Chords the native menu owns: consume them so they never reach the
        // pane as input. The keymap dispatches the matching action.
        if keystroke.modifiers.platform
            && matches!(
                keystroke.key.to_ascii_lowercase().as_str(),
                "," | "comma"
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
                    | "s"
                    | "t"
                    | "v"
                    | "m"
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
            let _ = self.connection.send(ClientMessage::Key(key));
        }
    }

    /// Send the system clipboard to the daemon, which applies the pane's own
    /// bracketed-paste rules.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if !text.is_empty() {
            let _ = self.connection.send(ClientMessage::Paste(text));
        }
    }

    /// Focus an agent's pane, switching workspace and tab as needed.
    fn focus_agent(&self, pane: &str) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.focus_pane(pane);
        }
    }

    /// Focus a workspace by id, switching the daemon's active workspace.
    fn focus_workspace(&mut self, workspace_id: &str) {
        let _ = self.connection.send(ClientMessage::ShellWorkspaceFocus {
            workspace_id: workspace_id.to_string(),
        });
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
        match l3s_client::connect(&self.target, self.cols, self.rows) {
            Ok(connection) => {
                self.connection = connection;
                let _ = self.connection.request_workspace_dock();
                self.surface = Surface::new();
                self.selection = None;
                self.hover_cell = None;
                self.press_anchor = None;
                self.dragging = false;
                self.sidebar_layout = None;
                self.sidebar_workspace_paths = false;
                self.sidebar_revision = 0;
                self.bar_widgets = Vec::new();
                self.tabs = Vec::new();
                self.agents = Vec::new();
                self.automations = AutomationSummary::default();
                self.tasks = TaskSummary::default();
                self.agent_feed = Some(AgentFeed::spawn(self.api_socket()));
                self.sessions = list_session_infos(&self.session_root);
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

    /// Send our sidebar layout to the daemon with a fresh revision.
    fn push_sidebar_layout(&mut self) {
        let Some(layout) = self.sidebar_layout.clone() else {
            return;
        };
        self.sidebar_revision = self.sidebar_revision.saturating_add(1);
        let _ = self
            .connection
            .send(ClientMessage::ShellSidebars(ShellSidebars {
                revision: self.sidebar_revision,
                layout,
                workspace_paths: self.sidebar_workspace_paths,
            }));
    }

    /// Collapse or expand the native sidebar, reclaiming the columns for panes.
    fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if let Some(layout) = self.sidebar_layout.as_mut() {
            layout.left.visible = self.sidebar_visible;
        }
        self.push_sidebar_layout();
    }

    /// Forward a synthetic left click at `cell` to the daemon.
    fn forward_click(&self, cell: Cell) {
        let modifiers = Modifiers::default();
        self.forward_mouse(
            MouseEventKind::Down(TerminalMouseButton::Left),
            cell,
            modifiers,
        );
        self.forward_mouse(
            MouseEventKind::Up(TerminalMouseButton::Left),
            cell,
            modifiers,
        );
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

    /// The chrome/base text size, following the user's UI font size.
    fn chrome_font(&self) -> f32 {
        self.settings.ui_font_size
    }

    /// The section-label size, following the base like the mock.
    fn section_font(&self) -> f32 {
        (self.settings.ui_font_size - (CHROME_FONT - SECTION_FONT)).max(8.0)
    }

    /// Secondary chrome text (footer, rows, badges), one step below the base.
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
        if self.session_menu_open {
            return true;
        }
        let x = f32::from(position.x);
        let y = f32::from(position.y);
        if y < HEADER_HEIGHT {
            return true;
        }
        if y >= f32::from(window.viewport_size().height) - FOOTER_HEIGHT {
            return true;
        }
        if self.sidebar_visible
            && let Some(dock) = self.surface.shell_dock()
        {
            let cell_width = self.cell_width(window);
            let left = f32::from(dock.x) * cell_width;
            let right = left + f32::from(dock.width) * cell_width;
            let bottom = f32::from(window.viewport_size().height) - FOOTER_HEIGHT;
            if x >= left && x < right && y < bottom {
                return true;
            }
        }
        // The gap ring around the floating card belongs to no pane.
        let layout = self.card_layout(window);
        let grid_right = layout.origin_x + f32::from(layout.cols) * layout.cell_w;
        let grid_bottom = layout.origin_y + f32::from(layout.rows) * layout.cell_h;
        if x < layout.origin_x || y < layout.origin_y || x >= grid_right || y >= grid_bottom {
            return true;
        }
        false
    }

    fn cell_at(&mut self, window: &Window, position: Point<Pixels>) -> Option<Cell> {
        let (width, height) = match self.surface.frame() {
            Some(frame) if frame.width > 0 && frame.height > 0 => (frame.width, frame.height),
            _ => return None,
        };
        let layout = self.card_layout(window);
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

    /// Send one mouse event to the daemon at `cell`.
    fn forward_mouse(&self, kind: MouseEventKind, cell: Cell, modifiers: Modifiers) {
        let _ = self.connection.send(ClientMessage::Mouse(MouseEvent {
            kind,
            column: cell.column,
            row: cell.row,
            modifiers: keys::modifiers(modifiers),
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
            return;
        };
        if self.hover_cell != Some(cell) {
            self.hover_cell = Some(cell);
            self.forward_mouse(MouseEventKind::Moved, cell, event.modifiers);
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
        if let Some(cell) = self.cell_at(window, event.position) {
            self.forward_mouse(MouseEventKind::Down(button), cell, event.modifiers);
        }
    }

    fn release(&mut self, event: &MouseUpEvent, button: TerminalMouseButton, window: &Window) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        if let Some(cell) = self.cell_at(window, event.position) {
            self.forward_mouse(MouseEventKind::Up(button), cell, event.modifiers);
        }
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &Window) {
        if self.hit_chrome(window, event.position) {
            return;
        }
        let Some(cell) = self.cell_at(window, event.position) else {
            return;
        };
        let cell_height = self.settings.terminal_font_size * LINE_HEIGHT_RATIO;
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
        let Some(frame) = self.surface.frame() else {
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

    /// Build the native sidebar: nav, sessions, and projects.
    ///
    /// The daemon leaves the dock cells blank and reports their exact rectangle
    /// in `ShellDock`; this paints the same region from the `ShellWorkspaces`
    /// catalog, so the panel is native while staying column-aligned with the
    /// terminal grid behind it.
    fn sidebar(&self, cx: &mut Context<Self>, cell_width: f32, bottom: f32) -> Div {
        if !self.sidebar_visible {
            return div();
        }
        let Some(dock) = self.surface.shell_dock() else {
            return div();
        };
        let theme = self.theme;

        let mut list = div()
            .flex()
            .flex_col()
            .flex_1()
            .px_3()
            .pt(px(SIDEBAR_TOP_INSET))
            .gap_1()
            .overflow_hidden()
            // Clearance for the floating traffic lights (transparent titlebar).
            .child(div().h(px(40.0)));

        for kind in NavKind::ALL {
            list = list.child(self.nav_row(kind, cx));
        }

        list = list.child(self.machines_section());
        list = list.child(self.agents_section(cx));
        list = list.child(self.projects_section(cx));

        // The panel starts on row 0, covering the daemon's `« session Menu`
        // chrome so the sidebar shows only native sections. It runs full
        // height down to the footer; the footer itself spans the full width
        // beneath it, VS Code style.
        div()
            .absolute()
            .left(px(f32::from(dock.x) * cell_width))
            .top(px(0.0))
            .w(px(f32::from(dock.width) * cell_width))
            .h(px(bottom))
            .flex()
            .flex_col()
            .bg(theme.window_bg)
            .overflow_hidden()
            .child(list)
    }

    /// The daemon's Menu-pill cell: scanned from the top rows, falling back
    /// to the dock's trailing cells when the label isn't visible (yet).
    ///
    /// Scanning (instead of trusting the dock rect) keeps the click working
    /// when hiding our sidebar reshuffles the daemon's chrome.
    fn menu_cell(&self) -> Option<Cell> {
        const LABEL: [&str; 4] = ["M", "e", "n", "u"];
        if let Some(frame) = self.surface.frame() {
            let width = usize::from(frame.width);
            for row in 0usize..3 {
                for start in 0..width.saturating_sub(LABEL.len()) {
                    let found = LABEL.iter().enumerate().all(|(offset, want)| {
                        frame
                            .cells
                            .get(row * width + start + offset)
                            .is_some_and(|cell| symbol(cell) == *want)
                    });
                    if found {
                        return Some(Cell {
                            column: start as u16 + 2,
                            row: row as u16,
                        });
                    }
                }
            }
        }
        let dock = self.surface.shell_dock()?;
        Some(Cell {
            column: dock.x + dock.width.saturating_sub(4),
            row: 0,
        })
    }

    /// Focus a workspace tab.
    fn focus_tab(&self, index: u16) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.focus_tab(index);
        }
    }

    /// Open a new tab in the active workspace.
    fn new_tab(&self) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.new_tab();
        }
    }

    /// Split the focused pane; `right` is side by side, `down` is stacked.
    fn split(&self, direction: &str) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.split(direction);
        }
    }

    /// Close the active daemon tab, falling back to the last known one.
    fn close_active_tab(&self) {
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .or_else(|| self.tabs.last());
        if let Some(tab) = tab {
            self.close_tab(tab.index);
        }
    }

    /// Close a workspace tab.
    fn close_tab(&self, index: u16) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.close_tab(index);
        }
    }

    /// The native tab strip, full window width: toggle beside the traffic
    /// lights, tabs, and settings at the far right.
    fn top_chrome(&self, cx: &mut Context<Self>, cell_width: f32) -> Div {
        let theme = self.theme;
        // Beside the sidebar when it is open; full width (clear of the
        // traffic lights) when it is hidden.
        let left = f32::from(self.sidebar_cells()) * cell_width;
        let pad = if self.sidebar_visible {
            12.0
        } else {
            TRAFFIC_LIGHT_CLEARANCE
        };
        let mut tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_1()
            .overflow_hidden();
        for tab in &self.tabs {
            tabs = tabs.child(self.tab_item(tab, cx));
        }
        tabs = tabs.child(
            div()
                .id("l3s-new-tab")
                .flex()
                .items_center()
                .justify_center()
                .h(px(TAB_HEIGHT))
                .w(px(28.0))
                .rounded_md()
                .cursor_pointer()
                .text_size(px(self.chrome_font()))
                .text_color(theme.muted)
                .hover(|style| style.text_color(theme.panel_fg).bg(theme.panel))
                .child(icon(
                    include_bytes!("../../assets/icons/plus.svg"),
                    13.0,
                    theme.muted,
                ))
                .on_click(cx.listener(|this, _, _, _| this.new_tab())),
        );
        let toggle = div()
            .id("l3s-sidebar-toggle")
            .flex()
            .items_center()
            .justify_center()
            .h(px(TAB_HEIGHT))
            .w(px(28.0))
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .child(icon(
                include_bytes!("../../assets/icons/panel-left.svg"),
                14.0,
                theme.muted,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_sidebar();
                cx.notify();
            }));
        let tab_strip = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(TAB_STRIP_HEIGHT))
            .pl(px(pad))
            .pr(px(8.0))
            .child(toggle)
            .child(tabs);
        div()
            .absolute()
            .left(px(left))
            .right(px(0.0))
            .top(px(4.0))
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .flex_col()
            .bg(theme.window_bg)
            .border_color(theme.border)
            .child(tab_strip)
    }

    fn tab_item(&self, tab: &Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = tab.active;
        let index = tab.index;
        let close_index = tab.index;
        let label = truncate_label(&tab.label(), 28);
        let group = format!("l3s-tab-{index}");
        let close = div()
            .id(format!("l3s-tab-close-{index}"))
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
            }));
        let fg = if active { theme.panel_fg } else { theme.muted };
        let mut tab = div()
            .id(format!("l3s-tab-{index}"))
            .group(group)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(TAB_HEIGHT))
            .min_w(px(100.0))
            .px_3()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .bg(if active { theme.panel } else { theme.window_bg })
            .text_color(fg)
            .hover(|style| style.bg(theme.panel).text_color(theme.panel_fg));
        if active {
            // Same low-contrast hairline as the terminal inset, so the active
            // tab reads as raised without borrowing the scheme's border.
            tab = tab.border_1().border_color(terminal_inset_border());
        }
        tab.child(div().flex_1().child(label))
            .child(close)
            .on_click(cx.listener(move |this, _, _, _| this.focus_tab(index)))
    }

    /// Open the daemon's Settings menu by clicking its Menu pill.
    fn click_menu(&self) {
        if let Some(cell) = self.menu_cell() {
            self.forward_click(cell);
        }
    }

    /// Daemon columns hidden beneath our sidebar: the dock the daemon blanks
    /// for us. Zero when our sidebar is hidden (the daemon draws its own
    /// there) or the dock sits elsewhere.
    fn skipped_cols(&self) -> u16 {
        if !self.sidebar_visible {
            return 0;
        }
        match self.surface.shell_dock() {
            Some(dock) if dock.x == 0 => dock.width,
            _ => 0,
        }
    }

    /// The sidebar's width in cells, or zero when it is hidden.
    fn sidebar_cells(&self) -> u16 {
        if self.sidebar_visible {
            self.surface.shell_dock().map_or(0, |dock| dock.width)
        } else {
            0
        }
    }

    /// Pixels the bottom chrome insets past the sidebar, when it is visible.
    /// The native bottom bar: session switcher on the left, widgets on right.
    ///
    /// The right side mirrors the daemon's own status-bar widgets (fetched
    /// via `ui.bar.list`). The session pill opens the session menu; the bar
    /// starts past the sidebar so it never hides beneath it.
    fn status_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let right = div().flex().flex_row().items_center().gap_2().child(
            div()
                .id("l3s-settings")
                .flex()
                .items_center()
                .justify_center()
                .h(px(28.0))
                .w(px(28.0))
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(theme.panel))
                .child(icon(
                    include_bytes!("../../assets/icons/settings.svg"),
                    14.0,
                    theme.muted,
                ))
                .on_click(cx.listener(|this, _, _, _| this.click_menu())),
        );
        div()
            .absolute()
            .left(px(0.0))
            .bottom(px(4.0))
            .w_full()
            .h(px(FOOTER_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .text_size(px(self.footer_font()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("l3s-session-button")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .h(px(28.0))
                            .px_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.panel))
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
                            })),
                    )
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(truncate_label(&self.crumb_path(), 48)),
                    )
                    .child(session_status(
                        self.status.clone(),
                        self.current_session_name(),
                        theme.muted,
                    )),
            )
            .child(right)
    }

    /// The footer session menu: switch sessions or start a fresh one.
    ///
    /// A transparent catcher covers the content area so a click anywhere
    /// outside dismisses; the panel stops propagation so its own clicks land.
    fn session_menu(&self, cx: &mut Context<Self>, card_x: f32) -> Div {
        if !self.session_menu_open {
            return div();
        }
        let theme = self.theme;
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
                    .id(format!("l3s-menu-session-{}", session.name))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.0))
                    .px_2()
                    .rounded_md()
                    .bg(bg)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.panel))
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
                .id("l3s-menu-new-session")
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(28.0))
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(theme.panel))
                .text_size(px(self.chrome_font()))
                .text_color(theme.panel_fg)
                .child(icon(
                    include_bytes!("../../assets/icons/plus.svg"),
                    14.0,
                    theme.muted,
                ))
                .child("New session…")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.start_new_session();
                    cx.notify();
                })),
        );
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .child(
                div()
                    .absolute()
                    .left(px(card_x + 12.0))
                    .bottom(px(FOOTER_HEIGHT + 8.0))
                    .w(px(264.0))
                    .rounded_md()
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.session_menu_open = false;
                    cx.notify();
                }),
            )
    }

    /// One project or nested workspace row; the active workspace gets the pill.
    fn workspace_row(
        &self,
        workspace: &ShellWorkspace,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let active = workspace.active;
        let bg = if active { theme.panel } else { theme.window_bg };
        let mut row = div()
            .id(format!("l3s-workspace-{}", workspace.index))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(if workspace.nested { 26.0 } else { 28.0 }))
            .px_2()
            .rounded_md()
            .bg(bg)
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .text_size(px(self.chrome_font()));
        if workspace.nested {
            row = row.pl(px(18.0));
        }
        let mut name = div().text_color(theme.panel_fg);
        if active {
            name = name.font_weight(FontWeight::BOLD);
        }
        row = row
            .child(if workspace.nested {
                icon(
                    include_bytes!("../../assets/icons/circle.svg"),
                    10.0,
                    theme.muted,
                )
            } else {
                icon(
                    include_bytes!("../../assets/icons/folder.svg"),
                    14.0,
                    theme.panel_fg,
                )
            })
            .child(name.child(workspace.name.clone()));
        // The branch stays subtle here; the prompt line below carries the color.
        if !workspace.nested
            && let Some(branch) = workspace.branch.clone()
        {
            row = row.child(div().text_color(theme.branch).child(branch));
        }
        let id = workspace.id.clone();
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, _| this.focus_workspace(&id)),
        )
    }

    /// Working directory for the footer: the active workspace's cwd.
    fn crumb_path(&self) -> String {
        if let Some(active) = self.surface.workspaces().iter().find(|w| w.active) {
            return shorten_home(&active.cwd);
        }
        self.target.label()
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

    /// One sidebar destination: lucide glyph, label, live badge, chord click.
    fn nav_row(&self, kind: NavKind, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = self.nav_active(kind);
        let bg = if active { theme.panel } else { theme.window_bg };
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
            .id(format!("l3s-nav-{}", kind.label()))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(28.0))
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(self.chrome_font()))
            .bg(bg)
            .text_color(theme.panel_fg)
            .hover(|style| style.bg(theme.panel))
            .child(icon(kind.icon(), 15.0, theme.muted))
            .child(label.child(kind.label().to_string()))
            .child(badge)
            .on_click(cx.listener(move |this, _, _, cx| {
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
        let _ = self
            .connection
            .send(press(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let _ = self
            .connection
            .send(press(KeyCode::Char(key), KeyModifiers::NONE));
    }

    /// MACHINES: saved SSH profiles under our state root, display-only.
    ///
    /// The daemon exposes no control method for machines and opening one
    /// needs an interactive terminal, so rows carry no action.
    fn machines_section(&self) -> Div {
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_1().pt(px(12.0)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(22.0))
                .px_2()
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.muted)
                        .child("MACHINES"),
                ),
        );
        if self.machines.is_empty() {
            return section.child(
                div()
                    .h(px(26.0))
                    .px_2()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No machines"),
            );
        }
        for machine in &self.machines {
            section = section.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.0))
                    .px_2()
                    .rounded_md()
                    .text_size(px(self.chrome_font()))
                    .child(icon(
                        include_bytes!("../../assets/icons/server.svg"),
                        14.0,
                        theme.muted,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_color(if machine.enabled {
                                theme.panel_fg
                            } else {
                                theme.muted
                            })
                            .child(machine.display().to_string()),
                    )
                    .child(machine_status(machine, theme, self.section_font())),
            );
        }
        section
    }

    /// AGENTS: live agent sessions; clicking a row focuses its pane.
    fn agents_section(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_1().pt(px(12.0)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(22.0))
                .px_2()
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.muted)
                        .child("AGENTS"),
                ),
        );
        if self.agents.is_empty() {
            return section.child(
                div()
                    .h(px(26.0))
                    .px_2()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No agents"),
            );
        }
        for agent in &self.agents {
            let pane = agent.pane.clone();
            section = section.child(
                div()
                    .id(format!("l3s-agent-{}", agent.pane))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.panel))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(self.chrome_font()))
                            .child(icon(
                                include_bytes!("../../assets/icons/circle.svg"),
                                9.0,
                                status_color(&agent.status, theme),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(theme.panel_fg)
                                    .child(agent.label().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(self.section_font()))
                                    .text_color(theme.muted)
                                    .child(agent.agent.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .pl(px(17.0))
                            .text_size(px(self.section_font()))
                            .text_color(theme.muted)
                            .child(agent.workspace_name.clone())
                            .child(match agent.branch.clone() {
                                Some(branch) => div().text_color(theme.ok).child(branch),
                                None => div(),
                            }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, _| this.focus_agent(&pane)),
                    ),
            );
        }
        section
    }

    /// WORKSPACES: the catalog grouped by nesting, with a picker shortcut.
    fn projects_section(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut section = div().flex().flex_col().gap_1().pt(px(12.0)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(22.0))
                .px_2()
                .child(
                    div()
                        .text_size(px(self.section_font()))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.muted)
                        .child("WORKSPACES"),
                )
                .child(
                    div()
                        .id("l3s-add-workspace")
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(22.0))
                        .w(px(22.0))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.panel))
                        .child(icon(
                            include_bytes!("../../assets/icons/plus.svg"),
                            11.0,
                            theme.muted,
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                let _ = this.connection.send(ClientMessage::OpenWorkspacePicker);
                            }),
                        ),
                ),
        );
        if self.surface.workspaces().is_empty() {
            return section.child(
                div()
                    .h(px(26.0))
                    .px_2()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child("No projects yet"),
            );
        }
        for workspace in self.surface.workspaces() {
            section = section.child(self.workspace_row(workspace, cx));
        }
        section
    }

    /// Measure the inset card and the grid that fits inside it.
    ///
    /// The card floats below the tab strip with a gap on top, right, and
    /// bottom, starting past the sidebar; the footer floats separately
    /// beneath it. The grid keeps a border on every side.
    fn card_layout(&mut self, window: &Window) -> CardLayout {
        let cell_w = self.cell_width(window);
        let cell_h = self.settings.terminal_font_size * LINE_HEIGHT_RATIO;
        let viewport = window.viewport_size();
        let card_x = f32::from(self.sidebar_cells()) * cell_w + CARD_GAP;
        let grid_w = f32::from(viewport.width) - card_x - CARD_GAP_RIGHT - 2.0;
        let grid_h =
            f32::from(viewport.height) - HEADER_HEIGHT - FOOTER_HEIGHT - CARD_GAP - CARD_GAP - 2.0;
        let cols = (grid_w / cell_w).clamp(20.0, 500.0) as u16;
        let rows = ((grid_h - TERMINAL_TOP_AIR) / cell_h).clamp(5.0, 300.0) as u16;
        CardLayout {
            card_x,
            origin_x: card_x + 1.0,
            origin_y: HEADER_HEIGHT + CARD_GAP + 1.0 + TERMINAL_TOP_AIR,
            cols,
            rows,
            cell_w,
            cell_h,
        }
    }

    /// Recompute the cell grid size and tell the daemon when it changes.
    ///
    /// The viewport is whatever fits inside the floating card; planning with
    /// anything else lays out columns the card clips off.
    fn sync_metrics(&mut self, window: &Window) {
        let layout = self.card_layout(window);
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
        let _ = self.connection.send(ClientMessage::Resize {
            cols,
            rows,
            cell_width_px: cell.0,
            cell_height_px: cell.1,
        });
    }
}

impl Render for L3sView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_metrics(window);
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }

        let theme = self.theme;
        let cell_height = self.settings.terminal_font_size * LINE_HEIGHT_RATIO;
        let cell_width = self.cell_width(window);
        let layout = self.card_layout(window);
        let line_height = px(cell_height);
        let selection = self.selection;
        // A vendored theme names the pane background directly, so the card's
        // padding matches the grid by construction. Otherwise fall back to the
        // dominant cell color the running app paints over the daemon base.
        let terminal_bg = if scheme::builtin(&self.appearance.active).is_some() {
            theme.bg
        } else {
            self.surface
                .frame()
                .map(|frame| {
                    terminal_background(
                        frame,
                        self.skipped_cols(),
                        layout.cols,
                        layout.rows,
                        theme.bg,
                    )
                })
                .unwrap_or(theme.bg)
        };
        let grid_theme = Theme {
            bg: terminal_bg,
            ..theme
        };
        let grid = match self.surface.frame() {
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
            ),
            None => div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_color(theme.muted)
                .child("Waiting for the daemon's first frame…"),
        };
        let grid = grid
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
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
        let sidebar = self.sidebar(
            cx,
            cell_width,
            f32::from(window.viewport_size().height) - FOOTER_HEIGHT,
        );

        // The unified top chrome paints over this row; the spacer only keeps
        // the grid and the pointer mapping anchored at HEADER_HEIGHT.
        let header = div().h(px(HEADER_HEIGHT)).flex_shrink_0();

        let mut root = div()
            .id("l3s-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key_down(event, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _window, cx| this.copy_selection(cx)))
            .on_action(cx.listener(|this, _: &PasteClipboard, _window, cx| {
                this.paste_clipboard(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllGrid, window, cx| {
                this.select_all(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, _window, _cx| this.new_tab()))
            .on_action(cx.listener(|this, _: &SplitVertical, _window, _cx| this.split("right")))
            .on_action(cx.listener(|this, _: &SplitHorizontal, _window, _cx| this.split("down")))
            .on_action(cx.listener(|this, _: &CloseTab, _window, _cx| this.close_active_tab()))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| {
                this.toggle_sidebar();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenAbout, _window, cx| {
                about::open(this.theme, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusTab1, _window, _cx| this.focus_tab(1)))
            .on_action(cx.listener(|this, _: &FocusTab2, _window, _cx| this.focus_tab(2)))
            .on_action(cx.listener(|this, _: &FocusTab3, _window, _cx| this.focus_tab(3)))
            .on_action(cx.listener(|this, _: &FocusTab4, _window, _cx| this.focus_tab(4)))
            .on_action(cx.listener(|this, _: &FocusTab5, _window, _cx| this.focus_tab(5)))
            .on_action(cx.listener(|this, _: &FocusTab6, _window, _cx| this.focus_tab(6)))
            .on_action(cx.listener(|this, _: &FocusTab7, _window, _cx| this.focus_tab(7)))
            .on_action(cx.listener(|this, _: &FocusTab8, _window, _cx| this.focus_tab(8)))
            .on_action(cx.listener(|this, _: &FocusTab9, _window, _cx| {
                if let Some(tab) = this.tabs.last() {
                    this.focus_tab(tab.index);
                }
            }));

        // The chrome inherits the chosen UI family; the grid overrides it.
        if let Some(font) = self.settings.ui_font.as_ref() {
            root = root.font_family(font.clone());
        }

        root.child(header)
            .child(
                div().relative().flex_1().child(
                    div()
                        .absolute()
                        .left(px(layout.card_x))
                        // Content already starts below the header spacer, so the
                        // top is just the gap (counting HEADER_HEIGHT twice
                        // pushes the whole grid down and clips its bottom).
                        .top(px(CARD_GAP))
                        .right(px(CARD_GAP_RIGHT))
                        // Fixed bottom: any floor slack stays inside the card
                        // on the grid's own background, invisible.
                        .bottom(px(FOOTER_HEIGHT + CARD_GAP))
                        .flex()
                        .flex_col()
                        .bg(terminal_bg)
                        .border_1()
                        .border_color(terminal_inset_border())
                        .rounded_md()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .pt(px(TERMINAL_TOP_AIR))
                                .child(grid),
                        ),
                ),
            )
            // Chrome layers live at the root so they paint above the content
            // and the sidebar: the menu overlay must never slide beneath it.
            .child(sidebar)
            .child(self.top_chrome(cx, cell_width))
            .child(self.session_menu(cx, layout.card_x))
            .child(self.status_bar(cx))
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
) -> Div {
    let width = usize::from(frame.width);
    let skip = usize::from(skip_cols).min(width);
    let paint_w = (width.saturating_sub(skip)).min(usize::from(max_cols));
    let start = usize::from(row) * width + skip;
    let end = (start + paint_w).min(frame.cells.len());
    let cells = frame.cells.get(start..end).unwrap_or(&[]);

    let mut element = div()
        .relative()
        .flex_shrink_0()
        .h(line_height)
        .w(px(
            f32::from(frame.width.saturating_sub(skip_cols)) * cell_width
        ));

    // A double-width glyph covers two columns; the daemon blanks the trailing
    // continuation cell. Drop it here too so the run advances exactly one cell
    // width per column and the grid stays aligned with the daemon's geometry.
    let mut rendered: Vec<(u16, &CellData, bool)> = Vec::with_capacity(cells.len());
    let mut column = 0usize;
    while column < cells.len() {
        let cell = &cells[column];
        let absolute = skip_cols.saturating_add(column as u16);
        rendered.push((absolute, cell, cursor_at(frame, absolute, row)));
        column += if l3s_protocol::symbol_width(symbol(cell)) >= 2 {
            2
        } else {
            1
        };
    }

    let selected = |column: u16| selection.is_some_and(|s| s.contains(column, row));
    let mut index = 0usize;
    while index < rendered.len() {
        let start_column = rendered[index].0;
        let (fg, bg, mods) = style_of(
            rendered[index].1,
            theme,
            rendered[index].2,
            selected(start_column),
        );
        let mut text = String::new();
        let mut next = index;
        while next < rendered.len() {
            let (column, cell, cursor) = rendered[next];
            if style_of(cell, theme, cursor, selected(column)) != (fg, bg, mods) {
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
            frame.width
        };
        let run_cells = end_column.saturating_sub(start_column).max(1);

        // Runs position relative to the painted slice: the container starts
        // at the first visible column, so absolute daemon columns would
        // shove every run right by the skipped width (and clip the edge).
        let mut span = div()
            .absolute()
            .left(px(
                f32::from(start_column.saturating_sub(skip_cols)) * cell_width
            ))
            .top(px(0.0))
            .w(px(f32::from(run_cells) * cell_width))
            .h(line_height)
            .flex()
            .items_center()
            .overflow_hidden()
            .font_family(font_family)
            .text_size(px(font_size))
            .line_height(line_height)
            .bg(bg)
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
        index = next;
    }
    element
}

fn style_of(
    cell: &CellData,
    theme: Theme,
    cursor: bool,
    selected: bool,
) -> (gpui::Rgba, gpui::Rgba, u16) {
    let mut fg = theme::resolve(cell.fg, theme.fg);
    let mut bg = theme::resolve(cell.bg, theme.bg);
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

fn symbol(cell: &CellData) -> &str {
    if cell.symbol.is_empty() {
        " "
    } else {
        cell.symbol.as_str()
    }
}

/// The terminal's own background: the most common resolved cell color over
/// the visible pane. The card's padding uses it so the blank frame blends with
/// the running app's surface instead of the daemon's base color.
fn terminal_background(
    frame: &FrameData,
    skip_cols: u16,
    max_cols: u16,
    max_rows: u16,
    fallback: gpui::Rgba,
) -> gpui::Rgba {
    use std::collections::HashMap;
    // TUI screens use a handful of backgrounds, so count the first few with a
    // cheap linear scan and only fall back to a map for truecolor-heavy ones.
    const DISTINCT_LIMIT: usize = 32;
    let width = usize::from(frame.width);
    let skip = usize::from(skip_cols).min(width);
    let first_row = usize::from(SKIPPED_DAEMON_ROWS);
    let last_row = first_row
        .saturating_add(usize::from(max_rows))
        .min(usize::from(frame.height).saturating_sub(usize::from(SKIPPED_DAEMON_BOTTOM_ROWS)));

    // Vote on *resolved* colors, not packed wire values: the same visible
    // color arrives as a palette index in one cell and an sRGB triple in
    // another, and splitting those votes is what lets the gap pick a wrong
    // winner when the surface is black.
    let mut small: Vec<(u32, usize, gpui::Rgba)> = Vec::new();
    let mut wide: Option<HashMap<u32, (usize, gpui::Rgba)>> = None;
    for row in first_row..last_row {
        let start = row * width + skip;
        let end = (start + usize::from(max_cols)).min((row + 1) * width);
        for cell in frame.cells.get(start..end).unwrap_or(&[]) {
            let color = theme::resolve(cell.bg, fallback);
            let key = color_key(color);
            if let Some(map) = wide.as_mut() {
                map.entry(key).or_insert((0, color)).0 += 1;
            } else if let Some(entry) = small.iter_mut().find(|(k, _, _)| *k == key) {
                entry.1 += 1;
            } else if small.len() < DISTINCT_LIMIT {
                small.push((key, 1, color));
            } else {
                let mut map: HashMap<u32, (usize, gpui::Rgba)> = small
                    .drain(..)
                    .map(|(k, count, color)| (k, (count, color)))
                    .collect();
                map.entry(key).or_insert((0, color)).0 += 1;
                wide = Some(map);
            }
        }
    }

    let winner = match wide {
        Some(map) => map.into_values().max_by_key(|(count, _)| *count),
        None => small
            .into_iter()
            .map(|(_, count, color)| (count, color))
            .max_by_key(|(count, _)| *count),
    };
    winner.map(|(_, color)| color).unwrap_or(fallback)
}

/// Canonical sRGB key for a resolved color, so equal colors that arrive under
/// different wire encodings (palette index, sRGB, reset) vote as one.
fn color_key(color: gpui::Rgba) -> u32 {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// Reachability text for a machine row: latency, state, or still probing.
fn machine_status(machine: &MachineInfo, theme: Theme, size: f32) -> Div {
    let (text, color) = match (machine.online, machine.latency_ms) {
        (Some(true), Some(ms)) => (format!("{ms} ms"), theme.ok),
        (Some(true), None) => ("online".to_string(), theme.ok),
        (Some(false), _) => ("offline".to_string(), theme.alert),
        (None, _) => ("…".to_string(), theme.muted),
    };
    div().text_size(px(size)).text_color(color).child(text)
}

/// The footer status text, hidden when it repeats the session pill.
fn session_status(status: String, session: &str, muted: gpui::Rgba) -> Div {
    if status == session {
        return div();
    }
    div().text_color(muted).child(status)
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

/// Outcome of a background `luvus server start` for a fresh session.
enum SessionOpResult {
    Started(String),
    StartFailed(String, String),
}

/// Start a named session's server and wait for its socket, off-thread.
fn start_session_blocking(root: &Path, name: &str) -> SessionOpResult {
    match l3s_client::ensure_server_for(root, Some(name)) {
        Ok(()) => SessionOpResult::Started(name.to_string()),
        Err(l3s_client::Error::NotFound { .. }) => {
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

/// The control socket for a launch target, matching `L3sView`'s derivation so
/// the Appearances window talks to the same daemon session.
fn api_socket_for_target(target: &ConnectTarget) -> PathBuf {
    let (root, session) = match &target.socket {
        Some(path) => split_client_socket(path).unwrap_or_else(|| (config_dir(), None)),
        None => (config_dir(), target.session.clone()),
    };
    api_socket_for(&root, session.as_deref())
}

/// Shorten a workspace path with `~`, the way the mock's breadcrumb reads.
fn shorten_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned())
        && let Some(rest) = path.strip_prefix(home.as_str())
    {
        return format!("~{rest}");
    }
    path.to_string()
}

/// Cap a tab label at `max` chars so one long name cannot crowd the strip.
fn truncate_label(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_string();
    }
    let kept: String = label.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
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

/// Remove one dock id and its parallel weight from a sidebar config.
fn remove_dock(side: &mut l3s_protocol::SideConfig, name: &str) {
    if let Some(index) = side.docks.iter().position(|dock| dock == name) {
        side.docks.remove(index);
        if index < side.dock_weights.len() {
            side.dock_weights.remove(index);
        }
    }
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
    fn terminal_background_takes_the_dominant_cell_color() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 4, 0, 0);
        for cell in &mut frame.cells {
            cell.bg = 0x0218_1825;
        }
        frame.cells[0].bg = 0;
        assert_eq!(
            terminal_background(&frame, 0, 4, 2, theme.bg),
            theme::resolve(0x0218_1825, theme.bg)
        );
    }

    #[test]
    fn terminal_background_merges_equivalent_encodings() {
        let theme = Theme::default();
        let mut frame = FrameData::blank(4, 4, 0, 0);
        for cell in &mut frame.cells {
            cell.bg = 0x021e_1e2e;
        }
        // On the visible row, black arrives under three different encodings.
        // Each bucket is a minority on its own, so only a resolved vote lets
        // black beat the surface color.
        frame.cells[2 * 4].bg = 0x0100_0000; // palette 0
        frame.cells[2 * 4 + 1].bg = 0x0100_0010; // palette 16
        frame.cells[2 * 4 + 2].bg = 0x0200_0000; // sRGB black
        assert_eq!(
            terminal_background(&frame, 0, 4, 2, theme.bg),
            theme::resolve(0x0200_0000, theme.bg)
        );
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
        let mut seen = std::collections::HashSet::new();
        for binding in app_key_bindings() {
            let chord = format!("{:?}", binding.keystrokes());
            assert!(seen.insert(chord.clone()), "duplicate shortcut: {chord}");
        }
    }
}
