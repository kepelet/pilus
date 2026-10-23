//! Pictures in the Files tab.
//!
//! A click on an image or an SVG used to hand the file to the daemon, or, for an
//! SVG, open its XML in the editor. Here the tab reads the bytes itself, as it
//! reads a document (`pilus_client::read_bytes`, locally or over SSH), and
//! [`load`] turns them into a [`Picture`] on that worker thread. A raster image
//! stays encoded for GPUI to decode when it is first painted; only its header is
//! read here, for the size. An SVG is drawn with resvg to a bitmap, since GPUI's
//! `svg()` repaints as a single-color mask. `FilesMsg::Picture` carries the
//! result back and [`view`] paints it, so the GUI thread decodes and parses
//! nothing.
//!
//! The bytes come from a file on whatever machine the tree belongs to, so an
//! SVG is read with `about::untrusted_svg_options` like any SVG from another
//! device, and both kinds are bounded before they are drawn.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use gpui::{
    AnyElement, Image, ImageFormat, IntoElement, ObjectFit, ParentElement, RenderImage, Styled,
    StyledImage, div, img, px,
};
use pilus_client::{MAX_IMAGE_BYTES, MAX_TEXT_BYTES, SshTarget, read_bytes};
use resvg::usvg;

use super::style::{self, RadiusExt as _};
use super::theme::Theme;
use super::{about, mermaid};

/// The longest edge an SVG is drawn to, in pixels. The picture is fitted to the
/// view, so a small icon is drawn large rather than blurred up from its own
/// size, and a huge canvas does not allocate a bitmap the size of a poster.
const SVG_EDGE: f32 = 2048.0;

/// What a file's name says it holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Kind {
    /// An encoded image GPUI can decode.
    Raster(ImageFormat),
    Svg,
}

/// The picture `path` names, by extension.
pub(super) fn kind(path: &str) -> Option<Kind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(Kind::Raster(
        match extension.to_ascii_lowercase().as_str() {
            "png" => ImageFormat::Png,
            "jpg" | "jpeg" => ImageFormat::Jpeg,
            "gif" => ImageFormat::Gif,
            "webp" => ImageFormat::Webp,
            "bmp" => ImageFormat::Bmp,
            "ico" => ImageFormat::Ico,
            "tif" | "tiff" => ImageFormat::Tiff,
            "svg" => return Some(Kind::Svg),
            _ => return None,
        },
    ))
}

/// What a picture paints from. Not `ImageSource`, which can hold a closure and
/// so cannot cross from the worker thread.
enum Pixels {
    /// Still encoded: GPUI decodes it when it is first painted.
    Encoded(Arc<Image>),
    /// Already a bitmap.
    Drawn(Arc<RenderImage>),
}

/// A picture read off a machine, ready to paint.
pub(super) struct Picture {
    pixels: Pixels,
    /// The picture's own size, in pixels (an SVG's, in its user units).
    width: u32,
    height: u32,
    /// The file's length, for the caption.
    bytes: usize,
    /// A vector picture is fitted to the view; a raster one is not blown up
    /// past its own size.
    vector: bool,
}

/// Read the picture at `path` on this machine, or on `target`'s host.
pub(super) fn load(target: Option<&SshTarget>, path: &str, kind: Kind) -> Result<Picture, String> {
    let limit = match kind {
        Kind::Svg => MAX_TEXT_BYTES,
        Kind::Raster(_) => MAX_IMAGE_BYTES,
    };
    decode(kind, read_bytes(target, path, limit)?)
}

/// The picture `bytes` hold, as a file of `kind`.
pub(super) fn decode(kind: Kind, bytes: Vec<u8>) -> Result<Picture, String> {
    match kind {
        // resvg is parsing text from another machine; a panic in it must end as
        // an error, not as a tab stuck on "Loading".
        Kind::Svg => catch_unwind(AssertUnwindSafe(|| draw_svg(&bytes)))
            .unwrap_or(None)
            .ok_or_else(|| "Could not draw this SVG".to_string()),
        Kind::Raster(format) => {
            let (width, height) = dimensions(format, &bytes)
                .ok_or_else(|| "Could not read this image".to_string())?;
            let len = bytes.len();
            Ok(Picture {
                pixels: Pixels::Encoded(Arc::new(Image::from_bytes(format, bytes))),
                width,
                height,
                bytes: len,
                vector: false,
            })
        }
    }
}

/// The size in the image's header, without decoding its pixels.
fn dimensions(format: ImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    let format = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Ico => image::ImageFormat::Ico,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        _ => return None,
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// `svg` drawn to a bitmap, or nothing when it does not parse or draws nothing.
fn draw_svg(svg: &[u8]) -> Option<Picture> {
    let mut options = about::untrusted_svg_options();
    options.fontdb = mermaid::fonts();
    let tree = usvg::Tree::from_data(svg, &options).ok()?;
    let (width, height) = (tree.size().width(), tree.size().height());
    if !(width >= 1.0 && height >= 1.0) {
        return None;
    }
    let scale = SVG_EDGE / width.max(height);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(
        (width * scale).ceil().max(1.0) as u32,
        (height * scale).ceil().max(1.0) as u32,
    )?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(Picture {
        pixels: Pixels::Drawn(about::image_from_pixmap(pixmap)?),
        width: width.round() as u32,
        height: height.round() as u32,
        bytes: svg.len(),
        vector: true,
    })
}

/// "340 KB" or "1.2 MB".
fn size_label(bytes: usize) -> String {
    const KB: f32 = 1024.0;
    let bytes = bytes as f32;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < KB * KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / (KB * KB))
    }
}

/// The picture on the panel color, fitted to the view, over a caption with its
/// size.
pub(super) fn view(picture: &Picture, theme: &Theme, fonts: &style::Fonts) -> AnyElement {
    let muted = theme.muted;
    let source = match &picture.pixels {
        Pixels::Encoded(image) => img(image.clone()),
        Pixels::Drawn(bitmap) => img(bitmap.clone()),
    };
    let mut shown = source
        .w_full()
        .h_full()
        .object_fit(ObjectFit::Contain)
        .with_fallback(move || {
            div()
                .text_color(muted)
                .child("Could not decode this image")
                .into_any_element()
        });
    if !picture.vector {
        shown = shown
            .max_w(px(picture.width as f32))
            .max_h(px(picture.height as f32));
    }
    div()
        .flex()
        .flex_col()
        .size_full()
        .min_h(px(0.0))
        .child(
            div().flex_1().min_h(px(0.0)).p_3().child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_full()
                    .radius_md()
                    .bg(theme.panel_dim)
                    .overflow_hidden()
                    .child(shown),
            ),
        )
        .child(
            div()
                .flex_shrink_0()
                .px_3()
                .pb_2()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(format!(
                    "{} × {}  ·  {}",
                    picture.width,
                    picture.height,
                    size_label(picture.bytes)
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_told_by_extension() {
        assert_eq!(kind("a/b.PNG"), Some(Kind::Raster(ImageFormat::Png)));
        assert_eq!(kind("a/b.jpeg"), Some(Kind::Raster(ImageFormat::Jpeg)));
        assert_eq!(kind("logo.svg"), Some(Kind::Svg));
        assert_eq!(kind("src/main.rs"), None);
        assert_eq!(kind(".png"), None);
        assert_eq!(kind("png"), None);
    }

    #[test]
    fn an_svg_draws_with_its_own_size() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#c33"/></svg>"##;
        let picture = draw_svg(svg).expect("draws");
        assert_eq!((picture.width, picture.height), (40, 20));
        assert!(picture.vector);
    }

    #[test]
    fn an_svg_that_draws_nothing_or_is_not_one_fails() {
        assert!(draw_svg(b"not xml").is_none());
        let empty = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#;
        assert!(draw_svg(empty).is_none());
    }

    #[test]
    fn an_svg_cannot_read_a_file_off_this_disk() {
        // The href is a path; only `data:` images are resolved.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><image xlink:href="/etc/hosts" width="10" height="10"/></svg>"#;
        assert!(draw_svg(svg).is_none());
    }

    #[test]
    fn a_png_header_gives_its_size() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(3, 2)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encodes");
        assert_eq!(dimensions(ImageFormat::Png, &png), Some((3, 2)));
        assert_eq!(dimensions(ImageFormat::Png, b"nope"), None);
    }

    #[test]
    fn sizes_read_in_kb_and_mb() {
        assert_eq!(size_label(10), "10 B");
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(size_label(3 * 1024 * 1024), "3.0 MB");
    }
}
