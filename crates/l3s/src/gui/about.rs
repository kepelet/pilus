//! The native About window: the app name, version, and credits.

use std::sync::{Arc, OnceLock};

use gpui::{
    App, Context, FocusHandle, KeyDownEvent, Render, RenderImage, Window, WindowBounds,
    WindowOptions, div, img, prelude::*, px, size,
};

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
    focus_handle: FocusHandle,
}

impl AboutView {
    fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            theme,
            focus_handle,
        }
    }
}

/// The app's own icon, decoded once from the embedded PNG. GPUI's image
/// buffers are BGRA, so the channels are swapped after decoding.
fn app_icon() -> Option<Arc<RenderImage>> {
    static ICON: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    ICON.get_or_init(|| {
        let decoded =
            image::load_from_memory(include_bytes!("../../assets/l3s-icon-1024.png")).ok()?;
        let mut buffer = decoded.to_rgba8();
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
        let mut credits = div().flex().flex_col().gap_3();
        for credit in CREDITS {
            credits = credits.child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.fg)
                            .child(credit.name),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.muted)
                            .child(credit.detail),
                    ),
            );
        }

        div()
            .id("l3s-about")
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
                    .id("l3s-about-scroll")
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
                                    .child(app_icon().map_or_else(
                                        || {
                                            div()
                                                .text_size(px(34.0))
                                                .text_color(theme.accent)
                                                .child("l3s")
                                                .into_any_element()
                                        },
                                        |icon| img(icon).w(px(96.0)).h(px(96.0)).into_any_element(),
                                    ))
                                    .child(
                                        div().text_size(px(12.0)).text_color(theme.muted).child(
                                            format!("version {}", env!("CARGO_PKG_VERSION")),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme.muted)
                                            .child("Native desktop client for Luvus."),
                                    ),
                            )
                            .child(div().px_6().pt_2().pb_6().child(credits)),
                    ),
            )
    }
}

/// Open the About window.
pub fn open(theme: Theme, cx: &mut App) {
    let bounds = gpui::Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("About l3s".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(move |cx| AboutView::new(theme, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("l3s: could not open about: {error}");
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
