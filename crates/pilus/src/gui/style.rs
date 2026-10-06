//! Shared chrome tokens and the control builders every surface draws from.
//!
//! The UI Playground is the visual contract for pilus chrome; these builders are
//! the single implementation of it, so a button or field cannot drift between
//! windows. Sizes here are design sizes at the default Interface size and are
//! scaled to the user's setting by [`scaled`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AbsoluteLength, Animation, AnimationElement, AnimationExt, AnyElement, App, Bounds, Context,
    Corners, DispatchPhase, Div, Edges, Element, ElementId, Font, FontWeight, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, Length,
    MouseMoveEvent, Path, PathBuilder, Pixels, Point, Position, Rems, Render, Rgba, SharedString,
    Stateful, Style, Transformation, Window, canvas, div, font, percentage, point, prelude::*, px,
    svg,
};

use crate::settings::{CHROME_FONT, Settings};

use super::theme::Theme;

/// How long a skeleton takes to fade from dim to lit and back. Long enough to
/// read as breathing rather than a blink, and it respects reduce motion
/// through [`AnimationExt::with_animation`].
const SKELETON_PULSE: Duration = Duration::from_millis(1400);
/// How long a floating panel takes to fade in when it opens: quick enough
/// that a menu never feels slow to answer, long enough to read as motion.
const ENTER: Duration = Duration::from_millis(140);
/// How long a background fill takes to ease into a new hover or selection
/// state. Short enough that a pointer sweeping down a list never trails.
const FILL_EASE: Duration = Duration::from_millis(120);
/// How long a selection pill takes to glide to its new target. A touch
/// longer than a fill fade: the spring's settle needs room to read.
const GLIDE: Duration = Duration::from_millis(340);
/// `rounded_sm`'s radius, for a fill under a dense list row.
pub(super) const RADIUS_SM: AbsoluteLength = AbsoluteLength::Rems(Rems(0.25));
/// `rounded_md`'s radius, for a fill under a control, row, or card.
pub(super) const RADIUS_MD: AbsoluteLength = AbsoluteLength::Rems(Rems(0.375));
/// `rounded_full`'s radius, for a fill under a pill or round button.
pub(super) const RADIUS_FULL: AbsoluteLength = AbsoluteLength::Pixels(px(9999.0));
/// How long a disclosure chevron takes to turn between its two states.
const CHEVRON_TURN: Duration = Duration::from_millis(160);
/// One full turn of the working spinner.
const SPINNER_TURN: Duration = Duration::from_millis(900);
/// The spinner's frame cap: a 7–14px glyph reads as smooth at 30fps, and a
/// sidebar full of working agents should not hold the GPU at the display rate.
const SPINNER_FPS: f32 = 30.0;

/// Base chrome text: body copy, rows, and editable control values.
pub(super) const TEXT_BODY: f32 = 13.0;
/// Headlines: the focal line of an empty state.
pub(super) const TEXT_HEADLINE: f32 = 22.0;
/// Titles: the heading at the top of a window or a floating panel.
pub(super) const TEXT_TITLE: f32 = 15.0;
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
    /// Window and panel titles, between the headline and the body.
    pub(super) title: f32,
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
            title: scaled(settings, TEXT_TITLE),
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

/// A window or panel title: bold body-colored text one step above the body.
pub(super) fn title(theme: Theme, text: impl IntoElement, size: f32) -> Div {
    div()
        .text_size(px(size))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.fg)
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
///
/// Flipping it slides the knob and eases both colors; a switch drawn for the
/// first time (a page opening) appears settled.
pub(super) fn toggle(id: impl Into<ElementId>, on: bool, theme: Theme) -> Stateful<Div> {
    let track = if on { theme.accent } else { theme.border };
    div()
        .id(id)
        .w(px(38.0))
        .h(px(20.0))
        .rounded_full()
        .cursor_pointer()
        .state_fill(track, track, RADIUS_FULL)
        .child(Knob {
            on,
            on_color: theme.window_bg.into(),
            off_color: theme.muted.into(),
        })
}

/// A toggle's knob: an absolute layer over the track that paints the round
/// knob at its eased position.
struct Knob {
    on: bool,
    on_color: Hsla,
    off_color: Hsla,
}

/// The knob's diameter and its inset from the track's edge.
const KNOB: f32 = 16.0;
const KNOB_INSET: f32 = 2.0;

/// A checkbox's edge length at the default Interface size.
const CHECKBOX: f32 = 14.0;

/// A labelled checkbox. Callers attach their own handler.
///
/// The box is two eased fills, an outline and a well inset by a hairline, so
/// checking it eases the edge and the body together instead of snapping a
/// border color. The mark fades in each time it is set.
pub(super) fn checkbox(
    id: impl Into<SharedString>,
    checked: bool,
    label: impl IntoElement,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    let id = id.into();
    let edge = if checked { theme.accent } else { theme.border };
    let well = if checked {
        theme.accent
    } else {
        theme.panel_dim
    };
    let mut inner = div()
        .id("well")
        // After the fill: `state_fill` makes its element relative.
        .state_fill(well, well, RADIUS_SM)
        .absolute()
        .inset(px(1.0))
        .flex()
        .items_center()
        .justify_center();
    if checked {
        inner = inner.child(
            svg()
                .data(include_bytes!("../../assets/icons/check.svg"))
                .w(px(CHECKBOX - 4.0))
                .h(px(CHECKBOX - 4.0))
                .text_color(theme.window_bg)
                .with_animation(
                    ElementId::NamedInteger(id.clone(), 1),
                    Animation::new(FILL_EASE).with_easing(gpui::ease_out_quint()),
                    |mark, delta| mark.opacity(delta),
                ),
        );
    }
    div()
        .id(ElementId::Name(id))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_shrink_0()
        .cursor_pointer()
        .text_size(px(text_size))
        .text_color(theme.fg)
        .child(
            div()
                .id("box")
                .w(px(CHECKBOX))
                .h(px(CHECKBOX))
                .flex_shrink_0()
                .state_fill(edge, edge, RADIUS_SM)
                .child(inner),
        )
        .child(label)
}

/// A knob's slide, kept across frames under its element id.
struct KnobState {
    from: f32,
    to: f32,
    started: Instant,
}

impl IntoElement for Knob {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Knob {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("pilus-knob".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let target = if self.on { 1.0 } else { 0.0 };
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let position = match id {
            Some(id) => window.with_element_state(id, |state: Option<KnobState>, window| {
                let mut state = state.unwrap_or(KnobState {
                    from: target,
                    to: target,
                    started: now,
                });
                let at = |state: &KnobState| {
                    let step =
                        now.duration_since(state.started).as_secs_f32() / FILL_EASE.as_secs_f32();
                    let eased = ease_out_cubic(step.clamp(0.0, 1.0));
                    state.from + (state.to - state.from) * eased
                };
                if state.to != target {
                    state.from = at(&state);
                    state.to = target;
                    state.started = now;
                }
                let position = if reduce_motion { target } else { at(&state) };
                if position != target {
                    window.request_animation_frame();
                }
                (position, state)
            }),
            None => target,
        };
        let travel = f32::from(bounds.size.width) - KNOB - 2.0 * KNOB_INSET;
        let origin = point(
            bounds.origin.x + px(KNOB_INSET + travel.max(0.0) * position),
            bounds.origin.y + (bounds.size.height - px(KNOB)) / 2.0,
        );
        let knob = Bounds::new(origin, gpui::size(px(KNOB), px(KNOB)));
        let color = mix(self.off_color.into(), self.on_color.into(), position);
        window.paint_quad(gpui::fill(knob, color).corner_radii(px(KNOB / 2.0)));
    }
}

/// An absolute layer covering its parent's box.
fn overlay_style() -> Style {
    Style {
        position: Position::Absolute,
        inset: Edges::all(Length::from(px(0.0))),
        ..Style::default()
    }
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
    // Any positive occupancy gets a visible minimum arc: a fraction this
    // small would tessellate to a sub-pixel sliver, so the gauge would read
    // as empty even though the session has used context.
    let sweep = if fraction > 0.0 {
        (TAU * fraction).max(0.14)
    } else {
        0.0
    };
    let arc = if warns { theme.alert } else { theme.accent };
    // The panel's own hairline colour is invisible on the composer fill, so
    // the track borrows the muted text role at low strength.
    let track_color = Hsla::from(theme.muted).opacity(0.28);
    let stroke = (size / 7.0).max(1.75);
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
                    window.paint_path(track, track_color);
                }
                if sweep > 0.0
                    && let Some(path) = ring_arc(center, radius, -FRAC_PI_2, sweep, stroke)
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
        .state_fill(bg, hover_bg, RADIUS_MD)
        .hover(move |style| style.text_color(hover_fg))
        .child(label)
}

/// The background, foreground, and border triple for a button treatment.
fn button_colors(kind: ButtonKind, theme: Theme) -> (Rgba, Rgba, Rgba) {
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

/// Fade a floating panel in as it opens: menus, pickers, the palette,
/// popovers, and disclosed content.
///
/// The animation is keyed by `id` and replays only when the panel is drawn
/// afresh, so a panel that stays open while its contents change never
/// flickers. Opacity alone keeps every layout untouched: the panel sits at
/// its final position from the first frame, so anchoring and hit-testing
/// are exact throughout. Reduce motion skips it.
pub(super) fn enter<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    panel: E,
) -> AnimationElement<E> {
    panel.with_animation(
        id,
        Animation::new(ENTER).with_easing(gpui::ease_out_quint()),
        |panel, delta| panel.opacity(delta),
    )
}

/// Animated backgrounds for anything a pointer or a selection lights up.
///
/// GPUI's `.hover()` swaps a style in a single frame. These paint the fill
/// as an absolute layer under the element's children instead, easing it
/// between states, so hovering a row, moving a selection, or switching tabs
/// glides rather than blinks. Call them before adding children: the layer
/// must be the first child to sit beneath them.
pub(super) trait FillExt: ParentElement + Styled + Sized {
    /// Ease the background to `hover` while the pointer is over it.
    fn hover_fill(self, hover: impl Into<Hsla>, radius: AbsoluteLength) -> Self {
        self.state_fill(gpui::transparent_black(), hover, radius)
    }

    /// Ease between a resting fill and `hover`. The resting fill animates
    /// too, so a row whose selection changes slides its pill in and out.
    /// It replaces the element's own `.bg()`, which would snap underneath.
    fn state_fill(
        self,
        rest: impl Into<Hsla>,
        hover: impl Into<Hsla>,
        radius: AbsoluteLength,
    ) -> Self {
        self.relative().child(Fill {
            rest: rest.into(),
            hover: hover.into(),
            radius,
        })
    }
}

impl<E: ParentElement + Styled> FillExt for E {}

/// The layer [`FillExt`] paints: its parent's whole box, behind the content.
struct Fill {
    rest: Hsla,
    hover: Hsla,
    radius: AbsoluteLength,
}

/// A fill's transition, kept across frames under the layer's element id.
struct FillState {
    from: Rgba,
    to: Rgba,
    started: Instant,
}

impl FillState {
    fn at(&self, now: Instant) -> Rgba {
        let step = now.duration_since(self.started).as_secs_f32() / FILL_EASE.as_secs_f32();
        mix(self.from, self.to, ease_out_cubic(step.clamp(0.0, 1.0)))
    }
}

fn ease_out_cubic(step: f32) -> f32 {
    1.0 - (1.0 - step).powi(3)
}

/// Blend two colors. A transparent end borrows the other end's hue, so a
/// fill fading in from nothing never passes through a grey or black tint.
fn mix(from: Rgba, to: Rgba, step: f32) -> Rgba {
    let from = if from.a == 0.0 {
        Rgba { a: 0.0, ..to }
    } else {
        from
    };
    let to = if to.a == 0.0 {
        Rgba { a: 0.0, ..from }
    } else {
        to
    };
    let lerp = |a: f32, b: f32| a + (b - a) * step;
    Rgba {
        r: lerp(from.r, to.r),
        g: lerp(from.g, to.g),
        b: lerp(from.b, to.b),
        a: lerp(from.a, to.a),
    }
}

impl IntoElement for Fill {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Fill {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        // Scoped under the parent's own id, so one fixed name is unique.
        Some("pilus-fill".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hovered = hitbox.is_hovered(window);
        let target = Rgba::from(if hovered { self.hover } else { self.rest });
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let color = match id {
            Some(id) => window.with_element_state(id, |state: Option<FillState>, window| {
                // A layer seen for the first time starts settled: a row
                // appearing already hovered or selected does not fade up.
                let mut state = state.unwrap_or(FillState {
                    from: target,
                    to: target,
                    started: now,
                });
                if state.to != target {
                    state.from = state.at(now);
                    state.to = target;
                    state.started = now;
                }
                let color = if reduce_motion { target } else { state.at(now) };
                if color != target {
                    window.request_animation_frame();
                }
                (color, state)
            }),
            None => target,
        };
        if color.a > 0.0 {
            let radius = self.radius.to_pixels(window.rem_size());
            window.paint_quad(
                gpui::fill(bounds, color)
                    .corner_radii(Corners::all(radius).clamp_radii_for_quad_size(bounds.size)),
            );
        }
        // Repaint when the pointer crosses the edge, the way `.hover()` does.
        let hitbox = hitbox.clone();
        window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, _cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) != hovered {
                window.refresh();
            }
        });
    }
}

/// A damped spring from 0 to 1: a quick departure, a slight overshoot,
/// and a settle — motion that reads as elastic rather than eased.
///
/// Lands within a hair of 1.0 at `step == 1.0`; callers still snap to the
/// target once their duration has passed.
pub(super) fn ease_spring(step: f32) -> f32 {
    if step >= 1.0 {
        return 1.0;
    }
    let step = step.max(0.0);
    // Damping and frequency are tuned for a ~6% overshoot that settles
    // inside the duration; stiffer reads as a bounce, softer as a lag.
    let decay = (-SPRING_DAMPING * step).exp();
    1.0 - decay
        * ((SPRING_FREQUENCY * step).cos()
            + (SPRING_DAMPING / SPRING_FREQUENCY) * (SPRING_FREQUENCY * step).sin())
}

/// How fast [`ease_spring`]'s oscillation dies away.
const SPRING_DAMPING: f32 = 8.0;
/// [`ease_spring`]'s angular frequency over the unit duration.
const SPRING_FREQUENCY: f32 = 9.0;

/// Where a glide's target sits this frame.
///
/// The target's marker writes its bounds during prepaint; the pill reads
/// them during paint, which GPUI runs after the whole tree has prepainted,
/// so the pill always lands on this frame's layout.
#[derive(Clone, Default)]
pub(super) struct GlideSlot(Rc<Cell<Option<Bounds<Pixels>>>>);

/// Mark the element this is a child of as the glide's target. Add it as a
/// child of the selected item only.
pub(super) fn glide_target(slot: &GlideSlot) -> impl IntoElement {
    GlideMarker(slot.clone())
}

/// A selection pill that slides between targets instead of fading out of
/// one and into the next: the selected tab's raised fill travels from the
/// old tab to the new one on a spring.
///
/// Place it as the first child of the targets' common parent, so it paints
/// beneath them and resets the slot before any marker writes it. A pill
/// seen for the first time starts settled.
pub(super) fn glide(
    id: impl Into<ElementId>,
    slot: &GlideSlot,
    color: Rgba,
    radius: AbsoluteLength,
) -> impl IntoElement {
    Glide {
        id: id.into(),
        slot: slot.clone(),
        color,
        radius,
    }
}

/// The hoverable rows a [`hover_glide`] pill can land on, as they were laid
/// out this frame.
#[derive(Clone, Default)]
pub(super) struct HoverSlot(Rc<RefCell<Vec<HoverRow>>>);

type HoverRow = (Bounds<Pixels>, Hitbox);

/// Mark the element this is a child of as a row the hover pill can land on.
pub(super) fn hover_target(slot: &HoverSlot) -> impl IntoElement {
    HoverMarker(slot.clone())
}

/// One hover highlight shared by a list of rows: it fades in under the row
/// the pointer enters, then flies from row to row on the tab strip's spring,
/// and fades out when the pointer leaves the list.
///
/// Place it as the first child of the rows' common ancestor so it paints
/// beneath them and clears the slot before any marker writes to it.
pub(super) fn hover_glide(
    id: impl Into<ElementId>,
    slot: &HoverSlot,
    color: Rgba,
    radius: AbsoluteLength,
) -> impl IntoElement {
    HoverGlide {
        id: id.into(),
        slot: slot.clone(),
        color,
        radius,
    }
}

struct HoverMarker(HoverSlot);

impl IntoElement for HoverMarker {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HoverMarker {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.0.0.borrow_mut().push((bounds, hitbox));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

struct HoverGlide {
    id: ElementId,
    slot: HoverSlot,
    color: Rgba,
    radius: AbsoluteLength,
}

/// The pill's flight and fade, in coordinates relative to the pill layer's
/// origin so scrolling the list moves it with its rows.
struct HoverState {
    flight: GlideState,
    fade_from: f32,
    fade_to: f32,
    fade_started: Instant,
}

impl HoverState {
    fn alpha(&self, now: Instant) -> f32 {
        let step = now.duration_since(self.fade_started).as_secs_f32() / FILL_EASE.as_secs_f32();
        self.fade_from + (self.fade_to - self.fade_from) * ease_out_cubic(step.clamp(0.0, 1.0))
    }

    fn fade_to(&mut self, to: f32, now: Instant) {
        if self.fade_to != to {
            self.fade_from = self.alpha(now);
            self.fade_to = to;
            self.fade_started = now;
        }
    }
}

impl IntoElement for HoverGlide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HoverGlide {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        self.slot.0.borrow_mut().clear();
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(id) = id else {
            return;
        };
        let hovered = self
            .slot
            .0
            .borrow()
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, hitbox))| hitbox.is_hovered(window))
            .map(|(index, (rect, _))| (index, *rect));
        let target = hovered.map(|(_, rect)| Bounds {
            origin: rect.origin - bounds.origin,
            size: rect.size,
        });
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let (pill, alpha) = window.with_element_state(id, |state: Option<HoverState>, window| {
            let mut state = state.unwrap_or_else(|| {
                let rest = target.unwrap_or_default();
                HoverState {
                    flight: GlideState {
                        from: rest,
                        to: rest,
                        started: now,
                    },
                    fade_from: 0.0,
                    fade_to: 0.0,
                    fade_started: now,
                }
            });
            match target {
                Some(target) => {
                    if state.fade_to == 0.0 && state.alpha(now) < 0.05 {
                        // Nothing is showing, so the pill appears under
                        // the row instead of flying in from the last one.
                        state.flight = GlideState {
                            from: target,
                            to: target,
                            started: now,
                        };
                    } else if state.flight.to != target {
                        state.flight.from = state.flight.at(now);
                        state.flight.to = target;
                        state.flight.started = now;
                    }
                    state.fade_to(1.0, now);
                }
                None => state.fade_to(0.0, now),
            }
            let flying = !state.flight.settled(now);
            let alpha = state.alpha(now);
            let fading = alpha != state.fade_to;
            let pill = if reduce_motion || !flying {
                state.flight.to
            } else {
                state.flight.at(now)
            };
            let alpha = if reduce_motion { state.fade_to } else { alpha };
            if !reduce_motion && (flying || fading) {
                window.request_animation_frame();
            }
            ((pill, alpha), state)
        });
        if alpha > 0.0 {
            let pill = Bounds {
                origin: pill.origin + bounds.origin,
                size: pill.size,
            };
            let radius = self.radius.to_pixels(window.rem_size());
            let color = Rgba {
                a: self.color.a * alpha,
                ..self.color
            };
            window.paint_quad(
                gpui::fill(pill, color)
                    .corner_radii(Corners::all(radius).clamp_radii_for_quad_size(pill.size)),
            );
        }
        // Repaint when the pointer crosses into another row, the way
        // `.hover()` does.
        let slot = self.slot.clone();
        let current = hovered.map(|(index, _)| index);
        window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, _cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let now = slot
                .0
                .borrow()
                .iter()
                .rposition(|(_, hitbox)| hitbox.is_hovered(window));
            if now != current {
                window.refresh();
            }
        });
    }
}

struct GlideMarker(GlideSlot);

impl IntoElement for GlideMarker {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GlideMarker {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        self.0.0.set(Some(bounds));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

struct Glide {
    id: ElementId,
    slot: GlideSlot,
    color: Rgba,
    radius: AbsoluteLength,
}

/// A glide's flight, in coordinates relative to the pill layer's origin so
/// scrolling the strip moves the pill with its tabs instead of animating.
struct GlideState {
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    started: Instant,
}

impl GlideState {
    fn at(&self, now: Instant) -> Bounds<Pixels> {
        let step = now.duration_since(self.started).as_secs_f32() / GLIDE.as_secs_f32();
        let eased = ease_spring(step);
        let lerp = |a: Pixels, b: Pixels| a + (b - a) * eased;
        Bounds {
            origin: point(
                lerp(self.from.origin.x, self.to.origin.x),
                lerp(self.from.origin.y, self.to.origin.y),
            ),
            size: gpui::size(
                lerp(self.from.size.width, self.to.size.width).max(px(0.0)),
                lerp(self.from.size.height, self.to.size.height).max(px(0.0)),
            ),
        }
    }

    fn settled(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= GLIDE
    }
}

impl IntoElement for Glide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Glide {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(overlay_style(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Siblings after this one prepaint next; only a marker drawn this
        // frame may name the target.
        self.slot.0.set(None);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(target) = self.slot.0.get() else {
            return;
        };
        let Some(id) = id else {
            return;
        };
        let target = Bounds {
            origin: target.origin - bounds.origin,
            size: target.size,
        };
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let pill = window.with_element_state(id, |state: Option<GlideState>, window| {
            let mut state = state.unwrap_or(GlideState {
                from: target,
                to: target,
                started: now,
            });
            if state.to != target {
                // A target that only resized (a label grew) follows at once;
                // a move to another item flies there from wherever the pill
                // is now.
                if state.to.origin == target.origin && state.settled(now) {
                    state.from = target;
                } else {
                    state.from = state.at(now);
                }
                state.to = target;
                state.started = now;
            }
            let pill = if reduce_motion || state.settled(now) {
                target
            } else {
                window.request_animation_frame();
                state.at(now)
            };
            (pill, state)
        });
        let pill = Bounds {
            origin: pill.origin + bounds.origin,
            size: pill.size,
        };
        let radius = self.radius.to_pixels(window.rem_size());
        window.paint_quad(
            gpui::fill(pill, self.color)
                .corner_radii(Corners::all(radius).clamp_radii_for_quad_size(pill.size)),
        );
    }
}

/// A disclosure chevron: pointing right when collapsed, down when open.
///
/// With `animate` it turns between the two; the caller passes it only right
/// after the user toggled, because a fresh element always plays from the
/// start, and a row scrolling into view should not spin its chevron.
pub(super) fn chevron(
    id: impl Into<SharedString>,
    expanded: bool,
    animate: bool,
    size: f32,
    color: impl Into<Hsla>,
) -> AnyElement {
    use std::f32::consts::FRAC_PI_2;
    let to = if expanded { FRAC_PI_2 } else { 0.0 };
    let from = FRAC_PI_2 - to;
    let glyph = svg()
        .data(include_bytes!("../../assets/icons/chevron-right.svg"))
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .text_color(color.into());
    if animate && from != to {
        // Keyed by state, so each toggle is a new element that plays afresh.
        glyph
            .with_animation(
                ElementId::NamedInteger(id.into(), u64::from(expanded)),
                Animation::new(CHEVRON_TURN).with_easing(gpui::ease_out_quint()),
                move |glyph, delta| {
                    glyph.with_transformation(Transformation::rotate(gpui::radians(
                        from + (to - from) * delta,
                    )))
                },
            )
            .into_any_element()
    } else {
        glyph
            .with_transformation(Transformation::rotate(gpui::radians(to)))
            .into_any_element()
    }
}

/// The working indicator: a loader ring that turns for as long as it is
/// painted.
///
/// It runs on its own animation clock, not on data arriving, so it keeps
/// moving while an agent thinks without output. Every spinner shares one
/// phase (`repeat_synced`), so a tab badge, a sidebar row, and the session
/// dock turn together instead of drifting apart.
pub(super) fn spinner(
    id: impl Into<ElementId>,
    size: f32,
    color: impl Into<Hsla>,
) -> impl IntoElement {
    svg()
        .data(include_bytes!("../../assets/icons/loader-circle.svg"))
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .text_color(color.into())
        .with_animation(
            id,
            Animation::new(SPINNER_TURN)
                .repeat_synced()
                .with_max_fps(SPINNER_FPS),
            |svg, phase| svg.with_transformation(Transformation::rotate(percentage(phase))),
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
