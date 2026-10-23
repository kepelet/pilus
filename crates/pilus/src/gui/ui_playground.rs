//! The UI Playground window: a working catalogue of every native element pilus
//! draws.
//!
//! Unlike a static gallery, the controls here actually work: the inputs take
//! typing, the dropdowns open and select, the tabs switch, and the buttons and
//! switches react. It runs against the active theme, so a palette change shows
//! up everywhere at once without touching live daemon data.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    Anchor, App, Bounds, Context, Div, Entity, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    Render, Window, WindowBounds, WindowOptions, anchored, deferred, div, point, prelude::*, px,
    size,
};

use crate::settings::Settings;

use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::text_input::TextInput;
use super::theme::Theme;
use super::toast::ToastKind;
use super::{CloseTab, CloseWindow, icon};

/// Width and height of the playground window.
const WINDOW_SIZE: (f32, f32) = (820.0, 740.0);
/// The smallest the window can be dragged to: below this its rows and
/// controls start to clip.
const WINDOW_MIN_SIZE: (f32, f32) = (640.0, 480.0);

/// Every icon the app vendors, so the gallery renders the real set.
const ICONS: &[(&str, &[u8])] = &[
    (
        "arrow-up",
        include_bytes!("../../assets/icons/arrow-up.svg"),
    ),
    (
        "chevron-down",
        include_bytes!("../../assets/icons/chevron-down.svg"),
    ),
    (
        "chevron-right",
        include_bytes!("../../assets/icons/chevron-right.svg"),
    ),
    ("circle", include_bytes!("../../assets/icons/circle.svg")),
    ("clock", include_bytes!("../../assets/icons/clock.svg")),
    ("cloud", include_bytes!("../../assets/icons/cloud.svg")),
    (
        "corner-down-right",
        include_bytes!("../../assets/icons/corner-down-right.svg"),
    ),
    ("cpu", include_bytes!("../../assets/icons/cpu.svg")),
    (
        "external-link",
        include_bytes!("../../assets/icons/external-link.svg"),
    ),
    ("forward", include_bytes!("../../assets/icons/forward.svg")),
    (
        "ethernet-port",
        include_bytes!("../../assets/icons/ethernet-port.svg"),
    ),
    ("folder", include_bytes!("../../assets/icons/folder.svg")),
    (
        "folder-git",
        include_bytes!("../../assets/icons/folder-git.svg"),
    ),
    (
        "activity",
        include_bytes!("../../assets/icons/activity.svg"),
    ),
    ("gauge", include_bytes!("../../assets/icons/gauge.svg")),
    ("house", include_bytes!("../../assets/icons/house.svg")),
    (
        "layout-panel-left",
        include_bytes!("../../assets/icons/layout-panel-left.svg"),
    ),
    (
        "list-checks",
        include_bytes!("../../assets/icons/list-checks.svg"),
    ),
    ("monitor", include_bytes!("../../assets/icons/monitor.svg")),
    (
        "panel-left",
        include_bytes!("../../assets/icons/panel-left.svg"),
    ),
    ("plus", include_bytes!("../../assets/icons/plus.svg")),
    ("server", include_bytes!("../../assets/icons/server.svg")),
    (
        "settings",
        include_bytes!("../../assets/icons/settings.svg"),
    ),
    (
        "workflow",
        include_bytes!("../../assets/icons/workflow.svg"),
    ),
    ("x", include_bytes!("../../assets/icons/x.svg")),
    (
        "agent: claude",
        include_bytes!("../../assets/icons/agents/claude-code.svg"),
    ),
    (
        "agent: codex",
        include_bytes!("../../assets/icons/agents/codex.svg"),
    ),
    (
        "agent: gemini",
        include_bytes!("../../assets/icons/agents/gemini.svg"),
    ),
    (
        "agent: pi",
        include_bytes!("../../assets/icons/agents/pi.svg"),
    ),
    (
        "agent: cursor",
        include_bytes!("../../assets/icons/agents/cursor.svg"),
    ),
    (
        "agent: bot",
        include_bytes!("../../assets/icons/agents/bot.svg"),
    ),
];

/// Which demo input owns the keyboard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputId {
    Single,
    Multi,
    /// Masked, and kept off the clipboard: an API key.
    Secret,
}

/// One working dropdown in the gallery.
struct Picker {
    icon: &'static [u8],
    options: Vec<&'static str>,
    selected: usize,
}

/// The playground window root view.
pub struct UiPlaygroundView {
    theme: Theme,
    /// Shared client settings, so the Interface font reaches this window.
    settings: Rc<RefCell<Settings>>,
    focus_handle: FocusHandle,
    single: Entity<TextInput>,
    multi: Entity<TextInput>,
    secret: Entity<TextInput>,
    search: Entity<TextInput>,
    pickers: Vec<Picker>,
    open_picker: Option<usize>,
    /// The picker an outside press just dismissed, so the chip's own click can
    /// tell a toggle-off from a fresh open.
    dismissed_picker: Option<usize>,
    tab: usize,
    toggle: bool,
    checked: bool,
    slider: f32,
    collapsed: bool,
    confirm: bool,
    clicks: u32,
    status: String,
}

impl UiPlaygroundView {
    pub(super) fn new(
        theme: Theme,
        settings: Rc<RefCell<Settings>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let fonts = style::Fonts::resolve(&settings);
        let font = style::chrome_font(&settings);
        let single = cx.new(|cx| {
            TextInput::new(
                "click and type".to_string(),
                "Type…",
                font.clone(),
                px(fonts.body),
                false,
                theme,
                cx,
            )
        });
        let multi = cx.new(|cx| {
            TextInput::new(
                "A multi-line\ntextarea.".to_string(),
                "Type…",
                font.clone(),
                px(fonts.body),
                true,
                theme,
                cx,
            )
        });
        let secret = cx.new(|cx| {
            TextInput::new(
                "sk-demo-0123456789".to_string(),
                "Paste your key",
                font.clone(),
                px(fonts.body),
                false,
                theme,
                cx,
            )
            .secret()
        });
        let search = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Filter files…",
                font.clone(),
                px(fonts.body),
                false,
                theme,
                cx,
            )
        });
        let input_handle = single.focus_handle(cx);
        window.focus(&input_handle, cx);
        Self {
            theme,
            settings,
            focus_handle,
            single,
            multi,
            secret,
            search,
            pickers: vec![
                Picker {
                    icon: include_bytes!("../../assets/icons/cpu.svg"),
                    options: vec!["Default", "DeepSeek V4.1 Flash", "Muse Spark 1.3"],
                    selected: 1,
                },
                Picker {
                    icon: include_bytes!("../../assets/icons/gauge.svg"),
                    options: vec!["Default", "Low", "Medium", "High"],
                    selected: 3,
                },
            ],
            open_picker: None,
            dismissed_picker: None,
            tab: 0,
            toggle: true,
            checked: false,
            slider: 0.85,
            collapsed: false,
            confirm: false,
            clicks: 0,
            status: "Everything here is live: type, open the dropdowns, switch tabs.".to_string(),
        }
    }

    /// Record the last interaction in the footer.
    fn note(&mut self, text: String, cx: &mut Context<Self>) {
        self.status = text;
        cx.notify();
    }

    /// The window's type scale, resolved from the shared settings.
    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.settings)
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            window.remove_window();
            return;
        }
        // Tab walks the inputs, so each is reachable by keyboard. Everything
        // else is the focused field's own business, which consumes what it
        // handles before it reaches here.
        if key == "tab" {
            let inputs = [&self.single, &self.multi, &self.secret];
            let at = inputs
                .iter()
                .position(|input| input.read(cx).is_focused(window));
            let next = inputs[at.map_or(0, |at| (at + 1) % inputs.len())].clone();
            let handle = next.focus_handle(cx);
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    /// One titled group in the catalogue.
    fn section(&self, title: &'static str, body: impl IntoElement) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(style::section_label(theme, title, self.fonts().label))
            .child(body)
    }

    /// A bordered surface holding a row of controls.
    fn tray(&self, body: impl IntoElement) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .w_full()
            .p_4()
            .radius_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .child(body)
    }

    /// One button in the requested treatment.
    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        kind: ButtonKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        style::button(id, label, kind, self.theme, self.fonts().secondary).on_click(cx.listener(
            move |this, _, _, cx| match kind {
                ButtonKind::Primary => {
                    this.clicks = this.clicks.saturating_add(1);
                    this.note(format!("Primary clicked {} times", this.clicks), cx);
                }
                ButtonKind::Secondary => {
                    this.toggle = !this.toggle;
                    this.note(
                        format!("Toggle is now {}", if this.toggle { "on" } else { "off" }),
                        cx,
                    );
                }
                ButtonKind::Ghost => this.note("Ghost clicked".to_string(), cx),
                ButtonKind::Danger => this.note("Danger clicked".to_string(), cx),
            },
        ))
    }

    /// The circular icon-only send button.
    /// The icon-only controls of a dense row: a quiet one, a plain one, and
    /// the destructive one.
    fn icon_buttons(&self) -> impl IntoElement {
        let theme = self.theme;
        let size = style::scaled(&self.settings, 24.0);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(style::icon_button(
                "pg-icon-ghost",
                include_bytes!("../../assets/icons/external-link.svg"),
                ButtonKind::Ghost,
                theme,
                size,
            ))
            .child(style::icon_button(
                "pg-icon-secondary",
                include_bytes!("../../assets/icons/forward.svg"),
                ButtonKind::Secondary,
                theme,
                size,
            ))
            .child(style::icon_button(
                "pg-icon-danger",
                include_bytes!("../../assets/icons/trash-2.svg"),
                ButtonKind::Danger,
                theme,
                size,
            ))
    }

    /// The strip the focused pane wears over its top-right corner.
    fn pane_toolbar(&self) -> impl IntoElement {
        let theme = self.theme;
        let size = style::scaled(&self.settings, style::PANE_TOOLBAR_BUTTON);
        style::pane_toolbar()
            .child(style::icon_button(
                "pg-pane-split-right",
                include_bytes!("../../assets/icons/columns-2.svg"),
                ButtonKind::Ghost,
                theme,
                size,
            ))
            .child(style::icon_button(
                "pg-pane-split-down",
                include_bytes!("../../assets/icons/rows-2.svg"),
                ButtonKind::Ghost,
                theme,
                size,
            ))
            .child(style::icon_button(
                "pg-pane-zoom",
                include_bytes!("../../assets/icons/maximize.svg"),
                ButtonKind::Ghost,
                theme,
                size,
            ))
            .child(style::icon_button(
                "pg-pane-close",
                include_bytes!("../../assets/icons/x.svg"),
                ButtonKind::Danger,
                theme,
                size,
            ))
    }

    /// Bordered pills: one that is itself a button (the sidebar's session
    /// picker) and one holding icon buttons (the top chrome's right toolbar).
    fn pill_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let raise = theme.sidebar_raise(1.0, 0.5);
        let button = |id: &'static str, glyph: &'static [u8]| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(24.0))
                .h(px(20.0))
                .radius_sm()
                .cursor_pointer()
                .hover_fill(raise, style::RADIUS_SM)
                .child(icon(glyph, 14.0, theme.muted))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .p_3()
            .radius_md()
            .bg(theme.window_bg)
            .border_1()
            .border_color(theme.border)
            .child(
                style::pill(theme, Some(raise))
                    .id("pg-pill-session")
                    .gap_2()
                    .h(px(26.0))
                    .px_2()
                    .cursor_pointer()
                    .text_size(px(fonts.body))
                    .child(div().text_color(theme.panel_fg).child("main"))
                    .child(icon(
                        include_bytes!("../../assets/icons/chevron-down.svg"),
                        12.0,
                        theme.muted,
                    )),
            )
            .child(
                style::pill(theme, None)
                    .p(px(2.0))
                    .child(button(
                        "pg-pill-browser",
                        include_bytes!("../../assets/icons/globe.svg"),
                    ))
                    .child(button(
                        "pg-pill-ports",
                        include_bytes!("../../assets/icons/ethernet-port.svg"),
                    ))
                    .child(button(
                        "pg-pill-panel",
                        include_bytes!("../../assets/icons/panel-right.svg"),
                    )),
            )
    }

    fn circle_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("pg-send")
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.0))
            .h(px(30.0))
            .radius_full()
            .cursor_pointer()
            .state_fill(theme.panel, theme.border, style::RADIUS_FULL)
            .on_click(cx.listener(|this, _, _, cx| this.note("Send clicked".to_string(), cx)))
            .child(icon(
                include_bytes!("../../assets/icons/arrow-up.svg"),
                15.0,
                theme.fg,
            ))
    }

    /// The label-less search field the Diff page filters its file list with.
    fn search_box(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let fonts = self.fonts();
        let entity = self.search.clone();
        entity.update(cx, |input, _| {
            input.set_style(theme, style::chrome_font(&self.settings), px(fonts.body));
        });
        let focused = entity.read(cx).is_focused(window);
        style::search_field(
            "pg-input-search",
            focused,
            theme,
            style::scaled(&self.settings, style::CONTROL_HEIGHT),
            fonts.body,
            entity,
        )
        .w(px(240.0))
        .text_size(px(fonts.body))
    }

    /// A labelled input box, editable while it owns the keyboard.
    fn input_box(
        &self,
        id: InputId,
        label: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = self.fonts();
        let entity = match id {
            InputId::Single => self.single.clone(),
            InputId::Multi => self.multi.clone(),
            InputId::Secret => self.secret.clone(),
        };
        entity.update(cx, |input, _| {
            input.set_style(theme, style::chrome_font(&self.settings), px(fonts.body));
        });
        let focused = entity.read(cx).is_focused(window);
        let multiline = id == InputId::Multi;
        let build = if multiline {
            style::textarea
        } else {
            style::field
        };
        build(
            match id {
                InputId::Single => "pg-input-single",
                InputId::Multi => "pg-input-multi",
                InputId::Secret => "pg-input-secret",
            },
            label,
            focused,
            theme,
            if multiline { 84.0 } else { 30.0 },
            fonts.label,
            entity,
        )
        .w(px(240.0))
        .text_size(px(fonts.body))
    }

    /// One chip in the interactive dropdown row.
    fn picker_chip(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let picker = &self.pickers[index];
        let value = picker
            .options
            .get(picker.selected)
            .copied()
            .unwrap_or_default();
        let glyph = picker.icon;
        let active = self.open_picker == Some(index);
        let fg = if active { theme.panel_fg } else { theme.muted };
        let chip = div()
            .id(format!("pg-picker-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(24.0))
            .px_2()
            .radius_md()
            .text_color(fg)
            .cursor_pointer()
            .state_fill(
                if active {
                    theme.panel
                } else {
                    theme.panel.opacity(0.0)
                },
                theme.panel,
                style::RADIUS_MD,
            )
            .hover(move |style| style.text_color(theme.panel_fg))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.dismissed_picker.take() == Some(index) {
                    // An outside press already closed this chip; stay closed.
                } else {
                    this.open_picker = (this.open_picker != Some(index)).then_some(index);
                }
                cx.notify();
            }));
        chip.child(icon(glyph, 14.0, fg))
            .child(
                div()
                    .max_w(px(180.0))
                    .truncate()
                    .text_size(px(self.fonts().secondary))
                    .text_color(fg)
                    .child(value),
            )
            .child(icon(
                include_bytes!("../../assets/icons/chevron-down.svg"),
                12.0,
                theme.muted,
            ))
    }

    /// The open picker's option list, shown under the chip row.
    fn picker_options(&self, cx: &mut Context<Self>) -> Div {
        let Some(index) = self.open_picker else {
            return div();
        };
        let theme = self.theme;
        let options = self.pickers[index].options.clone();
        let selected = self.pickers[index].selected;
        let mut list = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .w(px(220.0))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.open_picker.is_some() {
                    this.dismissed_picker = this.open_picker;
                    this.open_picker = None;
                } else {
                    this.dismissed_picker = None;
                }
                cx.notify();
            }))
            .radius_lg()
            .bg(theme.bg)
            .border_1()
            .border_color(theme.border);
        for (option_index, option) in options.into_iter().enumerate() {
            let active = option_index == selected;
            list = list.child(
                div()
                    .id(format!("pg-picker-{index}-{option_index}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(28.0))
                    .px_3()
                    .radius_md()
                    .text_color(theme.panel_fg)
                    .text_size(px(self.fonts().body))
                    .font_weight(if active {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .cursor_pointer()
                    .state_fill(
                        if active { theme.panel } else { theme.bg },
                        theme.panel,
                        style::RADIUS_MD,
                    )
                    .child(option)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pickers[index].selected = option_index;
                        this.open_picker = None;
                        this.dismissed_picker = None;
                        this.note(format!("Selected {option}"), cx);
                    })),
            );
        }
        list
    }

    /// One working tab pill.
    fn tab(&self, index: usize, label: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        style::tab(
            format!("pg-tab-{index}"),
            label,
            self.tab == index,
            self.theme,
            self.fonts().body,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.tab = index;
            this.note(format!("{label} tab selected"), cx);
        }))
    }

    /// A working on/off switch.
    fn toggle_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        style::toggle("pg-switch", self.toggle, self.theme).on_click(cx.listener(
            |this, _, _, cx| {
                this.toggle = !this.toggle;
                let on = this.toggle;
                this.note(format!("Switch {}", if on { "on" } else { "off" }), cx);
            },
        ))
    }

    /// A working amount slider.
    fn slider_demo(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.slider;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w_full()
            .child(div().flex_1().child(style::slider(
                "pg-slider",
                value,
                self.theme,
                cx,
                |this, step, cx| {
                    this.slider = step;
                    this.note(format!("Slider {:.0}%", step * 100.0), cx);
                },
            )))
            .child(
                div()
                    .w(px(44.0))
                    .text_right()
                    .text_size(px(self.fonts().body))
                    .text_color(self.theme.fg)
                    .child(format!("{:.0}%", value * 100.0)),
            )
    }

    /// The context gauge and its companion token counts across states:
    /// unknown, empty, partial, warning, and full occupancy.
    /// Meters at the readings Mission Control draws: unknown, partial, and
    /// past the compaction threshold.
    fn meter_demo(&self) -> Div {
        let theme = self.theme;
        let height = style::scaled(&self.settings, 6.0);
        let cases: [(&str, Option<f32>, bool); 4] = [
            ("unknown", None, false),
            ("28%", Some(0.28), false),
            ("60%", Some(0.6), false),
            ("91% · compact soon", Some(0.91), true),
        ];
        let mut column = div().flex().flex_col().gap_3().w_full();
        for (name, fraction, warns) in cases {
            column = column.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(160.0)).flex_shrink_0().child(style::meter(
                        format!("pg-meter-{name}"),
                        fraction,
                        if warns { theme.alert } else { theme.accent },
                        theme,
                        height,
                    )))
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(if warns { theme.alert } else { theme.muted })
                            .child(name.to_string()),
                    ),
            );
        }
        column
    }

    /// The Mission Control metrics row: one tile per headline figure.
    /// A QR code, as the pairing card draws it.
    fn qr_demo(&self) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .child(style::qr_code(
                "pilus://pair/endpointaaaabbbbccccddddeeeeffff?code=AB12-CD34-EF56-7890",
                176.0,
            ))
            .child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .text_color(self.theme.muted)
                    .child(
                        "Always dark on light, whatever the theme: a scanner needs the contrast.",
                    ),
            )
    }

    fn stat_tile_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        style::stat_strip(theme)
            .w_full()
            .child(style::stat_tile(
                theme,
                fonts,
                "AGENTS",
                "04",
                theme.accent,
                "3 live · 4 total",
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "BLOCKED",
                "01",
                theme.alert,
                "awaiting response",
            ))
            .child(
                style::stat_tile(theme, fonts, "SPEND", "$25.16", theme.ok, "$1.20/hr")
                    .border_r_0(),
            )
    }

    /// The Stats page's heatmap squares at every shade, and its ranking rows.
    fn heat_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let mut cells = div().flex().flex_row().items_center().gap_1();
        for level in 0..style::HEAT_LEVELS {
            cells = cells.child(style::heat_cell(
                format!("pg-heat-{level}"),
                level,
                style::scaled(&self.settings, 12.0),
                theme,
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(320.0))
            .child(cells)
            .child(style::rank_row(
                "pg-rank-a",
                theme,
                fonts,
                "claude-sonnet-5-5",
                "1.5B tokens · 197 sessions",
                1.0,
            ))
            .child(style::rank_row(
                "pg-rank-b",
                theme,
                fonts,
                "deepseek-v4.1-flash",
                "810M tokens · 90 sessions",
                0.54,
            ))
    }

    fn empty_state_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        style::empty_state(
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
        .w_full()
        .py_4()
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(style::button(
                    "playground-empty-install",
                    "Install Luvus",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                ))
                .child(style::button_enabled(
                    "playground-empty-installing",
                    "Installing…",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                    false,
                ))
                .child(style::button(
                    "playground-empty-recheck",
                    "Check again",
                    ButtonKind::Secondary,
                    theme,
                    fonts.secondary,
                )),
        )
    }

    fn context_gauge_demo(&self) -> Div {
        let theme = self.theme;
        let size = style::scaled(&self.settings, style::GAUGE_SIZE);
        let cases: [(&str, Option<f32>, bool, &str); 5] = [
            ("unknown", None, false, "–/1M"),
            ("empty", Some(0.0), false, "0/1M"),
            ("partial", Some(42.0), false, "421k/1M"),
            ("warning", Some(82.0), true, "860k/1M"),
            ("full", Some(100.0), true, "1M/1M"),
        ];
        let mut row = div().flex().flex_row().items_end().gap_4();
        for (name, percent, warns, counts) in cases {
            let color = if warns { theme.alert } else { theme.muted };
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(style::context_gauge(
                                format!("pg-gauge-{name}"),
                                percent,
                                warns,
                                theme,
                                size,
                            ))
                            .child(
                                div()
                                    .text_size(px(self.fonts().label))
                                    .text_color(color)
                                    .whitespace_nowrap()
                                    .child(counts.to_string()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(name.to_string()),
                    ),
            );
        }
        row
    }

    /// A working checkbox.
    fn checkbox(&self, cx: &mut Context<Self>) -> impl IntoElement {
        style::checkbox(
            "pg-checkbox",
            self.checked,
            "Checkbox",
            self.theme,
            self.fonts().body,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.checked = !this.checked;
            let checked = this.checked;
            this.note(
                format!("Checkbox {}", if checked { "on" } else { "off" }),
                cx,
            );
        }))
    }

    /// The color swatch grid.
    fn colors(&self) -> Div {
        let theme = self.theme;
        let entries: [(&str, gpui::Rgba); 15] = [
            ("window_bg", theme.window_bg),
            ("bg", theme.bg),
            ("sidebar_bg", theme.sidebar_bg),
            ("panel", theme.panel),
            ("panel_dim", theme.panel_dim),
            ("border", theme.border),
            ("fg", theme.fg),
            ("panel_fg", theme.panel_fg),
            ("muted", theme.muted),
            ("accent", theme.accent),
            ("branch", theme.branch),
            ("ok", theme.ok),
            ("alert", theme.alert),
            ("selection_bg", theme.selection_bg),
            ("selection_fg", theme.selection_fg),
        ];
        let mut grid = div().flex().flex_row().flex_wrap().gap_3();
        for (name, color) in entries {
            grid = grid.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(88.0))
                    .child(
                        div()
                            .w_full()
                            .h(px(36.0))
                            .radius_md()
                            .bg(color)
                            .border_1()
                            .border_color(theme.border),
                    )
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(name),
                    ),
            );
        }
        grid
    }

    /// The sixteen named ANSI terminal colors the grid resolves against.
    fn ansi_colors(&self) -> Div {
        let theme = self.theme;
        const NAMES: [&str; 16] = [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "white",
            "br black",
            "br red",
            "br green",
            "br yellow",
            "br blue",
            "br magenta",
            "br cyan",
            "br white",
        ];
        let mut grid = div().flex().flex_row().flex_wrap().gap_3();
        for (index, name) in NAMES.iter().enumerate() {
            grid = grid.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(88.0))
                    .child(
                        div()
                            .w_full()
                            .h(px(36.0))
                            .radius_md()
                            .bg(theme.ansi[index])
                            .border_1()
                            .border_color(theme.border),
                    )
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(*name),
                    ),
            );
        }
        grid
    }

    /// A representative saved-machine row.
    fn machine_row(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w_full()
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
                            .text_size(px(self.fonts().body))
                            .text_color(theme.fg)
                            .child("mac-mini"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child("alex@mac-mini")
                            .child(div().text_color(theme.ok).child("online")),
                    ),
            )
            .child(
                div()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.fg)
                    .child("Disable"),
            )
            .child(
                div()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child("Remove"),
            )
    }

    /// A representative sidebar navigation row.
    fn nav_row(&self, label: &'static str, active: bool) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .radius_md()
            .bg(if active {
                theme.panel
            } else {
                theme.window_bg.opacity(0.0)
            })
            .text_size(px(self.fonts().body))
            .text_color(if active { theme.panel_fg } else { theme.muted })
            .child(icon(
                include_bytes!("../../assets/icons/layout-panel-left.svg"),
                14.0,
                theme.muted,
            ))
            .child(label)
    }

    /// The badge and dot treatments.
    fn badges(&self) -> Div {
        let theme = self.theme;
        let letter = div()
            .flex()
            .items_center()
            .justify_center()
            .w(px(20.0))
            .h(px(20.0))
            .radius_sm()
            .bg(theme.border)
            .text_size(px(self.fonts().label))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.fg)
            .child("L");
        let nav_badge = div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(16.0))
            .min_w(px(16.0))
            .px_1()
            .radius_full()
            .bg(theme.accent)
            .text_size(px(self.fonts().label))
            .text_color(theme.window_bg)
            .child("3");
        let dot = |color: gpui::Rgba, label: &'static str, theme: Theme| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(icon(
                    include_bytes!("../../assets/icons/circle.svg"),
                    10.0,
                    color,
                ))
                .child(
                    div()
                        .text_size(px(self.fonts().label))
                        .text_color(theme.muted)
                        .child(label),
                )
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .child(letter)
            .child(nav_badge)
            .child(dot(theme.ok, "ok", theme))
            .child(dot(theme.alert, "alert", theme))
            .child(dot(theme.muted, "muted", theme))
    }

    /// The tab glyph tile: one card for a lone pane, a deck for several,
    /// capped at three cards; then the fill an agent's status gives it
    /// (working, blocked, done).
    fn glyph_tiles(&self) -> Div {
        let theme = self.theme;
        let terminal = include_bytes!("../../assets/icons/terminal.svg").as_slice();
        let mut row = div().flex().flex_row().items_center().gap_6();
        for panes in [1, 2, 3, 5] {
            row = row.child(style::glyph_tile(theme, terminal, panes, None));
        }
        for status in [theme.accent, theme.alert, theme.ok] {
            row = row.child(style::glyph_tile(theme, terminal, 1, Some(status)));
        }
        row
    }

    /// The task status glyphs, each tinted by its role, beside the chip that
    /// qualifies a task row.
    fn task_status_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let mut row = div().flex().flex_row().flex_wrap().items_center().gap_4();
        for status in pilus_client::TaskStatus::ALL {
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(super::tasks::status_glyph(status, fonts.body, theme))
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.panel_fg)
                            .child(status.label()),
                    ),
            );
        }
        row.child(style::chip(theme, "after t1", fonts.label))
            .child(style::chip(theme, "src/auth/** +2", fonts.label))
    }

    /// Board cards joined by connectors: a waiting link, a settled
    /// (dashed) one, and the selected card's accent ring.
    fn board_demo(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let (card_w, card_h, gap) = (150.0, 56.0, 64.0);
        let card = |id: &'static str, label: &'static str, selected: bool, x: f32, y: f32| {
            style::board_card(id, selected, theme)
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(card_w))
                .h(px(card_h))
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(fonts.body))
                        .text_color(theme.panel_fg)
                        .child(label),
                )
        };
        let muted = gpui::Hsla::from(theme.muted);
        let mid = card_h / 2.0;
        let right = card_w + gap;
        let link =
            |from: (f32, f32), to: (f32, f32), color: gpui::Hsla, dashed: bool| style::Connector {
                from,
                from_side: style::Side::Right,
                to,
                to_side: style::Side::Left,
                color,
                dashed,
            };
        div()
            .relative()
            .w(px(2.0 * card_w + gap))
            .h(px(2.0 * card_h + 16.0))
            .child(style::connectors(
                vec![
                    link((card_w, mid), (right, mid), muted.opacity(0.75), false),
                    link(
                        (card_w, card_h + 16.0 + mid),
                        (right, mid),
                        muted.opacity(0.35),
                        true,
                    ),
                ],
                1.5,
            ))
            .child(card("pg-board-a", "Waiting link", false, 0.0, 0.0))
            .child(card(
                "pg-board-b",
                "Settled link",
                false,
                0.0,
                card_h + 16.0,
            ))
            .child(card("pg-board-c", "Selected", true, right, 0.0))
    }

    /// The vendored icon grid.
    fn icon_grid(&self) -> Div {
        let theme = self.theme;
        let mut grid = div().flex().flex_row().flex_wrap().gap_3();
        for (name, bytes) in ICONS {
            grid = grid.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .w(px(84.0))
                    .child(icon(bytes, 18.0, theme.muted))
                    .child(
                        div()
                            .text_size(px(style::scaled(&self.settings, 9.0)))
                            .text_color(theme.muted)
                            .child(*name),
                    ),
            );
        }
        grid
    }

    /// Cards, boxes, and the composer's attached footer shape.
    fn containers(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(210.0))
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child("Composer card + footer"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(40.0))
                                    .px_3()
                                    .radius_t_lg()
                                    .bg(theme.panel.opacity(0.0))
                                    .border_t_1()
                                    .border_l_1()
                                    .border_r_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .text_size(px(self.fonts().label))
                                            .text_color(theme.muted)
                                            .child("composer"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .px_3()
                                    .py_2()
                                    .bg(theme.window_bg)
                                    .border_l_1()
                                    .border_r_1()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .radius_b_lg()
                                    .child(
                                        div()
                                            .text_size(px(self.fonts().label))
                                            .text_color(theme.muted)
                                            .child("footer"),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(210.0))
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child("Panel box"),
                    )
                    .child(
                        div()
                            .h(px(64.0))
                            .w_full()
                            .radius_md()
                            .bg(theme.panel)
                            .border_1()
                            .border_color(theme.border),
                    ),
            )
    }

    /// Hairline dividers, including the section-header rule.
    fn divider_demos(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_size(px(self.fonts().label))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted)
                    .child("SECTION")
                    .child(div().flex_1().h(px(1.0)).bg(theme.border)),
            )
            .child(div().w_full().h(px(1.0)).bg(theme.border))
            .child(div().w_full().h(px(1.0)).bg(theme.border))
    }

    /// A sidebar section header with a collapse toggle and add button.
    fn section_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let collapsed = self.collapsed;
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(22.0))
            .pl_2()
            .mr(px(-12.0))
            .child(
                div()
                    .id("pg-section-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .cursor_pointer()
                    .text_size(px(self.fonts().label))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted)
                    .hover(move |style| style.text_color(theme.panel_fg))
                    .child(icon(
                        if collapsed {
                            include_bytes!("../../assets/icons/chevron-right.svg")
                        } else {
                            include_bytes!("../../assets/icons/chevron-down.svg")
                        },
                        12.0,
                        theme.muted,
                    ))
                    .child("WORKSPACES")
                    .child(div().flex_1().h(px(1.0)).bg(theme.border))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("pg-section-add")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(22.0))
                    .w(px(22.0))
                    .radius_md()
                    .cursor_pointer()
                    .hover_fill(theme.panel, style::RADIUS_MD)
                    .child(icon(
                        include_bytes!("../../assets/icons/plus.svg"),
                        11.0,
                        theme.muted,
                    ))
                    .on_click(
                        cx.listener(|this, _, _, cx| this.note("Add workspace".to_string(), cx)),
                    ),
            )
    }

    /// A workspace row with its branch badge and cwd line.
    fn workspace_row_demo(&self, active: bool) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .px_2()
            .py_1()
            .radius_md()
            .bg(if active {
                theme.panel
            } else {
                theme.window_bg.opacity(0.0)
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .child(icon(
                        include_bytes!("../../assets/icons/folder.svg"),
                        14.0,
                        theme.panel_fg,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(self.fonts().body))
                            .text_color(theme.panel_fg)
                            .font_weight(if active {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child("quiet-comet"),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.branch)
                            .child("main"),
                    ),
            )
            .child(
                div()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child("~/Developer/pilus"),
            )
    }

    /// A file tree: an open folder with a nested closed one, then files, with
    /// the Git marks a changed file and the folders above it carry.
    fn tree_demo(&self) -> Div {
        let theme = self.theme;
        let row = |id: &'static str,
                   depth: u16,
                   name: &'static str,
                   folder: Option<bool>,
                   mark: Option<style::TreeMark>| {
            style::tree_row(
                id,
                style::TreeEntry {
                    depth,
                    name: name.into(),
                    folder,
                    animate: false,
                    mark,
                    selected: false,
                },
                theme,
                &self.settings,
            )
        };
        let status = |letter: &'static str, color| style::TreeMark::Status(letter.into(), color);
        div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(280.0))
            .child(row(
                "pg-tree-crates",
                0,
                "crates",
                Some(true),
                Some(style::TreeMark::Dot(theme.branch)),
            ))
            .child(row("pg-tree-client", 1, "pilus-client", Some(false), None))
            .child(row(
                "pg-tree-gui",
                1,
                "pilus",
                Some(true),
                Some(style::TreeMark::Dot(theme.branch)),
            ))
            .child(row(
                "pg-tree-main",
                2,
                "main.rs",
                None,
                Some(status("M", theme.branch)),
            ))
            .child(row(
                "pg-tree-new",
                2,
                "files_tab.rs",
                None,
                Some(status("?", theme.ok)),
            ))
            .child(row("pg-tree-readme", 0, "README.md", None, None))
    }

    /// An agent row with its brand icon and status. A working agent's
    /// spinner turns on its own clock wherever the status is drawn.
    fn agent_row_demo(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .radius_md()
            .child(icon(
                include_bytes!("../../assets/icons/agents/pi.svg"),
                14.0,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_size(px(self.fonts().body))
                    .text_color(theme.panel_fg)
                    .child("quiet-comet"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.accent)
                    .child(style::spinner(
                        "playground-agent-spinner",
                        10.0,
                        theme.accent,
                    ))
                    .child("working"),
            )
    }

    /// The command palette: query field and result rows.
    fn palette_demo(&self) -> Div {
        let theme = self.theme;
        let row = |title: &'static str,
                   group: &'static str,
                   shortcut: &'static str,
                   active: bool,
                   theme: Theme| {
            let (bg, fg, dim) = if active {
                (
                    theme.selection_bg,
                    theme.selection_fg,
                    theme.selection_fg.opacity(0.75),
                )
            } else {
                (theme.sidebar_bg, theme.panel_fg, theme.muted)
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .h(px(30.0))
                .px_3()
                .radius_md()
                .bg(bg)
                .child(
                    div()
                        .flex_1()
                        .text_size(px(self.fonts().body))
                        .text_color(fg)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(self.fonts().label))
                        .text_color(dim)
                        .child(group),
                )
                .child(
                    div()
                        .text_size(px(self.fonts().label))
                        .text_color(dim)
                        .child(shortcut),
                )
        };
        div()
            .flex()
            .flex_col()
            .w(px(360.0))
            .radius_lg()
            .bg(theme.sidebar_bg)
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(40.0))
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(self.fonts().body))
                    .child(div().w(px(1.5)).h(px(16.0)).bg(theme.accent))
                    .child(
                        div()
                            .pl(px(2.0))
                            .text_color(theme.muted)
                            .child("Type a command…"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .child(row("New Tab", "Session", "⌘T", true, theme))
                    .child(row("Open Settings", "Application", "⌘⇧.", false, theme)),
            )
    }

    /// The Open Workspace picker: path bar, rows, and footer.
    fn picker_demo(&self) -> Div {
        let theme = self.theme;
        let row = |name: &'static str, bytes: &'static [u8], active: bool, theme: Theme| {
            let (bg, fg) = if active {
                (theme.selection_bg, theme.selection_fg)
            } else {
                (theme.sidebar_bg, theme.panel_fg)
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(28.0))
                .px_2()
                .radius_md()
                .bg(bg)
                .text_size(px(self.fonts().body))
                .text_color(fg)
                .child(icon(bytes, 14.0, theme.muted))
                .child(name)
        };
        div()
            .flex()
            .flex_col()
            .w(px(360.0))
            .radius_lg()
            .bg(theme.bg)
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(38.0))
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(self.fonts().body))
                    .text_color(theme.fg)
                    .child("~/Developer"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .child(row(
                        "pilus",
                        include_bytes!("../../assets/icons/folder.svg"),
                        true,
                        theme,
                    ))
                    .child(row(
                        "luvus",
                        include_bytes!("../../assets/icons/folder.svg"),
                        false,
                        theme,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(self.fonts().secondary))
                            .text_color(theme.muted)
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(26.0))
                            .px_3()
                            .radius_md()
                            .bg(theme.accent)
                            .text_color(theme.window_bg)
                            .text_size(px(self.fonts().secondary))
                            .font_weight(FontWeight::BOLD)
                            .child("Open"),
                    ),
            )
    }

    /// The status bar: session, breadcrumb, status, and right icon actions.
    fn status_bar_demo(&self) -> Div {
        let theme = self.theme;
        let square = |id: &'static str, bytes: &'static [u8], theme: Theme| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .h(px(28.0))
                .w(px(28.0))
                .radius_md()
                .child(icon(bytes, 14.0, theme.muted))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(28.0))
            .px_3()
            .text_size(px(self.fonts().label))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().text_color(theme.panel_fg).child("default"))
                            .child(icon(
                                include_bytes!("../../assets/icons/chevron-down.svg"),
                                12.0,
                                theme.muted,
                            )),
                    )
                    .child(div().text_color(theme.muted).child("~/Developer/pilus"))
                    .child(div().text_color(theme.muted).child("connected")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(28.0))
                            .px_2()
                            .radius_md()
                            .text_color(theme.accent)
                            .child("Update available"),
                    )
                    .child(square(
                        "pg-status-machines",
                        include_bytes!("../../assets/icons/monitor.svg"),
                        theme,
                    ))
                    .child(square(
                        "pg-status-settings",
                        include_bytes!("../../assets/icons/settings.svg"),
                        theme,
                    )),
            )
    }

    /// A hover tooltip card.
    fn tooltip_demo(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(260.0))
            .px_3()
            .py_2()
            .radius_md()
            .bg(theme.sidebar_bg)
            .child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.panel_fg)
                    .child("quiet-comet"),
            )
            .child(
                div()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child("Directory: ~/Developer/pilus"),
            )
            .child(
                div()
                    .text_size(px(self.fonts().label))
                    .text_color(theme.branch)
                    .child("Branch: quiet-comet"),
            )
    }

    /// Empty, error, loading, and shortcut treatments.
    fn feedback_demos(&self) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_4()
            .child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .text_color(theme.muted)
                    .child("No workspaces yet."),
            )
            .child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .text_color(theme.alert)
                    .child("couldn't open workspace"),
            )
            .child(
                div()
                    .text_size(px(self.fonts().secondary))
                    .text_color(theme.muted)
                    .child("Loading…"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(220.0))
                    .child(style::skeleton(
                        "playground-skeleton-title",
                        theme,
                        px(180.0),
                        self.fonts().body,
                    ))
                    .child(style::skeleton(
                        "playground-skeleton-line",
                        theme,
                        px(120.0),
                        self.fonts().secondary,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(20.0))
                    .px_2()
                    .radius_sm()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child("⌘⇧P"),
            )
    }

    /// The in-app toast in each kind, with and without a title. Real toasts
    /// stack at the window's bottom-right and fade out on their own; here they
    /// rest so the card can be judged.
    fn toast_demos(&self) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_3()
            .child(style::toast(
                "pg-toast-info",
                ToastKind::Info,
                Some("quiet-comet"),
                "Build finished in 42s",
                theme,
                fonts,
            ))
            .child(style::toast(
                "pg-toast-success",
                ToastKind::Success,
                None,
                "Opened the workspace",
                theme,
                fonts,
            ))
            .child(style::toast(
                "pg-toast-error",
                ToastKind::Error,
                None,
                "Couldn't open workspace: the folder no longer exists on this machine",
                theme,
                fonts,
            ))
    }

    /// Small icon and text action buttons, including a two-step confirm.
    fn small_buttons(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let confirm = self.confirm;
        let fonts = self.fonts();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(style::small_button(
                "pg-small-primary",
                "New Widget",
                ButtonKind::Primary,
                theme,
                fonts.secondary,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(24.0))
                    .w(px(24.0))
                    .radius_md()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(theme.border)
                    .child(icon(
                        include_bytes!("../../assets/icons/plus.svg"),
                        12.0,
                        theme.muted,
                    )),
            )
            .child(
                style::small_button(
                    "pg-confirm",
                    if confirm { "Confirm" } else { "Remove" },
                    ButtonKind::Ghost,
                    theme,
                    fonts.label,
                )
                .text_color(if confirm { theme.alert } else { theme.muted })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.confirm = !this.confirm;
                    let armed = this.confirm;
                    this.note(
                        if armed {
                            "Remove armed".to_string()
                        } else {
                            "Remove cancelled".to_string()
                        },
                        cx,
                    );
                })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(24.0))
                    .px_2()
                    .radius_md()
                    .text_size(px(fonts.label))
                    .text_color(theme.fg)
                    .child("Disable"),
            )
    }
}

impl Render for UiPlaygroundView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let clicks = self.clicks;
        let body = div()
            .flex()
            .flex_col()
            .gap_6()
            .p_6()
            .child(self.section("COLORS", self.colors()))
            .child(self.section("ANSI", self.ansi_colors()))
            .child(self.section("CONTAINERS", self.containers()))
            .child(self.section("PILL", self.pill_demo()))
            .child(self.section("DIVIDERS", self.divider_demos()))
            .child(
                self.section(
                    "BUTTONS",
                    self.tray(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .child(self.button("pg-primary", "Primary", ButtonKind::Primary, cx))
                            .child(self.button(
                                "pg-secondary",
                                "Secondary",
                                ButtonKind::Secondary,
                                cx,
                            ))
                            .child(self.button("pg-ghost", "Ghost", ButtonKind::Ghost, cx))
                            .child(self.button("pg-danger", "Danger", ButtonKind::Danger, cx))
                            .child(self.circle_button(cx))
                            .child(self.icon_buttons())
                            .child(self.pane_toolbar())
                            .child(
                                div()
                                    .text_size(px(self.fonts().label))
                                    .text_color(theme.muted)
                                    .child(format!("primary clicks: {clicks}")),
                            ),
                    ),
                ),
            )
            .child(
                self.section(
                    "INPUTS",
                    self.tray(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_start()
                            .gap_4()
                            .child(self.input_box(InputId::Single, "Single line", window, cx))
                            .child(self.input_box(InputId::Multi, "Multi line", window, cx))
                            .child(self.input_box(InputId::Secret, "Secret", window, cx))
                            .child(self.search_box(window, cx)),
                    ),
                ),
            )
            .child(
                self.section(
                    "DROPDOWNS",
                    self.tray(
                        div()
                            .relative()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .child(self.picker_chip(0, cx))
                            .child(self.picker_chip(1, cx))
                            .when(self.open_picker.is_some(), |row| {
                                row.child(
                                    div().absolute().top(px(0.0)).left(px(0.0)).child(
                                        deferred(
                                            anchored()
                                                .anchor(Anchor::TopLeft)
                                                .offset(point(px(0.0), px(28.0)))
                                                .snap_to_window_with_margin(px(8.0))
                                                .child(style::enter(
                                                    "playground-picker-enter",
                                                    self.picker_options(cx),
                                                )),
                                        )
                                        .with_priority(1),
                                    ),
                                )
                            }),
                    ),
                ),
            )
            .child(
                self.section(
                    "PAGE MENU",
                    self.tray(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap_3()
                            .child(style::icon_button(
                                "pg-page-menu",
                                include_bytes!("../../assets/icons/ellipsis-vertical.svg"),
                                ButtonKind::Ghost,
                                theme,
                                style::scaled(&self.settings, style::CONTROL_HEIGHT_SMALL),
                            ))
                            .child(
                                super::picker_menu("pg-page-menu-list", theme)
                                    .w(px(style::scaled(&self.settings, 150.0)))
                                    .children(["Refresh", "Sources", "Close"].map(|label| {
                                        super::picker_row(
                                            format!("pg-page-menu-{label}"),
                                            label.to_string(),
                                            false,
                                            theme,
                                            self.fonts().body,
                                            |_, _, _| {},
                                        )
                                    })),
                            ),
                    ),
                ),
            )
            .child(
                self.section(
                    "TABS",
                    self.tray(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(self.tab(0, "Overview", cx))
                            .child(self.tab(1, "Changes", cx))
                            .child(style::tab_disabled(
                                "pg-tab-disabled",
                                "History",
                                self.theme,
                                self.fonts().body,
                            )),
                    ),
                ),
            )
            .child(
                self.section(
                    "TOGGLES",
                    self.tray(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_6()
                            .child(self.toggle_switch(cx))
                            .child(self.checkbox(cx)),
                    ),
                ),
            )
            .child(self.section("SLIDER", self.tray(self.slider_demo(cx))))
            .child(self.section("CONTEXT GAUGE", self.tray(self.context_gauge_demo())))
            .child(self.section("METER", self.tray(self.meter_demo())))
            .child(self.section("STAT TILE", self.tray(self.stat_tile_demo())))
            .child(self.section("HEATMAP AND RANKING", self.tray(self.heat_demo())))
            .child(self.section("EMPTY STATE", self.tray(self.empty_state_demo())))
            .child(self.section("QR CODE", self.tray(self.qr_demo())))
            .child(
                self.section(
                    "SIDEBAR",
                    self.tray(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .w_full()
                            .child(self.section_header(cx))
                            .child(self.workspace_row_demo(true))
                            .child(self.workspace_row_demo(false))
                            .child(self.agent_row_demo()),
                    ),
                ),
            )
            .child(self.section("FILE TREE", self.tray(self.tree_demo())))
            .child(self.section("COMMAND PALETTE", self.tray(self.palette_demo())))
            .child(self.section("WORKSPACE PICKER", self.tray(self.picker_demo())))
            .child(self.section("STATUS BAR", self.tray(self.status_bar_demo())))
            .child(self.section("TOOLTIP", self.tray(self.tooltip_demo())))
            .child(self.section("FEEDBACK", self.tray(self.feedback_demos())))
            .child(self.section("TOAST", self.tray(self.toast_demos())))
            .child(self.section("SMALL BUTTONS", self.tray(self.small_buttons(cx))))
            .child(self.section("BADGES & DOTS", self.tray(self.badges())))
            .child(self.section("TAB GLYPH", self.tray(self.glyph_tiles())))
            .child(self.section("TASK STATUS & CHIPS", self.tray(self.task_status_demo())))
            .child(self.section("BOARD", self.tray(self.board_demo())))
            .child(
                self.section(
                    "ROWS",
                    self.tray(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .w_full()
                            .child(self.machine_row())
                            .child(self.nav_row("Mission Control", false))
                            .child(self.nav_row("Orchestration", true)),
                    ),
                ),
            )
            .child(
                self.section(
                    "TYPOGRAPHY",
                    self.tray(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(self.fonts().headline))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.fg)
                                    .child("Headline text"),
                            )
                            .child(style::title(theme, "Title text", self.fonts().title))
                            .child(style::section_label(
                                theme,
                                "SECTION LABEL",
                                self.fonts().label,
                            ))
                            .child(
                                div()
                                    .text_size(px(self.fonts().body))
                                    .text_color(theme.panel_fg)
                                    .child("Body panel text"),
                            )
                            .child(
                                div()
                                    .text_size(px(self.fonts().secondary))
                                    .text_color(theme.muted)
                                    .child("Muted secondary text"),
                            )
                            .child(
                                div()
                                    .text_size(px(self.fonts().secondary))
                                    .text_color(theme.branch)
                                    .child("branch-name"),
                            )
                            .child(
                                div()
                                    .font_family(style::mono_family(&self.settings))
                                    .text_size(px(self.settings.borrow().terminal_font_size))
                                    .text_color(theme.fg)
                                    .child("monospace 0123456789"),
                            ),
                    ),
                ),
            )
            .child(
                self.section(
                    "SEGMENTED CONTROL",
                    self.tray(
                        style::segmented(theme)
                            .child(style::segment(
                                "pg-seg-view",
                                "View",
                                false,
                                true,
                                theme,
                                self.fonts().secondary,
                            ))
                            .child(style::segment(
                                "pg-seg-control",
                                "Control",
                                true,
                                true,
                                theme,
                                self.fonts().secondary,
                            ))
                            .child(style::segment(
                                "pg-seg-operate",
                                "Operate",
                                false,
                                false,
                                theme,
                                self.fonts().secondary,
                            )),
                    ),
                ),
            )
            .child(
                self.section(
                    "SECTION CARD",
                    div().mx_neg_3().child(
                        style::section_card_tab(theme, "NOTIFICATIONS", self.fonts().label)
                            .gap_2()
                            .child(
                                div()
                                    .px_3()
                                    .text_size(px(self.fonts().secondary))
                                    .text_color(theme.muted)
                                    .child("A page section: panel-dim fill inside a theme border."),
                            ),
                    ),
                ),
            )
            .child(self.section("ICONS", self.tray(self.icon_grid())));

        let root = div()
            .id("pilus-ui-playground")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .child(
                div()
                    .id("pilus-ui-playground-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child(self.status.clone()),
            );
        super::apply_chrome_font(root, &self.settings)
    }
}

/// Open the UI Playground window.
pub fn open(theme: Theme, settings: Rc<RefCell<Settings>>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("UI Playground".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(move |cx| UiPlaygroundView::new(theme, settings, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("pilus: could not open the UI playground: {error}");
    }
}
