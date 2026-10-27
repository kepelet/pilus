//! The native Browser tab: a system webview in the right column, with a strip
//! of page tabs and an address bar over it.
//!
//! The page is a `wry` webview that `gpui-wry` hosts as a native child of the
//! window, so it is drawn by the OS, above everything GPUI paints. That shapes
//! the tab: a webview is created on a page's first address, kept alive while
//! the tab is away, and shown only while nothing GPUI draws could land on it
//! (see [`PilusView::browser_on_screen`]). Page events come back on a queue
//! the webview's handlers fill, which [`PilusView::browser_tick`] drains with
//! the other tabs' background work.
//!
//! Each page tab owns its own webview and queue, so an event is never
//! mistaken for another page's. Only the selected page is shown; the others
//! are hidden and parked, and keep running like a browser's background tabs.
//! A native webview is a whole WebContent process, so the strip holds at most
//! [`MAX_PAGES`] of them.
//!
//! The address bar is a [`TextInput`]. It follows the selected page's address
//! while it is not being edited, and what the user typed becomes the next load.

use std::sync::{Arc, Mutex};

use gpui::{
    AnyElement, Context, Div, Entity, MouseButton, SharedString, Stateful, Window, div, prelude::*,
    px,
};
use gpui_wry::WebView;
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};

use super::browser_data;
use super::style::{self, FillExt, RadiusExt};
use super::toast::ToastKind;
use super::{PilusView, icon};

/// The most page tabs open at once. Each is a native webview with a
/// WebContent process of its own, which costs far more than a terminal pane.
const MAX_PAGES: usize = 8;

/// What the webview's handlers report.
enum Event {
    /// The page at this address started or finished loading.
    Loaded(String),
    /// The page's title changed.
    Titled(String),
    /// The page asked for a new window; the strip opens it as a tab instead.
    Opened(String),
}

type Events = Arc<Mutex<Vec<Event>>>;

/// Space between the page's frame and the column's edges. The frame's left
/// edge also leaves the resize handle (the strip beside it) just reachable.
const FRAME_GUTTER: f32 = 4.0;

fn push(events: &Events, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push(event);
    }
}

/// One page tab. It has no `view` until its first address is loaded.
struct Page {
    view: Option<Entity<WebView>>,
    events: Events,
    /// An address to load on the next frame, which has the window to build
    /// the webview with.
    pending: Option<String>,
    /// The page's address, as the webview last reported it.
    url: String,
    /// The page's title, as the webview last reported it.
    title: String,
    /// The corner radius last pushed to the webview's layer, so an idle
    /// frame does not re-send it and a radius setting change reaches it.
    corner: Option<f32>,
}

impl Page {
    fn new() -> Self {
        Self {
            view: None,
            events: Arc::default(),
            pending: None,
            url: String::new(),
            title: String::new(),
            corner: None,
        }
    }

    /// A page that will load `url` once it is on screen.
    fn loading(url: String) -> Self {
        Self {
            pending: Some(url),
            ..Self::new()
        }
    }

    /// Close the page and forget it. Dropping the entity frees the native
    /// `WKWebView` once the next frame lets go of the element's handle
    /// (wry's `Drop` looks like it retains the view, but the `Retained` it
    /// gets back is dropped on the spot, so the count nets out). Blanking
    /// first silences the page's media right away instead of a frame later.
    fn close(&mut self, cx: &gpui::App) {
        if let Some(view) = self.view.take() {
            let _ = view.read(cx).raw().load_url("about:blank");
        }
        self.forget();
    }

    /// Drop what the page knew. The queue is cleared so a late event cannot
    /// bring the old address back.
    fn forget(&mut self) {
        self.view = None;
        self.pending = None;
        self.url.clear();
        self.title.clear();
        self.corner = None;
        if let Ok(mut queue) = self.events.lock() {
            queue.clear();
        }
    }

    /// What the page's tab reads: its title, else its host, else a blank tab.
    fn label(&self) -> String {
        let title = self.title.trim();
        if !title.is_empty() {
            return title.to_string();
        }
        host_of(&self.url).unwrap_or_else(|| "New tab".to_string())
    }
}

/// The host of `url`, for a tab whose page has no title yet.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

pub(super) struct BrowserPanel {
    /// The open page tabs; never empty, and at most [`MAX_PAGES`].
    pages: Vec<Page>,
    /// The tab the column shows.
    active: usize,
    /// The address bar holds the keyboard.
    pub(super) editing: bool,
}

impl BrowserPanel {
    pub(super) fn new() -> Self {
        Self {
            pages: vec![Page::new()],
            active: 0,
            editing: false,
        }
    }

    fn page(&self) -> &Page {
        &self.pages[self.active.min(self.pages.len() - 1)]
    }

    fn page_mut(&mut self) -> &mut Page {
        let ix = self.active.min(self.pages.len() - 1);
        &mut self.pages[ix]
    }

    /// Whether the selected tab has a page open, so there is a webview to drive.
    pub(super) fn has_page(&self) -> bool {
        self.page().view.is_some()
    }

    fn full(&self) -> bool {
        self.pages.len() >= MAX_PAGES
    }

    /// Close tab `ix`. The last tab is emptied instead of removed, so the
    /// strip always has a page to show.
    fn close_page(&mut self, ix: usize, cx: &gpui::App) {
        if let Some(page) = self.pages.get_mut(ix) {
            page.close(cx);
        }
        self.close_page_state(ix);
    }

    /// The bookkeeping of [`Self::close_page`], once the webview is gone.
    fn close_page_state(&mut self, ix: usize) {
        if ix >= self.pages.len() {
            return;
        }
        self.pages[ix].forget();
        if self.pages.len() == 1 {
            return;
        }
        self.pages.remove(ix);
        self.active = selected_after_close(self.active, ix, self.pages.len());
    }
}

/// The selected tab once tab `closed` is gone and `left` remain. The same
/// page stays selected: closing one before it moves the index down, and
/// closing the selected one hands over to its right neighbour, or to the left
/// one when it was last.
fn selected_after_close(active: usize, closed: usize, left: usize) -> usize {
    if closed < active || active >= left {
        active.saturating_sub(1)
    } else {
        active
    }
}

/// Round the page's bottom-right corner by clipping its backing layer.
/// GPUI cannot clip a native view, so the layer does it. AppKit layers are
/// not flipped, so the bottom edge is the layer's min-y.
#[cfg(target_os = "macos")]
fn round_page_corner(webview: &wry::WebView, radius: f32) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use wry::WebViewExtMacOS;

    /// `kCALayerMaxXMinYCorner`.
    const BOTTOM_RIGHT: usize = 1 << 1;

    let page = webview.webview();
    // SAFETY: `layer` is an NSView/CALayer selector sent to a live view on
    // the main thread, which is where the GUI runs; the setters take the
    // CGFloat, BOOL, and CACornerMask values their signatures name.
    #[allow(unsafe_code)]
    unsafe {
        let _: () = msg_send![&*page, setWantsLayer: true];
        let layer: Option<Retained<AnyObject>> = msg_send![&*page, layer];
        if let Some(layer) = layer {
            let _: () = msg_send![&*layer, setCornerRadius: f64::from(radius)];
            let _: () = msg_send![&*layer, setMaskedCorners: BOTTOM_RIGHT];
            let _: () = msg_send![&*layer, setMasksToBounds: true];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn round_page_corner(_: &wry::WebView, _: f32) {}

/// Move a hidden page out of the window, keeping its size.
///
/// AppKit still hands a drag over a hidden view's frame to that view, so a
/// page hidden in place would take, and drop, files dragged onto the tab
/// showing in its place: the Files tab never hears of them. The size stays so
/// the page does not reflow, and showing it needs no undo here, since the
/// element sets the page's bounds on every frame it paints.
fn park_page(webview: &wry::WebView) {
    let Ok(bounds) = webview.bounds() else {
        return;
    };
    let _ = webview.set_bounds(wry::Rect {
        position: wry::dpi::LogicalPosition::new(-100_000.0, -100_000.0).into(),
        size: bounds.size,
    });
}

/// Whether the window's first responder is the webview or one of its
/// subviews (WebKit's content view takes it once the page is clicked).
#[cfg(target_os = "macos")]
fn page_holds_responder(webview: &wry::WebView) -> bool {
    use objc2_app_kit::NSView;
    use wry::WebViewExtMacOS;

    let page = webview.webview();
    let Some(window) = page.window() else {
        return false;
    };
    let Some(responder) = window.firstResponder() else {
        return false;
    };
    responder
        .downcast_ref::<NSView>()
        .is_some_and(|view| view.isDescendantOf(&page))
}

/// Deliver the key event AppKit is dispatching to the page's first responder.
#[cfg(target_os = "macos")]
fn send_key_to_page(webview: &wry::WebView) -> bool {
    use objc2::{MainThreadMarker, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_app_kit::NSApplication;
    use wry::WebViewExtMacOS;

    let Some(main) = MainThreadMarker::new() else {
        return false;
    };
    let Some(window) = webview.webview().window() else {
        return false;
    };
    let Some(responder) = window.firstResponder() else {
        return false;
    };
    // SAFETY: `currentEvent` returns the event being dispatched (or nil), and
    // `keyDown:` takes that `NSEvent` on the main thread.
    #[allow(unsafe_code)]
    unsafe {
        let app = NSApplication::sharedApplication(main);
        let event: Option<Retained<AnyObject>> = msg_send![&*app, currentEvent];
        let Some(event) = event else {
            return false;
        };
        let _: () = msg_send![&*responder, keyDown: &*event];
    }
    true
}

#[cfg(not(target_os = "macos"))]
fn send_key_to_page(_: &wry::WebView) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn page_holds_responder(_: &wry::WebView) -> bool {
    false
}

/// An Edit-menu command the page answers itself.
#[derive(Clone, Copy)]
pub(super) enum PageEdit {
    Copy,
    Paste,
    SelectAll,
}

/// Send `edit` down the responder chain, which ends at the webview while
/// the page holds the keyboard.
#[cfg(target_os = "macos")]
fn send_edit(edit: PageEdit) {
    use objc2::{MainThreadMarker, sel};
    use objc2_app_kit::NSApplication;

    let Some(main) = MainThreadMarker::new() else {
        return;
    };
    let action = match edit {
        PageEdit::Copy => sel!(copy:),
        PageEdit::Paste => sel!(paste:),
        PageEdit::SelectAll => sel!(selectAll:),
    };
    // SAFETY: the selectors take one sender argument and the sender is nil.
    #[allow(unsafe_code)]
    unsafe {
        NSApplication::sharedApplication(main).sendAction_to_from(action, None, None);
    }
}

#[cfg(not(target_os = "macos"))]
fn send_edit(_: PageEdit) {}

/// The address `typed` stands for: a full address as written, a host as
/// `https` (`http` for this machine, where dev servers rarely speak TLS), and
/// anything else as a search.
fn normalize(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    if typed.contains("://") {
        return Some(typed.to_string());
    }
    let host = typed.split(['/', '?', '#']).next().unwrap_or(typed);
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    let local = name == "localhost"
        || name.parse::<std::net::Ipv4Addr>().is_ok()
        || name.ends_with(".localhost");
    let looks_like_host = !typed.contains(char::is_whitespace) && (local || name.contains('.'));
    if local && looks_like_host {
        return Some(format!("http://{typed}"));
    }
    if looks_like_host {
        return Some(format!("https://{typed}"));
    }
    Some(format!(
        "https://duckduckgo.com/?q={}",
        percent_encode(typed)
    ))
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl PilusView {
    /// Whether the Browser tab is the one the column shows.
    pub(super) fn browser_visible(&self) -> bool {
        self.right.visible && self.right.tab == super::right_sidebar::SidebarTab::Browser
    }

    /// Whether the address bar holds the keyboard.
    pub(super) fn browser_focused(&self) -> bool {
        self.browser_visible() && self.right.focused && self.browser_panel.editing
    }

    /// Whether the page may be drawn: the tab is open and still, and no
    /// overlay of ours is up, since the webview would paint over it.
    fn browser_on_screen(&self) -> bool {
        self.browser_visible()
            && !self.right.moving()
            && self.right.openness() >= 1.0
            && !self.picker.is_open()
            && !self.palette.is_open()
            && !self.home.is_open()
            && !self.settings_open
            && self.menu_prompt.is_none()
    }

    /// Whether the page itself holds the keyboard: the OS-level first
    /// responder is inside the webview, which a click in the page puts there
    /// without GPUI hearing of it. Keys then belong to the page alone, so
    /// the window must neither route them to the terminal nor swallow them.
    pub(super) fn browser_page_focused(&self, cx: &gpui::App) -> bool {
        self.browser_visible()
            && self
                .browser_panel
                .page()
                .view
                .as_ref()
                .is_some_and(|view| page_holds_responder(view.read(cx).raw()))
    }

    /// Hand a keystroke straight to the page and keep the window from also
    /// reading it. Left to AppKit's own routing, a bare arrow or function key
    /// is first fed to the window's text input context, which types its
    /// private-use glyph instead of moving the caret. Command chords stay on
    /// the normal route, where the Edit actions answer them.
    pub(super) fn browser_forward_key(&self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform {
            return;
        }
        if let Some(view) = &self.browser_panel.page().view
            && send_key_to_page(view.read(cx).raw())
        {
            cx.stop_propagation();
        }
    }

    /// Run an Edit-menu command on the page when it holds the keyboard;
    /// false when it does not, so the caller acts on the terminal instead.
    pub(super) fn browser_edit(&self, edit: PageEdit, cx: &gpui::App) -> bool {
        let focused = self.browser_page_focused(cx);
        if focused {
            send_edit(edit);
        }
        focused
    }

    /// Give the address bar the keyboard.
    pub(super) fn browser_begin_edit(&mut self, cx: &mut Context<Self>) {
        self.browser_panel.editing = true;
        self.right.focused = true;
        if let Some(view) = &self.browser_panel.page().view {
            // The webview owns the OS-level first responder once clicked in;
            // take it back so the address bar's keys reach us.
            let _ = view.read(cx).raw().focus_parent();
        }
        cx.notify();
    }

    fn browser_end_edit(&mut self, cx: &mut Context<Self>) {
        self.browser_panel.editing = false;
        self.right.focused = false;
        self.browser_show_address(cx);
        cx.notify();
    }

    /// Put the selected page's address in the address bar.
    fn browser_show_address(&mut self, cx: &mut Context<Self>) {
        let url = self.browser_panel.page().url.clone();
        self.browser_input
            .update(cx, |input, cx| input.set_value(url, cx));
    }

    /// Load what the address bar holds.
    fn browser_submit(&mut self, cx: &mut Context<Self>) {
        let typed = self.browser_input.read(cx).value();
        let Some(url) = normalize(&typed) else {
            self.browser_end_edit(cx);
            return;
        };
        self.browser_input
            .update(cx, |input, cx| input.set_value(url.clone(), cx));
        self.browser_panel.page_mut().pending = Some(url);
        self.browser_panel.editing = false;
        self.right.focused = false;
        cx.notify();
    }

    pub(super) fn browser_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.browser_end_edit(cx),
            "enter" => self.browser_submit(cx),
            _ => {}
        }
    }

    /// Open an empty tab and give it the address bar. At the cap, or from a
    /// tab that is already empty, the selected tab is reused.
    pub(super) fn browser_new_tab(&mut self, cx: &mut Context<Self>) {
        if self.browser_panel.has_page() {
            if self.browser_panel.full() {
                self.toast(
                    ToastKind::Info,
                    format!("The Browser holds {MAX_PAGES} tabs. Close one to open another."),
                );
                return;
            }
            self.browser_panel.pages.push(Page::new());
            self.browser_panel.active = self.browser_panel.pages.len() - 1;
            self.browser_input.update(cx, |input, cx| input.clear(cx));
        }
        self.browser_begin_edit(cx);
    }

    /// Show tab `ix`. An empty tab opens with the address bar ready.
    fn browser_select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.browser_panel.pages.len() || ix == self.browser_panel.active {
            return;
        }
        self.browser_panel.active = ix;
        self.browser_panel.editing = false;
        self.right.focused = false;
        self.browser_show_address(cx);
        if !self.browser_panel.has_page() && self.browser_panel.page().pending.is_none() {
            self.browser_begin_edit(cx);
        }
        cx.notify();
    }

    /// Close tab `ix`: its webview is destroyed, and the next address in an
    /// emptied tab builds a fresh one.
    fn browser_close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let selected = ix == self.browser_panel.active;
        self.browser_panel.close_page(ix, cx);
        if selected {
            self.browser_panel.editing = false;
            self.right.focused = false;
            self.browser_show_address(cx);
            if !self.browser_panel.has_page() {
                self.browser_begin_edit(cx);
            }
        }
        cx.notify();
    }

    /// Run `script` in the page, if there is one.
    fn browser_script(&self, script: &str, cx: &Context<Self>) {
        if let Some(view) = &self.browser_panel.page().view {
            let _ = view.read(cx).raw().evaluate_script(script);
        }
    }

    fn browser_reload(&self, cx: &Context<Self>) {
        if let Some(view) = &self.browser_panel.page().view {
            let _ = view.read(cx).raw().reload();
        }
    }

    /// Drain what the pages reported; true when anything on screen changed.
    pub(super) fn browser_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        // A click in the page moves the OS first responder without GPUI
        // hearing of it, so the address bar learns it lost the keyboard here.
        if self.browser_panel.editing && self.browser_page_focused(cx) {
            self.browser_end_edit(cx);
            changed = true;
        }
        let active = self.browser_panel.active;
        let mut address_changed = false;
        // New windows the pages asked for, with the tab that asked.
        let mut opened = Vec::new();
        for (ix, page) in self.browser_panel.pages.iter_mut().enumerate() {
            let events = match page.events.lock() {
                Ok(mut queue) => std::mem::take(&mut *queue),
                Err(_) => continue,
            };
            for event in events {
                match event {
                    Event::Loaded(url) => {
                        if url != page.url {
                            address_changed |= ix == active;
                            changed = true;
                        }
                        page.url = url;
                    }
                    Event::Titled(title) => {
                        changed |= title != page.title;
                        page.title = title;
                    }
                    Event::Opened(url) => opened.push((ix, url)),
                }
            }
        }
        for (from, url) in opened {
            changed = true;
            if self.browser_panel.full() {
                // No room for another webview: the link loads where it was
                // clicked, as it did before there were tabs.
                self.browser_panel.pages[from].pending = Some(url);
                continue;
            }
            // Beside the opener, and in front only when the opener was.
            let at = from + 1;
            self.browser_panel.pages.insert(at, Page::loading(url));
            if from == self.browser_panel.active {
                self.browser_panel.active = at;
                address_changed = true;
            } else if at <= self.browser_panel.active {
                self.browser_panel.active += 1;
            }
        }
        if address_changed && !self.browser_panel.editing {
            self.browser_show_address(cx);
        }
        changed
    }

    /// Bring the native webviews in line with the tab: build the selected
    /// page's on its first address, load what was asked for, and show it only
    /// while it may be seen. Every other page is hidden and parked.
    pub(super) fn browser_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_screen = self.browser_on_screen();
        let active = self.browser_panel.active;
        if on_screen && let Some(url) = self.browser_panel.page_mut().pending.take() {
            match self.browser_panel.page().view.clone() {
                Some(view) => view.update(cx, |view, _| view.load_url(&url)),
                None => self.browser_open(&url, window, cx),
            }
        }
        // The frame's 1px border sits outside the page, so the page's
        // corner is that much tighter than the frame's.
        let corner = (style::Radius::Md.pixels().to_f64() as f32 - 1.0).max(0.0);
        for (ix, page) in self.browser_panel.pages.iter_mut().enumerate() {
            let Some(view) = &page.view else {
                continue;
            };
            if page.corner != Some(corner) {
                round_page_corner(view.read(cx).raw(), corner);
                page.corner = Some(corner);
            }
            let shown = on_screen && ix == active;
            view.update(cx, |view, _| {
                if shown && !view.visible() {
                    view.show();
                } else if !shown && view.visible() {
                    view.hide();
                    park_page(view.raw());
                }
            });
        }
    }

    fn browser_open(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = self.browser_panel.page().events.clone();
        let titled = loaded.clone();
        let opened = loaded.clone();
        let built = WebViewBuilder::new()
            .with_url(url)
            .with_on_page_load_handler(move |_: PageLoadEvent, url| {
                push(&loaded, Event::Loaded(url));
            })
            .with_document_title_changed_handler(move |title| {
                push(&titled, Event::Titled(title));
            })
            .with_new_window_req_handler(move |url, _| {
                push(&opened, Event::Opened(url));
                NewWindowResponse::Deny
            })
            .with_download_started_handler(|url, path| browser_data::confirm_download(&url, path))
            .with_download_completed_handler(|url, _, succeeded| {
                if !succeeded {
                    tracing::warn!(%url, "browser: download failed");
                }
            })
            .build_as_child(window);
        match built {
            Ok(webview) => {
                let page = self.browser_panel.page_mut();
                page.url = url.to_string();
                page.view = Some(cx.new(|cx| WebView::new(webview, window, cx)));
            }
            Err(error) => tracing::warn!(%error, "browser: could not create the webview"),
        }
    }
}

// Rendering.
impl PilusView {
    pub(super) fn browser_page(&mut self, cx: &mut Context<Self>) -> Div {
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.browser_tab_strip(cx))
            .child(self.browser_header(cx));
        match self.browser_panel.page().view.clone() {
            // While the column moves the webview is hidden, so the body is
            // left empty rather than showing a stale frame.
            //
            // The native view paints above GPUI and takes the mouse, so the
            // frame's gutter keeps it off the resize handle's strip; filling
            // the column would bury the handle and the column could not be
            // dragged.
            Some(view) if self.browser_on_screen() => page.child(self.browser_frame(Some(view))),
            Some(_) => page.child(self.browser_frame(None)),
            None => page.child(
                self.diff_message("Type an address above. Dev servers on localhost open here too."),
            ),
        }
    }

    /// The bordered area the page sits in, inset by a small gutter so the page
    /// reads as content in a frame. Only the bottom-right corner is round, to
    /// meet the window's own; the page's layer is clipped to match (see
    /// [`round_page_corner`]).
    fn browser_frame(&self, page: Option<Entity<WebView>>) -> Div {
        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .px(px(FRAME_GUTTER))
            .py(px(FRAME_GUTTER))
            .child(
                div()
                    .size_full()
                    .border_1()
                    .border_color(self.theme.border)
                    .rounded_br(style::Radius::Md.pixels())
                    .overflow_hidden()
                    .children(page),
            )
    }

    /// The strip of page tabs over the address bar, with a button for a new
    /// one. The tabs share the width and shrink, with their titles cut short,
    /// as a browser's do; the close button shows on the selected tab and on
    /// the one under the pointer, and a middle click closes any tab.
    fn browser_tab_strip(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let full = self.browser_panel.full();
        let active = self.browser_panel.active;
        let tabs: Vec<(usize, String)> = self
            .browser_panel
            .pages
            .iter()
            .enumerate()
            .map(|(ix, page)| (ix, page.label()))
            .collect();
        let tabs = tabs.into_iter().map(|(ix, label)| {
            let group = SharedString::from(format!("pilus-browser-tab-group-{ix}"));
            let selected = ix == active;
            let close = div()
                .id(("pilus-browser-tab-close", ix))
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(fonts.body + 4.0))
                .radius_sm()
                .opacity(if selected { 1.0 } else { 0.0 })
                .group_hover(group.clone(), |style| style.opacity(1.0))
                .hover_fill(theme.panel_dim, style::RADIUS_SM)
                .child(icon(
                    include_bytes!("../../assets/icons/x.svg"),
                    fonts.label,
                    theme.muted,
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.browser_close_tab(ix, cx);
                        cx.stop_propagation();
                    }),
                );
            style::tab(
                ("pilus-browser-tab", ix),
                div().flex_1().min_w(px(0.0)).truncate().child(label),
                selected,
                theme,
                fonts.body,
            )
            .group(group)
            .gap_1()
            .px_2()
            .flex_1()
            .min_w(px(28.0))
            .max_w(px(160.0))
            .child(close)
            .on_click(cx.listener(move |this, _, _, cx| this.browser_select_tab(ix, cx)))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| this.browser_close_tab(ix, cx)),
            )
        });
        let tip = if full {
            format!("{MAX_PAGES} tabs is the most. Close one to open another.")
        } else {
            "New tab".to_string()
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .px_3()
            .pt_2()
            .children(tabs)
            .child(
                self.diff_icon_button(
                    "pilus-browser-new-tab",
                    include_bytes!("../../assets/icons/plus.svg"),
                    tip,
                    !full,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.browser_new_tab(cx))),
            )
    }

    /// Back, forward and reload beside the address bar.
    fn browser_header(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL);
        let loaded = self.browser_panel.has_page();
        self.browser_input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        // Edited, the field scrolls to the caret; at rest a long address
        // is cut short with an ellipsis instead of running past the box.
        let address: AnyElement = if self.browser_panel.editing {
            self.browser_input.clone().into_any_element()
        } else {
            let shown = self.browser_panel.page().url.clone();
            div()
                .truncate()
                .text_color(if shown.is_empty() {
                    theme.muted
                } else {
                    theme.fg
                })
                .child(if shown.is_empty() {
                    "Search or enter address".to_string()
                } else {
                    shown
                })
                .into_any_element()
        };
        let field = style::search_field(
            "pilus-browser-address",
            self.browser_panel.editing,
            theme,
            height,
            fonts.secondary,
            address,
        )
        .flex_1()
        .min_w(px(0.0))
        .text_size(px(fonts.body))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| this.browser_begin_edit(cx)),
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                self.diff_icon_button(
                    "pilus-browser-back",
                    include_bytes!("../../assets/icons/chevron-left.svg"),
                    "Back".to_string(),
                    loaded,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.browser_script("history.back()", cx))),
            )
            .child(
                self.diff_icon_button(
                    "pilus-browser-forward",
                    include_bytes!("../../assets/icons/chevron-right.svg"),
                    "Forward".to_string(),
                    loaded,
                    false,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.browser_script("history.forward()", cx)),
                ),
            )
            .child(
                self.diff_icon_button(
                    "pilus-browser-reload",
                    include_bytes!("../../assets/icons/refresh-cw.svg"),
                    "Reload".to_string(),
                    loaded,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.browser_reload(cx))),
            )
            .child(field)
    }

    /// The globe, lit while the Browser is open.
    fn browser_glyph(&self, lit: bool) -> impl IntoElement {
        let theme = self.theme;
        icon(
            include_bytes!("../../assets/icons/globe.svg"),
            14.0,
            if lit { theme.panel_fg } else { theme.muted },
        )
    }

    /// The Browser button beside the Ports button, lit while the tab is open.
    pub(super) fn right_browser_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-right-browser-toggle")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .h(px(super::PILL_BUTTON_H))
            .w(px(super::PILL_BUTTON_W))
            .radius_sm()
            .cursor_pointer()
            .hover_fill(
                theme.sidebar_raise(self.glass_opacity(), super::SIDEBAR_HOVER_WEIGHT),
                style::RADIUS_SM,
            )
            .tooltip(super::row_tooltip(
                "Browser".to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .child(self.browser_glyph(self.browser_visible()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_browser(cx)))
    }

    /// The same button for the column's own strip, once the column is open.
    pub(super) fn strip_browser_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-right-sidebar-browser")
            .flex()
            .items_center()
            .justify_center()
            .w(px(super::PILL_BUTTON_W))
            .h(px(super::PILL_BUTTON_H))
            .radius_sm()
            .cursor_pointer()
            .hover_fill(
                theme.sidebar_raise(self.glass_opacity(), super::SIDEBAR_HOVER_WEIGHT),
                style::RADIUS_SM,
            )
            .flex_shrink_0()
            .tooltip(super::row_tooltip(
                "Browser".to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .child(self.browser_glyph(self.browser_visible()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_browser(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserPanel, MAX_PAGES, Page, host_of, normalize, selected_after_close};

    #[test]
    fn closing_keeps_the_same_page_selected() {
        // [a b c d], b selected: closing a shifts it to index 0.
        assert_eq!(selected_after_close(1, 0, 3), 0);
        // Closing the selected tab hands over to the one that slid into place.
        assert_eq!(selected_after_close(1, 1, 3), 1);
        // Closing the selected last tab falls back to its left neighbour.
        assert_eq!(selected_after_close(3, 3, 3), 2);
        // Closing a tab after the selected one changes nothing.
        assert_eq!(selected_after_close(0, 2, 3), 0);
    }

    #[test]
    fn the_last_tab_is_emptied_not_removed() {
        let mut panel = BrowserPanel::new();
        panel.pages[0].url = "https://example.com".to_string();
        panel.pages[0].pending = Some("https://example.org".to_string());
        panel.close_page_state(0);
        assert_eq!(panel.pages.len(), 1);
        assert!(panel.pages[0].url.is_empty());
        assert!(panel.pages[0].pending.is_none());
    }

    #[test]
    fn the_strip_stops_at_the_cap() {
        let mut panel = BrowserPanel::new();
        while !panel.full() {
            panel.pages.push(Page::new());
        }
        assert_eq!(panel.pages.len(), MAX_PAGES);
    }

    #[test]
    fn a_tab_reads_its_title_then_its_host() {
        let mut page = Page::new();
        assert_eq!(page.label(), "New tab");
        page.url = "https://user@docs.rs:8080/gpui?q=1".to_string();
        assert_eq!(page.label(), "docs.rs:8080");
        page.title = "  gpui - Rust  ".to_string();
        assert_eq!(page.label(), "gpui - Rust");
        assert_eq!(host_of("about:blank"), None);
    }

    #[test]
    fn full_addresses_pass_through() {
        assert_eq!(
            normalize(" https://example.com/a "),
            Some("https://example.com/a".to_string())
        );
        assert_eq!(
            normalize("file:///tmp/x.html"),
            Some("file:///tmp/x.html".to_string())
        );
    }

    #[test]
    fn hosts_get_a_scheme() {
        assert_eq!(
            normalize("example.com/docs"),
            Some("https://example.com/docs".to_string())
        );
        assert_eq!(
            normalize("localhost:5173"),
            Some("http://localhost:5173".to_string())
        );
        assert_eq!(
            normalize("127.0.0.1:3000/x"),
            Some("http://127.0.0.1:3000/x".to_string())
        );
    }

    #[test]
    fn anything_else_is_a_search() {
        assert_eq!(
            normalize("rust gpui"),
            Some("https://duckduckgo.com/?q=rust%20gpui".to_string())
        );
        assert_eq!(
            normalize("gpui"),
            Some("https://duckduckgo.com/?q=gpui".to_string())
        );
    }

    #[test]
    fn blank_loads_nothing() {
        assert_eq!(normalize("   "), None);
    }
}
