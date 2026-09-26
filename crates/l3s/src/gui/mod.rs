//! The GPUI desktop window.
//!
//! The window is a thin, native shell over one daemon connection: it forwards
//! input and resizes and paints the server's rendered cell grid. The daemon
//! still owns layout, but this client claims the workspaces dock so it can draw
//! that one panel natively, and selects text over the grid itself.

mod app_icon;
mod keys;
mod selection;
mod theme;

use std::time::Duration;

use crossterm::event::{
    KeyEventKind, MouseButton as TerminalMouseButton, MouseEvent, MouseEventKind,
};
use gpui::{
    App, Bounds, ClipboardItem, Context, Div, FocusHandle, FontWeight, Hsla, KeyDownEvent,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, TextRun, Window, WindowBounds, WindowOptions, div, font,
    prelude::*, px, size,
};
use gpui_platform::application;
use l3s_client::{
    Agent, AgentFeed, BarWidget, ClientEvent, ConnectTarget, Connection, Surface, SurfaceChange,
    Tab,
};
use l3s_protocol::{
    CellData, ClientMessage, FrameData, ShellDockRect, ShellSidebars, ShellWorkspace,
};
use selection::{Cell, Selection};
use theme::Theme;

/// Monospace family used for the cell grid.
const FONT_FAMILY: &str = "Menlo";
/// Base font size in logical pixels.
const FONT_SIZE: f32 = 13.0;
/// Advance width of one cell as a fraction of the font size, used only until
/// the real advance is measured from the font.
const CELL_WIDTH_RATIO: f32 = 0.6;
/// Line height as a fraction of the font size.
const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Height of the native header strip in pixels.
const HEADER_HEIGHT: f32 = 30.0;
/// Height of a tab in the unified top chrome, independent of the text grid.
const TAB_HEIGHT: f32 = 24.0;

// Ratatui `Modifier` bits, as packed by the daemon.
const M_BOLD: u16 = 1 << 0;
const M_ITALIC: u16 = 1 << 2;
const M_UNDERLINED: u16 = 1 << 3;
const M_REVERSED: u16 = 1 << 6;

/// Open the desktop window and attach to `target`.
pub fn run(target: ConnectTarget) -> anyhow::Result<()> {
    // The real viewport is not known until a window exists; the first resize
    // after the window opens corrects these provisional dimensions.
    let connection = l3s_client::connect(&target, 120, 40)?;
    // Claim the workspaces dock before the window paints, so the daemon blanks
    // its own copy and starts projecting the catalog to us.
    connection.request_workspace_dock()?;

    application().run(move |cx: &mut App| {
        app_icon::install();
        let bounds = Bounds::centered(None, size(px(1240.0), px(780.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(640.0), px(400.0))),
                ..Default::default()
            },
            move |window, cx| cx.new(move |cx| L3sView::new(target, connection, window, cx)),
        );
        if let Err(error) = opened {
            eprintln!("l3s: could not open a window: {error}");
            return;
        }
        cx.activate(true);
    });

    Ok(())
}

struct L3sView {
    target: ConnectTarget,
    connection: Connection,
    surface: Surface,
    theme: Theme,
    focus_handle: FocusHandle,
    font_size: f32,
    cols: u16,
    rows: u16,
    status: String,
    /// Measured monospace advance width, resolved once from the font.
    cell_width: Option<f32>,
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
    /// Background poller for the daemon's live agent list.
    agent_feed: Option<AgentFeed>,
    /// Latest agent snapshot.
    agents: Vec<Agent>,
    /// Latest status-bar widgets from `ui.bar.list`.
    bar_widgets: Vec<BarWidget>,
    /// Latest tabs of the active workspace.
    tabs: Vec<Tab>,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // The agent list comes from UHP, which is addressed by session; an
        // explicit client socket gives us no way to find it.
        let agent_feed = target.socket.is_none().then(|| {
            let socket = l3s_client::api_socket_path_for(target.session.as_deref());
            AgentFeed::spawn(socket)
        });

        let view = Self {
            target,
            connection,
            surface: Surface::new(),
            theme: Theme::default(),
            focus_handle,
            font_size: FONT_SIZE,
            cols: 120,
            rows: 40,
            status: "connecting…".to_string(),
            cell_width: None,
            selection: None,
            dragging: false,
            drag_moved: false,
            press_anchor: None,
            hover_cell: None,
            agent_feed,
            agents: Vec::new(),
            bar_widgets: Vec::new(),
            tabs: Vec::new(),
            sidebar_layout: None,
            sidebar_workspace_paths: false,
            sidebar_revision: 0,
            sidebar_visible: true,
        };

        cx.spawn(async move |this, cx| {
            loop {
                let updated = this.update(cx, |view, cx| {
                    if view.poll() {
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
        if let Some(feed) = self.agent_feed.as_ref() {
            while let Some(update) = feed.try_recv() {
                self.agents = update.agents;
                self.bar_widgets = update.bar;
                self.tabs = update.tabs;
                changed = true;
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
            self.sidebar_workspace_paths = sidebars.workspace_paths;
            self.sidebar_layout = Some(layout);
            self.push_sidebar_layout();
        }
        changed
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // The platform clipboard chords are ours: the pane must see the copied
        // text (or a bracketed paste), never the chord itself.
        if keystroke.modifiers.platform {
            if keystroke.key.eq_ignore_ascii_case("c")
                && self
                    .selection
                    .is_some_and(|selection| !selection.is_collapsed())
            {
                self.copy_selection(cx);
                return;
            }
            if keystroke.key.eq_ignore_ascii_case("v") {
                self.paste_clipboard(cx);
                return;
            }
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

    /// Focus a workspace by id, switching the daemon's active workspace.
    fn focus_workspace(&mut self, workspace_id: &str) {
        let _ = self.connection.send(ClientMessage::ShellWorkspaceFocus {
            workspace_id: workspace_id.to_string(),
        });
    }

    /// Focus an agent's pane, switching workspace and tab as needed.
    fn focus_agent(&self, pane: &str) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.focus_pane(pane);
        }
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
        let font = font(FONT_FAMILY);
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
            .shape_line("M".into(), px(self.font_size), &[run], None)
            .width
            .into();
        self.cell_width = Some(width);
        width
    }

    /// Convert a window-space pointer position to a grid cell, if it lands on
    /// the grid rather than the native header.
    fn cell_at(&self, position: Point<Pixels>) -> Option<Cell> {
        let frame = self.surface.frame()?;
        if frame.width == 0 || frame.height == 0 {
            return None;
        }
        let cell_width = self.cell_width.unwrap_or(self.font_size * CELL_WIDTH_RATIO);
        let cell_height = self.font_size * LINE_HEIGHT_RATIO;
        let y = (f32::from(position.y) - HEADER_HEIGHT) / cell_height;
        let x = f32::from(position.x) / cell_width;
        if y < 0.0 || x < 0.0 {
            return None;
        }
        let column = (x as u32).min(u32::from(frame.width - 1)) as u16;
        let row = (y as u32).min(u32::from(frame.height - 1)) as u16;
        Some(Cell { column, row })
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

    fn begin_selection(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(cell) = self.cell_at(event.position) else {
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

    fn extend_selection(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.dragging && event.pressed_button == Some(MouseButton::Left) {
            let Some(cell) = self.cell_at(event.position) else {
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
        let Some(cell) = self.cell_at(event.position) else {
            return;
        };
        if self.hover_cell != Some(cell) {
            self.hover_cell = Some(cell);
            self.forward_mouse(MouseEventKind::Moved, cell, event.modifiers);
        }
    }

    fn finish_selection(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if !self.dragging {
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
        let Some(cell) = self.cell_at(event.position) else {
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

    fn press(&self, event: &MouseDownEvent, button: TerminalMouseButton) {
        if let Some(cell) = self.cell_at(event.position) {
            self.forward_mouse(MouseEventKind::Down(button), cell, event.modifiers);
        }
    }

    fn release(&self, event: &MouseUpEvent, button: TerminalMouseButton) {
        if let Some(cell) = self.cell_at(event.position) {
            self.forward_mouse(MouseEventKind::Up(button), cell, event.modifiers);
        }
    }

    fn scroll(&self, event: &ScrollWheelEvent) {
        let Some(cell) = self.cell_at(event.position) else {
            return;
        };
        let cell_height = self.font_size * LINE_HEIGHT_RATIO;
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

    /// Build the native workspaces panel the daemon reserved for this client.
    ///
    /// The daemon leaves the dock cells blank and reports their exact rectangle
    /// in `ShellDock`; this paints the same region from the `ShellWorkspaces`
    /// catalog, so the panel is native while staying column-aligned with the
    /// terminal grid behind it.
    fn sidebar(&self, cx: &mut Context<Self>, cell_width: f32, cell_height: f32) -> Div {
        if !self.sidebar_visible {
            return div();
        }
        let Some(dock) = self.surface.shell_dock() else {
            return div();
        };
        let stride = if dock.show_paths { 2.0 } else { 1.0 };
        let row_height = stride * cell_height;
        let font_size = self.font_size;

        let add = div()
            .id("l3s-add-workspace")
            .cursor_pointer()
            .text_color(self.theme.accent)
            .child(" +")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    let _ = this.connection.send(ClientMessage::OpenWorkspacePicker);
                }),
            );

        let mut list = div()
            .flex()
            .flex_col()
            .flex_1()
            .pl(px(2.0 * cell_width))
            .pr(px(cell_width))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(cell_height))
                    .child(
                        div()
                            .text_size(px(font_size))
                            .font_weight(FontWeight::BOLD)
                            .text_color(self.theme.accent)
                            .child("WORKSPACES"),
                    )
                    .child(add),
            );

        for workspace in self.surface.workspaces() {
            list = list.child(self.workspace_row(workspace, &dock, cx, cell_height, row_height));
        }
        list = list.child(self.agents_section(cx, cell_height));

        // The panel starts on row 0, covering the daemon's `« session Menu`
        // chrome so the sidebar shows only the two native sections.
        div()
            .absolute()
            .left(px(f32::from(dock.x) * cell_width))
            .top(px(0.0))
            .w(px(f32::from(dock.width) * cell_width))
            .h(px((f32::from(dock.y) + f32::from(dock.height))
                * cell_height
                + HEADER_HEIGHT))
            .flex()
            .flex_col()
            .bg(self.theme.bg)
            .overflow_hidden()
            .child(list)
    }

    /// The daemon's Menu-pill cell, for the header button to click.
    fn menu_cell(&self) -> Option<Cell> {
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

    /// Close a workspace tab.
    fn close_tab(&self, index: u16) {
        if let Some(feed) = self.agent_feed.as_ref() {
            feed.close_tab(index);
        }
    }

    /// The unified top chrome: app controls plus the tab strip, painted over
    /// the daemon's tab row and the app header it replaces.
    ///
    /// It starts at the sidebar's right edge, so the sidebar keeps a full-height
    /// column and the chrome never competes with it for the same pixels.
    fn top_chrome(&self, cx: &mut Context<Self>, cell_width: f32, cell_height: f32) -> Div {
        let theme = self.theme;
        let left = f32::from(self.sidebar_cells());
        let mut tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_1()
            .overflow_hidden();
        for tab in &self.tabs {
            tabs = tabs.child(self.tab_item(tab, cx, TAB_HEIGHT));
        }
        tabs = tabs.child(
            div()
                .id("l3s-new-tab")
                .flex()
                .items_center()
                .h(px(TAB_HEIGHT))
                .px_2()
                .cursor_pointer()
                .text_color(theme.muted)
                .hover(|style| style.text_color(theme.accent))
                .child("+")
                .on_click(cx.listener(|this, _, _, _| this.new_tab())),
        );
        div()
            .absolute()
            .left(px(left * cell_width))
            .right(px(0.0))
            .top(px(0.0))
            .h(px(HEADER_HEIGHT + cell_height))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .bg(theme.bg)
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(self.font_size))
            .child(
                div()
                    .id("l3s-sidebar-toggle")
                    .cursor_pointer()
                    .text_color(theme.muted)
                    .hover(|style| style.text_color(theme.fg))
                    .child(if self.sidebar_visible { "«" } else { "»" })
                    .on_click(cx.listener(|this, _, _, _| this.toggle_sidebar())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .text_color(theme.panel_fg)
                    .child(div().w(px(8.0)).h(px(8.0)).rounded_md().bg(theme.accent))
                    .child(format!("l3s · {}", self.target.label())),
            )
            .child(tabs)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .flex_shrink_0()
                    .child(
                        div()
                            .id("l3s-menu")
                            .cursor_pointer()
                            .text_color(theme.accent)
                            .hover(|style| style.text_color(theme.panel_fg))
                            .child("Menu")
                            .on_click(cx.listener(|this, _, _, _| this.click_menu())),
                    )
                    .child(div().text_color(theme.muted).child(self.status.clone())),
            )
    }

    fn tab_item(&self, tab: &Tab, cx: &mut Context<Self>, tab_height: f32) -> impl IntoElement {
        let theme = self.theme;
        let active = tab.active;
        let index = tab.index;
        let close_index = tab.index;
        // The close button only brightens on hover: a resting tab bar reads as
        // labels, and the target is one cell wide anyway.
        let close = div()
            .id(format!("l3s-tab-close-{index}"))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_color(theme.muted)
            .hover(|style| style.text_color(theme.accent))
            .child("✕")
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.close_tab(close_index);
            }));
        let (bg, fg) = if active {
            (theme.panel, theme.panel_fg)
        } else {
            (theme.bg, theme.muted)
        };
        let mut item = div()
            .id(format!("l3s-tab-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .h(px(tab_height))
            .px_3()
            .rounded_t_md()
            .cursor_pointer()
            .text_size(px(self.font_size))
            .bg(bg)
            .text_color(fg)
            .child(div().child(tab.label()))
            .child(close)
            .hover(|style| style.bg(theme.panel).text_color(theme.panel_fg))
            .on_click(cx.listener(move |this, _, _, _| this.focus_tab(index)));
        // The active tab carries an accent cap, so it stays legible when a
        // second tab is under the pointer.
        if active {
            item = item.border_t_1().border_color(theme.accent);
        }
        item
    }

    /// Open the daemon's Settings menu by clicking its Menu pill.
    fn click_menu(&self) {
        if let Some(cell) = self.menu_cell() {
            self.forward_click(cell);
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

    /// The native bottom bar, covering the daemon's status line.
    ///
    /// The right side mirrors the daemon's own status-bar widgets (fetched via
    /// `ui.bar.list`); the left shows the connection and session.
    fn status_bar(&self, cell_height: f32) -> Div {
        let theme = self.theme;
        let font_size = self.font_size;
        let mut right = div().flex().flex_row().items_center().gap_2();
        for widget in &self.bar_widgets {
            if widget.region != "bottom-right" {
                continue;
            }
            for segment in &widget.content {
                if segment.separator {
                    right = right.child(div().text_color(theme.muted).child("·"));
                } else if !segment.text.is_empty() {
                    right = right.child(
                        div()
                            .text_color(tone_color(&segment.tone, theme))
                            .child(segment.text.clone()),
                    );
                }
            }
        }
        div()
            .absolute()
            .left(px(0.0))
            .bottom(px(0.0))
            .w_full()
            .h(px(cell_height))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .bg(theme.panel)
            .text_size(px(font_size))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(theme.accent).child("●"))
                    .child(div().text_color(theme.panel_fg).child(self.status.clone())),
            )
            .child(right)
    }

    fn workspace_row(
        &self,
        workspace: &ShellWorkspace,
        dock: &ShellDockRect,
        cx: &mut Context<Self>,
        cell_height: f32,
        row_height: f32,
    ) -> impl IntoElement {
        let theme = self.theme;
        let active = workspace.active;
        let background = if active {
            theme::resolve(dock.active_bg, theme.panel)
        } else {
            theme.bg
        };
        let name_color = if active {
            theme::resolve(dock.active_fg, theme.accent)
        } else {
            theme::resolve(dock.normal_fg, theme.fg)
        };
        let secondary = theme::resolve(dock.secondary_fg, theme.muted);
        let branch_color = theme::resolve(dock.branch_fg, theme.accent);
        let dot_color = theme::resolve(workspace.dot_color, theme.fg);

        let mut first = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(cell_height))
            .text_size(px(self.font_size))
            .gap_2();
        if workspace.nested {
            first = first.child(div().text_color(secondary).child("└"));
        }
        first = first.child(div().text_color(dot_color).child(workspace.dot.clone()));
        let mut name = div().text_color(name_color);
        if active {
            name = name.font_weight(FontWeight::BOLD);
        }
        first = first.child(name.child(workspace.name.clone()));
        if let Some(branch) = workspace.branch.clone() {
            first = first.child(
                div()
                    .text_color(if active { branch_color } else { secondary })
                    .child(format!("  {branch}")),
            );
        }

        let mut row = div()
            .id(format!("l3s-workspace-{}", workspace.index))
            .flex()
            .flex_col()
            .h(px(row_height))
            .bg(background)
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .child(first);
        if dock.show_paths {
            let indent = if workspace.nested { "    " } else { "  " };
            row = row.child(
                div()
                    .h(px(cell_height))
                    .text_size(px(self.font_size))
                    .text_color(secondary)
                    .child(format!("{indent}{}", workspace.cwd)),
            );
        }
        let id = workspace.id.clone();
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, _| this.focus_workspace(&id)),
        )
    }

    /// The native agents section: live agent sessions from the control API.
    fn agents_section(&self, cx: &mut Context<Self>, cell_height: f32) -> Div {
        let font_size = self.font_size;
        let mut section = div().flex().flex_col().pt(px(cell_height * 0.75)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(cell_height))
                .child(
                    div()
                        .text_size(px(font_size))
                        .font_weight(FontWeight::BOLD)
                        .text_color(self.theme.accent)
                        .child("AGENTS"),
                )
                .child(
                    div()
                        .text_size(px(font_size))
                        .text_color(self.theme.muted)
                        .child(self.agents.len().to_string()),
                ),
        );
        if self.agents.is_empty() {
            return section.child(
                div()
                    .h(px(cell_height))
                    .text_size(px(font_size))
                    .text_color(self.theme.muted)
                    .child("no active agents"),
            );
        }
        for agent in &self.agents {
            section = section.child(self.agent_row(agent, cx, cell_height));
        }
        section
    }

    fn agent_row(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
        cell_height: f32,
    ) -> impl IntoElement {
        let theme = self.theme;
        let dot = status_color(&agent.status, theme);
        let background = if agent.focused {
            theme::resolve(0, theme.panel)
        } else {
            theme.bg
        };
        let mut line = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(cell_height))
            .text_size(px(self.font_size))
            .gap_2()
            .child(div().text_color(dot).child("●"))
            .child(div().text_color(theme.accent).child(agent.agent.clone()));
        line = line.child(
            div()
                .text_color(if agent.focused {
                    theme.accent
                } else {
                    theme.fg
                })
                .child(agent.label().to_string()),
        );

        let mut detail = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(cell_height))
            .text_size(px(self.font_size))
            .gap_2()
            .text_color(theme.muted)
            .child(agent.workspace_name.clone());
        if let Some(branch) = agent.branch.clone() {
            detail = detail.child(div().text_color(theme.ok).child(branch));
        }

        let pane = agent.pane.clone();
        div()
            .id(format!("l3s-agent-{}", agent.pane))
            .flex()
            .flex_col()
            .h(px(cell_height * 2.0))
            .bg(background)
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .overflow_hidden()
            .child(line)
            .child(detail)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| this.focus_agent(&pane)),
            )
    }

    /// Recompute the cell grid size and tell the daemon when it changes.
    ///
    /// Columns are planned with the same measured advance the grid paints
    /// with: planning with the rough ratio while painting with the measured
    /// width makes the daemon lay out more columns than fit, clipping the
    /// right edge (modals, btop) off the window.
    fn sync_metrics(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let cell_width = self.cell_width(window);
        let cell_height = self.font_size * LINE_HEIGHT_RATIO;
        let cols = (viewport.width / px(cell_width)).clamp(20.0, 500.0) as u16;
        let rows =
            ((viewport.height - px(HEADER_HEIGHT)) / px(cell_height)).clamp(5.0, 300.0) as u16;
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let _ = self.connection.send(ClientMessage::Resize {
            cols,
            rows,
            cell_width_px: cell_width.round().clamp(1.0, 65535.0) as u16,
            cell_height_px: cell_height.round().clamp(1.0, 65535.0) as u16,
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
        let cell_height = self.font_size * LINE_HEIGHT_RATIO;
        let cell_width = self.cell_width(window);
        let line_height = px(cell_height);
        let selection = self.selection;
        let grid = match self.surface.frame() {
            Some(frame) => render_grid(
                frame,
                theme,
                self.font_size,
                line_height,
                selection,
                cell_width,
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
                cx.listener(|this, event: &MouseDownEvent, _, cx| this.begin_selection(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.press(event, TerminalMouseButton::Right);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.press(event, TerminalMouseButton::Middle);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| this.finish_selection(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _, _| {
                    this.release(event, TerminalMouseButton::Right);
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _, _| {
                    this.release(event, TerminalMouseButton::Middle);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.extend_selection(event, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, _| {
                this.scroll(event);
            }));
        let sidebar = self.sidebar(cx, cell_width, cell_height);

        // The unified top chrome paints over this row; the spacer only keeps
        // the grid and the pointer mapping anchored at HEADER_HEIGHT.
        let header = div().h(px(HEADER_HEIGHT)).flex_shrink_0();

        div()
            .id("l3s-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.fg)
            .font_family(FONT_FAMILY)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key_down(event, cx);
            }))
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .overflow_hidden()
                    .child(grid)
                    .child(self.status_bar(cell_height)),
            )
            // Both chrome layers live at the root so they can reach over the
            // header row; the content column clips its own overflow.
            .child(sidebar)
            .child(self.top_chrome(cx, cell_width, cell_height))
    }
}

fn render_grid(
    frame: &FrameData,
    theme: Theme,
    font_size: f32,
    line_height: Pixels,
    selection: Option<Selection>,
    cell_width: f32,
) -> Div {
    let mut column = div().flex().flex_col().size_full();
    for row in 0..frame.height {
        column = column.child(render_row(
            frame,
            row,
            theme,
            font_size,
            line_height,
            selection,
            cell_width,
        ));
    }
    column
}

fn render_row(
    frame: &FrameData,
    row: u16,
    theme: Theme,
    font_size: f32,
    line_height: Pixels,
    selection: Option<Selection>,
    cell_width: f32,
) -> Div {
    let width = usize::from(frame.width);
    let start = usize::from(row) * width;
    let end = (start + width).min(frame.cells.len());
    let cells = frame.cells.get(start..end).unwrap_or(&[]);

    let mut element = div()
        .relative()
        .flex_shrink_0()
        .h(line_height)
        .w(px(f32::from(frame.width) * cell_width));

    // A double-width glyph covers two columns; the daemon blanks the trailing
    // continuation cell. Drop it here too so the run advances exactly one cell
    // width per column and the grid stays aligned with the daemon's geometry.
    let mut rendered: Vec<(u16, &CellData, bool)> = Vec::with_capacity(cells.len());
    let mut column = 0usize;
    while column < cells.len() {
        let cell = &cells[column];
        rendered.push((column as u16, cell, cursor_at(frame, column as u16, row)));
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

        let mut span = div()
            .absolute()
            .left(px(f32::from(start_column) * cell_width))
            .top(px(0.0))
            .w(px(f32::from(run_cells) * cell_width))
            .h(line_height)
            .flex()
            .items_center()
            .overflow_hidden()
            .font_family(FONT_FAMILY)
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
    if mods & M_REVERSED != 0 {
        std::mem::swap(&mut fg, &mut bg);
        mods &= !M_REVERSED;
    }
    if cursor {
        std::mem::swap(&mut fg, &mut bg);
        if fg == bg {
            fg = theme.accent;
        }
    }
    if selected {
        fg = theme.selection_fg;
        bg = theme.selection_bg;
    }
    (fg, bg, mods)
}

fn cursor_at(frame: &FrameData, column: u16, row: u16) -> bool {
    frame.cursor_visible && frame.cursor == Some((column, row))
}

fn symbol(cell: &CellData) -> &str {
    if cell.symbol.is_empty() {
        " "
    } else {
        cell.symbol.as_str()
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

/// A color for a status-bar segment tone.
fn tone_color(tone: &str, theme: Theme) -> gpui::Rgba {
    match tone {
        "muted" => theme.muted,
        "accent" => theme.accent,
        "success" => theme.ok,
        "error" => theme.alert,
        _ => theme.panel_fg,
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
