//! The macOS menu bar item: a glanceable agent list that works while pilus is
//! in the background.
//!
//! The window folds `agent.list` from every connected endpoint into [`Row`]s
//! and hands them to [`MenuBar::sync`], which rebuilds the status item's menu
//! only when the rows change. A click never touches the view directly: the
//! AppKit action drops a [`Pick`] into a channel and the window drains it on
//! its next tick, so focusing an agent goes through the same path as a
//! sidebar click. One status item serves the whole process and outlives every
//! window: a single open window feeds it, so a second window does not stack a
//! second icon, and with none open [`headless`] feeds it from the app's own
//! watcher. Other platforms have no menu bar, so they do nothing.

/// One agent in the menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The owning endpoint's key, echoed back in the [`Pick`].
    pub endpoint: String,
    /// The endpoint's display name; shown as a header once more than one
    /// endpoint has agents.
    pub group: String,
    /// The agent's pane id.
    pub pane: String,
    /// The label to draw.
    pub title: String,
    /// `working`, `blocked`, `done`, or `idle`.
    pub status: String,
    /// The working directory, shown on hover.
    pub path: String,
    /// The status dot's color, resolved from the theme by the caller.
    pub color: [u8; 3],
}

/// What a menu click asks the window to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    /// Focus an agent's pane on its endpoint.
    Agent { endpoint: String, pane: String },
    /// Bring the window forward, opening one if none is left.
    Show,
    /// Quit pilus, as Cmd+Q does.
    Quit,
}

#[cfg(target_os = "macos")]
// SAFETY (module-wide): every call here runs on the main thread, gated by a
// `MainThreadMarker`, against live AppKit objects this module retains.
#[allow(unsafe_code)]
mod imp {
    use std::cell::RefCell;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
    use objc2::{
        AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
    };
    use objc2_app_kit::{
        NSAttributedStringNSExtendedStringDrawing, NSBaselineOffsetAttributeName, NSColor, NSFont,
        NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSMenu, NSMenuItem,
        NSStatusBar, NSStatusItem, NSStringDrawingOptions, NSVariableStatusItemLength,
    };
    use objc2_foundation::{
        NSAttributedString, NSData, NSMutableAttributedString, NSNumber, NSRange, NSSize, NSString,
    };

    use super::{Pick, Row};

    /// Separates the fields packed into a menu item's represented object.
    const FIELD: char = '\u{1f}';

    /// The status dot ahead of each agent's label.
    const DOT: &str = "●";
    /// The dot's size against the menu font. At full size it outweighs the
    /// label.
    const DOT_SCALE: f64 = 0.65;

    /// The pilus mark: a white face with dark eyes.
    const LOGO: &str = include_str!("../../assets/pilus.svg");
    /// The mark's own canvas leaves a wide margin around the face, which
    /// would shrink it in the menu bar; this frames the face alone.
    const LOGO_CANVAS: (&str, &str) = ("viewBox=\"0 0 100 100\"", "viewBox=\"16.8 17.2 66 66\"");

    struct Ivars {
        tx: Sender<Pick>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "PilusMenuTarget"]
        #[ivars = Ivars]
        struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(pick:))]
            fn pick(&self, sender: &NSMenuItem) {
                let packed = sender.representedObject()
                    .and_then(|object| object.downcast::<NSString>().ok())
                    .map(|text| text.to_string());
                if let Some(pick) = packed.as_deref().and_then(unpack) {
                    let _ = self.ivars().tx.send(pick);
                }
            }
        }

        unsafe impl NSObjectProtocol for MenuTarget {}
    );

    impl MenuTarget {
        fn new(mtm: MainThreadMarker, tx: Sender<Pick>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { tx });
            unsafe { msg_send![super(this), init] }
        }
    }

    fn pack(pick: &Pick) -> String {
        match pick {
            Pick::Agent { endpoint, pane } => format!("agent{FIELD}{endpoint}{FIELD}{pane}"),
            Pick::Show => "show".to_string(),
            Pick::Quit => "quit".to_string(),
        }
    }

    fn unpack(text: &str) -> Option<Pick> {
        let mut fields = text.split(FIELD);
        match fields.next()? {
            "agent" => Some(Pick::Agent {
                endpoint: fields.next()?.to_string(),
                pane: fields.next()?.to_string(),
            }),
            "show" => Some(Pick::Show),
            "quit" => Some(Pick::Quit),
            _ => None,
        }
    }

    /// Rasterize `svg` `px` pixels square.
    fn rasterize(svg: &str, px: u32) -> Option<resvg::tiny_skia::Pixmap> {
        let tree =
            resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default()).ok()?;
        let scale = px as f32 / tree.size().width().max(tree.size().height());
        let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px)?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        Some(pixmap)
    }

    /// The mark as a template mask: the white face is opaque and the dark
    /// eyes are cut out, so AppKit tints it for light, dark, and a
    /// highlighted menu bar.
    fn logo_template(px: u32) -> Option<Vec<u8>> {
        let mut pixmap = rasterize(&LOGO.replace(LOGO_CANVAS.0, LOGO_CANVAS.1), px)?;
        // Premultiplied, so a white pixel's red channel is its coverage and a
        // dark one's is near zero: that channel becomes the mask's alpha.
        for pixel in pixmap.data_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 0, pixel[0]]);
        }
        pixmap.encode_png().ok()
    }

    fn image_from_png(png: &[u8], points: f64) -> Option<Retained<NSImage>> {
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))?;
        image.setSize(NSSize::new(points, points));
        Some(image)
    }

    /// The vertical middle of `glyph`'s ink above the baseline when set in
    /// `font`, measured from the glyph itself so it holds for any font.
    fn ink_center(glyph: &str, font: &NSFont) -> f64 {
        let text = NSMutableAttributedString::initWithString(
            NSMutableAttributedString::alloc(),
            &NSString::from_str(glyph),
        );
        let font: &AnyObject = font;
        let all = NSRange::new(0, glyph.encode_utf16().count());
        unsafe { text.addAttribute_value_range(NSFontAttributeName, font, all) };
        let ink = text.boundingRectWithSize_options_context(
            NSSize::new(1e4, 1e4),
            NSStringDrawingOptions::UsesDeviceMetrics,
            None,
        );
        ink.origin.y + ink.size.height / 2.0
    }

    /// The dot's font and how far to lift it off the baseline. A smaller dot
    /// sits on the baseline, which leaves it low against the label; lifting
    /// it puts the dot's middle on the middle of the label's letters, between
    /// the centers of its lowercase and its capitals.
    fn dot_style() -> (Retained<NSFont>, f64) {
        let label = NSFont::menuFontOfSize(0.0);
        let font = NSFont::menuFontOfSize(label.pointSize() * DOT_SCALE);
        let letters = (label.xHeight() + label.capHeight()) / 4.0;
        (font.clone(), letters - ink_center(DOT, &font))
    }

    /// The status item and everything it keeps alive.
    struct Shared {
        mtm: MainThreadMarker,
        item: Retained<NSStatusItem>,
        /// AppKit holds menu targets weakly, so this keeps the target alive.
        target: Retained<MenuTarget>,
        rx: Receiver<Pick>,
        glyph: Option<Retained<NSImage>>,
        /// The dot's font and baseline lift, measured once.
        dot: (Retained<NSFont>, f64),
        shown: Option<Vec<Row>>,
        /// The window that feeds the menu; `None` leaves it to [`headless`].
        owner: Option<u64>,
    }

    impl Shared {
        fn install(mtm: MainThreadMarker) -> Self {
            let (tx, rx) = channel();
            let item =
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            let glyph = logo_template(36).and_then(|png| {
                let image = image_from_png(&png, 18.0)?;
                image.setTemplate(true);
                Some(image)
            });
            Self {
                mtm,
                item,
                target: MenuTarget::new(mtm, tx),
                rx,
                glyph,
                dot: dot_style(),
                shown: None,
                owner: None,
            }
        }

        /// A row's title with its status dot: a colored `●` in front of the
        /// label. Drawn as text, so it shows wherever the menu does.
        fn row_title(&self, label: &str, color: [u8; 3]) -> Retained<NSAttributedString> {
            let text = NSString::from_str(&format!("{DOT}  {label}"));
            let title = NSMutableAttributedString::initWithString(
                NSMutableAttributedString::alloc(),
                &text,
            );
            let [r, g, b] = color.map(|c| f64::from(c) / 255.0);
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
            let dot = NSRange::new(0, DOT.encode_utf16().count());
            let color: &AnyObject = &color;
            let font: &AnyObject = &self.dot.0;
            let lift = NSNumber::new_f64(self.dot.1);
            let lift: &AnyObject = &lift;
            unsafe {
                title.addAttribute_value_range(NSForegroundColorAttributeName, color, dot);
                title.addAttribute_value_range(NSFontAttributeName, font, dot);
                title.addAttribute_value_range(NSBaselineOffsetAttributeName, lift, dot);
            }
            Retained::into_super(title)
        }

        fn action_item(&self, title: &str, pick: Option<&Pick>) -> Retained<NSMenuItem> {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm),
                    &NSString::from_str(title),
                    Some(sel!(pick:)),
                    &NSString::new(),
                )
            };
            let target: &AnyObject = &self.target;
            unsafe { item.setTarget(Some(target)) };
            if let Some(pick) = pick {
                let packed = NSString::from_str(&pack(pick));
                let packed: &AnyObject = &packed;
                unsafe { item.setRepresentedObject(Some(packed)) };
            }
            item
        }

        fn header(&self, title: &str) -> Retained<NSMenuItem> {
            let item = self.action_item(title, None);
            item.setEnabled(false);
            item
        }

        fn rebuild(&mut self, rows: &[Row]) {
            if self.shown.as_deref() == Some(rows) {
                return;
            }
            let menu = NSMenu::new(self.mtm);
            menu.setAutoenablesItems(false);
            let grouped = rows
                .iter()
                .skip(1)
                .any(|row| row.endpoint != rows[0].endpoint);
            if rows.is_empty() {
                menu.addItem(&self.header("No agents"));
            }
            let mut group: Option<&str> = None;
            for row in rows {
                if grouped && group != Some(row.endpoint.as_str()) {
                    group = Some(row.endpoint.as_str());
                    menu.addItem(&self.header(&row.group));
                }
                let pick = Pick::Agent {
                    endpoint: row.endpoint.clone(),
                    pane: row.pane.clone(),
                };
                let item = self.action_item(&row.title, Some(&pick));
                item.setAttributedTitle(Some(&self.row_title(&row.title, row.color)));
                if !row.path.is_empty() {
                    item.setToolTip(Some(&NSString::from_str(&row.path)));
                }
                menu.addItem(&item);
            }
            menu.addItem(&NSMenuItem::separatorItem(self.mtm));
            menu.addItem(&self.action_item("Show pilus", Some(&Pick::Show)));
            menu.addItem(&self.action_item("Quit pilus", Some(&Pick::Quit)));
            self.item.setMenu(Some(&menu));

            if let Some(button) = self.item.button(self.mtm) {
                button.setImage(self.glyph.as_deref());
                // Blocked agents are the ones waiting on the user, so they get
                // a count beside the glyph.
                let blocked = rows.iter().filter(|row| row.status == "blocked").count();
                let title = if blocked > 0 {
                    format!(" {blocked}")
                } else {
                    String::new()
                };
                button.setTitle(&NSString::from_str(&title));
            }
            self.shown = Some(rows.to_vec());
        }
    }

    #[cfg(test)]
    mod tests {
        use super::logo_template;
        use super::{DOT, DOT_SCALE, dot_style, ink_center};
        use objc2_app_kit::NSFont;

        #[test]
        fn the_dot_is_lifted_onto_the_middle_of_the_label() {
            let label = NSFont::menuFontOfSize(0.0);
            // The measurement reads a capital's middle as half its height, so
            // the same reading can place the dot.
            let cap = ink_center("H", &label);
            assert!(
                (cap - label.capHeight() / 2.0).abs() < 0.6,
                "cap middle {cap}"
            );

            let (font, lift) = dot_style();
            assert!(font.pointSize() < label.pointSize() * (DOT_SCALE + 0.01));
            let middle = ink_center(DOT, &font) + lift;
            let target = (label.xHeight() + label.capHeight()) / 4.0;
            assert!((middle - target).abs() < 0.05, "dot {middle} vs {target}");
            assert!(lift > 0.0, "a smaller dot must be raised, not sunk");
        }

        #[test]
        fn the_template_keeps_the_face_and_cuts_out_the_eyes() {
            let png = logo_template(36).expect("rasterize");
            let mask = image::load_from_memory(&png).expect("decode").to_rgba8();
            let alpha = |x: u32, y: u32| mask.get_pixel(x, y)[3];
            // Between the eyes, a corner outside the oval, and the left eye.
            assert!(alpha(18, 24) > 240, "face is opaque");
            assert_eq!(alpha(0, 0), 0, "corner is clear");
            assert!(alpha(13, 15) < 40, "eye is cut out");
        }
    }

    impl Drop for Shared {
        fn drop(&mut self) {
            NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        }
    }

    thread_local! {
        static SHARED: RefCell<Option<Shared>> = const { RefCell::new(None) };
    }

    /// A window's claim on the process-wide menu bar item.
    pub struct MenuBar {
        id: u64,
    }

    impl MenuBar {
        /// `id` tells this window apart from the others.
        pub fn new(id: u64) -> Self {
            Self { id }
        }

        /// Show `rows`, installing the item when `enabled`, or remove it.
        /// While another window owns the item this does nothing.
        pub fn sync(&mut self, enabled: bool, rows: &[Row]) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            SHARED.with(|shared| {
                let mut shared = shared.borrow_mut();
                if !enabled {
                    // Turning it off removes the item for everyone.
                    if shared
                        .as_ref()
                        .is_some_and(|s| s.owner.is_none_or(|owner| owner == self.id))
                    {
                        *shared = None;
                    }
                    return;
                }
                let state = shared.get_or_insert_with(|| Shared::install(mtm));
                state.owner.get_or_insert(self.id);
                if state.owner == Some(self.id) {
                    state.rebuild(rows);
                }
            });
        }

        /// Clicks since the last call. Empty unless this window owns the item.
        pub fn drain(&mut self) -> Vec<Pick> {
            SHARED.with(|shared| {
                shared
                    .borrow()
                    .as_ref()
                    .filter(|state| state.owner == Some(self.id))
                    .map(|state| state.rx.try_iter().collect())
                    .unwrap_or_default()
            })
        }
    }

    impl Drop for MenuBar {
        /// A closing window leaves the item standing, now fed by [`headless`]
        /// until the next window adopts it.
        fn drop(&mut self) {
            let _ = SHARED.try_with(|shared| {
                if let Some(state) = shared.borrow_mut().as_mut()
                    && state.owner == Some(self.id)
                {
                    state.owner = None;
                }
            });
        }
    }

    /// Feed the item while no window owns it: install it when `enabled`,
    /// remove it otherwise, and return the clicks since the last call. Does
    /// nothing while a window owns the item.
    pub fn headless(enabled: bool, rows: &[Row]) -> Vec<Pick> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Vec::new();
        };
        SHARED.with(|shared| {
            let mut shared = shared.borrow_mut();
            if !enabled {
                if shared.as_ref().is_some_and(|s| s.owner.is_none()) {
                    *shared = None;
                }
                return Vec::new();
            }
            let state = shared.get_or_insert_with(|| Shared::install(mtm));
            if state.owner.is_some() {
                return Vec::new();
            }
            state.rebuild(rows);
            state.rx.try_iter().collect()
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Pick, Row};

    pub struct MenuBar;

    pub fn headless(_enabled: bool, _rows: &[Row]) -> Vec<Pick> {
        Vec::new()
    }

    impl MenuBar {
        pub fn new(_id: u64) -> Self {
            Self
        }

        pub fn sync(&mut self, _enabled: bool, _rows: &[Row]) {}

        pub fn drain(&mut self) -> Vec<Pick> {
            Vec::new()
        }
    }
}

pub use imp::{MenuBar, headless};
