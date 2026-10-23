//! Mermaid diagrams for rendered markdown.
//!
//! `markdown` asks [`lookup`] for the diagram a ```` ```mermaid ```` fence
//! describes and draws the fence as code until it is ready. A miss starts a
//! worker thread that lays the diagram out (`bezel-mermaid`), draws the SVG it
//! returns with resvg, and parks the bitmap in a cache keyed by the source and
//! the palette it was drawn in. The worker then bumps
//! [`generation`] and signals the tick loop; `PilusView::poll` repaints on the
//! bump, so the GUI thread never waits on a layout.
//!
//! The SVG comes from our own renderer, but its text comes from a file or a
//! GitHub body, so it is parsed with `about::untrusted_svg_options` like any
//! SVG from another machine, the source is capped before it is laid out, and
//! the renderer is told to leave links and images out of the drawing.
//!
//! Labels are measured with Arial's advances (`bezel::Metrics`), so they are
//! drawn in Arial too: another face would run over or short of its box.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use gpui::{RenderImage, Rgba};
use resvg::usvg::fontdb;

use super::about;
use super::style::mix;
use super::theme::Theme;

/// The longest diagram source laid out. Layout cost grows faster than the
/// text, and a pathological body must not be able to hold a worker for long.
const MAX_SOURCE: usize = 32 * 1024;
/// Diagrams kept at once. A bitmap is a few megabytes, so the cache stays small
/// and drops the one drawn least recently.
const MAX_ENTRIES: usize = 24;
/// The face labels are measured in and drawn in. See the module header.
const FAMILY: &str = "Arial";
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

/// The diagram for `source` in `theme`'s colors, with labels at `size`. Never
/// blocks: a diagram not yet drawn comes back [`State::Pending`] and a worker
/// draws it.
pub(super) fn lookup(source: &str, theme: &Theme, size: f32) -> State {
    if source.len() > MAX_SOURCE || source.trim().is_empty() {
        return State::Failed;
    }
    let style = Style::new(theme, size);
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
    let options = bezel::Options {
        font_family: Some(FAMILY.to_string()),
        font_size: f64::from(style.size) / 10.0,
        colors: Some(style.colors.clone()),
        // A bitmap has nothing to click, and an image would be a fetch the
        // source's author chose for the reader.
        links: false,
        images: false,
        ..bezel::Options::default()
    };
    let svg = match bezel::render(source, &options, &bezel::Metrics) {
        Ok(drawing) => drawing.svg,
        Err(error) => {
            tracing::debug!(?error, "mermaid diagram did not draw");
            return None;
        }
    };
    rasterize(&with_family(&svg))
}

/// `svg` with its face set as an attribute. resvg reads a font family from an
/// attribute but not from the `style` the renderer sets it in, and would draw
/// every label in its serif default.
fn with_family(svg: &str) -> String {
    svg.replacen("<svg ", &format!("<svg font-family=\"{FAMILY}\" "), 1)
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

/// What a diagram's look depends on: the cache key, and the renderer's colors.
#[derive(Clone, Hash)]
struct Style {
    /// Tenths of a pixel, so the key hashes.
    size: u32,
    colors: bezel::Colors,
}

impl Style {
    fn new(theme: &Theme, size: f32) -> Self {
        // Nodes stand a step above the card, and an edge label sits on it.
        let rule = mix(theme.border, theme.muted, 0.3);
        let colors = bezel::Colors {
            bg: bytes(theme.panel_dim),
            fg: bytes(theme.fg),
            muted: bytes(theme.muted),
            line: bytes(theme.muted),
            surface: bytes(theme.panel),
            surface_alt: bytes(theme.panel_dim),
            border: bytes(rule),
            accent: bytes(theme.accent),
            critical: bytes(theme.alert),
            // One hue per pie slice or branch, in the order the theme names them.
            series: [
                theme.accent,
                theme.ok,
                theme.branch,
                theme.alert,
                theme.ansi[3],
                theme.ansi[6],
                theme.ansi[5],
                mix(theme.accent, theme.fg, 0.5),
            ]
            .map(bytes),
        };
        Self {
            size: (size * 10.0).round() as u32,
            colors,
        }
    }
}

fn bytes(c: Rgba) -> [u8; 4] {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(c.r), byte(c.g), byte(c.b), byte(c.a)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "flowchart LR\n  A[Client] --> B{Auth?}\n  B -->|yes| C[Daemon]\n  B -->|no| D[Reject]\n";

    #[test]
    fn style_hashes_by_colors_and_size() {
        let theme = Theme::default();
        let a = Style::new(&theme, 13.0);
        let b = Style::new(&theme, 13.0);
        let c = Style::new(&theme, 14.0);
        let mut other = theme;
        other.panel = gpui::rgb(0x336699);
        let d = Style::new(&other, 13.0);
        assert_eq!(key("x", &a), key("x", &b));
        assert_ne!(key("x", &a), key("x", &c));
        assert_ne!(key("x", &a), key("x", &d));
        assert_ne!(key("x", &a), key("y", &a));
    }

    #[test]
    fn a_flowchart_draws_to_a_bitmap() {
        let style = Style::new(&Theme::default(), 13.0);
        let diagram = draw(FLOW, &style).expect("flowchart draws");
        assert!(diagram.width > 50.0 && diagram.height > 20.0);
    }

    #[test]
    fn nonsense_fails_instead_of_panicking() {
        let style = Style::new(&Theme::default(), 13.0);
        assert!(draw("this is not a diagram", &style).is_none());
    }

    #[test]
    fn oversized_source_is_refused_without_a_worker() {
        let source = "A-->B\n".repeat(MAX_SOURCE);
        assert!(matches!(
            lookup(&source, &Theme::default(), 13.0),
            State::Failed
        ));
    }

    #[test]
    fn a_source_cannot_ask_for_an_image() {
        let options = bezel::Options {
            images: false,
            links: false,
            ..bezel::Options::default()
        };
        let source = "flowchart LR\n  A@{ img: \"https://example.com/x.png\", label: \"hi\", pos: \"t\", w: 60, h: 60 }\n";
        let drawing = bezel::render(source, &options, &bezel::Metrics).expect("node draws");
        assert!(!drawing.svg.contains("<image"), "{}", drawing.svg);
    }

    #[test]
    fn the_face_lands_on_the_root_element_only() {
        let svg = with_family("<svg width=\"1\"><text>a</text><svg x=\"0\"/></svg>");
        assert_eq!(
            svg,
            "<svg font-family=\"Arial\" width=\"1\"><text>a</text><svg x=\"0\"/></svg>"
        );
    }

    #[test]
    fn a_drawing_names_the_face_it_was_measured_in() {
        let style = Style::new(&Theme::default(), 13.0);
        let options = bezel::Options {
            font_family: Some(FAMILY.to_string()),
            colors: Some(style.colors),
            ..bezel::Options::default()
        };
        let drawing = bezel::render(FLOW, &options, &bezel::Metrics).expect("flowchart draws");
        assert!(drawing.svg.contains("font-family:Arial"));
    }
}
