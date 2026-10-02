//! The macOS Dock and process identity for `pilus`.
//!
//! A bare `cargo run` binary has no bundle, so AppKit shows a generic
//! executable icon and the process name "pilus". This installs the embedded
//! multi-resolution `.icns` on the shared application. When `pilus` runs from a
//! real `.app` bundle, the bundle's `Info.plist` already names its icon and
//! this step defers to it.

/// A multi-resolution icon so the Dock picks a native representation instead
/// of downsampling one large bitmap.
#[cfg(target_os = "macos")]
const ICNS: &[u8] = include_bytes!("../../assets/pilus.icns");

/// Bundle identifier for unbundled builds. A packaged `.app` carries the real
/// identifier in its `Info.plist`; `scripts/bundle-macos.sh` sets it from
/// `PILUS_BUNDLE_ID`, and this is the fallback it defaults to.
pub const DEV_BUNDLE_ID: &str = "dev.pilus.app";

/// Install the Dock icon and process name on macOS; a no-op elsewhere.
#[cfg(target_os = "macos")]
pub fn install() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSBundle, NSData, NSProcessInfo, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    NSProcessInfo::processInfo().setProcessName(&NSString::from_str("pilus"));
    if NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str("CFBundleIconFile"))
        .is_some()
    {
        return;
    }
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICNS)) else {
        return;
    };
    // SAFETY: AppKit is initialized by the time the window is opened, the
    // main-thread marker gates access, and `image` is a live, non-null NSImage.
    #[allow(unsafe_code)]
    unsafe {
        NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image));
    }
}

/// Install the Dock icon on macOS; a no-op elsewhere.
#[cfg(not(target_os = "macos"))]
pub fn install() {}

/// The running app's bundle identifier, or [`DEV_BUNDLE_ID`] outside a `.app`.
///
/// `cargo run` has no bundle, so the development identifier stands in; a
/// packaged app reports whatever `Info.plist` names.
#[cfg(target_os = "macos")]
pub fn bundle_identifier() -> String {
    use objc2_foundation::NSBundle;
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|identifier| identifier.to_string())
        .unwrap_or_else(|| DEV_BUNDLE_ID.to_string())
}

/// The development bundle identifier; only macOS bundles carry a real one.
#[cfg(not(target_os = "macos"))]
pub fn bundle_identifier() -> String {
    DEV_BUNDLE_ID.to_string()
}
