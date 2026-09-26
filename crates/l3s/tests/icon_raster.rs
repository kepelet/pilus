//! Guard: the vendored lucide SVGs rasterize to a non-empty alpha mask
//! through the same resvg/usvg pipeline GPUI paints with.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

const ICONS: [&str; 13] = [
    "chevron-down",
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

#[test]
fn lucide_icons_rasterize_to_nonempty_alpha() {
    for name in ICONS {
        let bytes = std::fs::read(format!("assets/icons/{name}.svg")).expect("icon file exists");
        assert!(!bytes.is_empty(), "{name} is empty");
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
        assert!(alpha > 0, "{name} rendered an empty mask");
    }
}
