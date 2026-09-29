//! The native Appearances window: pick a Luvus theme and the fonts.
//!
//! It is a thin view over the daemon's theme registry (`theme.list` /
//! `theme.use`, both on a background thread) plus the client-owned font
//! settings, which are shared live with the main window and persisted.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Div, FocusHandle, KeyDownEvent, MouseButton, Render, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};
use l3s_client::{Appearance, ControlClient, ThemeInfo};

use super::style;
use super::theme::Theme;
use super::{CloseTab, CloseWindow, scheme};
use crate::settings::Settings;

/// Width and height of the appearances window.
const WINDOW_SIZE: (f32, f32) = (640.0, 600.0);

/// Smallest and largest selectable font size.
const FONT_SIZE_RANGE: (f32, f32) = (9.0, 32.0);

/// Which font a control edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FontField {
    /// The chrome/interface font.
    Ui,
    /// The monospace terminal font.
    Terminal,
}

/// The Appearances window root view.
pub struct AppearancesView {
    api_socket: PathBuf,
    config_root: PathBuf,
    shared_settings: Rc<RefCell<Settings>>,
    settings: Settings,
    ui_fonts: Vec<String>,
    terminal_fonts: Vec<String>,
    appearance: Appearance,
    theme: Theme,
    refreshing: bool,
    last_poll: Instant,
    rx: Receiver<l3s_client::Result<Appearance>>,
    tx: Sender<l3s_client::Result<Appearance>>,
    select_rx: Receiver<String>,
    select_tx: Sender<String>,
    open_picker: Option<FontField>,
    /// Where the open picker should drop, in window coordinates.
    picker_anchor: gpui::Point<gpui::Pixels>,
    focus_handle: FocusHandle,
}

impl AppearancesView {
    /// Build the view against the daemon's control socket.
    pub fn new(
        api_socket: PathBuf,
        config_root: PathBuf,
        shared_settings: Rc<RefCell<Settings>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (select_tx, select_rx) = std::sync::mpsc::channel();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let settings = shared_settings.borrow().clone();
        let mut all = cx.text_system().all_font_names();
        all.sort();
        all.dedup();
        let mut terminal_fonts: Vec<String> = all
            .iter()
            .filter(|name| is_monospace_name(name))
            .cloned()
            .collect();
        if !terminal_fonts.contains(&settings.terminal_font) {
            terminal_fonts.push(settings.terminal_font.clone());
            terminal_fonts.sort();
        }

        let view = Self {
            api_socket,
            config_root,
            shared_settings,
            settings,
            ui_fonts: all,
            terminal_fonts,
            appearance: Appearance::default(),
            theme: Theme::default(),
            refreshing: false,
            last_poll: Instant::now() - Duration::from_secs(11),
            rx,
            tx,
            select_rx,
            select_tx,
            open_picker: None,
            picker_anchor: gpui::point(px(0.0), px(0.0)),
            focus_handle,
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
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();

        view
    }

    /// Drain finished background work. Returns whether a repaint is needed.
    fn poll(&mut self) -> bool {
        let mut changed = false;

        // View-menu font changes land in the shared settings, so mirror them
        // here without waiting for a window reopen.
        let latest = self.shared_settings.borrow().clone();
        if latest != self.settings {
            self.settings = latest;
            changed = true;
        }

        while let Ok(id) = self.select_rx.try_recv() {
            self.refreshing = true;
            self.last_poll = Instant::now();
            let api = self.api_socket.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = ControlClient::connect(&api).and_then(|mut client| {
                    client.select_theme(&id)?;
                    client.appearance()
                });
                tx.send(result).ok();
            });
        }

        if !self.refreshing && self.last_poll.elapsed() >= Duration::from_secs(1) {
            self.refreshing = true;
            self.last_poll = Instant::now();
            let api = self.api_socket.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result =
                    ControlClient::connect(&api).and_then(|mut client| client.appearance());
                tx.send(result).ok();
            });
        }

        while let Ok(result) = self.rx.try_recv() {
            self.refreshing = false;
            if let Ok(appearance) = result {
                if appearance != self.appearance {
                    self.appearance = appearance;
                    self.theme = scheme::builtin(&self.appearance.active)
                        .map(Theme::from_palette)
                        .unwrap_or_default();
                }
                changed = true;
            }
        }
        changed
    }

    fn select(&self, id: String) {
        self.select_tx.send(id).ok();
    }

    /// Publish a font change to the main window and to disk.
    fn publish(&mut self, cx: &mut Context<Self>) {
        *self.shared_settings.borrow_mut() = self.settings.clone();
        self.settings.save(&self.config_root);
        cx.notify();
    }

    fn families(&self, field: FontField) -> &[String] {
        match field {
            FontField::Ui => &self.ui_fonts,
            FontField::Terminal => &self.terminal_fonts,
        }
    }

    fn family(&self, field: FontField) -> String {
        match field {
            FontField::Ui => self
                .settings
                .ui_font
                .clone()
                .unwrap_or_else(|| "System".to_string()),
            FontField::Terminal => self.settings.terminal_font.clone(),
        }
    }

    fn size(&self, field: FontField) -> f32 {
        match field {
            FontField::Ui => self.settings.ui_font_size,
            FontField::Terminal => self.settings.terminal_font_size,
        }
    }

    fn adjust_size(&mut self, field: FontField, step: f32, cx: &mut Context<Self>) {
        let value = (self.size(field) + step).clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1);
        match field {
            FontField::Ui => self.settings.ui_font_size = value,
            FontField::Terminal => self.settings.terminal_font_size = value,
        }
        self.publish(cx);
    }

    /// The window's type scale, resolved from the shared settings.
    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.shared_settings)
    }

    fn theme_row(&self, info: &ThemeInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = info.active;
        let id = info.id.clone();
        let palette = scheme::builtin(&info.id);
        let bg = if active { theme.panel } else { theme.window_bg };

        let mut swatches = div().flex().flex_row().gap_1();
        if let Some(p) = palette {
            for color in [p.crust, p.mantle, p.surface0, p.accent, p.text] {
                swatches = swatches.child(div().w(px(14.0)).h(px(14.0)).rounded_sm().bg(color));
            }
        } else {
            swatches = swatches.child(
                div()
                    .w(px(70.0))
                    .h(px(14.0))
                    .rounded_sm()
                    .bg(theme.panel_dim),
            );
        }

        let badge = div()
            .px_2()
            .py(px(1.0))
            .rounded_sm()
            .bg(theme.panel_dim)
            .text_size(px(style::scaled(&self.shared_settings, 10.0)))
            .text_color(theme.muted)
            .child(info.appearance.clone());

        let mut row = div()
            .id(format!("l3s-theme-{}", info.id))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(bg)
            .hover(|style| style.bg(theme.panel))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select(id.clone());
                cx.notify();
            }))
            .child(swatches)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(self.fonts().body))
                            .text_color(theme.fg)
                            .child(info.display_name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(info.description.clone()),
                    ),
            )
            .child(badge);

        if active {
            row = row.child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .text_color(theme.accent)
                    .child("✓"),
            );
        }
        row
    }

    fn step_button(
        &self,
        id: String,
        label: String,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<Div> {
        let theme = self.theme;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(24.0))
            .h(px(22.0))
            .rounded_md()
            .bg(theme.panel_dim)
            .cursor_pointer()
            .text_color(theme.fg)
            .hover(|style| style.bg(theme.panel))
            .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
            .child(label)
    }

    fn font_row(&self, field: FontField, label: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let family = self.family(field);
        let size = self.size(field);
        let key = match field {
            FontField::Ui => "ui",
            FontField::Terminal => "term",
        };
        let open = self.open_picker == Some(field);

        let trigger = div()
            .id(format!("l3s-font-{key}-pick"))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_1()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .bg(if open { theme.panel } else { theme.panel_dim })
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                    if this.open_picker == Some(field) {
                        this.open_picker = None;
                    } else {
                        this.open_picker = Some(field);
                        this.picker_anchor = event.position();
                    }
                    cx.notify();
                }),
            )
            .child(div().overflow_hidden().child(family))
            .child(div().text_color(theme.muted).child("▾"));

        let smaller = self.step_button(
            format!("l3s-font-{key}-smaller"),
            "−".to_string(),
            cx,
            move |this, cx| this.adjust_size(field, -1.0, cx),
        );
        let larger = self.step_button(
            format!("l3s-font-{key}-larger"),
            "+".to_string(),
            cx,
            move |this, cx| this.adjust_size(field, 1.0, cx),
        );

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .text_size(px(self.fonts().secondary))
            .text_color(theme.fg)
            .child(
                div()
                    .w(px(84.0))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(theme.muted)
                    .child(label.to_string()),
            )
            .child(trigger)
            .child(div().w(px(12.0)))
            .child(smaller)
            .child(div().w(px(28.0)).text_center().child(format!("{size:.0}")))
            .child(larger)
    }

    /// Escape closes the picker, then the window. Cmd+W is the `CloseWindow`
    /// action handled on this root.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.open_picker.is_some() {
                self.open_picker = None;
            } else {
                window.remove_window();
            }
            cx.notify();
        }
    }

    /// A section title, matching the weight of the list headings below.
    fn section_heading(&self, label: &str) -> Div {
        style::section_label(self.theme, label.to_string(), self.fonts().label)
            .px_3()
            .pt_3()
            .pb_1()
            .whitespace_nowrap()
    }

    /// A dropdown list of every available family for one field.
    fn picker_overlay(&self, field: FontField, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let current = self.family(field);
        let mut list = div().flex().flex_col().p_1();
        for name in self.families(field).iter().cloned() {
            let selected = name == current;
            let id = name.clone();
            list = list.child(
                div()
                    .id(format!("l3s-font-option-{name}"))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(self.fonts().secondary))
                    .text_color(if selected { theme.accent } else { theme.fg })
                    .hover(|style| style.bg(theme.panel))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match field {
                            FontField::Ui => this.settings.ui_font = Some(id.clone()),
                            FontField::Terminal => this.settings.terminal_font = id.clone(),
                        }
                        this.open_picker = None;
                        this.publish(cx);
                    }))
                    .child(name),
            );
        }

        // Drop below the trigger, or above it when there is more room there,
        // and clamp so the panel always stays inside the window.
        let viewport = window.viewport_size();
        let vw = f32::from(viewport.width);
        let vh = f32::from(viewport.height);
        let ax = f32::from(self.picker_anchor.x);
        let ay = f32::from(self.picker_anchor.y);
        let panel_w = 360.0_f32;
        let space_up = (ay - 12.0).max(0.0);
        let space_down = (vh - ay - 32.0).max(0.0);
        // Open downward (the familiar direction) unless there is little room
        // below; when flipping above, keep the panel near the cursor rather
        // than letting it stretch to the top of the window.
        let open_up = space_down < 160.0;
        let max_h = (if open_up { space_up } else { space_down }).clamp(80.0, 300.0);
        let top = if open_up {
            (ay - 6.0 - max_h).max(6.0)
        } else {
            ay + 26.0
        };
        let left = ax.clamp(8.0, (vw - panel_w - 8.0).max(8.0));

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
            .child(
                div()
                    .id("l3s-font-picker")
                    .absolute()
                    .top(px(top))
                    .left(px(left))
                    .w(px(panel_w))
                    .max_h(px(max_h))
                    .rounded_md()
                    .bg(theme.sidebar_bg)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_y_scroll()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(list),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.open_picker = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
    }
}

/// Whether a family name looks monospaced enough for the terminal grid.
fn is_monospace_name(name: &str) -> bool {
    const HINTS: [&str; 16] = [
        "mono",
        "code",
        "menlo",
        "monaco",
        "courier",
        "consolas",
        "hack",
        "iosevka",
        "jetbrains",
        "fira",
        "cascadia",
        "sf mono",
        "source code",
        "ubuntu mono",
        "andale",
        "plex mono",
    ];
    let lower = name.to_ascii_lowercase();
    HINTS.iter().any(|hint| lower.contains(hint))
}

impl Render for AppearancesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div()
            .id("l3s-theme-list")
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .pt_3()
            .pb_4()
            .flex_1()
            .overflow_y_scroll();

        list = list.child(self.section_heading("COLOR SCHEME"));
        if self.appearance.themes.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_size(px(self.fonts().secondary))
                    .text_color(theme.muted)
                    .child("No themes registered."),
            );
        }
        for info in self.appearance.themes.clone() {
            list = list.child(self.theme_row(&info, cx));
        }

        list = list
            .child(self.section_heading("FONTS"))
            .child(self.font_row(FontField::Ui, "Interface", cx))
            .child(self.font_row(FontField::Terminal, "Terminal", cx));

        let root = div()
            .id("l3s-appearances")
            .relative()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &CloseTab, window, _| window.remove_window()));
        let mut root = super::apply_chrome_font(root, &self.shared_settings).child(list);
        if let Some(field) = self.open_picker {
            root = root.child(self.picker_overlay(field, window, cx));
        }
        root
    }
}

/// Open the Appearances window on the daemon's control socket.
pub fn open(
    api_socket: PathBuf,
    shared_settings: Rc<RefCell<Settings>>,
    config_root: PathBuf,
    cx: &mut App,
) {
    let bounds = gpui::Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // A real (opaque) titlebar, so the window title is the system one
            // instead of a heading we paint ourselves.
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Appearances".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            cx.new(move |cx| {
                AppearancesView::new(api_socket, config_root, shared_settings, window, cx)
            })
        },
    );
    if let Err(error) = opened {
        eprintln!("l3s: could not open appearances: {error}");
    }
}
