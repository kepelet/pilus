//! The Dock's cue for a daemon push that lands while pilus is in the
//! background.
//!
//! A toast is only readable inside the window, so a push that arrives behind
//! another app would otherwise go unseen until the user happens to switch
//! back. [`Dock::alert`] bounces the icon once and counts the push in the
//! badge; [`Dock::sync`] clears both the moment the app is frontmost again.
//! Activity comes from `NSApplication` itself rather than the window's
//! render-time flag, because an occluded window may not render and that flag
//! would go stale. Other platforms have no equivalent, so they do nothing.

#[cfg(target_os = "macos")]
mod imp {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::NSString;

    /// Pushes counted since the app was last in front.
    pub struct Dock {
        unread: u32,
    }

    impl Dock {
        pub fn new() -> Self {
            Self { unread: 0 }
        }

        /// Count one push and bounce the icon, unless the app is already in
        /// front, where the toast is the cue.
        pub fn alert(&mut self) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            if app.isActive() {
                return;
            }
            self.unread = self.unread.saturating_add(1);
            app.dockTile()
                .setBadgeLabel(Some(&NSString::from_str(&self.unread.to_string())));
            // Informational bounces once; the critical kind would keep
            // bouncing until the user came back.
            app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }

        /// Drop the badge once the app is in front again. Cheap enough for
        /// every frame: it does nothing while nothing is unread.
        pub fn sync(&mut self) {
            if self.unread == 0 {
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            if app.isActive() {
                self.unread = 0;
                app.dockTile().setBadgeLabel(None);
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub struct Dock;

    impl Dock {
        pub fn new() -> Self {
            Self
        }

        pub fn alert(&mut self) {}

        pub fn sync(&mut self) {}
    }
}

pub use imp::Dock;
