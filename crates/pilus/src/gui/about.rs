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
        Self {
            theme,
            settings,
            version,
            focus_handle,
        }
    }

    /// The window's type scale, resolved from the shared settings.
    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.settings)
    }
}

/// The in-app mark, rasterized once from the embedded SVG so its two-tone
/// face survives: GPUI's `svg()` element repaints as a single-color alpha
/// mask, which would flatten the mark into a silhouette. GPUI's image
/// buffers are BGRA, so the channels are swapped after rasterizing.
pub(super) fn app_icon() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    ICON.get_or_init(|| {
        let tree = resvg::usvg::Tree::from_data(
            include_bytes!("../../assets/pilus.svg"),
            &resvg::usvg::Options::default(),
        )
        .ok()?;
        let extent = tree.size();
        // Well above the largest paint size (64px About, 52px Home) so both
        // downscale from one crisp buffer.
        let scale = 256.0 / extent.width().max(extent.height());
        let mut pixmap = resvg::tiny_skia::Pixmap::new(256, 256)?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        // A fully transparent raster paints as an empty box, so fall back
        // to the wordmark instead.
        if pixmap.pixels().iter().all(|pixel| pixel.alpha() == 0) {
            return None;
        }
        let mut buffer =
            image::ImageBuffer::from_raw(pixmap.width(), pixmap.height(), pixmap.take())?;
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
    })
    .clone()
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let version = format!("version {}", self.version);
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
}
