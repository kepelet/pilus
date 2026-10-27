//! The native frame drawn over each pane, and how it animates.
//!
//! `render_grid` hides the daemon's square border glyphs (`pane_frames`) and
//! asks this module for one [`PaneChrome`] per pane. With gaps on, a pane is a
//! rounded card floating on the surface, a gap from its neighbors; an unsplit
//! tab is one such card, inset from the window the same way. With gaps off,
//! panes sit flush: each is outlined in the theme's border, and neighbors
//! share one line. A pane that does not hold the caret is dimmed by a veil of
//! that surface over its card and text, which lifts under the pointer. A
//! floating card that holds the caret is outlined in the theme's border.
//!
//! The grid is rebuilt every frame, so the transition lives in GPUI element
//! state keyed by the pane's index. A pane seen for the first time fades in; a
//! pane whose focus or hover changes eases between the dimmed and clear look,
//! and its outline with the focus.
//! State carries colors, not geometry, so resizing a split never replays the
//! fade.

use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, Corners, DispatchPhase, Div, Edges, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, Length,
    MouseMoveEvent, ParentElement as _, Pixels, Rgba, Size, Style, Styled as _, Window, div, px,
    quad, transparent_black,
};

use super::pane_frames::{self, PaneFrame, PaneFrames};
use super::style::{self, RADIUS_MD, ease_step, mix};

/// Thickness of a pane's frame in logical pixels.
const BORDER_PX: f32 = 1.0;
/// The space between two cards, and between a card and the edge of the
/// surface, in logical pixels. Cells are wider than they are tall, so a gap
/// that fell out of the cell grid would differ by axis.
pub(super) const GAP_PX: f32 = 8.0;
/// The room between a floating card's edge and its text, in logical pixels:
/// sideways, and above and below an unsplit tab's card.
const PAD_X_PX: f32 = 12.0;
const PAD_Y_PX: f32 = 8.0;
/// The room either side of an unsplit card's text, in logical pixels. The
/// daemon keeps a blank column on each side of a lone pane, where a split
/// draws its borders, so a short lead plus that column matches the room a
/// split card's text has inside its border.
const PAD_LEAD_PX: f32 = 4.0;

/// How much of flush panes' blank top border row stays in view, in logical
/// pixels: a little room between the drawn top edge and the text.
const FLUSH_TOP_KEEP_PX: f32 = 6.0;

/// The top padding that tucks most of flush panes' blank top border row
/// under the tab strip: below zero, leaving [`FLUSH_TOP_KEEP_PX`] showing.
pub(super) fn flush_top_tuck(cell_height: f32) -> f32 {
    -(cell_height - FLUSH_TOP_KEEP_PX).max(0.0)
}

/// The padding the grid keeps clear around itself, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct GridPad {
    pub left: f32,
    pub right: f32,
    /// Below zero, rows tuck up under the tab strip instead.
    pub top: f32,
    pub bottom: f32,
}

/// The grid's padding. Flush panes keep none: the grid spends every pixel on
/// cells. In a split the daemon's border cells already hold each card's gap
/// and its rows above and below, so only the sides need room: a border column
/// is about as wide as the gap, which would otherwise leave a card's edge
/// against its text. An unsplit tab has no border cells, so its padding holds
/// the whole gap too, except at the top, where every card meets the tab strip.
/// Its sides take the same short lead, as the daemon's blank margin columns
/// already hold the rest.
pub(super) fn grid_pad(gaps: bool, split: bool) -> GridPad {
    match (gaps, split) {
        (false, _) => GridPad::default(),
        (true, true) => GridPad {
            left: PAD_X_PX,
            right: PAD_X_PX,
            top: 0.0,
            bottom: 0.0,
        },
        (true, false) => GridPad {
            left: GAP_PX + PAD_LEAD_PX,
            right: GAP_PX + PAD_LEAD_PX,
            top: PAD_Y_PX,
            bottom: GAP_PX + PAD_Y_PX,
        },
    }
}
/// How far each seam between side-by-side floating cards pushes the panes
/// right of it, in whole logical pixels (and even, so each card takes half).
/// The daemon spends two border columns on a seam, too little for the gap and
/// a [`PAD_X_PX`] on either side of it at most font sizes; the step makes up
/// the rest. None for flush panes, or when two columns are already enough.
pub(super) fn seam_step(gaps: bool, cell_width: f32) -> f32 {
    if !gaps {
        return 0.0;
    }
    let short = 2.0 * PAD_X_PX + GAP_PX - 2.0 * cell_width;
    (short.max(0.0) / 2.0).ceil() * 2.0
}

/// How far each pane's cells sit right of where the cell grid puts them, in
/// pixels: a [`seam_step`] for every seam between the pane and the split's
/// left edge. Painting, the cards, and the pointer all go through it, so a
/// cell is drawn, framed, and hit in the same place.
#[derive(Clone, Debug, Default)]
pub(super) struct ColumnShifts {
    /// Each pane with its shift and its reach: how much further its card's
    /// right edge runs to meet the nearest card past its seam, when that card
    /// is pushed more than one step (a deeper run of panes beside it).
    panes: Vec<(PaneFrame, f32, f32)>,
    step: f32,
    cell_width: f32,
    skip_cols: u16,
}

impl ColumnShifts {
    pub(super) fn new(panes: &PaneFrames, step: f32, cell_width: f32, skip_cols: u16) -> Self {
        if step <= 0.0 {
            return Self::default();
        }
        let shifts: Vec<f32> = panes
            .depths()
            .into_iter()
            .map(|depth| f32::from(depth) * step)
            .collect();
        let all = panes.panes();
        let panes = all
            .iter()
            .zip(&shifts)
            .map(|(pane, &shift)| {
                let reach = all
                    .iter()
                    .zip(&shifts)
                    .filter(|(other, _)| pane_frames::beside(pane, other))
                    .map(|(_, &next)| next - shift - step)
                    .reduce(f32::min)
                    .unwrap_or(0.0)
                    .max(0.0);
                (*pane, shift, reach)
            })
            .collect();
        Self {
            panes,
            step,
            cell_width,
            skip_cols,
        }
    }

    /// The step between two cards side by side; zero when nothing shifts.
    pub(super) fn step(&self) -> f32 {
        self.step
    }

    /// The shift and reach of the pane at `index` in `PaneFrames::panes`.
    fn of(&self, index: usize) -> (f32, f32) {
        self.panes
            .get(index)
            .map_or((0.0, 0.0), |&(_, shift, reach)| (shift, reach))
    }

    /// The panes crossing `row`, left to right, with their shifts.
    fn in_row(&self, row: u16) -> Vec<(PaneFrame, f32)> {
        let mut panes: Vec<(PaneFrame, f32)> = self
            .panes
            .iter()
            .filter(|(pane, ..)| (pane.top..=pane.bottom).contains(&row))
            .map(|&(pane, shift, _)| (pane, shift))
            .collect();
        panes.sort_by_key(|(pane, _)| pane.left);
        panes
    }

    /// The columns `first..end` of `row` cut where the shift changes:
    /// `(start, end, shift)` runs, end exclusive. Columns before the first
    /// pane keep no shift; columns past the last keep its shift.
    pub(super) fn segments(&self, row: u16, first: u16, end: u16) -> Vec<(u16, u16, f32)> {
        let mut segments: Vec<(u16, u16, f32)> = Vec::new();
        let mut push = |start: u16, stop: u16, shift: f32| {
            if start >= stop {
                return;
            }
            match segments.last_mut() {
                Some(last) if last.1 == start && last.2 == shift => last.1 = stop,
                _ => segments.push((start, stop, shift)),
            }
        };
        let mut at = first;
        let mut shift = 0.0;
        for (pane, pane_shift) in self.in_row(row) {
            push(at, pane.left.clamp(at, end), shift);
            at = at.max(pane.left).min(end);
            shift = pane_shift;
            let stop = pane.right.saturating_add(1).clamp(at, end);
            push(at, stop, shift);
            at = stop;
        }
        push(at, end, shift);
        segments
    }

    /// The shift of the cell at `column`, `row`.
    pub(super) fn at(&self, column: u16, row: u16) -> f32 {
        self.segments(row, column, column.saturating_add(1))
            .first()
            .map_or(0.0, |&(_, _, shift)| shift)
    }

    /// Undo the shift at `x`, pixels from the first visible column, on
    /// `row`: where the unshifted grid has that point. A point in the room a
    /// seam's step adds lands on the seam itself.
    pub(super) fn unshift(&self, x: f32, row: u16) -> f32 {
        let mut before = 0.0;
        for (pane, shift) in self.in_row(row) {
            let start = (f32::from(pane.left) - f32::from(self.skip_cols)) * self.cell_width;
            if x < start + shift {
                return (x - before).min(start);
            }
            before = shift;
        }
        x - before
    }
}

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
/// How far the focused pane's toolbar sits in from its card's top-right corner.
const TOOLBAR_INSET_PX: f32 = 6.0;

/// Surface beyond the cell grid that the outermost cards grow over, in
/// pixels: the padding before the first column and above the first row, and
/// the padding plus the sub-cell remainder past the last column and below the
/// last row. A `top` below zero is a row hidden under the tab strip.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Slack {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Where a pane's frame sits and what it looks like right now. Geometry is
/// in pixels relative to the grid's top-left.
pub(super) struct PaneChrome {
    index: usize,
    focused: bool,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    /// The pane's first text row, below the daemon's top border row, whose
    /// center the toolbar lines up with.
    first_row_top: f32,
    row_height: f32,
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
}

impl Look {
    fn mix(self, other: Look, step: f32) -> Look {
        Look {
            border: mix(self.border, other.border, step),
            dim: self.dim + (other.dim - self.dim) * step,
        }
    }
}

/// One pane's frame. With `gaps` on, each card is its box of cells pulled in
/// from the cell boundaries: half the gap on an edge it shares with a neighbor, so two cards
/// are a whole gap apart whichever way they are split, and a whole gap on an
/// edge that meets the surface's border. The grid's padding and sub-cell
/// remainder (`slack`) are surface too, so the outermost cards grow over
/// them, keeping those margins a gap whatever the window's size. The corner radius follows the
/// setting. `surface` is the color the surface layer paints, which the dimming
/// veil shares. A resting card has no visible stroke; the focused one strokes
/// `focus_border`, the hairline the sidebar parts with when panes are flush.
/// The stroke sits just outside the card. The top row of cards meets the
/// native tab strip, its stroke on the grid's first pixel row, so the strip
/// reads as opening into the card.
///
/// With `gaps` off the box is the card, outer edges included, and it draws its
/// own `border`. A pane's stroke on an edge it shares reaches a pixel into
/// the neighbor, whose own stroke lands on the same pixels, so a seam is one
/// line, not two.
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
    slack: Slack,
    shifts: &ColumnShifts,
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
    let slack = Slack {
        left: if outer_left { slack.left } else { 0.0 },
        top: if outer_top { slack.top } else { 0.0 },
        right: if outer_right { slack.right } else { 0.0 },
        bottom: if outer_bottom { slack.bottom } else { 0.0 },
    };
    let (left, top, right, bottom) = if gaps {
        let inset = |outer: bool| if outer { GAP_PX } else { GAP_PX / 2.0 };
        // A card at a seam moves with its pane's shift, half a step into the
        // room the step made, so the cards on either side meet a gap apart.
        let (shift, reach) = shifts.of(index);
        let half = shifts.step() / 2.0;
        let left = if outer_left {
            column_edge(pane.left) - slack.left + GAP_PX
        } else {
            column_edge(pane.left) + shift - half + GAP_PX / 2.0
        };
        let right = if outer_right {
            column_edge(pane.right + 1) + slack.right - GAP_PX
        } else {
            column_edge(pane.right + 1) + shift + half + reach - GAP_PX / 2.0
        };
        // The top row of cards meets the tab strip with no gap, so the
        // selected tab, in the card's own fill, reads as opening into it.
        // The stroke goes on the grid's first pixel row: any higher and the
        // card's clip cuts it, leaving the outline's corners hanging.
        let top = if pane.top < skip_rows {
            BORDER_PX
        } else if outer_top {
            row_edge(pane.top) - slack.top + BORDER_PX
        } else {
            row_edge(pane.top) + GAP_PX / 2.0
        };
        let bottom = row_edge(pane.bottom + 1) + slack.bottom - inset(outer_bottom);
        // The stroke grows the box outward, leaving the card as the inner area.
        (
            left - BORDER_PX,
            top - BORDER_PX,
            right + BORDER_PX,
            bottom + BORDER_PX,
        )
    } else {
        let left = column_edge(pane.left) - slack.left;
        // A top border row tucked under the tab strip (`slack.top` below
        // zero) brings the top edge down to the first row shown.
        let top = if pane.top < skip_rows {
            0.0
        } else {
            row_edge(pane.top) - slack.top
        };
        let overlap = |outer: bool| if outer { 0.0 } else { BORDER_PX };
        (
            left,
            top,
            column_edge(pane.right + 1) + slack.right + overlap(outer_right),
            row_edge(pane.bottom + 1) + slack.bottom + overlap(outer_bottom),
        )
    };
    let stroke = if focused { focus_border } else { border };
    let rest = Look {
        // A resting floating card keeps its ring clear, so it reads as the
        // card alone and its geometry matches the focused card's.
        border: if gaps && !focused {
            Rgba { a: 0.0, ..stroke }
        } else {
            stroke
        },
        dim: if focused { 0.0 } else { 1.0 },
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
        first_row_top: row_edge(pane.top.saturating_add(1)),
        row_height: height,
        rest,
        hover: Look { dim: 0.0, ..rest },
        surface,
        round,
        stroke_width,
    }
}

/// `bar` pinned to the grid's top-right corner and centered on the first row
/// of text, for a tab with one pane and so no frame to measure from. The row
/// is `row_height` tall, so the buttons sit level with the text beside them.
pub(super) fn lone_toolbar(bar: Div, row_height: Pixels) -> Div {
    div()
        .absolute()
        .top_0()
        .right(px(TOOLBAR_INSET_PX))
        .h(row_height)
        .flex()
        .flex_row()
        .items_center()
        .child(style::enter("pilus-pane-toolbar-lone", bar))
}

impl PaneChrome {
    /// Whether this pane holds the caret.
    pub(super) fn focused(&self) -> bool {
        self.focused
    }

    /// `bar` pinned inside the card's top-right corner, centered on the
    /// pane's first row of text. The fade is keyed by the pane, so moving
    /// focus to another pane replays it there.
    pub(super) fn toolbar(&self, bar: Div) -> Div {
        div()
            .absolute()
            .left(px(self.left))
            .top(px(self.first_row_top))
            .w(px(self.width))
            .h(px(self.row_height))
            .pr(px(BORDER_PX + TOOLBAR_INSET_PX))
            .flex()
            .flex_row()
            .items_center()
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
/// Floating cards (`gaps`) grow over the padding and remainder (`slack`), so
/// the surface spreads over them too, and the row fillers leave the base
/// color to it: a filler painting the surface would cover a card's edge, and
/// both painting it would double the tint under translucency.
///
/// Flush, the remainder is the row filler's. The right edge then runs a pixel
/// past the cell grid when the remainder is too thin for the filler to paint
/// (`render_row` skips it at 0.5px or less): the cell edge is rounded, so
/// without this a strip of card color shows along the sidebar. The card clips
/// the overshoot.
#[allow(clippy::too_many_arguments)]
pub(super) fn surface(
    tiled: &PaneFrame,
    fill: Rgba,
    line_height: Pixels,
    cell_width: f32,
    gaps: bool,
    slack: Slack,
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
    let (left, top, right, bottom) = if gaps {
        (
            left - slack.left,
            top - slack.top,
            right + slack.right,
            bottom + slack.bottom,
        )
    } else {
        let overshoot = if slack.right > 0.5 {
            0.0
        } else {
            slack.right + 1.0
        };
        (left, top, right + overshoot, bottom)
    };
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
            Slack::default(),
            &ColumnShifts::default(),
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
        };
        let rest = Look {
            border: Rgba {
                r: 0.1,
                g: 0.12,
                b: 0.14,
                a: 0.0,
            },
            dim: 0.35,
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
            Slack {
                right: 5.0,
                bottom: 9.0,
                ..Slack::default()
            },
            &ColumnShifts::default(),
            0,
            1,
        );
        assert_eq!(slack.width - bare.width, 5.0);
        assert_eq!(slack.height - bare.height, 9.0);
    }

    #[test]
    fn the_padding_before_the_grid_keeps_the_margin_a_gap() {
        let tiled = frame(0, 1, 39, 20);
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let padded = |pane| {
            pane_chrome(
                0,
                &pane,
                &tiled,
                true,
                true,
                clear,
                clear,
                clear,
                px(17.0),
                8.4,
                Slack {
                    left: 12.0,
                    right: 12.0,
                    ..Slack::default()
                },
                &ColumnShifts::default(),
                0,
                1,
            )
        };
        // The outer card grows over the padding, a gap in from its edge.
        let left = padded(frame(0, 1, 19, 20));
        assert_eq!(left.left + BORDER_PX, GAP_PX - 12.0);
        // A pane away from the edge ignores it: its seam does not move.
        let right = padded(frame(20, 1, 39, 20));
        let bare = chrome(frame(20, 1, 39, 20), tiled);
        assert_eq!(right.left, bare.left);
    }

    #[test]
    fn the_surface_margin_matches_the_gap_but_the_top_meets_the_strip() {
        let tiled = frame(0, 2, 39, 20);
        let card = chrome(tiled, tiled);
        assert_eq!(card.left + BORDER_PX, GAP_PX);
        // The stroke, just outside the card, lands on the grid's top edge.
        assert_eq!(card.top, (17.0f32 * 1.0).round());
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
    fn a_flush_top_edge_comes_down_past_the_tucked_row() {
        let tiled = frame(0, 1, 39, 20);
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let card = |pane| {
            pane_chrome(
                0,
                &pane,
                &tiled,
                true,
                false,
                clear,
                clear,
                clear,
                px(17.0),
                8.4,
                Slack {
                    top: flush_top_tuck(17.0),
                    ..Slack::default()
                },
                &ColumnShifts::default(),
                0,
                1,
            )
        };
        // Most of the top row is under the strip; the edge sits at the top
        // shown, a little above the first row of text.
        let top = card(frame(0, 1, 39, 9)).top;
        assert_eq!(top, -flush_top_tuck(17.0));
        assert_eq!(17.0 - top, FLUSH_TOP_KEEP_PX);
        // A pane lower down keeps its own top.
        assert_eq!(card(frame(0, 10, 39, 20)).top, 9.0 * 17.0);
    }

    #[test]
    fn flush_panes_have_no_margin() {
        let tiled = frame(0, 2, 39, 20);
        let card = chrome_with(tiled, tiled, false);
        assert_eq!(card.left, 0.0);
        assert_eq!(card.top, 17.0);
    }

    #[test]
    fn a_floating_card_is_outlined_only_while_it_holds_the_caret() {
        let tiled = frame(0, 1, 39, 20);
        let line = Rgba {
            r: 0.5,
            g: 0.5,
            b: 0.5,
            a: 1.0,
        };
        let card = |focused| {
            pane_chrome(
                0,
                &tiled,
                &tiled,
                focused,
                true,
                line,
                line,
                line,
                px(17.0),
                8.4,
                Slack::default(),
                &ColumnShifts::default(),
                0,
                1,
            )
        };
        assert_eq!(card(true).rest.border, line);
        assert_eq!(card(false).rest.border.a, 0.0);
    }

    #[test]
    fn an_unsplit_card_sits_a_gap_inside_its_padding() {
        // One pane spanning the whole grid, with the unsplit padding.
        let whole = frame(0, 1, 39, 20);
        let pad = grid_pad(true, false);
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let card = pane_chrome(
            0,
            &whole,
            &whole,
            true,
            true,
            clear,
            clear,
            clear,
            px(17.0),
            8.4,
            Slack {
                left: pad.left,
                top: pad.top,
                right: pad.right,
                bottom: pad.bottom,
            },
            &ColumnShifts::default(),
            0,
            1,
        );
        // The text starts at 0; the card's inner edge is the padding's
        // gap-wide outer part away from the surface's edge.
        assert_eq!(card.left + BORDER_PX, GAP_PX - pad.left);
        assert_eq!(card.top, -pad.top);
        assert_eq!(-(card.left + BORDER_PX), PAD_LEAD_PX);
        // Less the stroke, which now sits inside the padding.
        assert_eq!(-(card.top + BORDER_PX), PAD_Y_PX - BORDER_PX);
        // The far side keeps the same room past the last column.
        let right_inner = card.left + card.width - BORDER_PX;
        assert_eq!((40.0f32 * 8.4).round() + pad.right - right_inner, GAP_PX);
        assert_eq!(right_inner - (40.0f32 * 8.4).round(), PAD_LEAD_PX);
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

    #[test]
    fn a_seam_keeps_the_text_padding_on_both_sides() {
        let tiled = frame(0, 1, 39, 20);
        let (left, right) = (frame(0, 1, 19, 20), frame(20, 1, 39, 20));
        let panes = PaneFrames::from_panes(vec![left, right]);
        let cell = 8.4;
        let step = seam_step(true, cell);
        let shifts = ColumnShifts::new(&panes, step, cell, 0);
        let clear = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let card = |index, pane| {
            pane_chrome(
                index,
                &pane,
                &tiled,
                true,
                true,
                clear,
                clear,
                clear,
                px(17.0),
                cell,
                Slack::default(),
                &shifts,
                0,
                1,
            )
        };
        let (a, b) = (card(0, left), card(1, right));
        let a_inner_right = a.left + a.width - BORDER_PX;
        let b_inner_left = b.left + BORDER_PX;
        assert_eq!(b_inner_left - a_inner_right, GAP_PX);
        // The left pane's text ends where its border column starts; the
        // right pane's starts a column past its own, pushed by the step.
        let a_text_end = 19.0 * cell;
        let b_text_start = 21.0 * cell + shifts.at(21, 5);
        assert!(a_inner_right - a_text_end >= PAD_X_PX);
        assert!(b_text_start - b_inner_left >= PAD_X_PX);
    }

    #[test]
    fn unshifting_maps_the_step_onto_the_seam() {
        let panes = PaneFrames::from_panes(vec![frame(0, 1, 19, 20), frame(20, 1, 39, 20)]);
        let shifts = ColumnShifts::new(&panes, 16.0, 10.0, 0);
        assert_eq!(
            shifts.segments(5, 0, 40),
            vec![(0, 20, 0.0), (20, 40, 16.0)]
        );
        // The left pane is as drawn; the room the step adds is the seam; the
        // right pane comes back by the step.
        assert_eq!(shifts.unshift(55.0, 5), 55.0);
        assert_eq!(shifts.unshift(205.0, 5), 200.0);
        assert_eq!(shifts.unshift(226.0, 5), 210.0);
        // Rows outside the split are not shifted.
        assert_eq!(shifts.unshift(226.0, 30), 226.0);
    }
}
