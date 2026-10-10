//! The window's frosted backdrop.
//!
//! GPUI's `Blurred` background installs an `NSVisualEffectView` with the
//! weakest material and then strips its tint and saturation, so the "blur"
//! read as a faint desaturated smear under the window. Real glass is frosted
//! and vibrant: on macOS the window stays `Transparent` and [`Glass`] pins
//! its own effect view — a strong material, kept active — beneath the
//! content view. The blur slider drives the view's `alphaValue`, so the
//! backdrop crossfades between frosted and sharp instead of only dimming a
//! fixed-radius blur. Other platforms keep the built-in appearance toggle.

#[cfg(target_os = "macos")]
mod imp {
    use gpui::Window;
    use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAutoresizingMaskOptions, NSView,
        NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
        NSVisualEffectView, NSWindowOrderingMode,
    };
    use objc2_foundation::NSString;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// The effect view under the content view, dropped while the desktop
    /// should stay sharp.
    pub struct Glass {
        view: Option<Retained<NSVisualEffectView>>,
        /// Strength last pushed to the view, so an idle frame does not
        /// re-send the same alpha.
        applied: Option<f32>,
    }

    impl Glass {
        pub fn new() -> Self {
            Self {
                view: None,
                applied: None,
            }
        }

        /// Drive the backdrop toward `strength`: zero removes the view so the
        /// desktop renders sharp, above zero installs it once and fades
        /// frosted material against sharp via the view's alpha.
        pub fn apply(&mut self, window: &mut Window, strength: f32) {
            let strength = strength.clamp(0.0, 1.0);
            if strength <= 0.0 {
                if let Some(view) = self.view.take() {
                    view.removeFromSuperview();
                }
                self.applied = None;
                return;
            }
            if self.view.is_none() {
                self.view = install(window);
                self.applied = None;
            }
            if self.applied != Some(strength)
                && let Some(view) = &self.view
            {
                view.setAlphaValue(f64::from(strength));
                self.applied = Some(strength);
            }
        }
    }

    /// Pin a frosted effect view to the bottom of the window's content view,
    /// where it blurs everything behind the window without touching the
    /// GPUI view on top.
    fn install(window: &Window) -> Option<Retained<NSVisualEffectView>> {
        let mtm = MainThreadMarker::new()?;
        // `Window` also has an inherent `window_handle` (its GPUI handle), so
        // the trait method needs its full path to reach the AppKit view.
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        // SAFETY: the handle wraps the live NSView GPUI renders into, and
        // `mtm` proves this runs on the main thread where AppKit objects may
        // be touched.
        #[allow(unsafe_code)]
        let native = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
        let content = native.window()?.contentView()?;
        let effect =
            NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), content.bounds());
        // The frosted material, not the colorless `Selection` the stock
        // blur uses; vibrant and kept active so an unfocused window does
        // not lose its frost.
        effect.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
        effect.setState(NSVisualEffectState::Active);
        // The material follows the system appearance, so a light-mode Mac
        // frosts the window white and washes out the dark chrome above it.
        // Pin the dark one.
        effect.setAppearance(
            NSAppearance::appearanceNamed(&NSString::from_str("NSAppearanceNameDarkAqua"))
                .as_deref(),
        );
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        content.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, None);
        Some(effect)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use gpui::{Window, WindowBackgroundAppearance};

    /// Without the AppKit path the backdrop falls back to GPUI's built-in
    /// toggle: any strength above zero keeps the window blurred, zero drops
    /// it to a sharp desktop.
    pub struct Glass {
        blurred: Option<bool>,
    }

    impl Glass {
        pub fn new() -> Self {
            Self { blurred: None }
        }

        pub fn apply(&mut self, window: &mut Window, strength: f32) {
            let blurred = strength > 0.0;
            if self.blurred != Some(blurred) {
                window.set_background_appearance(if blurred {
                    WindowBackgroundAppearance::Blurred
                } else {
                    WindowBackgroundAppearance::Transparent
                });
                self.blurred = Some(blurred);
            }
        }
    }
}

pub use imp::Glass;
