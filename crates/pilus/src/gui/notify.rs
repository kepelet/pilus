//! Native notifications that carry a sound and an app badge.
//!
//! gpui's `show_system_notification` posts a banner only: it sets neither a
//! sound nor a badge, and it asks the user for alert and sound permission
//! alone. The menu bar helper has no window, so the daemon's own cue never
//! reaches it and the banner is all the user gets; this posts through
//! `UNUserNotificationCenter` directly so the banner chimes and the app icon
//! counts what is waiting. The badge is the notification center's own, so it
//! shows on the Dock icon even though the helper runs without one. Withdrawing
//! a banner still goes through gpui, which keys on the same tag. Other
//! platforms fall back to gpui's plain notification.

use gpui::App;

/// One notification to post.
pub struct Alert {
    /// Stable per agent pane, so a repeat replaces the earlier banner.
    pub tag: String,
    pub title: String,
    pub body: String,
    /// The app icon's badge once this is delivered.
    pub badge: u32,
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;

    use block2::RcBlock;
    use gpui::App;
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError, NSNumber, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };

    use super::Alert;

    thread_local! {
        static ASKED: Cell<bool> = const { Cell::new(false) };
    }

    /// The notification center, or `None` outside an app bundle, where the
    /// framework aborts the process instead of failing.
    fn center() -> Option<Retained<UNUserNotificationCenter>> {
        NSBundle::mainBundle().bundleIdentifier()?;
        Some(UNUserNotificationCenter::currentNotificationCenter())
    }

    /// Ask for alert, sound, and badge together. macOS prompts once, with the
    /// options of the first request, so asking for all three up front is what
    /// makes the sound and badge toggles available.
    fn authorize(center: &UNUserNotificationCenter) {
        if ASKED.replace(true) {
            return;
        }
        // A denial is the user's choice and there is nowhere to report one,
        // so the result is ignored.
        let done = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert
                | UNAuthorizationOptions::Sound
                | UNAuthorizationOptions::Badge,
            &done,
        );
    }

    pub fn post(_cx: &mut App, alert: Alert) {
        let Some(center) = center() else {
            return;
        };
        authorize(&center);
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&alert.title));
        content.setBody(&NSString::from_str(&alert.body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        content.setBadge(Some(&NSNumber::new_u32(alert.badge)));
        // A nil trigger delivers now, and the tag as identifier lets a newer
        // notification replace the older one.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&alert.tag),
            &content,
            None,
        );
        let done = RcBlock::new(|_error: *mut NSError| {});
        center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }

    pub fn set_badge(count: u32) {
        let Some(center) = center() else {
            return;
        };
        center.setBadgeCount_withCompletionHandler(count as isize, None);
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use gpui::{App, SharedString, SystemNotification};

    use super::Alert;

    pub fn post(cx: &mut App, alert: Alert) {
        cx.show_system_notification(SystemNotification {
            tag: SharedString::from(alert.tag),
            title: SharedString::from(alert.title),
            body: SharedString::from(alert.body),
            actions: Vec::new(),
        });
    }

    pub fn set_badge(_count: u32) {}
}

/// Post `alert` with the system sound, and badge the app icon.
pub fn post(cx: &mut App, alert: Alert) {
    imp::post(cx, alert);
}

/// Set the app icon's badge; zero clears it.
pub fn set_badge(count: u32) {
    imp::set_badge(count);
}
