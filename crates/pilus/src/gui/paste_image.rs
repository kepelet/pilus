//! Images pasted or dropped onto the terminal, handed to the pane as a path.
//!
//! The daemon's input is text, so a picture cannot go down the wire as one. It
//! is stored on the machine that owns the pane (`pilus_client::stash_image`:
//! straight to disk for the local machine, over the saved SSH profile for a
//! remote one) and the window pastes the path, which is what an agent or a
//! viewer in the pane reads.
//!
//! The clipboard and the drop both end in [`PilusView::paste_images`]. The
//! storage runs on a worker because a remote host is a network round trip, and
//! the path is pasted only if the window still shows the machine the image was
//! meant for, so a slow upload never types into a different host's shell.

use gpui::{ClipboardEntry, ClipboardItem, Context, ImageFormat};
use pilus_client::{PastedImage, SshTarget, is_image_file, shell_word, stash_image};
use std::path::PathBuf;

use super::toast::ToastKind;
use super::{EndpointId, PilusView};

/// The images a clipboard holds in place of text, or none when the paste should
/// stay the text paste it always was.
///
/// Finder lists a copied file by name as well as by path, so a file entry is
/// judged on its paths before text is considered. Anything else that carries
/// text is text: a page copied from a browser has an image, but the person
/// meant the words.
pub(super) fn clipboard_images(item: &ClipboardItem) -> Vec<PastedImage> {
    let files = item.entries.iter().find_map(|entry| match entry {
        ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
        _ => None,
    });
    if let Some(paths) = files {
        if !paths.is_empty() && paths.iter().all(|path| is_image_file(path)) {
            return paths.iter().cloned().map(PastedImage::File).collect();
        }
        return Vec::new();
    }
    if item
        .entries
        .iter()
        .any(|entry| matches!(entry, ClipboardEntry::String(_)))
    {
        return Vec::new();
    }
    item.entries
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(PastedImage::Pixels {
                bytes: image.bytes.clone(),
                extension: extension(image.format)?,
            }),
            _ => None,
        })
        .take(1)
        .collect()
}

/// The file extension a clipboard image is stored under. SVG is text a viewer
/// cannot show and icons and Netpbm are not worth a paste, so they are not
/// offered.
fn extension(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("png"),
        ImageFormat::Jpeg => Some("jpg"),
        ImageFormat::Webp => Some("webp"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::Bmp => Some("bmp"),
        ImageFormat::Tiff => Some("tiff"),
        ImageFormat::Svg | ImageFormat::Ico | ImageFormat::Pnm => None,
    }
}

/// What storing a batch of images came to: the paths that made it, in order,
/// and the reasons the others did not.
struct Stored {
    paths: Vec<String>,
    failed: Vec<String>,
}

/// Store each image, collecting what went through and what did not rather than
/// stopping at the first failure.
fn stash_all(target: Option<&SshTarget>, images: &[PastedImage]) -> Stored {
    let mut stored = Stored {
        paths: Vec::new(),
        failed: Vec::new(),
    };
    for image in images {
        match stash_image(target, image) {
            Ok(path) => stored.paths.push(path),
            Err(error) => stored.failed.push(error),
        }
    }
    stored
}

/// The text a pane is pasted: each path as one shell word, and a trailing space
/// so whatever is typed next is not glued to the last path.
fn pasted_text(paths: &[String]) -> String {
    let mut text = paths
        .iter()
        .map(|path| shell_word(path))
        .collect::<Vec<_>>()
        .join(" ");
    text.push(' ');
    text
}

impl PilusView {
    /// Files dropped on the terminal: the images among them go to the pane.
    pub(super) fn drop_on_terminal(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let images: Vec<PastedImage> = paths
            .iter()
            .filter(|path| is_image_file(path))
            .cloned()
            .map(PastedImage::File)
            .collect();
        let skipped = paths.len() - images.len();
        if images.is_empty() {
            self.toast(
                ToastKind::Info,
                "Only images can be dropped on the terminal",
            );
            cx.notify();
            return;
        }
        if skipped > 0 {
            self.toast(
                ToastKind::Info,
                format!("Pasting the images only: {skipped} other file(s) skipped"),
            );
        }
        self.paste_images(images, cx);
    }

    /// Store `images` on the active machine and paste their paths into the
    /// focused pane.
    pub(super) fn paste_images(&mut self, images: Vec<PastedImage>, cx: &mut Context<Self>) {
        if images.is_empty() || self.connection.is_none() {
            return;
        }
        let endpoint = self.active.clone();
        let target = match endpoint.machine_id() {
            None => None,
            Some(id) => match self.machine_target_by_key(id) {
                Some(target) => Some(target),
                None => {
                    self.toast(
                        ToastKind::Error,
                        "This machine has no SSH profile to send images over",
                    );
                    cx.notify();
                    return;
                }
            },
        };
        // A remote host takes a moment; say the paste was heard.
        if target.is_some() {
            self.toast(
                ToastKind::Info,
                match images.len() {
                    1 => "Sending the image to the host…".to_string(),
                    many => format!("Sending {many} images to the host…"),
                },
            );
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_executor()
                .spawn(async move { stash_all(target.as_ref(), &images) })
                .await;
            this.update(cx, |this, cx| this.images_stored(&endpoint, stored, cx))
                .ok();
        })
        .detach();
    }

    /// Paste the stored images' paths into the pane, if the window is still on
    /// the machine they were stored for, and say what did not make it.
    fn images_stored(&mut self, endpoint: &EndpointId, stored: Stored, cx: &mut Context<Self>) {
        for error in &stored.failed {
            self.toast(ToastKind::Error, error.clone());
        }
        if !stored.paths.is_empty() {
            if self.active == *endpoint {
                self.send_active(pilus_protocol::ClientMessage::Paste(pasted_text(
                    &stored.paths,
                )));
            } else {
                self.toast(
                    ToastKind::Error,
                    "The window switched machines before the image arrived. Paste it again.",
                );
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ClipboardString, ExternalPaths, Image};

    fn png() -> Image {
        Image {
            format: ImageFormat::Png,
            bytes: vec![1, 2, 3],
            id: 1,
        }
    }

    fn files(names: &[&str]) -> ClipboardEntry {
        ClipboardEntry::ExternalPaths(ExternalPaths(names.iter().map(PathBuf::from).collect()))
    }

    fn text(text: &str) -> ClipboardEntry {
        ClipboardEntry::String(ClipboardString::new(text.to_string()))
    }

    #[test]
    fn a_screenshot_on_the_clipboard_is_an_image() {
        let item = ClipboardItem::new_image(&png());
        assert_eq!(
            clipboard_images(&item),
            vec![PastedImage::Pixels {
                bytes: vec![1, 2, 3],
                extension: "png"
            }]
        );
    }

    #[test]
    fn copied_text_stays_text() {
        let item = ClipboardItem::new_string("hello".to_string());
        assert!(clipboard_images(&item).is_empty());
    }

    #[test]
    fn an_image_that_comes_with_words_is_the_words() {
        let item = ClipboardItem {
            entries: vec![text("a caption"), ClipboardEntry::Image(png())],
        };
        assert!(clipboard_images(&item).is_empty());
    }

    #[test]
    fn copied_image_files_are_pasted_even_though_finder_adds_their_names() {
        let item = ClipboardItem {
            entries: vec![
                files(&["/a/one.png", "/a/two.JPG"]),
                text("one.png\ntwo.JPG"),
            ],
        };
        assert_eq!(
            clipboard_images(&item),
            vec![
                PastedImage::File("/a/one.png".into()),
                PastedImage::File("/a/two.JPG".into())
            ]
        );
    }

    #[test]
    fn copied_files_that_are_not_all_images_stay_text() {
        let item = ClipboardItem {
            entries: vec![files(&["/a/one.png", "/a/notes.txt"]), text("names")],
        };
        assert!(clipboard_images(&item).is_empty());
    }

    #[test]
    fn a_vector_image_is_not_offered() {
        let svg = Image {
            format: ImageFormat::Svg,
            ..png()
        };
        assert!(clipboard_images(&ClipboardItem::new_image(&svg)).is_empty());
    }

    #[test]
    fn paths_are_pasted_as_shell_words_with_a_trailing_space() {
        let paths = vec!["/a b/one.png".to_string(), "/c/two.png".to_string()];
        assert_eq!(pasted_text(&paths), "/a\\ b/one.png /c/two.png ");
    }
}
