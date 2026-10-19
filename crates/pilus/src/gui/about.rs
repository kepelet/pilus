//! The native About window: the app name, version, and credits.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use gpui::{
    App, Context, FocusHandle, KeyDownEvent, Render, RenderImage, Window, WindowBounds,
    WindowOptions, div, img, prelude::*, px, size,
};

use super::style;
use super::theme::Theme;
use super::{CloseTab, CloseWindow};

/// Titlebar inset, matching the Appearances window.
const WINDOW_SIZE: (f32, f32) = (420.0, 360.0);
/// The smallest the window can be dragged to: below this its rows and
/// controls start to clip.
const WINDOW_MIN_SIZE: (f32, f32) = (360.0, 280.0);

/// One credit line: a name and what it is.
struct Credit {
    name: &'static str,
    detail: &'static str,
}

const CREDITS: &[Credit] = &[
    Credit {
        name: "Luvus",
        detail: "The daemon this client attaches to.",
    },
    Credit {
        name: "GPUI",
        detail: "The GPU-accelerated UI framework.",
    },
    Credit {
        name: "Lucide",
        detail: "The open-source icon set.",
    },
    Credit {
        name: "Ratatui",
        detail: "The daemon's terminal UI toolkit.",
    },
];

/// The About window root view.
pub struct AboutView {
    theme: Theme,
    /// Shared client settings; only the Interface font is read here.
    settings: Rc<RefCell<crate::settings::Settings>>,
    /// Version to show: the preview stamp when this build carries one, else
    /// the Cargo version.
    version: String,
    /// The installed Luvus binary's version, resolved in the background.
    luvus_version: Option<String>,
    focus_handle: FocusHandle,
}

impl AboutView {
    pub(super) fn new(
        theme: Theme,
        settings: Rc<RefCell<crate::settings::Settings>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        // `preview-feed.sh` stamps the feed's synthetic version into the
        // build, so an installed preview names itself here.
        let version = crate::update::current_version().to_string();
        let view = Self {
            theme,
            settings,
            version,
            luvus_version: None,
            focus_handle,
        };
        // `luvus --version` shells out, so resolve it off the GUI thread and
        // fill the line in when it lands.
        cx.spawn(async move |this, cx| {
            let version = cx
                .background_executor()
                .spawn(async { crate::luvus_update::installed_version().map(|v| v.to_string()) })
                .await;
            this.update(cx, |this, cx| {
                this.luvus_version = version;
                cx.notify();
            })
            .ok();
        })
        .detach();
        view
    }

    /// The window's type scale, resolved from the shared settings.
    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.settings)
    }
}

/// How an SVG from another device is read. usvg resolves an `<image>` href that
/// is a path by reading that file from this disk, so a logo could make the
/// window read `/dev/zero` until it runs out of memory, or draw a private file.
/// Only images embedded in the SVG itself (`data:` URLs) are taken.
fn untrusted_svg_options() -> resvg::usvg::Options<'static> {
    resvg::usvg::Options {
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: resvg::usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..resvg::usvg::Options::default()
    }
}

/// The in-app mark, rasterized once from the embedded SVG so its two-tone
/// face survives: GPUI's `svg()` element repaints as a single-color alpha
/// mask, which would flatten the mark into a silhouette. GPUI's image
/// buffers are BGRA, so the channels are swapped after rasterizing.
pub(super) fn app_icon() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    // Well above the largest paint size (64px About, 52px Home) so both
    // downscale from one crisp buffer.
    ICON.get_or_init(|| rasterize(include_bytes!("../../assets/pilus.svg"), 256))
        .clone()
}

/// `svg` drawn into a square image of `extent` pixels, or nothing when it is
/// not an SVG or draws nothing. A sipil's logo comes from the network, so it
/// is only ever parsed and drawn here, never fetched from or run.
pub(super) fn rasterize(svg: &[u8], extent: u32) -> Option<Arc<RenderImage>> {
    let tree = resvg::usvg::Tree::from_data(svg, &untrusted_svg_options()).ok()?;
    let size = tree.size();
    let scale = extent as f32 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(extent, extent)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // A fully transparent raster paints as an empty box, so there is none.
    if pixmap.pixels().iter().all(|pixel| pixel.alpha() == 0) {
        return None;
    }
    let mut buffer = image::ImageBuffer::from_raw(pixmap.width(), pixmap.height(), pixmap.take())?;
    // GPUI's image buffers are BGRA.
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let version = format!("version {}", self.version);
        // The daemon pilus attaches to; unknown until the background probe
        // lands, and absent when no `luvus` binary is on `PATH`.
        let luvus = self.luvus_version.as_ref().map_or_else(
            || "luvus not found".to_string(),
            |version| format!("luvus {version}"),
        );
        let mut credits = div().flex().flex_col().gap_3();
        for credit in CREDITS {
            credits = credits.child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(self.fonts().body))
                            .text_color(theme.fg)
                            .child(credit.name),
                    )
                    .child(
                        div()
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(credit.detail),
                    ),
            );
        }

        let root = div()
            .id("pilus-about")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .on_key_down(|event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .child(
                div()
                    .id("pilus-about-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .items_center()
                                    .px_6()
                                    .pt_8()
                                    .pb_6()
                                    .child(div().pb_2().child(app_icon().map_or_else(
                                        || {
                                            div()
                                                .text_size(px(style::scaled(&self.settings, 28.0)))
                                                .text_color(theme.accent)
                                                .child("pilus")
                                                .into_any_element()
                                        },
                                        |icon| img(icon).w(px(64.0)).h(px(64.0)).into_any_element(),
                                    )))
                                    .child(
                                        div()
                                            .text_size(px(self.fonts().secondary))
                                            .text_color(theme.muted)
                                            .child(version),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(self.fonts().label))
                                            .text_color(theme.muted)
                                            .child(luvus),
                                    ),
                            )
                            .child(div().px_6().pt_2().pb_6().child(credits)),
                    ),
            );
        super::apply_chrome_font(root, &self.settings)
    }
}

/// Open the About window.
pub fn open(theme: Theme, settings: Rc<RefCell<crate::settings::Settings>>, cx: &mut App) {
    let bounds = gpui::Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("About pilus".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(move |cx| AboutView::new(theme, settings, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("pilus: could not open about: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_decodes_to_a_square_image() {
        let icon = app_icon().expect("embedded icon decodes");
        let size = icon.size(0);
        assert!(size.width.0 > 0);
        assert_eq!(size.width.0, size.height.0);
    }

    #[test]
    fn a_logo_is_drawn_only_when_it_is_an_svg_that_draws_something() {
        let ok = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#f00"/></svg>"##;
        let drawn = rasterize(ok.as_bytes(), 32).expect("a drawn logo");
        assert_eq!(drawn.size(0).width.0, 32);
        assert!(rasterize(b"not an svg", 32).is_none());
        assert!(rasterize(b"\x89PNG", 32).is_none());
        let empty = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"/>"#;
        assert!(
            rasterize(empty.as_bytes(), 32).is_none(),
            "nothing drawn is no logo"
        );
    }

    #[test]
    fn a_logo_cannot_reach_a_file_on_this_disk() {
        let dir = std::env::temp_dir().join(format!("pilus-logo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let png = dir.join("private.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(&png)
            .expect("write png");
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 10 10"><image width="10" height="10" xlink:href="{}"/></svg>"#,
            png.display()
        );
        let drawn = rasterize(svg.as_bytes(), 16);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(drawn.is_none(), "the file was read and drawn");
    }
}
