//! The native Browser tab: a system webview in the right column, with an
//! address bar over it.
//!
//! The page is a `wry` webview that `gpui-wry` hosts as a native child of the
//! window, so it is drawn by the OS, above everything GPUI paints. That shapes
//! the tab: the webview is created on the first address, kept alive while the
//! tab is away, and shown only while nothing GPUI draws could land on it (see
//! [`PilusView::browser_on_screen`]). Page events come back on a queue the
//! webview's handlers fill, which [`PilusView::browser_tick`] drains with the
//! other tabs' background work.
//!
//! The address bar is a [`TextInput`]. It follows the page's address while it
//! is not being edited, and what the user typed becomes the next load.

use std::sync::{Arc, Mutex};

use gpui::{Context, Div, Entity, MouseButton, Stateful, Window, div, prelude::*, px};
use gpui_wry::WebView;
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};

use super::style::{self, FillExt, RadiusExt};
use super::{PilusView, icon};

/// What the webview's handlers report.
enum Event {
    /// The page at this address started or finished loading.
    Loaded(String),
    /// The page asked for a new window; the tab opens it in place instead.
    Opened(String),
}

type Events = Arc<Mutex<Vec<Event>>>;

fn push(events: &Events, event: Event) {
    if let Ok(mut queue) = events.lock() {
        queue.push(event);
    }
}

pub(super) struct BrowserPanel {
    view: Option<Entity<WebView>>,
    events: Events,
    /// An address to load on the next frame, which has the window to build
    /// the webview with.
    pending: Option<String>,
    /// The address bar holds the keyboard.
    pub(super) editing: bool,
    /// The page's address, as the webview last reported it.
    url: String,
}

impl BrowserPanel {
    pub(super) fn new() -> Self {
        Self {
            view: None,
            events: Arc::default(),
            pending: None,
            editing: false,
            url: String::new(),
        }
    }

    /// Close the page and forget it. Dropping the entity frees the native
    /// `WKWebView` once the next frame lets go of the element's handle
    /// (wry's `Drop` looks like it retains the view, but the `Retained` it
    /// gets back is dropped on the spot, so the count nets out). Blanking
    /// first silences the page's media right away instead of a frame later.
    /// The queue is cleared so a late event cannot bring the old address back.
    fn close(&mut self, cx: &gpui::App) {
        if let Some(view) = self.view.take() {
            let _ = view.read(cx).raw().load_url("about:blank");
        }
        self.pending = None;
        self.editing = false;
        self.url.clear();
        if let Ok(mut queue) = self.events.lock() {
            queue.clear();
        }
    }

    /// Whether a page has been opened, so there is a webview to drive.
    pub(super) fn has_page(&self) -> bool {
        self.view.is_some()
    }
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
            && self.context_menu.is_none()
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
        if let Some(view) = &self.browser_panel.view
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
        if let Some(view) = &self.browser_panel.view {
            // The webview owns the OS-level first responder once clicked in;
            // take it back so the address bar's keys reach us.
            let _ = view.read(cx).raw().focus_parent();
        }
        cx.notify();
    }

    fn browser_end_edit(&mut self, cx: &mut Context<Self>) {
        self.browser_panel.editing = false;
        self.right.focused = false;
        let url = self.browser_panel.url.clone();
        self.browser_input
            .update(cx, |input, cx| input.set_value(url, cx));
        cx.notify();
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
        self.browser_panel.pending = Some(url);
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

    /// Stop the page and close the browser, not just hide it: the webview is
    /// destroyed, and the next address builds a fresh one.
    fn browser_stop(&mut self, cx: &mut Context<Self>) {
        self.browser_panel.close(cx);
        self.right.focused = false;
        self.browser_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Run `script` in the page, if there is one.
    fn browser_script(&self, script: &str, cx: &Context<Self>) {
        if let Some(view) = &self.browser_panel.view {
            let _ = view.read(cx).raw().evaluate_script(script);
        }
    }

    fn browser_reload(&self, cx: &Context<Self>) {
        if let Some(view) = &self.browser_panel.view {
            let _ = view.read(cx).raw().reload();
        }
    }

    /// Drain what the page reported; true when the address bar changed.
    pub(super) fn browser_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let events = match self.browser_panel.events.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => return false,
        };
        let mut changed = false;
        // A click in the page moves the OS first responder without GPUI
        // hearing of it, so the address bar learns it lost the keyboard here.
        if self.browser_panel.editing && self.browser_page_focused(cx) {
            self.browser_end_edit(cx);
            changed = true;
        }
        for event in events {
            match event {
                Event::Loaded(url) => {
                    changed |= url != self.browser_panel.url;
                    self.browser_panel.url = url;
                }
                Event::Opened(url) => {
                    self.browser_panel.pending = Some(url);
                    changed = true;
                }
            }
        }
        if changed && !self.browser_panel.editing {
            let url = self.browser_panel.url.clone();
            self.browser_input
                .update(cx, |input, cx| input.set_value(url, cx));
        }
        changed
    }

    /// Bring the native webview in line with the tab: build it on the first
    /// address, load what was asked for, and show it only while it may be seen.
    pub(super) fn browser_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on_screen = self.browser_on_screen();
        if on_screen && let Some(url) = self.browser_panel.pending.take() {
            match self.browser_panel.view.clone() {
                Some(view) => view.update(cx, |view, _| view.load_url(&url)),
                None => self.browser_open(&url, window, cx),
            }
        }
        if let Some(view) = &self.browser_panel.view {
            view.update(cx, |view, _| {
                if on_screen && !view.visible() {
                    view.show();
                } else if !on_screen && view.visible() {
                    view.hide();
                }
            });
        }
    }

    fn browser_open(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = self.browser_panel.events.clone();
        let opened = self.browser_panel.events.clone();
        let built = WebViewBuilder::new()
            .with_url(url)
            .with_on_page_load_handler(move |_: PageLoadEvent, url| {
                push(&loaded, Event::Loaded(url));
            })
            .with_new_window_req_handler(move |url, _| {
                push(&opened, Event::Opened(url));
                NewWindowResponse::Deny
            })
            .build_as_child(window);
        match built {
            Ok(webview) => {
                self.browser_panel.url = url.to_string();
                self.browser_panel.view = Some(cx.new(|cx| WebView::new(webview, window, cx)));
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
            .child(self.browser_header(cx));
        match self.browser_panel.view.clone() {
            // While the column moves the webview is hidden, so the body is
            // left empty rather than showing a stale frame.
            //
            // The native view paints above GPUI and takes the mouse, so it
            // leaves the resize handle's strip bare; filling the column would
            // bury the handle and the column could not be dragged.
            Some(view) if self.browser_on_screen() => page.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .pl(px(if self.right.expanded() {
                        0.0
                    } else {
                        super::right_sidebar::HANDLE_WIDTH
                    }))
                    .child(view),
            ),
            Some(_) => page.child(div().flex_1()),
            None => page.child(
                self.diff_message("Type an address above. Dev servers on localhost open here too."),
            ),
        }
    }

    /// Back, forward and reload beside the address bar.
    fn browser_header(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL);
        let loaded = self.browser_panel.view.is_some();
        self.browser_input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let field = style::search_field(
            "pilus-browser-address",
            self.browser_panel.editing,
            theme,
            height,
            fonts.secondary,
            self.browser_input.clone(),
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
            .child(
                self.diff_icon_button(
                    "pilus-browser-stop",
                    include_bytes!("../../assets/icons/square-fill.svg"),
                    "Stop and close the page".to_string(),
                    loaded,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.browser_stop(cx))),
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
    use super::normalize;

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
