//! The gate every Browser tab download passes.
//!
//! Downloads are decided on the main thread, inside WebKit's delegate, which
//! wants an answer before it returns. [`confirm_download`] therefore asks
//! with a modal alert, and it also repairs the destination: when a response
//! carries no filename, `wry` builds `~/Downloads/` and renames it to
//! `~/ (1)`, a nameless file outside Downloads.

use std::path::{Path, PathBuf};

/// Where a download will be written, and whether the site named it.
#[derive(Debug, PartialEq, Eq)]
struct Destination {
    path: PathBuf,
    site_named: bool,
}

fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Downloads")
}

/// A name `wry` made out of nothing: blank, or only the `(n)` suffix it adds
/// to avoid overwriting a file.
fn nameless(name: &str) -> bool {
    let name = name.trim();
    name.is_empty()
        || name
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The last path segment of `url`, which is what a browser names an
/// unnamed download after; `download` when there is none.
fn name_from_url(url: &str) -> String {
    // A `data:` URL's "path" is the payload, not a name.
    if url.starts_with("data:") {
        return "download".to_string();
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let name: String = path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':'))
        .collect();
    if nameless(&name) || name.bytes().all(|b| b == b'.') {
        "download".to_string()
    } else {
        name
    }
}

/// `path`, or the first `name (n).ext` beside it that is free.
fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(path)
}

/// Settle the file a download is written to. `proposed` is what `wry` came up
/// with; it is trusted unless its name is blank, and the file always lands in
/// `downloads`.
fn plan_download(url: &str, proposed: &Path, downloads: &Path) -> Destination {
    let proposed_name = proposed
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !nameless(name));
    let site_named = proposed_name.is_some();
    let name = proposed_name.unwrap_or_else(|| name_from_url(url));
    Destination {
        path: unique(downloads.join(name)),
        site_named,
    }
}

/// A URL short enough for an alert; `data:` and `blob:` ones can run to
/// megabytes.
fn shorten(url: &str) -> String {
    const LIMIT: usize = 160;
    if url.chars().count() <= LIMIT {
        return url.to_string();
    }
    let head: String = url.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// Whether the page may save `url` to `path`. Every download asks first; an
/// accepted one has `path` replaced with the repaired destination.
pub(super) fn confirm_download(url: &str, path: &mut PathBuf) -> bool {
    let destination = plan_download(url, path, &downloads_dir());
    let name = destination
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut detail = format!(
        "From {}\nSaves to {}",
        shorten(url),
        destination
            .path
            .parent()
            .unwrap_or(Path::new(""))
            .display()
    );
    if !destination.site_named {
        detail.push_str("\nThe site did not name this file.");
    }
    if !ask(&format!("Download “{name}”?"), &detail) {
        tracing::info!(%name, "browser: download declined");
        return false;
    }
    if let Some(dir) = destination.path.parent()
        && let Err(error) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(%error, "browser: could not create the downloads directory");
        return false;
    }
    *path = destination.path;
    true
}

/// A modal alert with Download and Cancel; true for Download.
#[cfg(target_os = "macos")]
fn ask(title: &str, detail: &str) -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn};
    use objc2_foundation::NSString;

    // WebKit calls its delegates on the main thread; anywhere else there is
    // no way to show an alert, and not asking means not downloading.
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str("Download"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

/// Without a native alert there is nothing to ask with, so nothing downloads.
#[cfg(not(target_os = "macos"))]
fn ask(_: &str, _: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn blank_and_suffix_only_names_are_nameless() {
        assert!(nameless(""));
        assert!(nameless(" (1)"));
        assert!(nameless("(12)"));
        assert!(!nameless("report (1).pdf"));
        assert!(!nameless("(draft)"));
    }

    #[test]
    fn a_named_download_keeps_its_name_and_moves_into_downloads() {
        let downloads = scratch("named");
        let plan = plan_download(
            "https://example.com/get?id=1",
            Path::new("/elsewhere/report.pdf"),
            &downloads,
        );
        assert_eq!(plan.path, downloads.join("report.pdf"));
        assert!(plan.site_named);
    }

    #[test]
    fn wrys_nameless_destination_is_replaced_by_a_name_from_the_url() {
        let downloads = scratch("nameless");
        let plan = plan_download(
            "https://example.com/files/archive.zip?token=abc#top",
            Path::new("/Users/someone/ (1)"),
            &downloads,
        );
        assert_eq!(plan.path, downloads.join("archive.zip"));
        assert!(!plan.site_named);
    }

    #[test]
    fn a_url_with_no_usable_segment_downloads_as_download() {
        assert_eq!(name_from_url("https://example.com/"), "download");
        assert_eq!(name_from_url("blob:https://example.com/1f2e"), "1f2e");
        assert_eq!(name_from_url("data:text/plain;base64,AAAA"), "download");
        assert_eq!(name_from_url("https://example.com/.."), "download");
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let downloads = scratch("unique");
        std::fs::write(downloads.join("a.txt"), b"x").expect("write");
        std::fs::write(downloads.join("a (1).txt"), b"x").expect("write");
        assert_eq!(
            unique(downloads.join("a.txt")),
            downloads.join("a (2).txt")
        );
    }
}
