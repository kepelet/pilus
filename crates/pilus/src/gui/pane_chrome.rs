//! The native frame drawn over each pane, and how it animates.
//!
//! `render_grid` hides the daemon's square border glyphs (`pane_frames`) and
//! asks this module for one [`PaneChrome`] per pane: a rounded border on the
//! center lines of the cells it replaces, in the surface color, so a pane
//! reads as its card alone. A pane that does not hold the caret is dimmed by a
//! veil of that surface over its card and text, which lifts under the pointer.
//!
//! The grid is rebuilt every frame, so the transition lives in GPUI element
//! state keyed by the pane's index. A pane seen for the first time fades in; a
//! pane whose focus or hover changes eases between the dimmed and clear look.
//! State carries colors, not geometry, so resizing a split never replays the
//! fade.

use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, Corners, DispatchPhase, Div, Edges, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, Length,
    MouseMoveEvent, Pixels, Rgba, Size, Style, Styled as _, Window, div, px, quad,
    transparent_black,
};

use super::pane_frames::PaneFrame;
use super::style::{RADIUS_MD, RadiusExt as _, ease_out_cubic, mix};

/// Thickness of a pane's frame in logical pixels.
const BORDER_PX: f32 = 2.0;
/// How long a pane's frame takes to fade in when it first appears.
const ENTER: Duration = Duration::from_millis(260);
/// How long the frame takes to move between resting and focused.
const FOCUS_EASE: Duration = Duration::from_millis(160);
/// How much of the surface a dimmed pane is veiled with.
const DIM: f32 = 0.4;

/// Where a pane's frame sits and what it looks like right now. Geometry is
/// in pixels relative to the grid's top-left.
pub(super) struct PaneChrome {
    index: usize,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    rest: Look,
    hover: Look,
    surface: Rgba,
}

/// The two things a pane's frame animates between.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    border: Rgba,
    /// How much the pane is dimmed, 0 to 1.
    dim: f32,
}

impl Look {
    fn mix(self, other: Look, step: f32) -> Look {
        Look {
            border: mix(self.border, other.border, step),
            dim: self.dim + (other.dim - self.dim) * step,
        }
    }
}

/// One pane's frame, on the center lines of the daemon's border cells so it
/// sits where the glyphs did. The corner radius follows the setting. `surface`
/// is the color the surface layer paints, which the dimming veil shares;
/// `border` is the stroke's color, clear when the window is translucent, since
/// a stroke over the surface there would double its alpha into a visible ring.
/// A top edge hidden behind the native tab strip is drawn at the
/// grid's top.
#[allow(clippy::too_many_arguments)]
pub(super) fn pane_chrome(
    index: usize,
    pane: &PaneFrame,
    focused: bool,
    surface: Rgba,
    border: Rgba,
    line_height: Pixels,
    cell_width: f32,
    skip_cols: u16,
    skip_rows: u16,
) -> PaneChrome {
    let height = f32::from(line_height);
    let column_center = |column: u16| (f32::from(column) - f32::from(skip_cols) + 0.5) * cell_width;
    let row_center = |row: u16| ((f32::from(row) - f32::from(skip_rows) + 0.5) * height).max(0.0);
    // The stroke straddles the center line, so the box grows by half of it
    // on every side.
    let half = BORDER_PX / 2.0;
    let left = column_center(pane.left).round() - half;
    let right = column_center(pane.right).round() + half;
    let top = row_center(pane.top).round() - half;
    let bottom = row_center(pane.bottom).round() + half;
    let rest = Look {
        border,
        dim: if focused { 0.0 } else { 1.0 },
    };
    PaneChrome {
        index,
        left,
        top,
        width: right - left,
        height: bottom - top,
        rest,
        hover: Look { dim: 0.0, ..rest },
        surface,
    }
}

impl PaneChrome {
    /// The pane's card: the rounded fill inside its frame, painted under the
    /// grid's text. A card has no animation of its own; the frame fades in
    /// around it.
    pub(super) fn card(&self, fill: Rgba) -> Div {
        div()
            .absolute()
            .left(px(self.left))
            .top(px(self.top))
            .w(px(self.width))
            .h(px(self.height))
            .radius_md()
            .bg(fill)
    }
}

/// The surface under every card: the whole box the panes tile, cell-aligned,
/// so the gutters between cards and the margin around them show it. A top
/// row hidden behind the native tab strip starts at the grid's top.
pub(super) fn surface(
    tiled: &PaneFrame,
    fill: Rgba,
    line_height: Pixels,
    cell_width: f32,
    skip_cols: u16,
    skip_rows: u16,
) -> Div {
    let height = f32::from(line_height);
    let left = ((f32::from(tiled.left) - f32::from(skip_cols)) * cell_width)
        .round()
        .max(0.0);
    let right = ((f32::from(tiled.right) + 1.0 - f32::from(skip_cols)) * cell_width).round();
    let top = ((f32::from(tiled.top) - f32::from(skip_rows)) * height)
        .round()
        .max(0.0);
    let bottom = ((f32::from(tiled.bottom) + 1.0 - f32::from(skip_rows)) * height).round();
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px((right - left).max(0.0)))
        .h(px((bottom - top).max(0.0)))
        .bg(fill)
}

/// A look's transition, kept across frames under the pane's element id.
struct Transition {
    from: Look,
    to: Look,
    started: Instant,
    duration: Duration,
}

impl Transition {
    fn at(&self, now: Instant) -> Look {
        let step = now.duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32();
        self.from.mix(self.to, ease_out_cubic(step.clamp(0.0, 1.0)))
    }
}

impl IntoElement for PaneChrome {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PaneChrome {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(("pilus-pane", self.index as u64).into())
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
        let style = Style {
            position: gpui::Position::Absolute,
            inset: Edges {
                top: Length::from(px(self.top)),
                right: Length::Auto,
                bottom: Length::Auto,
                left: Length::from(px(self.left)),
            },
            size: Size {
                width: Length::from(px(self.width)),
                height: Length::from(px(self.height)),
            },
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
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
        // A normal hitbox only observes the pointer: clicks still reach the
        // grid underneath.
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
        let target = if hovered { self.hover } else { self.rest };
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let look = match id {
            Some(id) => window.with_element_state(id, |state: Option<Transition>, window| {
                // A pane seen for the first time fades up from nothing.
                let mut state = state.unwrap_or(Transition {
                    from: Look {
                        border: Rgba {
                            a: 0.0,
                            ..target.border
                        },
                        dim: target.dim,
                    },
                    to: target,
                    started: now,
                    duration: ENTER,
                });
                if state.to != target {
                    state.from = state.at(now);
                    state.to = target;
                    state.started = now;
                    state.duration = FOCUS_EASE;
                }
                let look = if reduce_motion { target } else { state.at(now) };
                if look != target {
                    window.request_animation_frame();
                }
                (look, state)
            }),
            None => target,
        };

        let radius = RADIUS_MD.pixels();
        let corners = Corners::all(radius).clamp_radii_for_quad_size(bounds.size);
        if look.dim > 0.0 {
            let veil = Rgba {
                a: self.surface.a * DIM * look.dim,
                ..self.surface
            };
            window.paint_quad(quad(
                bounds,
                corners,
                veil,
                Edges::all(px(0.0)),
                transparent_black(),
                BorderStyle::Solid,
            ));
        }
        if look.border.a > 0.0 {
            // The fill is the border's own color at zero alpha: the quad
            // blends border into fill across the anti-aliased inner edge,
            // and a black fill would leave a dark fringe on the corners.
            window.paint_quad(quad(
                bounds,
                corners,
                Rgba {
                    a: 0.0,
                    ..look.border
                },
                Edges::all(px(BORDER_PX)),
                look.border,
                BorderStyle::Solid,
            ));
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
