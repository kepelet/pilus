//! The macOS Dock and process identity for `l3s`.
//!
//! A bare `cargo run` binary has no bundle, so AppKit shows a generic
//! executable icon and the process name "l3s". This installs the embedded
//! multi-resolution `.icns` on the shared application. When `l3s` runs from a
//! real `.app` bundle, the bundle's `Info.plist` already names its icon and
//! this step defers to it.

/// A multi-resolution icon so the Dock picks a native representation instead
/// of downsampling one large bitmap.
#[cfg(target_os = "macos")]
const ICNS: &[u8] = include_bytes!("../../assets/l3s.icns");

/// Install the Dock icon and process name on macOS; a no-op elsewhere.
#[cfg(target_os = "macos")]
pub fn install() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSBundle, NSData, NSProcessInfo, NSString};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    NSProcessInfo::processInfo().setProcessName(&NSString::from_str("l3s"));
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
