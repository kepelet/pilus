//! The native frame drawn over each pane, and how it animates.
//!
//! `render_grid` hides the daemon's square border glyphs (`pane_frames`) and
//! asks this module for one [`PaneChrome`] per pane. With gaps on, a pane is a
//! rounded card floating on the surface, a gap from its neighbors, its border
//! in the surface color so it reads as the card alone. With gaps off, panes
//! sit flush: each is outlined in the theme's border, and neighbors share one
//! line. A pane that does not hold the caret is dimmed by a
//! veil of that surface over its card and text, which lifts under the pointer.
//! The pane that does hold the caret also casts a soft shadow around its card.
//!
//! The grid is rebuilt every frame, so the transition lives in GPUI element
//! state keyed by the pane's index. A pane seen for the first time fades in; a
//! pane whose focus or hover changes eases between the dimmed and clear look,
//! and its shadow with the focus.
//! State carries colors, not geometry, so resizing a split never replays the
//! fade.

use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, ContentMask, Corners, DispatchPhase, Div, Edges, Element,
    ElementId, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    Length, MouseMoveEvent, ParentElement as _, Pixels, Rgba, Size, Style, Styled as _, Window,
    black, div, point, px, quad, transparent_black,
};

use super::pane_frames::PaneFrame;
use super::style::{self, RADIUS_MD, ease_step, mix};

/// Thickness of a pane's frame in logical pixels.
const BORDER_PX: f32 = 1.0;
/// The space between two cards, and between a card and the edge of the
/// surface, in logical pixels. Cells are wider than they are tall, so a gap
/// that fell out of the cell grid would differ by axis.
pub(super) const GAP_PX: f32 = 8.0;
/// How far either side of a seam a press still grabs it, in logical pixels:
/// the gutter between floating cards, or a little slack around the single
/// shared line of flush panes, which is too thin to aim at.
pub(super) fn divider_slop(gaps: bool) -> f32 {
    if gaps { GAP_PX / 2.0 + 1.0 } else { 3.0 }
}
/// How long a pane's frame takes to fade in when it first appears.
const ENTER: Duration = Duration::from_millis(260);
/// How long the frame takes to move between resting and focused.
const FOCUS_EASE: Duration = Duration::from_millis(160);
/// How much of the surface a dimmed pane is veiled with.
const DIM: f32 = 0.4;
/// The focused card's shadow: how dark and how soft; kept faint. It is painted with the
/// frame, above the rows, because the row fills cover anything drawn under
/// them; [`paint_shadow_outside`] keeps it off the card.
const SHADOW_ALPHA: f32 = 0.1;
const SHADOW_BLUR_PX: f32 = 6.0;
/// How far the focused pane's toolbar sits in from its card's top-right corner.
const TOOLBAR_INSET_PX: f32 = 6.0;

/// Where a pane's frame sits and what it looks like right now. Geometry is
/// in pixels relative to the grid's top-left.
pub(super) struct PaneChrome {
    index: usize,
    focused: bool,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    rest: Look,
    hover: Look,
    surface: Rgba,
    round: RoundedCorners,
    stroke_width: Edges<Pixels>,
}

/// Which of a card's corners are rounded. A floating card rounds all four;
/// flush panes round none, whatever the corner setting, so the split reads as
/// one square block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RoundedCorners {
    top_left: bool,
    top_right: bool,
    bottom_left: bool,
    bottom_right: bool,
}

impl RoundedCorners {
    const ALL: Self = Self {
        top_left: true,
        top_right: true,
        bottom_left: true,
        bottom_right: true,
    };
    const NONE: Self = Self {
        top_left: false,
        top_right: false,
        bottom_left: false,
        bottom_right: false,
    };

    fn pixels(self, radius: Pixels) -> Corners<Pixels> {
        let pick = |round: bool| if round { radius } else { px(0.0) };
        Corners {
            top_left: pick(self.top_left),
            top_right: pick(self.top_right),
            bottom_left: pick(self.bottom_left),
            bottom_right: pick(self.bottom_right),
        }
    }
}

/// The two things a pane's frame animates between.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    border: Rgba,
    /// How much the pane is dimmed, 0 to 1.
    dim: f32,
    /// How much of the focused card's shadow shows, 0 to 1.
    shadow: f32,
}

impl Look {
    fn mix(self, other: Look, step: f32) -> Look {
        Look {
            border: mix(self.border, other.border, step),
            dim: self.dim + (other.dim - self.dim) * step,
            shadow: self.shadow + (other.shadow - self.shadow) * step,
        }
    }
}

/// One pane's frame. With `gaps` on, each card is its box of cells pulled in
/// from the cell boundaries: half the gap on an edge it shares with a neighbor, so two cards
/// are a whole gap apart whichever way they are split, and a whole gap on an
/// edge that meets the surface's border. The grid's sub-cell remainder
/// (`slack`) is surface too, so the outermost right and bottom cards grow
/// over it, keeping those margins a gap whatever the window's size. The corner radius follows the
/// setting. `surface` is the color the surface layer paints, which the dimming
/// veil shares; `border` is the stroke's color, clear when the window is
/// translucent, since a stroke over the surface there would double its alpha
/// into a visible ring. The focused pane strokes `focus_border` instead, the
/// hairline the sidebar parts with. The stroke sits just outside the card. A top edge
/// hidden behind the native tab strip is drawn flush with the grid's top, so
/// the strip reads as opening into the card.
///
/// With `gaps` off the box is the card, outer edges included, and it draws its
/// own `border`. A pane's stroke on an edge it shares reaches a pixel into
/// the neighbor, whose own stroke lands on the same pixels, so a seam is one
/// line, not two. The card casts no shadow, which would fall on the neighbor.
#[allow(clippy::too_many_arguments)]
pub(super) fn pane_chrome(
    index: usize,
    pane: &PaneFrame,
    tiled: &PaneFrame,
    focused: bool,
    gaps: bool,
    surface: Rgba,
    border: Rgba,
    focus_border: Rgba,
    line_height: Pixels,
    cell_width: f32,
    slack: (f32, f32),
    skip_cols: u16,
    skip_rows: u16,
) -> PaneChrome {
    let height = f32::from(line_height);
    let column_edge =
        |column: u16| ((f32::from(column) - f32::from(skip_cols)) * cell_width).round();
    let row_edge = |row: u16| ((f32::from(row) - f32::from(skip_rows)) * height).round();
    let outer_left = pane.left == tiled.left;
    let outer_right = pane.right == tiled.right;
    let outer_top = pane.top == tiled.top;
    let outer_bottom = pane.bottom == tiled.bottom;
    let slack = (
        if outer_right { slack.0 } else { 0.0 },
        if outer_bottom { slack.1 } else { 0.0 },
    );
    let (left, top, right, bottom) = if gaps {
        let inset = |outer: bool| if outer { GAP_PX } else { GAP_PX / 2.0 };
        let left = column_edge(pane.left) + inset(outer_left);
        let right = column_edge(pane.right + 1) + slack.0 - inset(outer_right);
        let top = if pane.top < skip_rows {
            0.0
        } else {
            row_edge(pane.top) + inset(outer_top)
        };
        let bottom = row_edge(pane.bottom + 1) + slack.1 - inset(outer_bottom);
        // The stroke grows the box outward, leaving the card as the inner area.
        (
            left - BORDER_PX,
            top - BORDER_PX,
            right + BORDER_PX,
            bottom + BORDER_PX,
        )
    } else {
        let left = column_edge(pane.left);
        let top = if pane.top < skip_rows {
            0.0
        } else {
            row_edge(pane.top)
        };
        let overlap = |outer: bool| if outer { 0.0 } else { BORDER_PX };
        (
            left,
            top,
            column_edge(pane.right + 1) + slack.0 + overlap(outer_right),
            row_edge(pane.bottom + 1) + slack.1 + overlap(outer_bottom),
        )
    };
    let stroke = if focused { focus_border } else { border };
    let rest = Look {
        // Floating cards keep the stroke's ring as surface, so the geometry
        // and the shadow's overlap match an outlined card's; it only shows
        // when the panes are flush.
        border: if gaps {
            Rgba { a: 0.0, ..stroke }
        } else {
            stroke
        },
        dim: if focused { 0.0 } else { 1.0 },
        shadow: if focused && gaps { 1.0 } else { 0.0 },
    };
    let round = if gaps {
        RoundedCorners::ALL
    } else {
        RoundedCorners::NONE
    };
    // Flush panes leave the split's left and right edges unstroked: each
    // sidebar's hairline already parts them, and a second line beside it
    // reads doubled.
    let stroke_width = Edges {
        left: px(if !gaps && outer_left { 0.0 } else { BORDER_PX }),
        right: px(if !gaps && outer_right { 0.0 } else { BORDER_PX }),
        ..Edges::all(px(BORDER_PX))
    };
    PaneChrome {
        index,
        focused,
        left,
        top,
        width: (right - left).max(0.0),
        height: (bottom - top).max(0.0),
        rest,
        hover: Look { dim: 0.0, ..rest },
        surface,
        round,
        stroke_width,
    }
}

/// `bar` pinned to the grid's top-right corner, for a tab with one pane and
/// so no frame to measure from.
pub(super) fn lone_toolbar(bar: Div) -> Div {
    div()
        .absolute()
        .top(px(TOOLBAR_INSET_PX))
        .right(px(TOOLBAR_INSET_PX))
        .child(style::enter("pilus-pane-toolbar-lone", bar))
}

impl PaneChrome {
    /// Whether this pane holds the caret.
    pub(super) fn focused(&self) -> bool {
        self.focused
    }

    /// `bar` pinned inside the card's top-right corner. The fade is keyed by
    /// the pane, so moving focus to another pane replays it there.
    pub(super) fn toolbar(&self, bar: Div) -> Div {
        div()
            .absolute()
            .left(px(self.left))
            .top(px(self.top + BORDER_PX + TOOLBAR_INSET_PX))
            .w(px(self.width))
            .pr(px(BORDER_PX + TOOLBAR_INSET_PX))
            .flex()
            .flex_row()
            .justify_end()
            .child(style::enter(("pilus-pane-toolbar", self.index as u64), bar))
    }

    /// The pane's card: the rounded fill inside its frame, painted under the
    /// grid's text. A card has no animation of its own; the frame fades in
    /// around it.
    pub(super) fn card(&self, fill: Rgba) -> Div {
        let corners = self.round.pixels(RADIUS_MD.pixels());
        div()
            .absolute()
            .left(px(self.left))
            .top(px(self.top))
            .w(px(self.width))
            .h(px(self.height))
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .bg(fill)
    }
}

/// The surface under every card: the whole box the panes tile, cell-aligned,
/// so the gutters between cards and the margin around them show it. A top
/// row hidden behind the native tab strip starts at the grid's top.
///
/// The right edge runs a pixel past the cell grid when the remainder (`slack_x`)
/// is too thin for the row filler to paint (`render_row` skips it at 0.5px or
/// less): the cell edge is rounded, so without this a strip of card color
/// shows along the sidebar. A wider remainder is the filler's, and covering it
/// here too would double the tint under translucency. The card clips the
/// overshoot.
pub(super) fn surface(
    tiled: &PaneFrame,
    fill: Rgba,
    line_height: Pixels,
    cell_width: f32,
    slack_x: f32,
    skip_cols: u16,
    skip_rows: u16,
) -> Div {
    let height = f32::from(line_height);
    let left = ((f32::from(tiled.left) - f32::from(skip_cols)) * cell_width)
        .round()
        .max(0.0);
    let right = ((f32::from(tiled.right) + 1.0 - f32::from(skip_cols)) * cell_width).round()
        + if slack_x > 0.5 { 0.0 } else { slack_x + 1.0 };
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

/// The focused card's shadow along its straight edges only. GPUI's drop
/// shadow also fills the element it belongs to, which would dim the card's text
/// and show through a translucent window, and a content mask can only be a
/// rectangle, so the shadow is painted once per side of the card instead.
///
/// Each side's shadow rectangle is shortened along the edge by the corner
/// radius plus the blur's reach, so it fades out by itself before the corner
/// rather than ending at a mask's hard cut.
///
/// The scene drops a shadow whose own rectangle (without its blur) misses the
/// mask, so each side reaches [`BORDER_PX`] into the card, under the frame's
/// stroke. That also rules out an offset: it would pull a side off the card
/// again.
fn paint_shadow_outside(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    corners: Corners<Pixels>,
    strength: f32,
) {
    let stroke = px(BORDER_PX);
    let radius = corners
        .top_left
        .max(corners.top_right)
        .max(corners.bottom_left)
        .max(corners.bottom_right);
    let (left, top) = (bounds.origin.x, bounds.origin.y);
    let (right, bottom) = (bounds.right(), bounds.bottom());
    let reach = px(SHADOW_BLUR_PX * 3.0);
    let shrink = (radius + px(SHADOW_BLUR_PX * 2.0))
        .min(bounds.size.width / 2.0 - px(1.0))
        .min(bounds.size.height / 2.0 - px(1.0))
        .max(px(0.0));
    let shadow = [BoxShadow {
        color: black().opacity(SHADOW_ALPHA * strength),
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(SHADOW_BLUR_PX),
        spread_radius: px(0.0),
        inset: false,
    }];
    for (rect, from, to) in [
        // Top, bottom, left, right.
        (
            Bounds::from_corners(point(left + shrink, top), point(right - shrink, bottom)),
            point(left - reach, top - reach),
            point(right + reach, top + stroke),
        ),
        (
            Bounds::from_corners(point(left + shrink, top), point(right - shrink, bottom)),
            point(left - reach, bottom - stroke),
            point(right + reach, bottom + reach),
        ),
        (
            Bounds::from_corners(point(left, top + shrink), point(right, bottom - shrink)),
            point(left - reach, top - reach),
            point(left + stroke, bottom + reach),
        ),
        (
            Bounds::from_corners(point(left, top + shrink), point(right, bottom - shrink)),
            point(right - stroke, top - reach),
            point(right + reach, bottom + reach),
        ),
    ] {
        let mask = ContentMask {
            bounds: Bounds::from_corners(from, to),
        };
        window.with_content_mask(Some(mask), |window| {
            window.paint_drop_shadows(rect, Corners::default(), &shadow);
        });
    }
}

/// A look's transition, kept across frames under the pane's element id.
struct Transition {
    from: Look,
    to: Look,
    started: Instant,
    duration: Duration,
}

impl Transition {
    /// Exactly `to` once the time is up: the frame keeps asking for repaints
    /// until the look it paints equals its target (see [`ease_step`]).
    fn at(&self, now: Instant) -> Look {
        match ease_step(self.started, now, self.duration) {
            Some(step) => self.from.mix(self.to, step),
            None => self.to,
        }
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
                        shadow: 0.0,
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
        let corners = self
            .round
            .pixels(radius)
            .clamp_radii_for_quad_size(bounds.size);
        if look.shadow > 0.0 && !style::energy() {
            paint_shadow_outside(window, bounds, corners, look.shadow);
        }
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
                self.stroke_width,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome(pane: PaneFrame, tiled: PaneFrame) -> PaneChrome {
        chrome_with(pane, tiled, true)
    }

    fn chrome_with(pane: PaneFrame, tiled: PaneFrame, gaps: bool) -> PaneChrome {
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        pane_chrome(
            0,
            &pane,
            &tiled,
            true,
            gaps,
            clear,
            clear,
            clear,
            px(17.0),
            8.4,
            (0.0, 0.0),
            0,
            1,
        )
    }

    #[test]
    fn a_look_fading_to_a_clear_border_comes_to_rest() {
        // `mix` fades a solid border out in its own hue, not the clear
        // target's, so the look must land on its target exactly, or the pane
        // asks for frames forever and repaints the window with each one.
        let focus = Look {
            border: Rgba {
                r: 0.3,
                g: 0.3,
                b: 0.35,
                a: 1.0,
            },
            dim: 0.0,
            shadow: 1.0,
        };
        let rest = Look {
            border: Rgba {
                r: 0.1,
                g: 0.12,
                b: 0.14,
                a: 0.0,
            },
            dim: 0.35,
            shadow: 0.0,
        };
        let started = Instant::now();
        let fade = Transition {
            from: focus,
            to: rest,
            started,
            duration: FOCUS_EASE,
        };
        assert!(fade.at(started + FOCUS_EASE) == rest);
    }

    fn frame(left: u16, top: u16, right: u16, bottom: u16) -> PaneFrame {
        PaneFrame {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn side_by_side_and_stacked_panes_share_one_gap() {
        let tiled = frame(0, 1, 39, 20);
        let visible = |c: &PaneChrome| {
            (
                c.left + BORDER_PX,
                c.top + BORDER_PX,
                c.left + c.width - BORDER_PX,
                c.top + c.height - BORDER_PX,
            )
        };
        let left = visible(&chrome(frame(0, 1, 19, 20), tiled));
        let right = visible(&chrome(frame(20, 1, 39, 20), tiled));
        let upper = visible(&chrome(frame(0, 1, 39, 9), tiled));
        let lower = visible(&chrome(frame(0, 10, 39, 20), tiled));
        assert_eq!(right.0 - left.2, GAP_PX);
        assert_eq!(lower.1 - upper.3, GAP_PX);
    }

    #[test]
    fn the_remainder_below_the_grid_does_not_widen_the_margin() {
        let tiled = frame(0, 1, 39, 20);
        let bare = chrome(tiled, tiled);
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let slack = pane_chrome(
            0,
            &tiled,
            &tiled,
            true,
            true,
            clear,
            clear,
            clear,
            px(17.0),
            8.4,
            (5.0, 9.0),
            0,
            1,
        );
        assert_eq!(slack.width - bare.width, 5.0);
        assert_eq!(slack.height - bare.height, 9.0);
    }

    #[test]
    fn the_surface_margin_matches_the_gap() {
        let tiled = frame(0, 2, 39, 20);
        let card = chrome(tiled, tiled);
        assert_eq!(card.left + BORDER_PX, GAP_PX);
        assert_eq!(card.top + BORDER_PX, (17.0f32 * 1.0).round() + GAP_PX);
    }

    #[test]
    fn flush_panes_meet_on_one_line() {
        let tiled = frame(0, 1, 39, 20);
        let left = chrome_with(frame(0, 1, 19, 20), tiled, false);
        let right = chrome_with(frame(20, 1, 39, 20), tiled, false);
        // Each stroke is the outermost pixel of its box; the left pane's
        // right stroke and the right pane's left stroke are the same pixels.
        let seam = (left.left + left.width - BORDER_PX, right.left);
        assert_eq!(seam.0, seam.1);
        let upper = chrome_with(frame(0, 1, 39, 9), tiled, false);
        let lower = chrome_with(frame(0, 10, 39, 20), tiled, false);
        assert_eq!(upper.top + upper.height - BORDER_PX, lower.top);
    }

    #[test]
    fn flush_panes_have_no_margin_and_cast_no_shadow() {
        let tiled = frame(0, 2, 39, 20);
        let card = chrome_with(tiled, tiled, false);
        assert_eq!(card.left, 0.0);
        assert_eq!(card.top, 17.0);
        assert_eq!(card.rest.shadow, 0.0);
        assert_eq!(chrome(tiled, tiled).rest.shadow, 1.0);
    }

    #[test]
    fn flush_panes_stay_square_whatever_the_corner_setting() {
        let tiled = frame(0, 1, 39, 20);
        for pane in [frame(0, 1, 19, 20), frame(20, 10, 39, 20), tiled] {
            assert_eq!(chrome_with(pane, tiled, false).round, RoundedCorners::NONE);
        }
    }

    #[test]
    fn flush_panes_leave_the_sidebar_sides_unstroked() {
        let tiled = frame(0, 1, 39, 20);
        let left = chrome_with(frame(0, 1, 19, 20), tiled, false).stroke_width;
        assert_eq!(left.left, px(0.0));
        assert_eq!(left.right, px(BORDER_PX));
        let right = chrome_with(frame(20, 1, 39, 20), tiled, false).stroke_width;
        assert_eq!(right.left, px(BORDER_PX));
        assert_eq!(right.right, px(0.0));
        let floating = chrome(frame(0, 1, 19, 20), tiled).stroke_width;
        assert_eq!(floating.left, px(BORDER_PX));
    }

    #[test]
    fn floating_cards_round_every_corner() {
        let tiled = frame(0, 1, 39, 20);
        assert_eq!(
            chrome(frame(0, 1, 19, 20), tiled).round,
            RoundedCorners::ALL
        );
    }
}
