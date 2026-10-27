//! The macOS context menu: turns a [`sidebar_menu`](super::sidebar_menu)
//! entry tree into an `NSMenu` and pops it up at the pointer.
//!
//! The window builds the entries and hands over a callback; picking a row
//! calls it with that row's [`Item`]. `NSMenu` runs its own event loop until
//! it closes, and opening it from a GPUI listener would run that loop while
//! the app is borrowed, where the window's own frames and timers would trip
//! over the borrow. So [`show`] builds the menu at once and opens it on the
//! next pass of the run loop, from a run-loop timer rather than a GCD block:
//! GPUI's main-queue work keeps running underneath, and the window keeps
//! painting behind the menu. Two right-click listeners under the pointer can
//! both ask in one event; the last one wins.

use super::sidebar_menu::{Entry, Item};

#[cfg(target_os = "macos")]
// SAFETY (module-wide): every call here runs on the main thread, gated by a
// `MainThreadMarker`, against AppKit objects this module retains.
#[allow(unsafe_code)]
mod imp {
    use std::cell::RefCell;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSEvent, NSImage, NSMenu, NSMenuItem,
    };
    use objc2_foundation::NSString;

    use super::{Entry, Item};

    /// What a pick hands its row to.
    type OnPick = Box<dyn FnOnce(Item)>;

    struct Ivars {
        /// Set once, right after the target exists, since every row needs
        /// the target before the menu can be built.
        menu: RefCell<Option<Retained<NSMenu>>>,
        /// The item each row stands for, indexed by the row's tag.
        items: RefCell<Vec<Item>>,
        /// Taken by the first pick, so a menu runs at most one row.
        on_pick: RefCell<Option<OnPick>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "PilusContextMenu"]
        #[ivars = Ivars]
        struct Target;

        impl Target {
            #[unsafe(method(pick:))]
            fn pick(&self, sender: &NSMenuItem) {
                let item = usize::try_from(sender.tag())
                    .ok()
                    .and_then(|index| self.ivars().items.borrow().get(index).copied());
                let on_pick = self.ivars().on_pick.borrow_mut().take();
                if let (Some(item), Some(on_pick)) = (item, on_pick) {
                    on_pick(item);
                }
            }

            #[unsafe(method(popUp:))]
            fn pop_up(&self, _sender: Option<&AnyObject>) {
                // A later ask in the same event replaced this one.
                let current = PENDING.with(|pending| {
                    pending
                        .borrow()
                        .as_ref()
                        .is_some_and(|pending| std::ptr::eq(&**pending, self))
                });
                if !current {
                    return;
                }
                PENDING.with(|pending| pending.borrow_mut().take());
                let menu = self.ivars().menu.borrow().clone();
                if let Some(menu) = menu {
                    // A nil view reads the location in screen coordinates.
                    menu.popUpMenuPositioningItem_atLocation_inView(
                        None,
                        NSEvent::mouseLocation(),
                        None,
                    );
                }
            }
        }

        unsafe impl NSObjectProtocol for Target {}
    );

    thread_local! {
        /// The menu waiting for its run-loop pass.
        static PENDING: RefCell<Option<Retained<Target>>> = const { RefCell::new(None) };
    }

    impl Target {
        fn new(mtm: MainThreadMarker, on_pick: OnPick) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars {
                menu: RefCell::new(None),
                items: RefCell::new(Vec::new()),
                on_pick: RefCell::new(Some(on_pick)),
            });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// A context menu that has been asked for. Holding it keeps the menu's
    /// target alive, which AppKit only references weakly.
    pub struct Menu {
        target: Retained<Target>,
    }

    impl Menu {
        /// Insert `entry` at `index` among the top-level rows, also while the
        /// menu is up.
        pub fn insert(&self, index: usize, entry: &Entry) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let Some(menu) = self.target.ivars().menu.borrow().clone() else {
                return;
            };
            let row = {
                let mut items = self.target.ivars().items.borrow_mut();
                row(mtm, &self.target, entry, &mut items, None)
            };
            let index = isize::try_from(index)
                .unwrap_or(isize::MAX)
                .min(menu.numberOfItems());
            menu.insertItem_atIndex(&row, index);
        }
    }

    /// Ask for a menu of `entries` at the pointer, drawn dark or light to
    /// match the window. It opens on the next pass of the run loop.
    pub fn show(
        entries: &[Entry],
        dark: bool,
        on_pick: impl FnOnce(Item) + 'static,
    ) -> Option<Menu> {
        let mtm = MainThreadMarker::new()?;
        let target = Target::new(mtm, Box::new(on_pick));
        let appearance = NSAppearance::appearanceNamed(&NSString::from_str(if dark {
            "NSAppearanceNameDarkAqua"
        } else {
            "NSAppearanceNameAqua"
        }));
        let menu = {
            let mut items = target.ivars().items.borrow_mut();
            build(mtm, &target, entries, &mut items, appearance.as_deref())
        };
        target.ivars().menu.replace(Some(menu));
        PENDING.with(|pending| pending.replace(Some(target.clone())));
        let receiver: &AnyObject = &target;
        let none: Option<&AnyObject> = None;
        // The run loop retains the receiver until the selector has run.
        let _: () = unsafe {
            msg_send![receiver, performSelector: sel!(popUp:), withObject: none, afterDelay: 0.0f64]
        };
        Some(Menu { target })
    }

    fn build(
        mtm: MainThreadMarker,
        target: &Target,
        entries: &[Entry],
        items: &mut Vec<Item>,
        appearance: Option<&NSAppearance>,
    ) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setAppearance(appearance);
        for entry in entries {
            menu.addItem(&row(mtm, target, entry, items, appearance));
        }
        menu
    }

    fn row(
        mtm: MainThreadMarker,
        target: &Target,
        entry: &Entry,
        items: &mut Vec<Item>,
        appearance: Option<&NSAppearance>,
    ) -> Retained<NSMenuItem> {
        let (title, symbol) = match entry {
            Entry::Separator => return NSMenuItem::separatorItem(mtm),
            Entry::Action { title, symbol, .. } | Entry::Submenu { title, symbol, .. } => {
                (title, *symbol)
            }
        };
        let action = matches!(entry, Entry::Action { .. }).then_some(sel!(pick:));
        let row = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                action,
                &NSString::new(),
            )
        };
        match entry {
            Entry::Action { item, .. } => {
                let receiver: &AnyObject = target;
                unsafe { row.setTarget(Some(receiver)) };
                row.setTag(isize::try_from(items.len()).unwrap_or(isize::MAX));
                items.push(*item);
            }
            Entry::Submenu { entries, .. } => {
                row.setSubmenu(Some(&build(mtm, target, entries, items, appearance)));
            }
            Entry::Separator => {}
        }
        if !symbol.is_empty()
            && let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                None,
            )
        {
            row.setImage(Some(&image));
        }
        row
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Entry, Item};

    /// Without AppKit there is no menu to show.
    pub struct Menu;

    impl Menu {
        pub fn insert(&self, _index: usize, _entry: &Entry) {}
    }

    pub fn show(
        _entries: &[Entry],
        _dark: bool,
        _on_pick: impl FnOnce(Item) + 'static,
    ) -> Option<Menu> {
        None
    }
}

pub use imp::{Menu, show};
