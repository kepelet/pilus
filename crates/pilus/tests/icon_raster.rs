//! Guard: the vendored lucide SVGs rasterize to a non-empty alpha mask
//! through the same resvg/usvg pipeline GPUI paints with.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

const ICONS: [&str; 14] = [
    "chevron-down",
    "chevron-right",
    "circle",
    "clock",
    "cloud",
    "folder",
    "layout-panel-left",
    "list-checks",
    "panel-left",
    "plus",
    "server",
    "settings",
    "workflow",
    "x",
];

/// The vendored agent brand marks, mirrored from `agent_icon` in the GUI.
const AGENT_ICONS: [&str; 16] = [
    "amp",
    "antigravity",
    "arc",
    "bot",
    "claude-code",
    "codex",
    "cursor",
    "gemini",
    "github-copilot",
    "grok",
    "hermes",
    "kilo-code",
    "kimi",
    "opencode",
    "pi",
    "qwen",
];

/// Assert one SVG parses and paints a non-empty alpha mask.
fn assert_alpha(path: &str) {
    let bytes = std::fs::read(path).expect("icon file exists");
    assert!(!bytes.is_empty(), "{path} is empty");
    let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default())
        .expect("icon parses");
    let size = tree.size();
    let scale = 30.0 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(30, 30).expect("pixmap");
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let alpha: u32 = pixmap
        .pixels()
        .iter()
        .map(|pixel| u32::from(pixel.alpha()))
        .sum();
    assert!(alpha > 0, "{path} rendered an empty mask");
}

#[test]
fn lucide_icons_rasterize_to_nonempty_alpha() {
    for name in ICONS {
        assert_alpha(&format!("assets/icons/{name}.svg"));
    }
}

#[test]
fn agent_icons_rasterize_to_nonempty_alpha() {
    for name in AGENT_ICONS {
        assert_alpha(&format!("assets/icons/agents/{name}.svg"));
    }
}

#[test]
fn app_mark_rasterizes_to_nonempty_alpha() {
    assert_alpha("assets/pilus.svg");
}
