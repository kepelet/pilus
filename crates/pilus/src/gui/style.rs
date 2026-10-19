//! Shared chrome tokens and the control builders every surface draws from.
//!
//! The UI Playground is the visual contract for pilus chrome; these builders are
//! the single implementation of it, so a button or field cannot drift between
//! windows. Sizes here are design sizes at the default Interface size and are
//! scaled to the user's setting by [`scaled`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationElement, AnimationExt, AnyElement, App, Bounds, Context, Corners,
    DispatchPhase, Div, Edges, Element, ElementId, Font, FontWeight, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, Length, MouseMoveEvent, Path,
    PathBuilder, Pixels, Point, Position, Render, Rgba, SharedString, Stateful, Style,
    Transformation, Window, canvas, div, font, percentage, point, prelude::*, px, svg,
};

use crate::settings::{CHROME_FONT, CORNER_RADIUS, DEFAULT_CORNER_RADIUS, Settings};

use super::theme::Theme;
use super::toast::ToastKind;

/// How long a skeleton takes to fade from dim to lit and back. Long enough to
/// read as breathing rather than a blink, and it respects reduce motion
/// through [`AnimationExt::with_animation`].
const SKELETON_PULSE: Duration = Duration::from_millis(1400);
/// How many pulses a skeleton makes before it rests dim; energy mode skips them. A placeholder whose
/// data never arrives (a daemon whose feed is down) would otherwise repaint
/// the window every frame for as long as it is open.
const SKELETON_PULSES: u32 = 6;
/// The most frames a second a skeleton asks for; a slow fade needs no more.
const SKELETON_FPS: f32 = 30.0;
/// How long a floating panel takes to fade in when it opens: quick enough
/// that a menu never feels slow to answer, long enough to read as motion.
const ENTER: Duration = Duration::from_millis(140);
/// How long a background fill takes to ease into a new hover or selection
/// state. Short enough that a pointer sweeping down a list never trails.
const FILL_EASE: Duration = Duration::from_millis(120);
/// How much of the accent a hovered primary button keeps.
const PRIMARY_HOVER_ALPHA: f32 = 0.82;
/// How long a meter's fill takes to slide to a new reading: slow enough to
/// see which way a value moved.
const METER_EASE: Duration = Duration::from_millis(420);
/// How long a selection pill takes to glide to its new target. A touch
/// longer than a fill fade: the spring's settle needs room to read.
const GLIDE: Duration = Duration::from_millis(340);
/// A radius step, resolved against the user's corner-radius setting when it
/// is drawn: never a fixed length, so every rounded element follows the
/// Appearances slider and zero squares them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Radius {
    /// Square whatever the setting: a row that meets its neighbours flush.
    None,
    /// A fill under a dense list row.
    Sm,
    /// A control, row, card, or overlay.
    Md,
    /// A large floating panel.
    Lg,
    /// A pill, status dot, or round button: fully round whatever the
    /// setting, since a square one stops reading as what it is. Only energy
    /// mode squares it.
    Full,
    /// A tab: never squarer than the default radius, so zero corners leave
    /// the strip soft.
    Soft,
}

/// The corner radius in logical pixels, as `f32` bits so any window can read
/// it without threading the settings through every builder. The windows
/// share one thread and one setting, and [`sync_corner_radius`] writes it
/// before they build their trees.
static CORNER_RADIUS_PX: AtomicU32 = AtomicU32::new(DEFAULT_CORNER_RADIUS.to_bits());

/// Whether energy mode is in effect, for painters that have no settings to ask.
static ENERGY: AtomicBool = AtomicBool::new(false);

/// Publish the radius being drawn, and whether energy mode is in effect, for
/// [`Radius`] and [`energy`] to resolve against. Energy mode squares the
/// corners here, so every window follows without a check of its own.
pub(super) fn sync_corner_radius(settings: &Settings) {
    CORNER_RADIUS_PX.store(settings.drawn_radius().to_bits(), Ordering::Relaxed);
    ENERGY.store(settings.energy_saving(), Ordering::Relaxed);
}

/// Whether energy mode is in effect. Decorative paint that costs the GPU
/// something (a shadow) checks it; motion goes through `reduce_motion`.
pub(super) fn energy() -> bool {
    ENERGY.load(Ordering::Relaxed)
}

impl Radius {
    /// This step's radius, scaled from the `Md` base the setting names.
    pub(super) fn pixels(self) -> Pixels {
        let base = f32::from_bits(CORNER_RADIUS_PX.load(Ordering::Relaxed));
        self.resolve(base, energy())
    }

    /// The radius for a `base` setting. Energy mode squares every step, the
    /// pill and the tab included, which the setting alone never does.
    fn resolve(self, base: f32, energy: bool) -> Pixels {
        if energy {
            return px(0.0);
        }
        match self {
            Self::None => px(0.0),
            Self::Sm => px(base * 2.0 / 3.0),
            Self::Md => px(base),
            Self::Lg => px(base * 4.0 / 3.0),
            Self::Full => px(9999.0),
            Self::Soft => px(base.max(CORNER_RADIUS)),
        }
    }
}

/// `Radius::Sm`, for a fill under a dense list row.
pub(super) const RADIUS_SM: Radius = Radius::Sm;
/// `Radius::Md`, for a fill under a control, row, or card.
pub(super) const RADIUS_MD: Radius = Radius::Md;
/// `Radius::Full`, for a fill under a pill or round button.
pub(super) const RADIUS_FULL: Radius = Radius::Full;
/// `Radius::Soft`, for a fill under a tab.
pub(super) const RADIUS_SOFT: Radius = Radius::Soft;

/// The `rounded_*` family, resolved through [`Radius`] instead of GPUI's fixed
/// rems. Chrome calls these so the corner-radius setting reaches it; a fixed
/// `rounded_md()` is a bug even when the default happens to match.
pub(super) trait RadiusExt: Styled + Sized {
    fn radius_sm(self) -> Self {
        self.rounded(Radius::Sm.pixels())
    }

    fn radius_md(self) -> Self {
        self.rounded(Radius::Md.pixels())
    }

    fn radius_lg(self) -> Self {
        self.rounded(Radius::Lg.pixels())
    }

    fn radius_soft(self) -> Self {
        self.rounded(Radius::Soft.pixels())
    }

    fn radius_full(self) -> Self {
        self.rounded(Radius::Full.pixels())
    }

    fn radius_t_md(self) -> Self {
        self.rounded_t(Radius::Md.pixels())
    }

    fn radius_t_lg(self) -> Self {
        self.rounded_t(Radius::Lg.pixels())
    }

    fn radius_b_lg(self) -> Self {
        self.rounded_b(Radius::Lg.pixels())
    }
}

impl<E: Styled> RadiusExt for E {}

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

/// A page section's frame: a `panel_dim` fill inside a `theme.border` edge, so
/// each group on a long page reads as its own block. Cards a section draws
/// inside it use `panel`, which sits a step above this fill.
pub(super) fn section_card(theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .mx_3()
        .py_3()
        .radius_md()
        .bg(theme.panel_dim)
        .border_1()
        .border_color(theme.border)
}

/// A section card with its title on a folder tab: the tab rises from the
/// card's top-left edge in the card's own fill. Its bottom border lies on the
/// card's top border, so the card's outline runs unbroken beneath the tab.
/// The tab's top corners and the card's follow the corner-radius setting, and
/// the tab starts past the card's own top-left curve so the two never overlap.
pub(super) fn section_card_tab(theme: Theme, title: &'static str, size: f32) -> Div {
    let height = size + 14.0;
    let inset = Radius::Md.pixels().max(px(12.0));
    section_card(theme).relative().mt(px(height)).child(
        div()
            .absolute()
            .top(px(-height))
            .left(inset)
            .h(px(height))
            .flex()
            .items_center()
            .px_3()
            .radius_t_md()
            .bg(theme.panel_dim)
            .border_1()
            .border_color(theme.border)
            .text_size(px(size))
            .font_weight(FontWeight::BOLD)
            .text_color(theme.muted)
            .child(title),
    )
}

/// A small outlined tag for a value that qualifies a row: a dependency, a
/// path glob, a mode. It sits in a list row or a property cell and never
/// takes a click of its own.
pub(super) fn chip(theme: Theme, text: impl IntoElement, size: f32) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(size * 1.7))
        .px(px(6.0))
        .radius_sm()
        .border_1()
        .border_color(theme.border)
        .text_size(px(size))
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

/// How far a disabled control dims.
pub(super) const DISABLED_OPACITY: f32 = 0.55;

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
    button_at(
        id,
        label,
        kind,
        theme,
        text_size,
        CONTROL_HEIGHT,
        px(12.0),
        true,
    )
}

/// A regular-height button that can be switched off. A disabled one keeps its
/// place but dims, takes no pointer, and ignores hover, so an action already
/// under way cannot be started twice. Callers still guard their handler.
pub(super) fn button_enabled(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
    enabled: bool,
) -> Stateful<Div> {
    button_at(
        id,
        label,
        kind,
        theme,
        text_size,
        CONTROL_HEIGHT,
        px(12.0),
        enabled,
    )
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
        true,
    )
}

/// A square icon-only control for a dense row. The treatment sets the icon's
/// tone: `Danger` is alert-red at rest and washes red under the pointer, the
/// rest are muted and wash to the panel fill. Callers attach the tooltip, which
/// the label would otherwise carry, and their own handler.
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    glyph: &'static [u8],
    kind: ButtonKind,
    theme: Theme,
    size: f32,
) -> Stateful<Div> {
    let (fg, hover_bg) = match kind {
        ButtonKind::Danger => (theme.alert, tint(theme.alert, 0.16)),
        ButtonKind::Primary => (theme.accent, theme.panel_dim),
        ButtonKind::Secondary => (theme.panel_fg, theme.panel_dim),
        ButtonKind::Ghost => (theme.muted, theme.panel_dim),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(size))
        .h(px(size))
        .radius_md()
        .cursor_pointer()
        .hover_fill(hover_bg, RADIUS_MD)
        .child(
            svg()
                .data(glyph)
                .w(px(size * 0.58))
                .h(px(size * 0.58))
                .flex_shrink_0()
                .text_color(fg),
        )
}

/// Side of a pane toolbar button. Smaller than a regular compact control: the
/// strip floats over terminal text, so it should stay out of the way.
pub(super) const PANE_TOOLBAR_BUTTON: f32 = 20.0;

/// The strip of icon buttons that floats over the focused pane's top-right
/// corner. It has no fill, so the terminal shows through between the glyphs;
/// it still occludes the grid under it, so a press on a button never also
/// starts a selection or reaches the daemon. Fill it with [`icon_button`]s.
pub(super) fn pane_toolbar() -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .p_1()
        .occlude()
}

/// A bordered pill for a button or a group of them in the top chrome. It has
/// no fill of its own, so the strip shows through. `hover` raises a fill
/// under the whole pill, for a pill that is itself a button; one that holds
/// buttons leaves it `None` and lets them raise their own. Content goes in as
/// children.
pub(super) fn pill(theme: Theme, hover: Option<Rgba>) -> Div {
    let pill = div()
        .flex()
        .flex_row()
        .items_center()
        .radius_soft()
        .border_1()
        .border_color(theme.border);
    match hover {
        Some(hover) => pill.state_fill(gpui::transparent_black(), hover, Radius::Soft),
        None => pill,
    }
}

/// The frame of a segmented control: a short row of mutually exclusive
/// choices read as one object. Fill it with [`segment`]s.
pub(super) fn segmented(theme: Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .p(px(2.0))
        .radius_md()
        .bg(theme.panel_dim)
        .border_1()
        .border_color(theme.border)
}

/// One choice of a [`segmented`] control. The chosen one takes the accent; the
/// rest wash on hover. A segment that cannot be picked (a level that can only
/// be reached with a new pairing code, say) stays visible but muted, takes no
/// pointer and no hover, so the control still shows the whole range.
pub(super) fn segment(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    active: bool,
    enabled: bool,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    let clear = gpui::transparent_black();
    let (rest, hover): (Hsla, Hsla) = if active {
        (theme.accent.into(), theme.accent.into())
    } else if enabled {
        (clear, theme.panel.into())
    } else {
        (clear, clear)
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(CONTROL_HEIGHT_SMALL - 6.0))
        .px_2()
        .radius_sm()
        .text_size(px(text_size))
        .font_weight(if active {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(if active {
            theme.window_bg
        } else if enabled {
            theme.fg
        } else {
            tint(theme.muted, 0.55)
        })
        .when(enabled && !active, |segment| segment.cursor_pointer())
        .state_fill(rest, hover, RADIUS_SM)
        .child(label)
}

/// One tab of a strip: the selected tab is raised by its panel fill alone,
/// with no border, and the others rest on the strip. Callers attach their own
/// handler.
pub(super) fn tab(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    active: bool,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(super::TAB_HEIGHT))
        .px_3()
        .radius_md()
        .text_color(if active { theme.panel_fg } else { theme.muted })
        .text_size(px(text_size))
        .cursor_pointer()
        .state_fill(
            if active {
                theme.panel.into()
            } else {
                gpui::transparent_black()
            },
            theme.panel,
            RADIUS_MD,
        )
        .hover(move |style| style.text_color(theme.panel_fg))
        .child(label)
}

/// A tab that cannot be opened: dimmed, and deaf to the pointer.
pub(super) fn tab_disabled(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(super::TAB_HEIGHT))
        .px_3()
        .radius_md()
        .text_color(theme.muted)
        .text_size(px(text_size))
        .opacity(0.35)
        .child(label)
}

/// How far each level of a tree steps in.
pub(super) const TREE_INDENT: f32 = 14.0;
const TREE_DISCLOSURE: f32 = 12.0;
const TREE_GLYPH: f32 = 14.0;
/// Space between the chevron, the glyph, and the name.
const TREE_GAP: f32 = 4.0;
/// Extra room before the name, so it does not crowd its glyph.
const TREE_NAME_PAD: f32 = 4.0;
const TREE_EDGE: f32 = 8.0;

/// Where a row's glyph starts, so a row of another shape under a folder can
/// stand beneath its icon.
pub(super) fn tree_glyph_x(depth: u16) -> f32 {
    TREE_EDGE + f32::from(depth) * TREE_INDENT + TREE_DISCLOSURE + TREE_GAP
}

/// Where a row's name starts, so a placeholder under it lines up.
pub(super) fn tree_name_x(depth: u16) -> f32 {
    tree_glyph_x(depth) + TREE_GLYPH + TREE_GAP + TREE_NAME_PAD
}

/// What a tree row says about Git, at its trailing edge.
pub(super) enum TreeMark {
    /// A file's status letter.
    Status(SharedString, Rgba),
    /// A folder with a changed file somewhere below it.
    Dot(Rgba),
}

/// What [`tree_row`] draws.
pub(super) struct TreeEntry {
    pub(super) depth: u16,
    pub(super) name: SharedString,
    /// `None` for a file, `Some(open)` for a folder.
    pub(super) folder: Option<bool>,
    /// Turn the chevron; only for the row the user just toggled, as for
    /// [`chevron`].
    pub(super) animate: bool,
    pub(super) mark: Option<TreeMark>,
    /// The entry the page beside the tree is showing: a raised pill, as a
    /// selected sidebar row is.
    pub(super) selected: bool,
}

/// One row of a file tree: indent, a disclosure chevron for a folder (a file
/// keeps the slot empty so names line up), a glyph, the name, and any Git
/// mark. Callers attach their own handler and place it in a flex column.
pub(super) fn tree_row(
    id: impl Into<ElementId>,
    entry: TreeEntry,
    theme: Theme,
    settings: &Rc<RefCell<Settings>>,
) -> Stateful<Div> {
    let fonts = Fonts::resolve(settings);
    let TreeEntry {
        depth,
        name,
        folder,
        animate,
        mark,
        selected,
    } = entry;
    let disclosure = match folder {
        Some(open) => chevron(
            format!("pilus-tree-chevron-{depth}-{name}"),
            open,
            animate,
            TREE_DISCLOSURE,
            theme.muted,
        ),
        None => div()
            .w(px(TREE_DISCLOSURE))
            .flex_shrink_0()
            .into_any_element(),
    };
    let glyph = if folder.is_some() {
        include_bytes!("../../assets/icons/folder.svg").as_slice()
    } else {
        include_bytes!("../../assets/icons/file.svg").as_slice()
    };
    let mark = mark.map(|mark| match mark {
        TreeMark::Status(letter, color) => div()
            .flex_shrink_0()
            .text_size(px(fonts.label))
            .font_weight(FontWeight::BOLD)
            .text_color(color)
            .child(letter)
            .into_any_element(),
        TreeMark::Dot(color) => div()
            .flex_shrink_0()
            .size(px(6.0))
            .radius_full()
            .bg(color)
            .into_any_element(),
    });
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(TREE_GAP))
        .h(px(scaled(settings, 24.0)))
        // The indent is margin, so the hover fill covers the row and not the
        // empty space left of it. That leaves the width to the parent: it
        // must be a flex column, which stretches the row to what remains.
        .ml(px(f32::from(depth) * TREE_INDENT))
        .pl(px(TREE_EDGE))
        .pr_2()
        .radius_md()
        .cursor_pointer()
        .state_fill(
            if selected {
                theme.panel.into()
            } else {
                gpui::transparent_black()
            },
            if selected { theme.panel } else { theme.panel_dim },
            RADIUS_MD,
        )
        .child(disclosure)
        .child(super::icon(glyph, TREE_GLYPH, theme.muted))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .pl(px(TREE_NAME_PAD))
                .truncate()
                .text_size(px(fonts.body))
                .text_color(theme.panel_fg)
                .when(selected, |name| name.font_weight(FontWeight::BOLD))
                .child(name),
        )
        .children(mark)
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
        .radius_full()
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
        // Round like the track around it, so a square toggle keeps a square knob.
        let radius = Radius::Full.pixels().min(px(KNOB / 2.0));
        window.paint_quad(gpui::fill(knob, color).corner_radii(radius));
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
        .radius_full()
        .bg(theme.panel_dim)
        .child(
            div()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .radius_full()
                .bg(theme.accent)
                .flex_grow(value)
                .child(
                    div()
                        .w(px(10.0))
                        .h(px(10.0))
                        .radius_full()
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

/// A horizontal fill bar: context fill, a share of spend.
///
/// The fill eases from its last width to `fraction` whenever the value
/// changes, so a refreshed reading slides rather than snaps; a meter drawn
/// for the first time appears settled. `None` paints the empty track for a
/// value not known yet. `color` is the caller's role (`accent`, or `alert`
/// past a warning threshold); `height` comes from [`scaled`].
pub(super) fn meter(
    id: impl Into<ElementId>,
    fraction: Option<f32>,
    color: Rgba,
    theme: Theme,
    height: f32,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w_full()
        .h(px(height))
        .flex_shrink_0()
        .radius_full()
        .bg(Hsla::from(theme.muted).opacity(0.18))
        .child(Meter {
            fraction: fraction.unwrap_or(0.0).clamp(0.0, 1.0),
            color,
        })
}

/// The eased fill [`meter`] paints over its track.
struct Meter {
    fraction: f32,
    color: Rgba,
}

/// A meter's transition, kept across frames under the element's id.
struct MeterState {
    from: f32,
    to: f32,
    started: Instant,
}

impl MeterState {
    fn at(&self, now: Instant) -> f32 {
        let step = now.duration_since(self.started).as_secs_f32() / METER_EASE.as_secs_f32();
        self.from + (self.to - self.from) * ease_out_cubic(step.clamp(0.0, 1.0))
    }
}

impl IntoElement for Meter {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Meter {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("pilus-meter".into())
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
        let target = self.fraction;
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let fraction = match id {
            Some(id) => window.with_element_state(id, |state: Option<MeterState>, window| {
                let mut state = state.unwrap_or(MeterState {
                    from: target,
                    to: target,
                    started: now,
                });
                if (state.to - target).abs() > f32::EPSILON {
                    state.from = state.at(now);
                    state.to = target;
                    state.started = now;
                }
                let fraction = if reduce_motion { target } else { state.at(now) };
                if (fraction - target).abs() > f32::EPSILON {
                    window.request_animation_frame();
                }
                (fraction, state)
            }),
            None => target,
        };
        if fraction <= 0.0 {
            return;
        }
        // Any positive reading keeps a visible sliver, like the gauge's
        // minimum arc: a hairline would read as empty.
        let width = (bounds.size.width.as_f32() * fraction).max(bounds.size.height.as_f32());
        let fill = Bounds::new(
            bounds.origin,
            gpui::size(
                px(width.min(bounds.size.width.as_f32())),
                bounds.size.height,
            ),
        );
        window.paint_quad(gpui::fill(fill, self.color).corner_radii(Corners::all(
            Radius::Full.pixels().min(bounds.size.height / 2.0),
        )));
    }
}

/// The row [`stat_tile`]s sit in. Tiles are square and flush: the strip owns
/// the bottom and left edges and each tile its right one, so every seam is a
/// single hairline. The last tile drops its rule (`.border_r_0()`) so the strip
/// does not close on a line against the page edge. It takes no top edge: the
/// page header above already ends in one.
pub(super) fn stat_strip(theme: Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .border_b_1()
        .border_l_1()
        .border_color(theme.border)
}

/// One cell of a page table: a fixed `width`, or the flexible column when it
/// is `None`. Every cell but the `last` closes with a column rule, so a
/// table's header and rows share one grid. The cell stretches to its row,
/// whose height the caller sets, and centres its content.
pub(super) fn table_cell(
    settings: &Rc<RefCell<Settings>>,
    theme: Theme,
    width: Option<f32>,
    last: bool,
) -> Div {
    let cell = div().flex().items_center().px_2().min_w(px(0.0));
    let cell = match width {
        Some(width) => cell.w(px(scaled(settings, width))),
        None => cell.flex_1(),
    };
    let cell = cell.flex_shrink_0();
    if last {
        cell
    } else {
        cell.border_r_1().border_color(theme.border)
    }
}

/// One headline figure on a dashboard: a section label, the value in its
/// role color, and a muted line of detail beneath. Tiles sit flush in a
/// [`stat_strip`] and share it evenly; they take no click.
pub(super) fn stat_tile(
    theme: Theme,
    fonts: Fonts,
    label: &'static str,
    value: impl Into<SharedString>,
    value_color: Rgba,
    detail: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_w(px(0.0))
        .px_3()
        .py_2()
        .border_r_1()
        .border_color(theme.border)
        .child(section_label(theme, label, fonts.label))
        .child(
            div()
                .text_size(px(fonts.headline))
                .font_weight(FontWeight::BOLD)
                .text_color(value_color)
                .child(value.into()),
        )
        .child(
            div()
                .truncate()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(detail.into()),
        )
}

/// How many shades a [`heat_cell`] has: none, then four steps of the accent.
pub(super) const HEAT_LEVELS: usize = 5;

/// A heatmap cell's fill. Level 0 is the quiet track a meter rests on; the
/// four above it are one hue, the accent, at rising strength, so a darker
/// cell always means more.
pub(super) fn heat_color(level: usize, theme: Theme) -> Rgba {
    match level {
        0 => tint(theme.muted, 0.16),
        1 => tint(theme.accent, 0.30),
        2 => tint(theme.accent, 0.52),
        3 => tint(theme.accent, 0.76),
        _ => theme.accent,
    }
}

/// One square of a heatmap. The caller lays the grid out and attaches the
/// tooltip, since only it knows what a cell stands for.
pub(super) fn heat_cell(
    id: impl Into<ElementId>,
    level: usize,
    side: f32,
    theme: Theme,
) -> Stateful<Div> {
    // A cell is a data mark, not a hover or selection target, so it takes a
    // plain fill. Its corner follows the setting but never rounds a small
    // square into a dot.
    div()
        .id(id)
        .w(px(side))
        .h(px(side))
        .flex_shrink_0()
        .rounded(Radius::Sm.pixels().min(px(side * 0.25)))
        .bg(heat_color(level, theme))
}

/// One entry of a ranking: its name, a muted figure beside it, and a meter
/// showing its share of the leader. The meter is the one accent, since the
/// name already says which entry it is.
pub(super) fn rank_row(
    id: impl Into<ElementId>,
    theme: Theme,
    fonts: Fonts,
    name: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    fraction: f32,
) -> Div {
    let id = id.into();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .truncate()
                        .min_w(px(0.0))
                        .text_size(px(fonts.body))
                        .text_color(theme.panel_fg)
                        .child(name.into()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(detail.into()),
                ),
        )
        .child(meter(
            id,
            Some(fraction),
            theme.accent,
            theme,
            fonts.label * 0.5,
        ))
}

/// A centered empty state: a glyph, a title, and a muted line of detail.
/// Callers append the call to action, so one frame serves a surface with
/// nothing to show and one that cannot show anything yet.
pub(super) fn empty_state(
    theme: Theme,
    fonts: Fonts,
    glyph: impl IntoElement,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .text_center()
        .child(glyph)
        .child(self::title(theme, title.into(), fonts.title))
        .child(
            div()
                .max_w(px(fonts.secondary * 30.0))
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(detail.into()),
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
#[allow(clippy::too_many_arguments)]
fn button_at(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
    height: f32,
    padding: Pixels,
    enabled: bool,
) -> Stateful<Div> {
    let (bg, fg, border) = button_colors(kind, theme);
    let primary = kind == ButtonKind::Primary;
    // The accent fill dims in place: swapping to a panel color would change
    // the button's identity, not just acknowledge the pointer.
    let hover_bg = match kind {
        ButtonKind::Primary => tint(theme.accent, PRIMARY_HOVER_ALPHA),
        ButtonKind::Ghost => theme.panel_dim,
        _ => theme.panel,
    };
    let hover_fg = match kind {
        ButtonKind::Danger => theme.alert,
        _ => theme.panel_fg,
    };
    let bold = primary;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(height))
        // The accent button has no border to dim alongside its fill, so it
        // takes the border's width as padding to stay as wide as its siblings.
        .px(if primary { padding + px(1.0) } else { padding })
        .radius_md()
        .text_color(fg)
        .when(!primary, |button| button.border_1().border_color(border))
        .text_size(px(text_size))
        .font_weight(if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        })
        .when(enabled, |button| button.cursor_pointer())
        .when(!enabled, |button| button.opacity(DISABLED_OPACITY))
        .state_fill(bg, if enabled { hover_bg } else { bg }, RADIUS_MD)
        .when(!primary && enabled, |button| {
            button.hover(move |style| style.text_color(hover_fg))
        })
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
/// the line the content will fill. It pulses a few times and rests dim, so a
/// feed that never answers cannot keep the window repainting; under energy mode
/// it is solid and never moves.
pub(super) fn skeleton(
    id: impl Into<ElementId>,
    theme: Theme,
    width: impl Into<Length> + Clone,
    height: f32,
) -> AnyElement {
    let bar = div()
        .w(width)
        .h(px(height))
        .radius_full()
        .bg(theme.panel_dim);
    // Energy mode rests the placeholder solid rather than pulsing it.
    if energy() {
        return bar.into_any_element();
    }
    bar.with_animation(
        id,
        Animation::new(SKELETON_PULSE * SKELETON_PULSES).with_max_fps(SKELETON_FPS),
        |this, delta| {
            let phase = (delta * SKELETON_PULSES as f32).fract();
            this.opacity(0.55 + 0.45 * phase)
        },
    )
    .into_any_element()
}

/// Width of a toast card: wide enough for a sentence on two lines, narrow
/// enough to leave the terminal readable beside it.
pub(super) const TOAST_WIDTH: f32 = 320.0;

/// An in-app notice: a glyph for what happened, an optional bold title, and
/// the message, clamped to four lines.
///
/// A toast floats over content, so it rests on the sidebar fill with the
/// scheme's border, and its kind shows only in the glyph color. The caller
/// attaches the dismissing click handler and the fade, which belong to the
/// queue that owns the toast.
pub(super) fn toast(
    id: impl Into<ElementId>,
    kind: ToastKind,
    title: Option<&str>,
    body: &str,
    theme: Theme,
    fonts: Fonts,
) -> Stateful<Div> {
    let (icon, tone): (&'static [u8], Rgba) = match kind {
        ToastKind::Info => (include_bytes!("../../assets/icons/info.svg"), theme.muted),
        ToastKind::Success => (include_bytes!("../../assets/icons/check.svg"), theme.ok),
        ToastKind::Error => (
            include_bytes!("../../assets/icons/circle-alert.svg"),
            theme.alert,
        ),
    };
    let glyph = |bytes: &'static [u8], size: f32, color: Rgba| {
        svg()
            .data(bytes)
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .text_color(color)
    };
    let mut text = div().flex().flex_col().gap_1().flex_1().min_w_0();
    if let Some(title) = title {
        text = text.child(
            div()
                .text_size(px(fonts.secondary))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.panel_fg)
                .child(title.to_string()),
        );
    }
    text = text.child(
        div()
            .text_size(px(fonts.secondary))
            .text_color(if title.is_some() {
                theme.muted
            } else {
                theme.panel_fg
            })
            .line_clamp(4)
            .child(body.to_string()),
    );
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .w(px(TOAST_WIDTH))
        .px_3()
        .py_2()
        .radius_md()
        .bg(theme.sidebar_bg)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover_fill(theme.panel, RADIUS_MD)
        .child(glyph(icon, fonts.body, tone))
        .child(text)
        .child(glyph(
            include_bytes!("../../assets/icons/x.svg"),
            fonts.label,
            theme.muted,
        ))
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
    fn hover_fill(self, hover: impl Into<Hsla>, radius: Radius) -> Self {
        self.state_fill(gpui::transparent_black(), hover, radius)
    }

    /// Ease between a resting fill and `hover`. The resting fill animates
    /// too, so a row whose selection changes slides its pill in and out.
    /// It replaces the element's own `.bg()`, which would snap underneath.
    fn state_fill(self, rest: impl Into<Hsla>, hover: impl Into<Hsla>, radius: Radius) -> Self {
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
    radius: Radius,
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

pub(super) fn ease_out_cubic(step: f32) -> f32 {
    1.0 - (1.0 - step).powi(3)
}

/// `color` at `alpha` of its own opacity: a wash that lets the surface under
/// it show through, such as the diff's added and removed lines.
pub(super) fn tint(color: Rgba, alpha: f32) -> Rgba {
    Rgba {
        a: color.a * alpha,
        ..color
    }
}

/// Blend two colors. A transparent end borrows the other end's hue, so a
/// fill fading in from nothing never passes through a grey or black tint.
pub(super) fn mix(from: Rgba, to: Rgba, step: f32) -> Rgba {
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
            let radius = self.radius.pixels();
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

/// How long a page's side pane takes to slide open or shut: long enough for
/// the spring to overshoot and settle, as the sidebar's does.
const SLIDE: Duration = Duration::from_millis(420);

/// A page's side pane sliding open or shut on [`ease_spring`]. The page calls
/// [`PaneSlide::sync`] once per render with whether the pane should be open and
/// draws what it returns, so a reversal mid-slide glides from where it is.
#[derive(Debug, Default)]
pub(super) struct PaneSlide {
    open: bool,
    from: f32,
    started: Option<Instant>,
}

/// One frame of a [`PaneSlide`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Slide {
    /// How open the pane is: 0 shut, 1 open, a touch past 1 on the overshoot.
    pub(super) factor: f32,
    /// Whether it is still moving, so the page asks for another frame.
    pub(super) moving: bool,
}

impl PaneSlide {
    fn at(&self, cx: &App) -> Slide {
        let target = if self.open { 1.0 } else { 0.0 };
        let step = self
            .started
            .map(|started| started.elapsed().as_secs_f32() / SLIDE.as_secs_f32());
        match step {
            Some(step) if step < 1.0 && !cx.reduce_motion() => Slide {
                factor: (self.from + (target - self.from) * ease_spring(step)).max(0.0),
                moving: true,
            },
            _ => Slide {
                factor: target,
                moving: false,
            },
        }
    }

    /// Aim the pane at `open` and sample this frame.
    pub(super) fn sync(&mut self, open: bool, cx: &App) -> Slide {
        if open != self.open {
            self.from = self.at(cx).factor;
            self.started = Some(Instant::now());
            self.open = open;
        }
        self.at(cx)
    }
}

impl Slide {
    /// Whether the pane takes any room this frame: open, or still closing.
    pub(super) fn shown(self) -> bool {
        self.factor > 0.0 || self.moving
    }

    /// The room the pane takes of a `width` it opens to.
    pub(super) fn room(self, width: f32) -> f32 {
        width * self.factor
    }

    /// `pane` in a clip that opens to `width`. The pane keeps its own width
    /// while the clip grows, so it is revealed rather than squeezed, and
    /// stretches only for the overshoot.
    pub(super) fn wrap(self, width: f32, pane: impl IntoElement) -> Div {
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .overflow_hidden()
            .w(px(self.room(width)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .h_full()
                    .w(px(width * self.factor.max(1.0)))
                    .child(pane),
            )
            .children(self.moving.then(next_frame))
    }
}

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
    glide_target_shifted(slot, 0.0)
}

/// [`glide_target`] for an item drawn `shift` pixels right of its layout slot:
/// the pill flies to the slot, not after the item, so an item that slides on
/// its own spring does not drag a second spring behind it.
pub(super) fn glide_target_shifted(slot: &GlideSlot, shift: f32) -> impl IntoElement {
    let slot = slot.clone();
    BoundsMarker(Box::new(move |bounds| {
        slot.0.set(Some(Bounds {
            origin: point(bounds.origin.x - px(shift), bounds.origin.y),
            size: bounds.size,
        }));
    }))
}

/// Where each keyed item was drawn last frame, in window coordinates.
#[derive(Clone, Default)]
pub(super) struct BoundsLedger(Rc<RefCell<HashMap<String, Bounds<Pixels>>>>);

impl BoundsLedger {
    pub(super) fn get(&self, key: &str) -> Option<Bounds<Pixels>> {
        self.0.borrow().get(key).copied()
    }
}

/// Write the element this is a child of into `ledger` under `key`, so the next
/// change to the list can start its motion from where each item actually is.
pub(super) fn record_bounds(ledger: &BoundsLedger, key: String) -> impl IntoElement {
    let ledger = ledger.clone();
    BoundsMarker(Box::new(move |bounds| {
        ledger.0.borrow_mut().insert(key.clone(), bounds);
    }))
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
    radius: Radius,
    bleed: f32,
) -> impl IntoElement {
    Glide {
        id: id.into(),
        slot: slot.clone(),
        color,
        radius,
        bleed,
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
    radius: Radius,
    bleed: f32,
) -> impl IntoElement {
    HoverGlide {
        id: id.into(),
        slot: slot.clone(),
        color,
        radius,
        bleed,
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
    radius: Radius,
    bleed: f32,
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
            let pill = bled(
                Bounds {
                    origin: pill.origin + bounds.origin,
                    size: pill.size,
                },
                self.bleed,
            );
            let radius = self.radius.pixels();
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

/// A zero-size overlay that hands its parent's bounds to `record` while the
/// frame prepaints, ahead of any paint that reads them.
struct BoundsMarker(Box<dyn Fn(Bounds<Pixels>)>);

impl IntoElement for BoundsMarker {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BoundsMarker {
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
        (self.0)(bounds);
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
    radius: Radius,
    bleed: f32,
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
        let pill = bled(
            Bounds {
                origin: pill.origin + bounds.origin,
                size: pill.size,
            },
            self.bleed,
        );
        let radius = self.radius.pixels();
        window.paint_quad(
            gpui::fill(pill, self.color)
                .corner_radii(Corners::all(radius).clamp_radii_for_quad_size(pill.size)),
        );
    }
}

/// A pill widened by `bleed` on both sides while the corner radius is zero,
/// so a square selection or hover fill runs flush to the edge of the list it
/// sits in instead of leaving the rounded pill's gap.
fn bled(pill: Bounds<Pixels>, bleed: f32) -> Bounds<Pixels> {
    if bleed <= 0.0 || Radius::Md.pixels() > px(0.0) {
        return pill;
    }
    Bounds {
        origin: point(pill.origin.x - px(bleed), pill.origin.y),
        size: gpui::size(pill.size.width + px(2.0 * bleed), pill.size.height),
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
        .child(field_box(focused, theme, height).child(body))
}

/// The boxed control a [`field`] and a [`search_field`] share.
fn field_box(focused: bool, theme: Theme, height: f32) -> Div {
    div()
        .flex()
        .items_center()
        .min_h(px(height))
        .px_2()
        .radius_md()
        .bg(theme.panel_dim)
        .border_1()
        .border_color(if focused {
            theme.border_focus
        } else {
            theme.border
        })
        .cursor_pointer()
}

/// A field with no label, for narrowing a list in place: a magnifier before
/// the editable body. The caller owns focus and click behavior, as with
/// [`field`].
pub(super) fn search_field(
    id: impl Into<ElementId>,
    focused: bool,
    theme: Theme,
    height: f32,
    icon_size: f32,
    body: impl IntoElement,
) -> Stateful<Div> {
    div().id(id).child(
        field_box(focused, theme, height)
            .gap_2()
            .child(super::icon(
                include_bytes!("../../assets/icons/search.svg"),
                icon_size,
                theme.muted,
            ))
            .child(div().flex_1().min_w(px(0.0)).child(body)),
    )
}

/// How long a folding card takes to reach its new height.
pub(super) const FOLD: Duration = Duration::from_millis(220);

/// How far a size change started at `started` has eased over `duration`,
/// from 0 to 1, or `None` once it has settled. Reduce motion settles it at
/// once.
pub(super) fn eased(started: Instant, duration: Duration, cx: &App) -> Option<f32> {
    let step = started.elapsed().as_secs_f32() / duration.as_secs_f32();
    (!cx.reduce_motion() && step < 1.0).then(|| ease_out_cubic(step))
}

/// The quiet zone around a QR code, in modules. The spec asks for four; three
/// scans reliably on a phone camera and keeps the code compact in a panel.
const QR_QUIET: usize = 3;

/// A QR code for `text`, as a square plate of at most `side` pixels.
///
/// Modules are painted as horizontal runs, one quad per run, and snapped to
/// whole pixels so a scanner never sees a hairline between neighbours. The
/// plate is always light and the modules always dark, whatever the theme.
/// `text` that does not fit a QR code paints an empty plate rather than failing.
pub(super) fn qr_code(text: &str, side: f32) -> Div {
    use qrcode::{Color, QrCode};

    let grid = QrCode::new(text.as_bytes()).ok().map(|code| {
        let width = code.width();
        (width, code.to_colors())
    });
    let modules = grid.as_ref().map_or(1, |(width, _)| *width) + 2 * QR_QUIET;
    // Whole pixels per module; a code too large for `side` shrinks the plate
    // below it rather than blurring the modules.
    let cell = (side / modules as f32).floor().max(1.0);
    let plate = cell * modules as f32;
    let dark: Hsla = super::theme::qr_dark().into();
    let light: Hsla = super::theme::qr_light().into();
    div()
        .w(px(plate))
        .h(px(plate))
        .flex_shrink_0()
        .radius_md()
        .overflow_hidden()
        .bg(light)
        .child(
            canvas(
                move |_bounds, _window, _cx| (),
                move |bounds, (), window, _cx| {
                    let Some((width, colors)) = &grid else {
                        return;
                    };
                    for row in 0..*width {
                        let mut column = 0;
                        while column < *width {
                            if colors[row * width + column] != Color::Dark {
                                column += 1;
                                continue;
                            }
                            let start = column;
                            while column < *width && colors[row * width + column] == Color::Dark {
                                column += 1;
                            }
                            let run = Bounds::new(
                                point(
                                    bounds.origin.x + px(cell * (QR_QUIET + start) as f32),
                                    bounds.origin.y + px(cell * (QR_QUIET + row) as f32),
                                ),
                                gpui::size(px(cell * (column - start) as f32), px(cell)),
                            );
                            window.paint_quad(gpui::fill(run, dark));
                        }
                    }
                },
            )
            .size_full(),
        )
}

/// An empty element that asks for another frame each time it paints. A
/// view whose layout is mid-tween (a card folding, its lines following)
/// adds one for as long as the tween runs.
pub(super) fn next_frame() -> impl IntoElement {
    canvas(
        |_bounds, _window, _cx| (),
        |_bounds, (), window, _cx| window.request_animation_frame(),
    )
    .absolute()
    .size_0()
}

/// A card on a board: a fixed box the caller fills and places.
///
/// Cards are interactive content, so they outline with `theme.border`; the
/// selected one rests on the stronger panel fill and takes an accent ring,
/// which fades in rather than snapping on. The caller sets the size, because
/// a board positions its cards and draws connectors to their edges.
pub(super) fn board_card(
    id: impl Into<SharedString>,
    selected: bool,
    theme: Theme,
) -> Stateful<Div> {
    let id = id.into();
    let rest = if selected {
        theme.panel
    } else {
        theme.panel_dim
    };
    div()
        .id(ElementId::Name(id.clone()))
        .flex()
        .flex_col()
        .overflow_hidden()
        .radius_md()
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .state_fill(rest, theme.panel, RADIUS_MD)
        .children(selected.then(|| {
            enter(
                ElementId::Name(format!("{id}-ring").into()),
                // Laid over the card's own border: the card clips its
                // overflow, so a ring outside it would never show.
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .radius_md()
                    .border_1()
                    .border_color(theme.border_focus),
            )
        }))
}

/// Which way a connector leaves or enters a card.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Side {
    Left,
    Right,
}

impl Side {
    /// The outward direction along x.
    fn sign(self) -> f32 {
        match self {
            Self::Left => -1.0,
            Self::Right => 1.0,
        }
    }
}

/// One line between two cards, in the coordinates of the board it is drawn
/// on. It ends in an arrowhead pointing into the `to` card.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Connector {
    pub(super) from: (f32, f32),
    pub(super) from_side: Side,
    pub(super) to: (f32, f32),
    pub(super) to_side: Side,
    pub(super) color: Hsla,
    /// A settled link draws dashed, so the lines still holding work back
    /// read first.
    pub(super) dashed: bool,
}

/// How far an arrowhead reaches back from its tip, at stroke width 1.5.
const ARROW_LENGTH: f32 = 7.0;

impl Connector {
    /// The curve's end at its arrowhead's base, and its two control points.
    /// Controls reach out sideways, so every curve leaves and enters its card
    /// level; a link inside one lane bows out instead of cutting across the
    /// cards between. The arrowhead takes the last stretch, so the curve stops
    /// at its base and the stroke never pokes through the tip.
    fn controls(&self, stroke: f32) -> ((f32, f32), (f32, f32), (f32, f32)) {
        let (fx, fy) = self.from;
        let (tx, ty) = self.to;
        let head = ARROW_LENGTH * (stroke / 1.5).max(1.0);
        let base = (tx + self.to_side.sign() * head, ty);
        let reach = ((tx - fx).abs() / 2.0).max(36.0);
        let ctrl_a = (fx + self.from_side.sign() * reach, fy);
        let ctrl_b = (base.0 + self.to_side.sign() * reach, base.1);
        (base, ctrl_a, ctrl_b)
    }

    /// Where the line passes its own middle: the spot for a handle on it.
    pub(super) fn midpoint(&self, stroke: f32) -> (f32, f32) {
        let (base, a, b) = self.controls(stroke);
        // A cubic Bezier at t = 1/2 weighs its four points 1:3:3:1.
        (
            (self.from.0 + 3.0 * a.0 + 3.0 * b.0 + base.0) / 8.0,
            (self.from.1 + 3.0 * a.1 + 3.0 * b.1 + base.1) / 8.0,
        )
    }
}

/// Draw `connectors` over the whole parent box. Place it before the cards so
/// the lines run beneath them.
pub(super) fn connectors(connectors: Vec<Connector>, stroke: f32) -> gpui::Canvas<()> {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, (), window, _cx| {
            let at = |(x, y): (f32, f32)| point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            for line in &connectors {
                let head = ARROW_LENGTH * (stroke / 1.5).max(1.0);
                let (base, ctrl_a, ctrl_b) = line.controls(stroke);
                let mut curve = PathBuilder::stroke(px(stroke));
                if line.dashed {
                    curve = curve.dash_array(&[px(4.0), px(4.0)]);
                }
                curve.move_to(at(line.from));
                curve.cubic_bezier_to(at(base), at(ctrl_a), at(ctrl_b));
                if let Ok(path) = curve.build() {
                    window.paint_path(path, line.color);
                }
                let spread = head * 0.55;
                let mut arrow = PathBuilder::fill();
                arrow.add_polygon(
                    &[
                        at(line.to),
                        at((base.0, base.1 - spread)),
                        at((base.0, base.1 + spread)),
                    ],
                    true,
                );
                if let Ok(path) = arrow.build() {
                    window.paint_path(path, line.color);
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A field of dots `step` pixels apart over the whole parent box, the ground
/// a canvas sits on. Only the part of the field in view is painted, so a
/// canvas as large as its scroll extent costs no more than the window.
pub(super) fn dot_grid(color: Hsla, step: f32) -> gpui::Canvas<()> {
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, (), window, _cx| {
            let view = window.content_mask().bounds.intersect(&bounds);
            if view.size.width <= px(0.0) || view.size.height <= px(0.0) {
                return;
            }
            let dot = 2.0;
            let first = |from: Pixels, origin: Pixels| {
                let offset = (from - origin).as_f32();
                origin.as_f32() + (offset / step).floor().max(0.0) * step
            };
            let (left, top) = (view.left(), view.top());
            let mut y = first(top, bounds.top());
            while y < view.bottom().as_f32() {
                let mut x = first(left, bounds.left());
                while x < view.right().as_f32() {
                    window.paint_quad(
                        gpui::fill(
                            Bounds::new(point(px(x), px(y)), gpui::size(px(dot), px(dot))),
                            color,
                        )
                        .corner_radii(Corners::all(px(dot / 2.0))),
                    );
                    x += step;
                }
                y += step;
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_radius_leaves_the_pill_and_the_tab_soft() {
        assert_eq!(Radius::Full.resolve(0.0, false), px(9999.0));
        assert_eq!(Radius::Soft.resolve(0.0, false), px(CORNER_RADIUS));
    }

    #[test]
    fn energy_mode_squares_every_step() {
        for step in [
            Radius::None,
            Radius::Sm,
            Radius::Md,
            Radius::Lg,
            Radius::Full,
            Radius::Soft,
        ] {
            assert_eq!(step.resolve(8.0, true), px(0.0), "{step:?}");
        }
    }
}
