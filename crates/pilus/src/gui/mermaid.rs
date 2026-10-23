//! Mermaid diagrams for rendered markdown.
//!
//! `markdown` asks [`lookup`] for the diagram a ```` ```mermaid ```` fence
//! describes and draws the fence as code until it is ready. A miss starts a
//! worker thread that lays the diagram out (`mermaid-rs-renderer`), draws the
//! SVG it returns with resvg, and parks the bitmap in a cache keyed by the
//! source and the palette it was drawn in. The worker then bumps
//! [`generation`] and signals the tick loop; `PilusView::poll` repaints on the
//! bump, so the GUI thread never waits on a layout.
//!
//! The SVG comes from our own renderer, but its text comes from a file or a
//! GitHub body, so it is parsed with `about::untrusted_svg_options` like any
//! SVG from another machine, and the source is capped before it is laid out.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use gpui::{RenderImage, Rgba};
use mermaid_rs_renderer::{LayoutConfig, RenderOptions};
use resvg::usvg::fontdb;

use super::about;
use super::theme::Theme;

/// The longest diagram source laid out. Layout cost grows faster than the
/// text, and a pathological body must not be able to hold a worker for long.
const MAX_SOURCE: usize = 32 * 1024;
/// Diagrams kept at once. A bitmap is a few megabytes, so the cache stays small
/// and drops the one drawn least recently.
const MAX_ENTRIES: usize = 24;
/// Pixels drawn per logical pixel, so a diagram stays sharp on a retina screen.
const SHARPNESS: f32 = 2.0;
/// The longest edge of a bitmap, in pixels. A larger diagram is drawn smaller
/// rather than allocating a bitmap the size of a poster.
const MAX_EDGE: f32 = 3000.0;

/// A drawn diagram.
pub(super) struct Diagram {
    pub(super) image: Arc<RenderImage>,
    /// Its natural size in logical pixels.
    pub(super) width: f32,
    pub(super) height: f32,
}

/// Where a diagram stands.
#[derive(Clone)]
pub(super) enum State {
    /// A worker is laying it out.
    Pending,
    Ready(Arc<Diagram>),
    /// The source does not parse, or drew nothing.
    Failed,
}

struct Entry {
    state: State,
    /// [`CLOCK`] when it was last asked for.
    used: u64,
}

static CACHE: Mutex<Option<HashMap<u64, Entry>>> = Mutex::new(None);
static CLOCK: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Counts finished diagrams, so the window can repaint when one lands.
pub(super) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The diagram for `source` in `theme`'s colors, set in `family` at `size`.
/// Never blocks: a diagram not yet drawn comes back [`State::Pending`] and a
/// worker draws it.
pub(super) fn lookup(source: &str, theme: &Theme, family: &str, size: f32) -> State {
    if source.len() > MAX_SOURCE || source.trim().is_empty() {
        return State::Failed;
    }
    let style = Style::new(theme, family, size);
    let key = key(source, &style);
    let now = CLOCK.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = cache();
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(entry) = map.get_mut(&key) {
            entry.used = now;
            return entry.state.clone();
        }
        if map.len() >= MAX_ENTRIES {
            let oldest = map
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, State::Pending))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(
            key,
            Entry {
                state: State::Pending,
                used: now,
            },
        );
    }
    let source = source.to_string();
    let worker = std::thread::Builder::new()
        .name("pilus-mermaid".into())
        .spawn(move || {
            // The renderer is young; a panic in it must end as a failed
            // diagram, not as a card stuck on "pending" forever.
            let drawn = catch_unwind(AssertUnwindSafe(|| draw(&source, &style))).unwrap_or(None);
            let state = drawn.map_or(State::Failed, |diagram| State::Ready(Arc::new(diagram)));
            finish(key, state);
        });
    if worker.is_err() {
        finish(key, State::Failed);
    }
    State::Pending
}

fn finish(key: u64, state: State) {
    if let Some(entry) = cache().get_or_insert_with(HashMap::new).get_mut(&key) {
        entry.state = state;
    }
    GENERATION.fetch_add(1, Ordering::Release);
    // The tick loop sleeps between ticks; this wakes it to repaint now.
    pilus_client::Arrival::global().signal();
}

fn cache() -> MutexGuard<'static, Option<HashMap<u64, Entry>>> {
    CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(source: &str, style: &Style) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    style.hash(&mut hasher);
    hasher.finish()
}

/// Lay out `source` and draw it.
fn draw(source: &str, style: &Style) -> Option<Diagram> {
    let options = RenderOptions {
        theme: style.theme(),
        layout: LayoutConfig::default(),
    };
    let svg = match mermaid_rs_renderer::render_with_options(source, options) {
        Ok(svg) => svg,
        Err(error) => {
            tracing::debug!(%error, "mermaid diagram did not parse");
            return None;
        }
    };
    rasterize(&svg)
}

/// Every system font, loaded once. A diagram is mostly labels, and resvg
/// draws none of them without a font database. The Files tab's SVG viewer
/// shares it.
pub(super) fn fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut database = fontdb::Database::new();
            database.load_system_fonts();
            Arc::new(database)
        })
        .clone()
}

fn rasterize(svg: &str) -> Option<Diagram> {
    let mut options = about::untrusted_svg_options();
    options.fontdb = fonts();
    let tree = resvg::usvg::Tree::from_str(svg, &options).ok()?;
    let (width, height) = (tree.size().width(), tree.size().height());
    if !(width >= 1.0 && height >= 1.0) {
        return None;
    }
    let scale = SHARPNESS.min(MAX_EDGE / width).min(MAX_EDGE / height);
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new((width * scale).ceil() as u32, (height * scale).ceil() as u32)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(Diagram {
        image: about::image_from_pixmap(pixmap)?,
        width,
        height,
    })
}

/// What a diagram's look depends on: the cache key, and the renderer's theme.
#[derive(Clone, Hash)]
struct Style {
    dark: bool,
    family: String,
    /// Tenths of a pixel, so the key hashes.
    size: u32,
    /// Node fill.
    node: String,
    /// Text.
    text: String,
    /// Lines, arrowheads and node outlines.
    line: String,
    /// What the diagram sits on, which edge labels paint over.
    surface: String,
    /// Group boxes and their outlines.
    group: String,
    group_edge: String,
    /// Notes in a sequence diagram.
    note: String,
    note_edge: String,
    /// One color per pie slice or git branch: the theme's hues, so a series
    /// stays apart on the card.
    series: Vec<String>,
}

impl Style {
    fn new(theme: &Theme, family: &str, size: f32) -> Self {
        // A family with a quote in it would end the SVG attribute it lands in.
        let family: String = family.chars().filter(|c| !"\"'<>&".contains(*c)).collect();
        Self {
            dark: luminance(theme.bg) < 0.5,
            family: format!("{family}, Helvetica Neue, Arial, sans-serif"),
            size: (size * 10.0).round() as u32,
            node: hex(theme.panel),
            text: hex(theme.fg),
            line: hex(theme.muted),
            surface: hex(theme.panel_dim),
            group: hex(theme.bg),
            group_edge: hex(theme.border),
            note: hex(theme.selection_bg),
            note_edge: hex(theme.accent),
            series: (1..=6).chain(9..=14).map(|slot| hex(theme.ansi[slot])).collect(),
        }
    }

    /// The renderer's theme with our colors over the nearest of its own, so the
    /// diagram types that color by series (pie, git) still get a palette.
    fn theme(&self) -> mermaid_rs_renderer::Theme {
        let mut theme = if self.dark {
            mermaid_rs_renderer::Theme::dark()
        } else {
            mermaid_rs_renderer::Theme::modern()
        };
        theme.font_family = self.family.clone();
        theme.font_size = self.size as f32 / 10.0;
        // The card behind the diagram is the background.
        theme.background = "none".into();
        theme.primary_color = self.node.clone();
        theme.primary_text_color = self.text.clone();
        theme.primary_border_color = self.line.clone();
        theme.text_color = self.text.clone();
        theme.line_color = self.line.clone();
        theme.secondary_color = self.group.clone();
        theme.tertiary_color = self.surface.clone();
        theme.edge_label_background = self.surface.clone();
        theme.cluster_background = self.group.clone();
        theme.cluster_border = self.group_edge.clone();
        theme.sequence_actor_fill = self.node.clone();
        theme.sequence_actor_border = self.line.clone();
        theme.sequence_actor_line = self.line.clone();
        theme.sequence_note_fill = self.note.clone();
        theme.sequence_note_border = self.note_edge.clone();
        theme.sequence_activation_fill = self.group_edge.clone();
        theme.sequence_activation_border = self.line.clone();
        for (slot, color) in theme.pie_colors.iter_mut().zip(&self.series) {
            *slot = color.clone();
        }
        for (slot, color) in theme.git_colors.iter_mut().zip(&self.series) {
            *slot = color.clone();
        }
        theme.pie_title_text_color = self.text.clone();
        theme.pie_section_text_color = self.text.clone();
        theme.pie_legend_text_color = self.text.clone();
        theme.pie_stroke_color = self.surface.clone();
        theme.pie_outer_stroke_color = self.group_edge.clone();
        theme
    }
}

fn luminance(c: Rgba) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

fn hex(c: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "flowchart LR\n  A[Client] --> B{Auth?}\n  B -->|yes| C[Daemon]\n  B -->|no| D[Reject]\n";

    #[test]
    fn style_hashes_by_colors_and_family() {
        let dark = Theme::default();
        let a = Style::new(&dark, "Inter", 13.0);
        let b = Style::new(&dark, "Inter", 13.0);
        let c = Style::new(&dark, "Menlo", 13.0);
        assert_eq!(key("x", &a), key("x", &b));
        assert_ne!(key("x", &a), key("x", &c));
        assert_ne!(key("x", &a), key("y", &a));
    }

    #[test]
    fn family_cannot_break_out_of_its_attribute() {
        let style = Style::new(&Theme::default(), "Evil\" onload=\"x", 13.0);
        assert!(!style.family.contains('"'));
    }

    #[test]
    fn a_flowchart_draws_to_a_bitmap() {
        let style = Style::new(&Theme::default(), "Helvetica Neue", 13.0);
        let diagram = draw(FLOW, &style).expect("flowchart draws");
        assert!(diagram.width > 50.0 && diagram.height > 20.0);
    }

    #[test]
    fn nonsense_fails_instead_of_panicking() {
        let style = Style::new(&Theme::default(), "Helvetica Neue", 13.0);
        assert!(draw("this is not a diagram", &style).is_none());
    }

    #[test]
    fn oversized_source_is_refused_without_a_worker() {
        let source = "A-->B\n".repeat(MAX_SOURCE);
        assert!(matches!(
            lookup(&source, &Theme::default(), "Inter", 13.0),
            State::Failed
        ));
    }
}
