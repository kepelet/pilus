//! Shared chrome tokens and the control builders every surface draws from.
//!
//! The UI Playground is the visual contract for pilus chrome; these builders are
//! the single implementation of it, so a button or field cannot drift between
//! windows. Sizes here are design sizes at the default Interface size and are
//! scaled to the user's setting by [`scaled`].

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, Context, Div, ElementId, Font, FontWeight, IntoElement, Length,
    MouseMoveEvent, Path, PathBuilder, Pixels, Point, Render, Stateful, canvas, div, font, point,
    prelude::*, px,
};

use crate::settings::{CHROME_FONT, Settings};

use super::theme::Theme;

/// How long a skeleton takes to fade from dim to lit and back. Long enough to
/// read as breathing rather than a blink, and it respects reduce motion
/// through [`AnimationExt::with_animation`].
const SKELETON_PULSE: Duration = Duration::from_millis(1400);

/// Base chrome text: body copy, rows, and editable control values.
pub(super) const TEXT_BODY: f32 = 13.0;
/// Headlines: the focal line of an empty state.
pub(super) const TEXT_HEADLINE: f32 = 20.0;
/// Secondary text: footer, row detail, and feedback messages.
pub(super) const TEXT_SECONDARY: f32 = 12.0;
/// Section labels and small action text.
pub(super) const TEXT_LABEL: f32 = 11.0;
/// Regular control height: buttons and field boxes.
pub(super) const CONTROL_HEIGHT: f32 = 28.0;
/// Compact control height: dense rows and section-header actions.
pub(super) const CONTROL_HEIGHT_SMALL: f32 = 24.0;
/// Click/drag zones splitting a slider track, so a press lands on 5% steps
/// without any coordinate math.
const SLIDER_SEGMENTS: usize = 20;
/// The context gauge's outer diameter at the default Interface size.
pub(super) const GAUGE_SIZE: f32 = 16.0;

/// Scale a design-space chrome size to the user's Interface size.
pub(super) fn scaled(settings: &Rc<RefCell<Settings>>, design: f32) -> f32 {
    (settings.borrow().ui_font_size + (design - CHROME_FONT)).max(8.0)
}

/// The type scale a window renders with, resolved once per frame.
#[derive(Clone, Copy)]
pub(super) struct Fonts {
    /// Headlines: the focal line of an empty state.
    pub(super) headline: f32,
    /// Body text.
    pub(super) body: f32,
    /// Secondary text, one step below the body.
    pub(super) secondary: f32,
    /// Section labels and small action text, two steps below the body.
    pub(super) label: f32,
}

impl Fonts {
    /// Resolve the scale from the shared settings.
    pub(super) fn resolve(settings: &Rc<RefCell<Settings>>) -> Self {
        Self {
            headline: scaled(settings, TEXT_HEADLINE),
            body: scaled(settings, TEXT_BODY),
            secondary: scaled(settings, TEXT_SECONDARY),
            label: scaled(settings, TEXT_LABEL),
        }
    }
}

/// The chrome font family, or the platform default when the setting is unset.
///
/// Text drawn through GPUI's shaper needs the family explicitly; a window's
/// `font_family` style does not reach [`gpui::ShapedLine`].
pub(super) fn chrome_font(settings: &Rc<RefCell<Settings>>) -> Font {
    match settings.borrow().ui_font.as_ref() {
        Some(family) => font(family.clone()),
        None => Font::default(),
    }
}

/// The monospace family for machine text: paths, branches, and cwds.
pub(super) fn mono_family(settings: &Rc<RefCell<Settings>>) -> String {
    settings.borrow().terminal_font.clone()
}

/// A section label: small, bold, and muted. Labels are uppercase by convention.
pub(super) fn section_label(theme: Theme, text: impl IntoElement, size: f32) -> Div {
    div()
        .text_size(px(size))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.muted)
        .child(text)
}

/// The catalogue's button treatments.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ButtonKind {
    /// Accent-filled and bold: the one affirmative action in a surface.
    Primary,
    /// Panel-dim with a border: an alternate action next to a primary.
    Secondary,
    /// Transparent and muted: cancel, remove, and other quiet actions.
    Ghost,
    /// Panel-dim with alert text: destructive.
    Danger,
}

/// A regular-height chrome button. Callers attach their own handler.
pub(super) fn button(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    button_at(id, label, kind, theme, text_size, CONTROL_HEIGHT, px(12.0))
}

/// A compact-height chrome button for dense rows and section headers.
pub(super) fn small_button(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    button_at(
        id,
        label,
        kind,
        theme,
        text_size,
        CONTROL_HEIGHT_SMALL,
        px(8.0),
    )
}

/// A compact on/off switch. Callers attach their own handler.
pub(super) fn toggle(id: impl Into<ElementId>, on: bool, theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .w(px(38.0))
        .h(px(20.0))
        .p(px(2.0))
        .rounded_full()
        .bg(if on { theme.accent } else { theme.border })
        .cursor_pointer()
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .rounded_full()
                .bg(if on { theme.window_bg } else { theme.muted })
                .ml(px(if on { 18.0 } else { 0.0 })),
        )
}

/// A horizontal amount slider: a filled track with a knob, reporting 5%
/// steps. Zones cover the track so both a click and a press-drag land
/// without coordinate math; `set` stages the value and the caller commits
/// (e.g. persists) on mouse-up, which it chains onto the returned element.
pub(super) fn slider<V: Render>(
    id: &str,
    value: f32,
    theme: Theme,
    cx: &mut Context<V>,
    set: impl Fn(&mut V, f32, &mut Context<V>) + Clone + 'static,
) -> Stateful<Div> {
    let value = value.clamp(0.0, 1.0);
    // The fill and the remainder split the track by grow factor, so no
    // percentage widths are needed.
    let track = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .h(px(6.0))
        .rounded_full()
        .bg(theme.panel_dim)
        .child(
            div()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .rounded_full()
                .bg(theme.accent)
                .flex_grow(value)
                .child(
                    div()
                        .w(px(10.0))
                        .h(px(10.0))
                        .rounded_full()
                        .bg(theme.panel_fg)
                        .mr(px(2.0)),
                ),
        )
        .child(div().h_full().flex_grow(1.0 - value));
    let mut zones = div().absolute().inset_0().flex().flex_row();
    for index in 0..SLIDER_SEGMENTS {
        let step = (index as f32 + 0.5) / SLIDER_SEGMENTS as f32;
        let set = set.clone();
        let drag = set.clone();
        zones = zones.child(
            div()
                .id(format!("{id}-zone-{index}"))
                .flex_1()
                .h_full()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| set(this, step, cx)))
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if event.dragging() {
                        drag(this, step, cx);
                    }
                })),
        );
    }
    div()
        .id(id.to_string())
        .flex()
        .flex_row()
        .items_center()
        .h(px(CONTROL_HEIGHT_SMALL))
        .w_full()
        .child(track.child(zones))
}

/// A compact donut gauge for context occupancy: a track ring with an accent
/// arc for the used fraction. The caller pairs it with the token counts.
///
/// `percent` is on the 0–100 scale pi reports; `None` paints the empty track,
/// the state before pi re-counts after a compaction. `warns` turns the arc
/// `theme.alert`. `size` comes from [`scaled`] so the gauge follows the
/// Interface setting.
pub(super) fn context_gauge(
    id: impl Into<ElementId>,
    percent: Option<f32>,
    warns: bool,
    theme: Theme,
    size: f32,
) -> impl IntoElement {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let fraction = percent.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
    let arc = if warns { theme.alert } else { theme.accent };
    let stroke = (size / 7.0).max(1.5);
    let radius = (size - stroke) / 2.0;
    div().id(id).w(px(size)).h(px(size)).flex_shrink_0().child(
        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, (), window, _cx| {
                let center = point(
                    bounds.origin.x + px(bounds.size.width.as_f32() / 2.0),
                    bounds.origin.y + px(bounds.size.height.as_f32() / 2.0),
                );
                if let Some(track) = ring_arc(center, radius, -FRAC_PI_2, TAU, stroke) {
                    window.paint_path(track, theme.border);
                }
                if fraction > 0.0
                    && let Some(path) = ring_arc(center, radius, -FRAC_PI_2, TAU * fraction, stroke)
                {
                    window.paint_path(path, arc);
                }
            },
        )
        .size_full(),
    )
}

/// A stroked circular arc: `sweep` radians clockwise from `start` (0 is the
/// 3 o'clock position), or the full ring when the sweep closes the circle.
fn ring_arc(
    center: Point<Pixels>,
    radius: f32,
    start: f32,
    sweep: f32,
    stroke: f32,
) -> Option<Path<Pixels>> {
    use std::f32::consts::{PI, TAU};
    let at = |angle: f32| {
        point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        )
    };
    let radii = point(px(radius), px(radius));
    let mut builder = PathBuilder::stroke(px(stroke));
    builder.move_to(at(start));
    if sweep >= TAU {
        builder.arc_to(radii, px(0.0), false, true, at(start + PI));
        builder.arc_to(radii, px(0.0), false, true, at(start));
    } else {
        builder.arc_to(radii, px(0.0), sweep > PI, true, at(start + sweep));
    }
    builder.build().ok()
}

/// One button at an explicit height and horizontal padding.
fn button_at(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
    height: f32,
    padding: Pixels,
) -> Stateful<Div> {
    let (bg, fg, border) = button_colors(kind, theme);
    let hover_bg = match kind {
        ButtonKind::Ghost => theme.panel_dim,
        _ => theme.panel,
    };
    let hover_fg = match kind {
        ButtonKind::Danger => theme.alert,
        _ => theme.panel_fg,
    };
    let bold = matches!(kind, ButtonKind::Primary);
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(height))
        .px(padding)
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .text_size(px(text_size))
        .font_weight(if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .child(label)
}

/// The background, foreground, and border triple for a button treatment.
fn button_colors(kind: ButtonKind, theme: Theme) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    match kind {
        ButtonKind::Primary => (theme.accent, theme.window_bg, theme.accent),
        ButtonKind::Secondary => (theme.panel_dim, theme.fg, theme.border),
        ButtonKind::Ghost => (
            theme.panel.opacity(0.0),
            theme.muted,
            theme.panel.opacity(0.0),
        ),
        ButtonKind::Danger => (theme.panel_dim, theme.alert, theme.border),
    }
}

/// A placeholder bar for content that has not arrived yet.
///
/// A surface whose real content is still loading shows one of these instead of
/// a fallback label: the shape reserves the space the text will take, so
/// nothing jumps when it lands. `height` follows the text it stands in for,
/// and the width is the caller's because only the caller knows how much of
/// the line the content will fill.
pub(super) fn skeleton(
    id: impl Into<ElementId>,
    theme: Theme,
    width: impl Into<Length> + Clone,
    height: f32,
) -> impl IntoElement {
    div()
        .w(width)
        .h(px(height))
        .rounded_full()
        .bg(theme.panel_dim)
        .with_animation(
            id,
            Animation::new(SKELETON_PULSE).repeat(),
            |this, phase| this.opacity(0.55 + 0.45 * phase),
        )
}

/// One labelled field: a small muted label above a boxed control.
///
/// The caller supplies the editable body and owns focus and click behavior.
/// `height` is a minimum, so a multi-line body can grow.
pub(super) fn field(
    id: impl Into<ElementId>,
    label: &str,
    focused: bool,
    theme: Theme,
    height: f32,
    label_size: f32,
    body: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(label_size))
                .text_color(theme.muted)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .min_h(px(height))
                .px_2()
                .rounded_md()
                .bg(theme.panel_dim)
                .border_1()
                .border_color(if focused { theme.accent } else { theme.border })
                .cursor_pointer()
                .child(body),
        )
}
