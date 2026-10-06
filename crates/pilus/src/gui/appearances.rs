//! The native Appearances window: pick a Luvus theme and the fonts.
//!
//! It is a thin view over the daemon's theme registry (`theme.list` /
//! `theme.use`, both on a background thread) plus the client-owned font
//! settings, which are shared live with the main window and persisted.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, App, Bounds, Context, Div, FocusHandle, KeyDownEvent, MouseButton, Render,
    Window, WindowBounds, WindowOptions, anchored, canvas, deferred, div, point, prelude::*, px,
    size, svg,
};
use pilus_client::{Appearance, ControlClient, ThemeInfo};

use super::style::{self, ButtonKind, FillExt};
use super::theme::Theme;
use super::{CloseTab, CloseWindow, scheme};
use crate::settings::Settings;

/// Width and height of the appearances window.
const WINDOW_SIZE: (f32, f32) = (640.0, 600.0);

/// Smallest and largest selectable font size.
const FONT_SIZE_RANGE: (f32, f32) = (9.0, 32.0);

/// The label for the platform default chrome font, which is stored as `None`.
const SYSTEM_FONT_LABEL: &str = "System";

/// Which font a control edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FontField {
    /// The chrome/interface font.
    Ui,
    /// The monospace terminal font.
    Terminal,
}

impl FontField {
    fn index(self) -> usize {
        match self {
            Self::Ui => 0,
            Self::Terminal => 1,
        }
    }
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
    rx: Receiver<pilus_client::Result<Appearance>>,
    tx: Sender<pilus_client::Result<Appearance>>,
    select_rx: Receiver<String>,
    select_tx: Sender<String>,
    open_picker: Option<FontField>,
    /// Each trigger's last painted bounds, in window coordinates, so the
    /// dropdown lines up with the control rather than with the click.
    trigger_bounds: [Rc<Cell<Bounds<gpui::Pixels>>>; 2],
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
            trigger_bounds: [
                Rc::new(Cell::new(Bounds::default())),
                Rc::new(Cell::new(Bounds::default())),
            ],
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
        self.stage(cx);
        self.settings.save(&self.config_root);
    }

    /// Share a change with the main window without touching disk. Dragging
    /// the opacity slider stages every step live and commits on mouse-up,
    /// so a sweep does not rewrite the settings file on every tick.
    fn stage(&mut self, cx: &mut Context<Self>) {
        *self.shared_settings.borrow_mut() = self.settings.clone();
        cx.notify();
    }

    /// Stage an opacity step from the slider; the mouse-up commit persists it.
    fn set_opacity(&mut self, value: f32, cx: &mut Context<Self>) {
        // The top zone stops at 97.5%; snapping it restores fully opaque so
        // the inset card can come back.
        self.settings.opacity = if value >= 0.975 {
            1.0
        } else {
            value.clamp(0.1, 1.0)
        };
        self.stage(cx);
    }

    /// Step the opacity by `delta`, reaching both ends exactly so 100% is
    /// always one tap away no matter where the slider zones land.
    fn adjust_opacity(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.opacity = (self.settings.glass_opacity() + delta).clamp(0.1, 1.0);
        self.publish(cx);
    }

    /// Stage a blur step from the slider; the mouse-up commit persists it.
    /// The top zone stops at 97.5%, so it snaps to full blur like opacity.
    fn set_blur(&mut self, value: f32, cx: &mut Context<Self>) {
        self.settings.blur = if value >= 0.975 {
            1.0
        } else {
            value.clamp(0.0, 1.0)
        };
        self.stage(cx);
    }

    /// Step the blur by `delta`, reaching both ends exactly: 0% drops the
    /// blur view so the desktop shows sharp.
    fn adjust_blur(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.blur = (self.settings.blur.clamp(0.0, 1.0) + delta).clamp(0.0, 1.0);
        self.publish(cx);
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
                .unwrap_or_else(|| SYSTEM_FONT_LABEL.to_string()),
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
            .text_size(px(self.fonts().label))
            .text_color(theme.muted)
            .child(info.appearance.clone());

        let mut row = div()
            .id(format!("pilus-theme-{}", info.id))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .state_fill(bg, theme.panel, style::RADIUS_MD)
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
                svg()
                    .data(include_bytes!("../../assets/icons/check.svg"))
                    .w(px(14.0))
                    .h(px(14.0))
                    .flex_shrink_0()
                    .text_color(theme.accent),
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
        style::small_button(
            id,
            label,
            ButtonKind::Secondary,
            self.theme,
            self.fonts().secondary,
        )
        .w(px(style::CONTROL_HEIGHT_SMALL))
        .px_0()
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
    }

    fn font_row(
        &self,
        field: FontField,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let family = self.family(field);
        let size = self.size(field);
        let key = match field {
            FontField::Ui => "ui",
            FontField::Terminal => "term",
        };
        let open = self.open_picker == Some(field);
        let measured = self.trigger_bounds[field.index()].clone();

        let trigger = div()
            .id(format!("pilus-font-{key}-pick"))
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_1()
            .h(px(style::CONTROL_HEIGHT_SMALL))
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .state_fill(
                if open { theme.panel } else { theme.panel_dim },
                theme.panel,
                style::RADIUS_MD,
            )
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                if this.open_picker == Some(field) {
                    this.open_picker = None;
                } else {
                    this.open_picker = Some(field);
                }
                cx.notify();
            }))
            .child(
                canvas(move |bounds, _, _| measured.set(bounds), |_, (), _, _| {})
                    .absolute()
                    .size_full(),
            )
            .when(open, |this| {
                this.child(
                    // Pinned to the trigger's corner: left in the flex row, the
                    // anchored child would be centered by `items_center`.
                    div()
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .child(self.picker_panel(field, window, cx)),
                )
            })
            .child(div().overflow_hidden().child(family))
            .child(
                svg()
                    .data(include_bytes!("../../assets/icons/chevron-down.svg"))
                    .w(px(12.0))
                    .h(px(12.0))
                    .flex_shrink_0()
                    .text_color(theme.muted),
            );

        let smaller = self.step_button(
            format!("pilus-font-{key}-smaller"),
            "−".to_string(),
            cx,
            move |this, cx| this.adjust_size(field, -1.0, cx),
        );
        let larger = self.step_button(
            format!("pilus-font-{key}-larger"),
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

    /// The window-opacity row: a slider with steppers and a live percentage.
    fn opacity_row(&self, cx: &mut Context<Self>) -> Div {
        let value = self.settings.glass_opacity();
        let smaller = self.step_button(
            "pilus-opacity-smaller".to_string(),
            "−".to_string(),
            cx,
            |this, cx| this.adjust_opacity(-0.05, cx),
        );
        let larger = self.step_button(
            "pilus-opacity-larger".to_string(),
            "+".to_string(),
            cx,
            |this, cx| this.adjust_opacity(0.05, cx),
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .text_size(px(self.fonts().secondary))
            .text_color(self.theme.fg)
            .child(
                div()
                    .w(px(84.0))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(self.theme.muted)
                    .child("Opacity".to_string()),
            )
            .child(
                div().flex_1().child(
                    style::slider("pilus-opacity", value, self.theme, cx, |this, step, cx| {
                        this.set_opacity(step, cx);
                    })
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.publish(cx)),
                    ),
                ),
            )
            .child(smaller)
            .child(
                div()
                    .w(px(44.0))
                    .text_center()
                    .child(format!("{:.0}%", value * 100.0)),
            )
            .child(larger)
    }

    /// The blur-strength row: a slider with steppers and a live percentage.
    /// Zero drops the blur view so the desktop shows sharp.
    fn blur_row(&self, cx: &mut Context<Self>) -> Div {
        let value = self.settings.blur.clamp(0.0, 1.0);
        let smaller = self.step_button(
            "pilus-blur-smaller".to_string(),
            "−".to_string(),
            cx,
            |this, cx| this.adjust_blur(-0.05, cx),
        );
        let larger = self.step_button(
            "pilus-blur-larger".to_string(),
            "+".to_string(),
            cx,
            |this, cx| this.adjust_blur(0.05, cx),
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .text_size(px(self.fonts().secondary))
            .text_color(self.theme.fg)
            .child(
                div()
                    .w(px(84.0))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(self.theme.muted)
                    .child("Blur".to_string()),
            )
            .child(
                div().flex_1().child(
                    style::slider("pilus-blur", value, self.theme, cx, |this, step, cx| {
                        this.set_blur(step, cx);
                    })
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.publish(cx)),
                    ),
                ),
            )
            .child(smaller)
            .child(
                div()
                    .w(px(44.0))
                    .text_center()
                    .child(format!("{:.0}%", value * 100.0)),
            )
            .child(larger)
    }

    /// A section title, matching the weight of the list headings below.
    fn section_heading(&self, label: &str) -> Div {
        style::section_label(self.theme, label.to_string(), self.fonts().label)
            .px_3()
            .pt_3()
            .pb_1()
            .whitespace_nowrap()
    }

    /// A dropdown list of every available family for one field, anchored to
    /// the trigger it is a child of so window-coordinate drift cannot move it.
    fn picker_panel(
        &self,
        field: FontField,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let current = self.family(field);
        let names = font_options(field, self.families(field));

        let mut list = div().flex().flex_col().p_1();
        for name in names {
            let selected = name == current;
            let id = name.clone();
            list = list.child(
                div()
                    .id(format!("pilus-font-option-{name}"))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(self.fonts().secondary))
                    .text_color(if selected { theme.accent } else { theme.fg })
                    .hover_fill(theme.panel, style::RADIUS_SM)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match field {
                            FontField::Ui => {
                                this.settings.ui_font = if id == SYSTEM_FONT_LABEL {
                                    None
                                } else {
                                    Some(id.clone())
                                };
                            }
                            FontField::Terminal => this.settings.terminal_font = id.clone(),
                        }
                        this.open_picker = None;
                        this.publish(cx);
                    }))
                    .child(name),
            );
        }

        // Drop below the trigger, or above it when there is more room there.
        let trigger = self.trigger_bounds[field.index()].get();
        let vh = f32::from(window.viewport_size().height);
        let ty = f32::from(trigger.origin.y);
        let th = f32::from(trigger.size.height);
        let panel_w = f32::from(trigger.size.width).max(240.0);
        let gap = 4.0;
        let space_up = (ty - gap - 8.0).max(0.0);
        let space_down = (vh - ty - th - gap - 8.0).max(0.0);
        let open_up = space_down < 160.0 && space_up > space_down;
        let max_h = (if open_up { space_up } else { space_down }).clamp(80.0, 300.0);
        let (corner, offset) = if open_up {
            (Anchor::BottomLeft, point(px(0.0), px(-gap)))
        } else {
            (Anchor::TopLeft, point(px(0.0), px(th + gap)))
        };

        deferred(
            anchored()
                .anchor(corner)
                .offset(offset)
                .snap_to_window_with_margin(px(8.0))
                .child(style::enter(
                    "appearances-picker-enter",
                    div()
                        .id("pilus-font-picker")
                        .w(px(panel_w))
                        .max_h(px(max_h))
                        .rounded_md()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_y_scroll()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(list),
                )),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// Full-window scrim: a click outside the open dropdown closes it.
    fn picker_scrim(&self, cx: &mut Context<Self>) -> Div {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
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

/// The family choices offered for `field`.
///
/// The UI list leads with the platform default so a custom pick can always be
/// undone; a real family named "System" is dropped rather than shadowing it.
fn font_options(field: FontField, families: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    if field == FontField::Ui {
        names.push(SYSTEM_FONT_LABEL.to_string());
    }
    names.extend(
        families
            .iter()
            .filter(|name| field != FontField::Ui || name.as_str() != SYSTEM_FONT_LABEL)
            .cloned(),
    );
    names
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
            .id("pilus-theme-list")
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
            .child(self.font_row(FontField::Ui, "Interface", window, cx))
            .child(self.font_row(FontField::Terminal, "Terminal", window, cx));

        list = list
            .child(self.section_heading("TRANSPARENCY"))
            .child(self.opacity_row(cx))
            .child(self.blur_row(cx));

        let root = div()
            .id("pilus-appearances")
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
        if self.open_picker.is_some() {
            root = root.child(self.picker_scrim(cx));
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
    let bounds = Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
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
        eprintln!("pilus: could not open appearances: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_font_options_lead_with_system() {
        let families = vec!["Menlo".to_string(), "Inter".to_string()];
        assert_eq!(
            font_options(FontField::Ui, &families),
            ["System", "Menlo", "Inter"]
        );
    }

    #[test]
    fn a_real_system_family_does_not_shadow_the_reset_entry() {
        let families = vec!["System".to_string(), "Menlo".to_string()];
        assert_eq!(font_options(FontField::Ui, &families), ["System", "Menlo"]);
    }

    #[test]
    fn terminal_fonts_have_no_default_alias() {
        let families = vec!["JetBrains Mono".to_string()];
        assert_eq!(
            font_options(FontField::Terminal, &families),
            ["JetBrains Mono"]
        );
    }
}
