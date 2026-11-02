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
//!
//! Two buttons after it annotate the page (see [`super::browser_notes`]): one
//! points at an element, the other opens the tray of notes, whose *Send* pastes
//! them to the clipboard.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, MouseButton, SharedString, Stateful, Window, div,
    prelude::*, px,
};
use gpui_wry::WebView;
use serde_json::json;
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};

use super::browser_control::{Access, Ask, Opener, Pending};
use super::browser_data;
use super::browser_inspect::{self, INIT_SCRIPT, Inspect};
use super::browser_notes::{self, NoteEvent, Notes, Pick};
use super::browser_shield::Shield;
use super::style::{self, FillExt, RadiusExt};
use super::toast::ToastKind;
use super::{PilusView, icon};
use crate::ctl::{PageOp, Replier, Reply};

/// The most page tabs open at once. Each is a native webview with a
/// WebContent process of its own, which costs far more than a terminal pane.
const MAX_PAGES: usize = 8;

/// What the webview's handlers report.
enum Event {
    /// The page at this address started or finished loading.
    Loaded(String),
    /// The page finished loading, so a script sent now finds its document.
    Finished,
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
    /// What `pilus ctl` has seen of the page, and who waits on it. The
    /// webview's handlers hold the other end.
    inspect: Arc<Inspect>,
    /// Who opened the tab and who may use it now (see `browser_control`).
    access: Access,
    /// The native cover that keeps the person out while an agent holds the
    /// tab (see `browser_shield`).
    shield: Shield,
    /// An agent's request for this tab, waiting on the person.
    ask: Option<Pending>,
    /// The person's notes on the page.
    notes: Notes,
    /// The picker is on: the next click in the page chooses an element.
    picking: bool,
    /// The element just chosen, waiting for its note.
    draft: Option<Pick>,
    /// The tray of notes is open.
    tray: bool,
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
            inspect: Arc::default(),
            access: Access::person(),
            shield: Shield::default(),
            ask: None,
            notes: Notes::default(),
            picking: false,
            draft: None,
            tray: false,
        }
    }

    /// A page that will load `url` once it is on screen.
    fn loading(url: String) -> Self {
        Self {
            pending: Some(url),
            ..Self::new()
        }
    }

    /// The same, opened by whoever `access` says.
    fn loading_as(url: String, access: Access) -> Self {
        Self {
            access,
            ..Self::loading(url)
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
        // An emptied tab is the person's again, and nothing covers it.
        self.access = Access::person();
        self.shield.remove();
        // Dropping the request tells the agent that asked.
        self.ask = None;
        self.notes = Notes::default();
        self.picking = false;
        self.draft = None;
        self.tray = false;
        self.view = None;
        self.pending = None;
        self.url.clear();
        self.title.clear();
        self.corner = None;
        if let Ok(mut queue) = self.events.lock() {
            queue.clear();
        }
        // Anyone still waiting on this page is told it is gone.
        self.inspect.reset();
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

/// What the strip draws for one tab.
struct TabLook {
    ix: usize,
    label: String,
    /// An agent opened it: dashed border.
    agent_opened: bool,
    /// An agent may use it, or is asking: the chip.
    chip: bool,
    /// An agent is working in it, or waits on a yes: lit.
    busy: bool,
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

    /// How many tabs are an agent's to use (it opened them, or was lent them),
    /// and how many of those it holds now.
    fn agent_tabs(&self) -> (usize, usize) {
        let agent = self.pages.iter().filter(|page| page.access.agent_usable());
        let (mut total, mut held) = (0, 0);
        for page in agent {
            total += 1;
            held += usize::from(page.access.held());
        }
        (total, held)
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

/// The address a script may load for `typed`: the address bar's own fix-ups,
/// but only http and https, since the bar lets a person type `file:` and a
/// script should not (see security.md).
fn web_url(typed: &str) -> Result<String, String> {
    let url = normalize(typed).ok_or("the URL is empty")?;
    let web = url.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    });
    if web {
        Ok(url)
    } else {
        Err(format!(
            "the Browser opens only http and https URLs, not {typed:?}"
        ))
    }
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
        self.browser_visible()
            && self.right.focused
            && (self.browser_panel.editing || self.browser_panel.page().draft.is_some())
    }

    /// Whether the note field, not the address bar, holds the keyboard.
    pub(super) fn browser_note_focused(&self) -> bool {
        self.browser_focused() && !self.browser_panel.editing
    }

    /// Whether the page may be drawn: the tab is open and not sliding, and no
    /// overlay of ours is up, since the webview would paint over it.
    ///
    /// A handle drag does not hide it: the page's native frame is re-set from
    /// GPUI's layout on every paint, so it follows the column's edge, and the
    /// drag keeps receiving events because AppKit sends them to the view that
    /// took the mouse-down. Only a tween hides it, as the native view cannot
    /// follow a slide that is still moving the column out from under it.
    fn browser_on_screen(&self) -> bool {
        self.browser_visible()
            && !self.right.tweening()
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
        // The address is part of the tab an agent is working in.
        if self.browser_panel.page().access.held() {
            return;
        }
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
        if self.browser_note_focused() {
            match event.keystroke.key.as_str() {
                "escape" => self.browser_note_cancel(cx),
                "enter" => self.browser_note_save(cx),
                _ => {}
            }
            return;
        }
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

    /// Whether the selected tab has a page, or one on its way.
    pub(super) fn browser_has_content(&self) -> bool {
        let page = self.browser_panel.page();
        page.view.is_some() || page.pending.is_some()
    }

    /// How many tabs the Browser has, the selected one's address, and how
    /// many tabs agents opened and hold, for `pilus ctl state`.
    pub(super) fn browser_summary(&self) -> (usize, String, usize, usize) {
        let page = self.browser_panel.page();
        let url = if page.url.is_empty() {
            page.pending.clone().unwrap_or_default()
        } else {
            page.url.clone()
        };
        let (agent, held) = self.browser_panel.agent_tabs();
        (self.browser_panel.pages.len(), url, agent, held)
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

    // ---- notes on the page -------------------------------------------------
    //
    // The picker runs in the page; the note is typed in the window's own field
    // (see `browser_notes`).

    /// Put the picker, and a badge on each note made on this address, into
    /// page `ix`. A page that has just loaded has lost both.
    fn browser_notes_sync(&self, ix: usize, cx: &gpui::App) {
        let Some(page) = self.browser_panel.pages.get(ix) else {
            return;
        };
        let Some(view) = &page.view else {
            return;
        };
        if !page.picking && page.notes.is_empty() {
            return;
        }
        let webview = view.read(cx).raw();
        let _ = webview.evaluate_script(browser_notes::SCRIPT);
        let _ = webview.evaluate_script(&page.notes.marks_script(&page.url));
        if page.picking {
            let _ = webview.evaluate_script("window.__pilusNotes&&window.__pilusNotes.start()");
        }
    }

    /// Switch the picker off in page `ix`.
    fn browser_pick_stop(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(page) = self.browser_panel.pages.get_mut(ix) else {
            return;
        };
        page.picking = false;
        if let Some(view) = &page.view {
            let _ = view
                .read(cx)
                .raw()
                .evaluate_script("window.__pilusNotes&&window.__pilusNotes.stop()");
        }
        cx.notify();
    }

    /// The square beside the address bar: start picking an element, or stop.
    fn browser_toggle_pick(&mut self, cx: &mut Context<Self>) {
        let ix = self.browser_panel.active;
        let page = self.browser_panel.page();
        if page.picking || page.draft.is_some() {
            self.browser_pick_stop(ix, cx);
            self.browser_note_cancel(cx);
            return;
        }
        if page.view.is_none() {
            self.toast(ToastKind::Info, "Open a page to annotate first");
            cx.notify();
            return;
        }
        if page.access.held() {
            self.toast(ToastKind::Info, "An agent is working in this tab");
            cx.notify();
            return;
        }
        self.browser_panel.page_mut().picking = true;
        self.browser_notes_sync(ix, cx);
        cx.notify();
    }

    /// The bubble beside the square: show or hide the tray of notes.
    fn browser_toggle_tray(&mut self, cx: &mut Context<Self>) {
        let page = self.browser_panel.page_mut();
        page.tray = !page.tray;
        cx.notify();
    }

    /// The picker reported something in page `ix`.
    fn browser_note_event(&mut self, ix: usize, event: NoteEvent, cx: &mut Context<Self>) {
        let active = ix == self.browser_panel.active;
        let Some(page) = self.browser_panel.pages.get_mut(ix) else {
            return;
        };
        // Only the picker we switched on may choose; a page that sends a pick
        // by itself is ignored.
        if !page.picking {
            return;
        }
        page.picking = false;
        let NoteEvent::Picked(pick) = event else {
            return;
        };
        page.draft = Some(pick);
        page.tray = true;
        if active {
            self.browser_note_input.update(cx, |input, cx| input.clear(cx));
            self.browser_focus_note(cx);
        }
    }

    /// Give the note field the keyboard.
    pub(super) fn browser_focus_note(&mut self, cx: &mut Context<Self>) {
        if self.browser_panel.page().draft.is_none() {
            return;
        }
        self.browser_panel.editing = false;
        self.right.focused = true;
        if let Some(view) = &self.browser_panel.page().view {
            // As for the address bar: the page owns the first responder after a
            // click in it.
            let _ = view.read(cx).raw().focus_parent();
        }
        cx.notify();
    }

    /// Save what the note field holds as a note on the chosen element.
    fn browser_note_save(&mut self, cx: &mut Context<Self>) {
        let typed = self.browser_note_input.read(cx).value();
        let ix = self.browser_panel.active;
        let page = self.browser_panel.page_mut();
        let Some(pick) = page.draft.take() else {
            return;
        };
        if typed.trim().is_empty() {
            // Nothing to say yet: keep waiting for it.
            page.draft = Some(pick);
            return;
        }
        let url = pick.url.clone();
        if !page.notes.add(pick, &typed) {
            self.toast(
                ToastKind::Info,
                format!(
                    "A tab keeps at most {} notes. Send or clear some first.",
                    browser_notes::MAX_NOTES
                ),
            );
        } else if let Some(view) = &self.browser_panel.pages[ix].view {
            let webview = view.read(cx).raw();
            let _ = webview.evaluate_script(browser_notes::SCRIPT);
            let _ = webview
                .evaluate_script(&self.browser_panel.pages[ix].notes.marks_script(&url));
        }
        self.browser_note_input.update(cx, |input, cx| input.clear(cx));
        self.browser_panel.editing = false;
        self.right.focused = false;
        cx.notify();
    }

    /// Drop the element that was chosen without a note.
    fn browser_note_cancel(&mut self, cx: &mut Context<Self>) {
        self.browser_panel.page_mut().draft = None;
        self.browser_note_input.update(cx, |input, cx| input.clear(cx));
        if !self.browser_panel.editing {
            self.right.focused = false;
        }
        cx.notify();
    }

    /// Take note `n` off the page.
    fn browser_note_remove(&mut self, n: u32, cx: &mut Context<Self>) {
        let ix = self.browser_panel.active;
        self.browser_panel.page_mut().notes.remove(n);
        self.browser_notes_sync_marks(ix, cx);
        cx.notify();
    }

    /// Redraw page `ix`'s badges after its notes changed.
    fn browser_notes_sync_marks(&self, ix: usize, cx: &gpui::App) {
        let page = &self.browser_panel.pages[ix];
        if let Some(view) = &page.view {
            let _ = view
                .read(cx)
                .raw()
                .evaluate_script(&page.notes.marks_script(&page.url));
        }
    }

    /// Forget every note on the selected tab.
    fn browser_notes_clear(&mut self, cx: &mut Context<Self>) {
        let ix = self.browser_panel.active;
        self.browser_panel.page_mut().notes.clear();
        self.browser_notes_sync_marks(ix, cx);
        cx.notify();
    }

    /// Copy the selected tab's notes to the clipboard, for the person to paste
    /// where they choose. The notes stay, so a copy loses nothing; Clear is
    /// the way to drop them.
    fn browser_notes_copy(&mut self, cx: &mut Context<Self>) {
        let page = self.browser_panel.page();
        if page.notes.is_empty() {
            return;
        }
        let count = page.notes.len();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(page.notes.message()));
        self.toast(
            ToastKind::Info,
            match count {
                1 => "Copied 1 note".to_string(),
                many => format!("Copied {many} notes"),
            },
        );
        cx.notify();
    }

    fn browser_reload(&self, cx: &Context<Self>) {
        if let Some(view) = &self.browser_panel.page().view {
            let _ = view.read(cx).raw().reload();
        }
    }

    /// Drain what the pages reported; true when anything on screen changed.
    pub(super) fn browser_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        // The person took a held tab back: with Take over on its shield, or,
        // where the shield is not up, with a real click or keypress in the
        // page, which its script can tell from the agent's. Every flag is
        // cleared each tick, so input from before the hold never counts.
        let mut back = Vec::new();
        for (ix, page) in self.browser_panel.pages.iter().enumerate() {
            let pressed = page.shield.taken();
            let typed = page.inspect.take_user_input();
            if (pressed || typed) && page.access.held() {
                back.push(ix);
            }
        }
        for ix in back {
            self.browser_take_over(ix, cx);
            changed = true;
        }
        // A click in the page moves the OS first responder without GPUI
        // hearing of it, so the address bar learns it lost the keyboard here.
        if self.browser_focused() && self.browser_page_focused(cx) {
            self.browser_end_edit(cx);
            changed = true;
        }
        let active = self.browser_panel.active;
        let mut address_changed = false;
        // New windows the pages asked for, with the tab that asked.
        let mut opened = Vec::new();
        // Pages that finished loading, which the picker and the badges need to
        // be put back into, and what the picker reported.
        let mut finished = Vec::new();
        let mut picked = Vec::new();
        for (ix, page) in self.browser_panel.pages.iter_mut().enumerate() {
            for event in page.inspect.take_note_events() {
                picked.push((ix, event));
            }
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
                    Event::Finished => finished.push(ix),
                    Event::Titled(title) => {
                        changed |= title != page.title;
                        page.title = title;
                    }
                    Event::Opened(url) => opened.push((ix, url)),
                }
            }
        }
        for ix in finished {
            self.browser_notes_sync(ix, cx);
        }
        for (ix, event) in picked {
            changed = true;
            self.browser_note_event(ix, event, cx);
        }
        // An agent that takes a tab ends the person's picking in it: the cover
        // would swallow the click anyway.
        for ix in 0..self.browser_panel.pages.len() {
            if self.browser_panel.pages[ix].access.held() && self.browser_panel.pages[ix].picking {
                self.browser_pick_stop(ix, cx);
                changed = true;
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
            // Beside the opener, and in front only when the opener was. It
            // belongs to whoever was working the opener.
            let at = from + 1;
            let access = self.browser_panel.pages[from].access.child();
            self.browser_panel
                .pages
                .insert(at, Page::loading_as(url, access));
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
            // The cover goes over a held page while it is on screen, and is
            // taken away when the page is hidden or let go.
            page.shield
                .sync(view.read(cx).raw(), shown && page.access.held());
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
        let inspect = self.browser_panel.page().inspect.clone();
        let (on_load, on_title, on_message) = (inspect.clone(), inspect.clone(), inspect);
        let access = self.browser_panel.page().access;
        let mut builder = WebViewBuilder::new().with_url(url);
        // The agent's script goes only into a tab an agent may use. A tab the
        // person keeps runs none of it; lending one injects it then.
        if access.agent_usable() {
            builder = builder.with_initialization_script(INIT_SCRIPT);
        }
        // A tab an agent opened keeps no one's sign-ins and leaves none behind:
        // it can load any address and be driven, and must not be a way into the
        // person's accounts. A tab the person lends keeps its own, since they
        // said yes to that tab.
        let built = builder
            .with_incognito(access.opener() == Opener::Agent)
            .with_ipc_handler(move |request| on_message.on_message(request.body()))
            .with_on_page_load_handler(move |event: PageLoadEvent, url| {
                match event {
                    PageLoadEvent::Started => on_load.load_started(),
                    PageLoadEvent::Finished => on_load.load_finished(&url),
                }
                let done = matches!(event, PageLoadEvent::Finished);
                push(&loaded, Event::Loaded(url));
                if done {
                    push(&loaded, Event::Finished);
                }
            })
            .with_document_title_changed_handler(move |title| {
                on_title.set_title(&title);
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

// `pilus ctl browser ...`. A method returns the answer when it has one, and
// `None` when it kept the `Replier` for something that finishes later: a page
// loading, or the page answering a script. See `browser_inspect`.
impl PilusView {
    /// `browser release`: the agent lets go of its tabs, so the person can use
    /// them. With a `tab` that one, else every tab the agent holds.
    pub(super) fn browser_ctl_release(
        &mut self,
        tab: Option<u16>,
        cx: &mut Context<Self>,
    ) -> Reply {
        let tabs: Vec<usize> = match tab {
            Some(_) => match self.ctl_page_index(tab) {
                Ok(ix) if self.browser_panel.pages[ix].access.agent_usable() => {
                    vec![ix]
                }
                Ok(ix) => {
                    return Reply::err(format!(
                        "Browser tab {} is the person's and was not lent to you",
                        ix + 1
                    ));
                }
                Err(message) => return Reply::err(message),
            },
            None => (0..self.browser_panel.pages.len()).collect(),
        };
        let mut released = 0;
        for ix in tabs {
            let page = &mut self.browser_panel.pages[ix];
            if page.access.release() {
                page.shield.remove();
                released += 1;
            }
        }
        cx.notify();
        Reply::data(json!({ "released": released }))
    }

    /// The person takes tab `ix` back: the hold ends, the shield comes off and
    /// whatever the agent had in flight is told why.
    pub(super) fn browser_take_over(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(page) = self.browser_panel.pages.get_mut(ix) else {
            return;
        };
        if !page.access.take_over(Instant::now()) {
            return;
        }
        page.shield.remove();
        page.inspect.fail_calls("the person took over this tab");
        self.toast(ToastKind::Info, "You have the tab");
        cx.notify();
    }

    /// `browser request`: an agent asks for a tab the person opened. Answers
    /// now when it may use it already or was turned away, else the prompt goes
    /// up and the answer comes when the person decides.
    pub(super) fn browser_ctl_request(
        &mut self,
        tab: u16,
        reason: Option<&str>,
        replier: &Replier,
        cx: &mut Context<Self>,
    ) -> Option<Reply> {
        let ix = match self.ctl_page_index(Some(tab)) {
            Ok(ix) => ix,
            Err(message) => return Some(Reply::err(message)),
        };
        let page = &mut self.browser_panel.pages[ix];
        match page.access.ask(Instant::now()) {
            Ask::Already => Some(Reply::data(json!({ "granted": true, "tab": ix + 1 }))),
            Ask::Refused(why) => Some(Reply::err(why)),
            Ask::Prompt if page.view.is_none() && page.pending.is_none() => {
                Some(Reply::err(format!(
                    "Browser tab {} is empty, so there is nothing to lend; open your own with \
                 `pilus ctl browser new URL`",
                    ix + 1
                )))
            }
            Ask::Prompt => {
                match &mut page.ask {
                    Some(pending) => pending.join(replier),
                    None => page.ask = Some(Pending::new(reason, replier)),
                }
                cx.notify();
                None
            }
        }
    }

    /// The person answers the request for tab `ix`: a yes lends it and the
    /// agent holds it from now on, a no turns the agent away for a minute.
    fn browser_answer_request(&mut self, ix: usize, allow: bool, cx: &mut Context<Self>) {
        let Some(page) = self.browser_panel.pages.get_mut(ix) else {
            return;
        };
        let Some(pending) = page.ask.take() else {
            return;
        };
        let reply = if allow {
            page.access.allow();
            // The page was built without the agent's script; give it one now.
            // It sees only what happens from here on.
            if let Some(view) = &page.view {
                let _ = view.read(cx).raw().evaluate_script(INIT_SCRIPT);
            }
            Reply::data(json!({ "granted": true, "tab": ix + 1 }))
        } else {
            Reply::err(page.access.deny(Instant::now()))
        };
        pending.answer(&reply);
        cx.notify();
    }

    /// The index of the tab the agent names, else of the selected one, if an
    /// agent may use it: one the person opened is theirs. This does not take
    /// the hold, so it also serves `close` and `release`.
    fn ctl_agent_index(&self, tab: Option<u16>) -> Result<usize, String> {
        let ix = self.ctl_page_index(tab)?;
        self.browser_panel.pages[ix]
            .access
            .permits(Instant::now())
            .map_err(|why| format!("Browser tab {}: {why}", ix + 1))?;
        Ok(ix)
    }

    /// The same, and the agent starts working in it: the hold is taken, which
    /// puts the shield over the page on the next frame.
    fn ctl_agent_hold(
        &mut self,
        tab: Option<u16>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let ix = self.ctl_agent_index(tab)?;
        self.browser_panel.pages[ix]
            .access
            .acquire(Instant::now())?;
        cx.notify();
        Ok(ix)
    }

    /// `browser open` and `browser new URL`: load `url` in a tab of the
    /// agent's, and show it. With `fresh` that is always a new tab. Otherwise
    /// it is the tab named, else the selected one when the agent opened it,
    /// else a new one. A tab the person opened is never reused.
    pub(super) fn browser_ctl_open(
        &mut self,
        typed: &str,
        tab: Option<u16>,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let url = web_url(typed)?;
        let now = Instant::now();
        let reuse = if fresh {
            None
        } else if tab.is_some() {
            Some(self.ctl_agent_index(tab)?)
        } else {
            self.ctl_agent_index(None).ok()
        };
        match reuse {
            Some(ix) => {
                self.browser_panel.pages[ix].access.acquire(now)?;
                self.browser_select_tab(ix, cx);
                self.browser_panel.pages[ix].pending = Some(url.clone());
            }
            None => {
                if self.browser_panel.full() {
                    return Err(format!(
                        "the Browser holds {MAX_PAGES} tabs; close one first"
                    ));
                }
                let mut access = Access::agent();
                access.acquire(now)?;
                self.browser_panel
                    .pages
                    .push(Page::loading_as(url.clone(), access));
                self.browser_panel.active = self.browser_panel.pages.len() - 1;
            }
        }
        self.browser_input
            .update(cx, |input, cx| input.set_value(url, cx));
        self.browser_panel.editing = false;
        self.right.focused = false;
        cx.notify();
        Ok(())
    }

    /// The index of the 1-based `tab`, else of the selected one.
    fn ctl_page_index(&self, tab: Option<u16>) -> Result<usize, String> {
        let Some(tab) = tab else {
            return Ok(self
                .browser_panel
                .active
                .min(self.browser_panel.pages.len() - 1));
        };
        let ix = usize::from(tab).saturating_sub(1);
        if ix < self.browser_panel.pages.len() {
            Ok(ix)
        } else {
            Err(format!(
                "there is no Browser tab {tab}; there are {}",
                self.browser_panel.pages.len()
            ))
        }
    }

    /// How tab `ix` reads in an answer. A page opened by a link stays
    /// `pending` until it is selected, since only the selected page builds
    /// its webview.
    fn ctl_describe(&self, ix: usize) -> serde_json::Value {
        let page = &self.browser_panel.pages[ix];
        let url = if page.url.is_empty() {
            page.pending.clone().unwrap_or_default()
        } else {
            page.url.clone()
        };
        let state = if page.view.is_some() {
            if page.inspect.loading() {
                "loading"
            } else {
                "ready"
            }
        } else if page.pending.is_some() {
            "pending"
        } else {
            "empty"
        };
        let mut title = page.inspect.title();
        if title.is_empty() {
            title = page.title.clone();
        }
        json!({
            "tab": ix + 1,
            "active": ix == self.browser_panel.active,
            "url": url,
            "title": title,
            "state": state,
            "inspectable": page.view.is_some() && page.access.agent_usable(),
            "opened_by": page.access.opener().label(),
            "held": page.access.held(),
            "lent": page.access.lent(),
            "asking": page.ask.is_some(),
        })
    }

    /// `browser tabs`.
    pub(super) fn browser_ctl_tabs(&self) -> Reply {
        let tabs: Vec<_> = (0..self.browser_panel.pages.len())
            .map(|ix| self.ctl_describe(ix))
            .collect();
        Reply::data(json!({ "tabs": tabs }))
    }

    /// Answer `replier` when the selected tab has loaded, or now with how it
    /// stands when the caller would rather not wait.
    pub(super) fn browser_ctl_loaded(&self, wait: bool, replier: &Replier) -> Option<Reply> {
        let ix = self
            .browser_panel
            .active
            .min(self.browser_panel.pages.len() - 1);
        if wait {
            self.browser_panel.pages[ix]
                .inspect
                .answer_on_load(replier.clone(), Some(ix + 1));
            None
        } else {
            Some(Reply::data(self.ctl_describe(ix)))
        }
    }

    /// `browser new` without an address: an empty tab of the agent's.
    pub(super) fn browser_ctl_blank(&mut self, cx: &mut Context<Self>) -> Reply {
        if self.browser_panel.full() {
            return Reply::err(format!(
                "the Browser holds {MAX_PAGES} tabs; close one first"
            ));
        }
        let mut page = Page::new();
        page.access = Access::agent();
        if let Err(message) = page.access.acquire(Instant::now()) {
            return Reply::err(message);
        }
        self.browser_panel.pages.push(page);
        self.browser_panel.active = self.browser_panel.pages.len() - 1;
        self.browser_panel.editing = false;
        self.right.focused = false;
        self.browser_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
        Reply::data(self.ctl_describe(self.browser_panel.active))
    }

    /// `browser close`.
    pub(super) fn browser_ctl_close(&mut self, tab: Option<u16>, cx: &mut Context<Self>) -> Reply {
        match self.ctl_agent_index(tab) {
            Ok(ix) => {
                self.browser_close_tab(ix, cx);
                self.browser_ctl_tabs()
            }
            Err(message) => Reply::err(message),
        }
    }

    /// `browser focus`: select the tab. One that has yet to build its webview
    /// does so now, and the answer waits for it to load.
    pub(super) fn browser_ctl_focus(
        &mut self,
        tab: u16,
        replier: &Replier,
        cx: &mut Context<Self>,
    ) -> Option<Reply> {
        let ix = match self.ctl_agent_hold(Some(tab), cx) {
            Ok(ix) => ix,
            Err(message) => return Some(Reply::err(message)),
        };
        self.browser_select_tab(ix, cx);
        let page = &self.browser_panel.pages[ix];
        if page.pending.is_some() || (page.view.is_some() && page.inspect.loading()) {
            page.inspect.answer_on_load(replier.clone(), Some(ix + 1));
            return None;
        }
        Some(Reply::data(self.ctl_describe(ix)))
    }

    /// `browser snapshot`, `click`, `eval` and the rest.
    pub(super) fn browser_ctl_page(
        &mut self,
        tab: Option<u16>,
        op: &PageOp,
        replier: &Replier,
        cx: &mut Context<Self>,
    ) -> Option<Reply> {
        let ix = match self.ctl_agent_hold(tab, cx) {
            Ok(ix) => ix,
            Err(message) => return Some(Reply::err(message)),
        };
        let page = &self.browser_panel.pages[ix];
        // The logs are the app's own record, kept in the window.
        if let Some(reply) = page.inspect.read(op) {
            return Some(reply);
        }
        let Some(view) = &page.view else {
            let state = if page.pending.is_some() {
                "still opening"
            } else {
                "empty"
            };
            return Some(Reply::err(format!(
                "Browser tab {} is {state}; `browser focus {}` builds it",
                ix + 1,
                ix + 1
            )));
        };
        let webview = view.read(cx).raw();
        match op {
            // The page's own buttons, which are open to any page.
            PageOp::Back | PageOp::Forward => {
                let script = if matches!(op, PageOp::Back) {
                    "history.back()"
                } else {
                    "history.forward()"
                };
                let _ = webview.evaluate_script(script);
                Some(Reply::data(self.ctl_describe(ix)))
            }
            PageOp::Reload => {
                page.inspect.answer_on_load(replier.clone(), Some(ix + 1));
                let _ = webview.reload();
                None
            }
            _ => {
                let script_id = page.inspect.begin_call(replier.clone());
                let Some(script) = browser_inspect::call_script(script_id, op) else {
                    page.inspect
                        .fail_call(script_id, "that command has no page script");
                    return None;
                };
                let inspect = page.inspect.clone();
                let started = webview.evaluate_script_with_callback(&script, move |result| {
                    // The script ends in a string, so no result means it never
                    // ran: a syntax error, or a page without the helper.
                    if result.is_empty() {
                        inspect.fail_call(
                            script_id,
                            "the page did not run the script: it has a syntax error (statements need \
                             --body), or the page has not loaded yet",
                        );
                    }
                });
                if let Err(error) = started {
                    page.inspect
                        .fail_call(script_id, &format!("could not reach the page: {error}"));
                }
                None
            }
        }
    }
}

// Rendering.
impl PilusView {
    pub(super) fn browser_page(&mut self, cx: &mut Context<Self>) -> Div {
        let banner = self.browser_request_banner(cx);
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .children(banner)
            .child(self.browser_tab_strip(cx))
            .child(self.browser_header(cx))
            .children(self.browser_notes_tray(cx));
        match self.browser_panel.page().view.clone() {
            // While the column slides the webview is hidden, so the body is
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

    /// What an agent has asked for, over the tab strip: which tab, why, and
    /// the way to answer. It shows only while a request waits. It is drawn by
    /// us, not the page, so a page cannot imitate it.
    fn browser_request_banner(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let (ix, page) = self
            .browser_panel
            .pages
            .iter()
            .enumerate()
            .find(|(_, page)| page.ask.is_some())?;
        let reason = page
            .ask
            .as_ref()
            .and_then(Pending::reason)
            .map(str::to_string);
        let title = format!("An agent wants to use \u{201c}{}\u{201d}", page.label());
        let mut text = div().flex().flex_col().flex_1().min_w(px(0.0)).child(
            div()
                .truncate()
                .text_size(px(fonts.body))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.fg)
                .child(title),
        );
        if let Some(reason) = reason {
            text = text.child(
                div()
                    .truncate()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(reason),
            );
        }
        let buttons = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .child(
                style::small_button(
                    "pilus-browser-deny",
                    "Deny",
                    style::ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.browser_answer_request(ix, false, cx);
                })),
            )
            .child(
                style::small_button(
                    "pilus-browser-allow",
                    "Allow",
                    style::ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.browser_answer_request(ix, true, cx);
                })),
            );
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .bg(theme.panel_dim)
                .border_b_1()
                .border_color(theme.accent)
                .child(text)
                .child(buttons),
        )
    }

    /// The bordered area the page sits in, inset by a small gutter so the page
    /// reads as content in a frame. Only the bottom-right corner is round, to
    /// meet the window's own; the page's layer is clipped to match (see
    /// [`round_page_corner`]).
    fn browser_frame(&self, page: Option<Entity<WebView>>) -> Div {
        // A page an agent opened is framed with a dashed line, as its tab is.
        let dashed = self.browser_panel.page().access.opener() == Opener::Agent;
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
                    .when(dashed, |frame| frame.border_dashed())
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
        let tabs: Vec<TabLook> = self
            .browser_panel
            .pages
            .iter()
            .enumerate()
            .map(|(ix, page)| TabLook {
                ix,
                label: page.label(),
                agent_opened: page.access.opener() == Opener::Agent,
                chip: page.access.agent_usable() || page.ask.is_some(),
                busy: page.access.held() || page.ask.is_some(),
            })
            .collect();
        let tabs = tabs.into_iter().map(|look| {
            let TabLook {
                ix,
                label,
                agent_opened,
                chip,
                busy,
            } = look;
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
            let title = div().flex_1().min_w(px(0.0)).truncate().child(label);
            // A tab an agent may use wears a chip, lit while it works there or
            // waits on a yes, so the person sees whose each tab is without
            // opening it.
            let title = if chip {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(icon(
                        include_bytes!("../../assets/icons/cpu.svg"),
                        fonts.label,
                        if busy { theme.accent } else { theme.muted },
                    ))
                    .child(title)
            } else {
                title
            };
            style::tab(
                ("pilus-browser-tab", ix),
                title,
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
            // A tab an agent opened has a dashed border. Every tab keeps a
            // 1px one, clear when it is not dashed, so none shifts.
            .border_1()
            .when(agent_opened, |tab| tab.border_dashed())
            .border_color(if agent_opened {
                if busy { theme.accent } else { theme.muted }
            } else {
                gpui::transparent_black().into()
            })
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
        // The buttons act on the page, so they wait while an agent holds it.
        let loaded = self.browser_panel.has_page() && !self.browser_panel.page().access.held();
        let (picking, tray, notes) = {
            let page = self.browser_panel.page();
            (
                page.picking || page.draft.is_some(),
                page.tray,
                page.notes.len(),
            )
        };
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
            .child(
                self.diff_icon_button(
                    "pilus-browser-pick",
                    include_bytes!("../../assets/icons/square-mouse-pointer.svg"),
                    if picking {
                        "Stop picking (Esc)".to_string()
                    } else {
                        "Point at an element to leave a note".to_string()
                    },
                    picking,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.browser_toggle_pick(cx))),
            )
            .child(
                self.diff_icon_button(
                    "pilus-browser-notes",
                    include_bytes!("../../assets/icons/message-square.svg"),
                    "Notes for the agent".to_string(),
                    tray || notes > 0,
                    false,
                )
                .when(notes > 0, |button| {
                    button.child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.accent)
                            .child(notes.to_string()),
                    )
                })
                .on_click(cx.listener(|this, _, _, cx| this.browser_toggle_tray(cx))),
            )
    }

    /// What the person has pointed at, under the address bar: the hint while
    /// picking, the field for the note on the chosen element, and the notes
    /// with the way to send them. It is drawn by us, so the page can neither
    /// read a note being written nor imitate the field.
    fn browser_notes_tray(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL);
        let page = self.browser_panel.page();
        if !(page.tray || page.picking || page.draft.is_some()) {
            return None;
        }
        let picking = page.picking;
        let draft = page.draft.as_ref().map(|pick| pick.label.clone());
        let notes: Vec<(u32, String, String)> = page
            .notes
            .items()
            .iter()
            .map(|note| (note.n, note.pick.label.clone(), note.text.clone()))
            .collect();
        let muted = |text: String| {
            div()
                .truncate()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(text)
        };
        let mut tray = div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border);

        if picking {
            tray = tray.child(muted(
                "Click an element on the page. \u{2191} picks its container, Esc stops."
                    .to_string(),
            ));
        }
        if let Some(label) = draft {
            self.browser_note_input.update(cx, |input, _| {
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(fonts.body),
                );
            });
            let field = style::bare_field(
                "pilus-browser-note-field",
                self.browser_note_focused(),
                theme,
                height,
                self.browser_note_input.clone(),
            )
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(fonts.body))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.browser_focus_note(cx)),
            );
            tray = tray
                .child(muted(format!("Note on {label}")))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(field)
                        .child(
                            style::small_button(
                                "pilus-browser-note-cancel",
                                "Cancel",
                                style::ButtonKind::Ghost,
                                theme,
                                fonts.secondary,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.browser_note_cancel(cx))),
                        )
                        .child(
                            style::small_button(
                                "pilus-browser-note-add",
                                "Add",
                                style::ButtonKind::Primary,
                                theme,
                                fonts.secondary,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.browser_note_save(cx))),
                        ),
                );
        }
        if notes.is_empty() {
            if !picking && self.browser_panel.page().draft.is_none() {
                tray = tray.child(muted("No notes yet".to_string()));
            }
            return Some(tray);
        }
        let rows = notes.into_iter().map(|(n, label, text)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .min_w(px(fonts.body + 4.0))
                        .text_size(px(fonts.label))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.accent)
                        .child(format!("{n}")),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(fonts.body))
                                .text_color(theme.fg)
                                .child(text),
                        )
                        .child(muted(label)),
                )
                .child(
                    div()
                        .id(("pilus-browser-note-remove", n as usize))
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .size(px(fonts.body + 4.0))
                        .radius_sm()
                        .cursor_pointer()
                        .hover_fill(theme.panel_dim, style::RADIUS_SM)
                        .child(icon(
                            include_bytes!("../../assets/icons/x.svg"),
                            fonts.label,
                            theme.muted,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.browser_note_remove(n, cx);
                        })),
                )
        });
        tray = tray.child(
            div()
                .id("pilus-browser-note-list")
                .flex()
                .flex_col()
                .gap_1()
                .max_h(px(180.0))
                .overflow_y_scroll()
                .children(rows),
        );
        Some(
            tray.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(
                        style::small_button(
                            "pilus-browser-notes-clear",
                            "Clear",
                            style::ButtonKind::Ghost,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.browser_notes_clear(cx))),
                    )
                    .child(
                        style::small_button(
                            "pilus-browser-notes-copy",
                            "Copy notes",
                            style::ButtonKind::Primary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.browser_notes_copy(cx))),
                    ),
            ),
        )
    }

    /// The globe, lit while the Browser is open.
    fn browser_glyph(&self, lit: bool) -> impl IntoElement {
        let theme = self.theme;
        // Accent while an agent works in a tab, wherever the sidebar is.
        let color = if self.browser_panel.agent_tabs().1 > 0 {
            theme.accent
        } else if lit {
            theme.panel_fg
        } else {
            theme.muted
        };
        icon(include_bytes!("../../assets/icons/globe.svg"), 14.0, color)
    }

    /// The Browser button beside the Ports button, lit while the tab is open.
    pub(super) fn right_browser_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        // The same dress as the open column's icon buttons, so hover and the
        // lit state read alike whether the column is away or open.
        style::strip_button(
            "pilus-right-browser-toggle",
            self.browser_visible(),
            true,
            theme,
        )
        .occlude()
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
        style::strip_button(
            "pilus-right-sidebar-browser",
            self.right.tab == super::right_sidebar::SidebarTab::Browser,
            true,
            theme,
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
