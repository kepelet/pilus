//! The lock over a Browser tab an agent is working in.
//!
//! A page is a native `WKWebView` that the OS draws above everything GPUI
//! paints, so nothing GPUI draws can cover it, and a click on it never reaches
//! GPUI. The cover therefore has to be a native view of its own: a sibling
//! placed just above the page, with the page's frame, that swallows the mouse,
//! the wheel and the keyboard, so the person cannot interrupt an agent half
//! way through a form. It draws nothing over the page, so the person watches
//! the agent work. It spans the whole page, edge to edge, and paints a faint
//! accent wash with an accent frame so the person can see at a glance that the
//! page is held; its one control is a Take over button in a corner. It is
//! drawn by pilus, not injected into the page, so a page can neither remove
//! nor imitate it, and it works on pages that run none of our script.
//!
//! `browser_tab::browser_sync` calls [`Shield::sync`] on every frame with
//! whether the page is on screen and held. A click on the button sets a flag
//! that `browser_tick` reads with the other page events, so no callback
//! reaches into the view. Where there is no AppKit the shield does nothing and
//! the older rule still holds: a real click or keypress in the page, which the
//! page's own script can tell from the agent's, takes the tab back.

/// The cover of one page. Empty until the page is held and shown.
#[derive(Default)]
pub(super) struct Shield(imp::Shield);

impl Shield {
    /// Put the cover over `webview` when `up`, keep it on the page's frame,
    /// and take it away when not. Cheap when nothing changed.
    pub(super) fn sync(&mut self, webview: &wry::WebView, up: bool) {
        self.0.sync(webview, up);
    }

    /// Whether the person pressed Take over since the last call.
    pub(super) fn taken(&self) -> bool {
        self.0.taken()
    }

    /// Take the cover away.
    pub(super) fn remove(&mut self) {
        self.0.remove();
    }
}

#[cfg(target_os = "macos")]
// SAFETY (module-wide): every call here runs on the main thread, gated by a
// `MainThreadMarker`, against AppKit views this module creates and retains or
// borrows from the live webview it is handed.
#[allow(unsafe_code)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSButton, NSColor, NSCompositingOperation, NSEvent, NSRectFillUsingOperation, NSView,
        NSWindowOrderingMode,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use wry::WebViewExtMacOS;

    /// Space between the button and the page's corner.
    const CORNER_GAP: f64 = 8.0;
    /// How strong the wash over the page is: enough to read as "held", not
    /// enough to hide the page or the agent's cursor in it.
    const WASH: f64 = 0.06;
    /// Thickness and strength of the frame around the page's edge.
    const FRAME: f64 = 3.0;
    const FRAME_ALPHA: f64 = 0.7;

    struct Ivars {
        /// Set by the button, read by [`Shield::taken`].
        taken: Arc<AtomicBool>,
    }

    // Clear, so the page shows through. Each input method is overridden to do
    // nothing: without that, AppKit passes an event this view does not handle
    // on to its superview, which is the window's own content view.
    define_class!(
        #[unsafe(super(NSView))]
        #[thread_kind = MainThreadOnly]
        #[name = "PilusAgentShield"]
        #[ivars = Ivars]
        struct Cover;

        impl Cover {
            // The wash and the frame. The view is not opaque, so the page
            // shows through whatever is not painted.
            #[unsafe(method(drawRect:))]
            fn draw_rect(&self, _dirty: NSRect) {
                let bounds = self.bounds();
                let accent = NSColor::controlAccentColor();
                accent.colorWithAlphaComponent(WASH).setFill();
                NSRectFillUsingOperation(bounds, NSCompositingOperation::SourceOver);
                accent.colorWithAlphaComponent(FRAME_ALPHA).setFill();
                let (w, h) = (bounds.size.width, bounds.size.height);
                let t = FRAME.min(w / 2.0).min(h / 2.0);
                for edge in [
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, t)),
                    NSRect::new(NSPoint::new(0.0, h - t), NSSize::new(w, t)),
                    NSRect::new(NSPoint::new(0.0, t), NSSize::new(t, h - 2.0 * t)),
                    NSRect::new(NSPoint::new(w - t, t), NSSize::new(t, h - 2.0 * t)),
                ] {
                    NSRectFillUsingOperation(edge, NSCompositingOperation::SourceOver);
                }
            }

            #[unsafe(method(takeOver:))]
            fn take_over(&self, _sender: Option<&AnyObject>) {
                self.ivars().taken.store(true, Ordering::Relaxed);
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, _event: &NSEvent) {}
            #[unsafe(method(mouseUp:))]
            fn mouse_up(&self, _event: &NSEvent) {}
            #[unsafe(method(mouseDragged:))]
            fn mouse_dragged(&self, _event: &NSEvent) {}
            #[unsafe(method(rightMouseDown:))]
            fn right_mouse_down(&self, _event: &NSEvent) {}
            #[unsafe(method(rightMouseUp:))]
            fn right_mouse_up(&self, _event: &NSEvent) {}
            #[unsafe(method(rightMouseDragged:))]
            fn right_mouse_dragged(&self, _event: &NSEvent) {}
            #[unsafe(method(otherMouseDown:))]
            fn other_mouse_down(&self, _event: &NSEvent) {}
            #[unsafe(method(otherMouseUp:))]
            fn other_mouse_up(&self, _event: &NSEvent) {}
            #[unsafe(method(otherMouseDragged:))]
            fn other_mouse_dragged(&self, _event: &NSEvent) {}
            #[unsafe(method(scrollWheel:))]
            fn scroll_wheel(&self, _event: &NSEvent) {}
            #[unsafe(method(magnifyWithEvent:))]
            fn magnify(&self, _event: &NSEvent) {}
            #[unsafe(method(swipeWithEvent:))]
            fn swipe(&self, _event: &NSEvent) {}
            #[unsafe(method(keyDown:))]
            fn key_down(&self, _event: &NSEvent) {}
            #[unsafe(method(keyUp:))]
            fn key_up(&self, _event: &NSEvent) {}
        }

        unsafe impl NSObjectProtocol for Cover {}
    );

    impl Cover {
        fn new(mtm: MainThreadMarker, taken: Arc<AtomicBool>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { taken });
            // SAFETY: `init` is NSView's designated initialiser through its
            // superclass chain; the ivars were set first.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// The cover and the button in it, once built.
    struct Built {
        cover: Retained<Cover>,
        button: Retained<NSButton>,
    }

    #[derive(Default)]
    pub struct Shield {
        built: Option<Built>,
        taken: Arc<AtomicBool>,
    }

    impl Shield {
        pub fn sync(&mut self, webview: &wry::WebView, up: bool) {
            if !up {
                self.remove();
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let page = webview.webview();
            // SAFETY: `superview` is a plain getter on a live view.
            let Some(parent) = (unsafe { page.superview() }) else {
                return;
            };
            if self.built.is_none() {
                let cover = Cover::new(mtm, self.taken.clone());
                let title = NSString::from_str("Agent is working \u{b7} Take over");
                let target: &AnyObject = &cover;
                // SAFETY: the target is the cover, which answers `takeOver:`
                // with the one sender argument a button sends.
                let button = unsafe {
                    NSButton::buttonWithTitle_target_action(
                        &title,
                        Some(target),
                        Some(sel!(takeOver:)),
                        mtm,
                    )
                };
                button.sizeToFit();
                cover.addSubview(&button);
                // Directly above the page, so anything stacked over the page
                // by someone else stays over the cover too.
                parent.addSubview_positioned_relativeTo(
                    &cover,
                    NSWindowOrderingMode::Above,
                    Some(&page),
                );
                // The page may hold the keyboard from before; a key must not
                // reach it now.
                let _ = webview.focus_parent();
                self.built = Some(Built { cover, button });
            }
            let Some(built) = &self.built else {
                return;
            };
            let frame = page.frame();
            if built.cover.frame() != frame {
                built.cover.setFrame(frame);
                // The wash and frame follow the page's size.
                built.cover.setNeedsDisplay(true);
            }
            let size = built.button.frame().size;
            // The top-right corner, out of the way of the page's content. The
            // cover is not flipped, so its top is its height.
            let origin = NSPoint::new(
                (frame.size.width - size.width - CORNER_GAP)
                    .max(0.0)
                    .floor(),
                (frame.size.height - size.height - CORNER_GAP)
                    .max(0.0)
                    .floor(),
            );
            let at = NSRect::new(origin, size);
            if built.button.frame() != at {
                built.button.setFrame(at);
            }
        }

        pub fn taken(&self) -> bool {
            self.taken.swap(false, Ordering::Relaxed)
        }

        pub fn remove(&mut self) {
            if let Some(built) = self.built.take() {
                built.cover.removeFromSuperview();
            }
        }
    }

    impl Drop for Shield {
        fn drop(&mut self) {
            // The window's view holds the cover, so letting go of our
            // reference alone would leave it over the page.
            self.remove();
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    #[derive(Default)]
    pub struct Shield;

    impl Shield {
        pub fn sync(&mut self, _: &wry::WebView, _: bool) {}

        pub fn taken(&self) -> bool {
            false
        }

        pub fn remove(&mut self) {}
    }
}
