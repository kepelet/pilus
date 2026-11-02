//! The window's half of `pilus ctl`: take a [`Call`] from the control socket
//! and do what a click would.
//!
//! `crate::ctl` parses the request on the listener's thread and sends it down
//! a channel. A task on the app's executor, parked until a call arrives, picks
//! it up here, finds the window and runs it on the GUI thread, so no view is
//! ever touched from the listener. Each verb calls the same method as the
//! button or key that does it by hand.
//!
//! A verb answers in one of two ways. Most return their [`Reply`] at once. One
//! that finishes later (a page loading, a script running in the page, an SSH
//! forward coming up) returns `None` and keeps a clone of the call's
//! [`Replier`] for whoever will know. Nothing on this side waits for it.

use std::path::Path;

use futures::StreamExt;
use gpui::{App, Context, Window};

use super::right_sidebar::SidebarTab;
use super::{PilusView, tab_exists};
use crate::ctl::{self, Replier, Reply, Request, Tab};

/// Start the control socket and serve it for as long as the app runs. A socket
/// that cannot be opened is logged and skipped: the window works without it.
pub(super) fn start(pilus_root: &Path, cx: &mut App) {
    let path = pilus_root.join("pilus.sock");
    let mut calls = match ctl::serve(&path) {
        Ok(calls) => calls,
        Err(error) => {
            tracing::warn!("pilus ctl is unavailable: {error:#}");
            return;
        }
    };
    cx.spawn(async move |cx| {
        while let Some(call) = calls.next().await {
            let replier = call.replier();
            if let Some(reply) = cx.update(|cx| apply(&call.request, &replier, cx)) {
                call.respond(reply);
            }
        }
    })
    .detach();
}

/// Run `request` on the window the person is using. `None` means `replier`
/// was kept to answer later.
fn apply(request: &Request, replier: &Replier, cx: &mut App) -> Option<Reply> {
    if matches!(request, Request::Ping) {
        return Some(Reply::ok());
    }
    // The active window may be Settings or Appearances; the request is for a
    // main window, so fall back to any.
    let handle = cx
        .active_window()
        .and_then(|handle| handle.downcast::<PilusView>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|handle| handle.downcast::<PilusView>())
        });
    let Some(handle) = handle else {
        return Some(Reply::err("no pilus window is open"));
    };
    handle
        .update(cx, |view, window, cx| {
            view.apply_ctl(request, replier, window, cx)
        })
        .unwrap_or_else(|error| Some(Reply::err(format!("the window is busy: {error}"))))
}

fn sidebar_tab(tab: Tab) -> SidebarTab {
    match tab {
        Tab::Changes => SidebarTab::Diff,
        Tab::History => SidebarTab::Git,
        Tab::Files => SidebarTab::Files,
        Tab::Ports => SidebarTab::Ports,
        Tab::Browser => SidebarTab::Browser,
        Tab::Sipil => SidebarTab::Sipil,
    }
}

impl PilusView {
    fn apply_ctl(
        &mut self,
        request: &Request,
        replier: &Replier,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Reply> {
        Some(match request {
            Request::Ping => Reply::ok(),
            Request::State => {
                let (tabs, url, agent_tabs, held) = self.browser_summary();
                ctl::state_reply(
                    self.right.visible,
                    &self.right.tab.label().to_ascii_lowercase(),
                    tabs,
                    &url,
                    agent_tabs,
                    held,
                )
            }
            Request::SidebarOpen { tab } => {
                self.ctl_show_sidebar(tab.map(sidebar_tab), cx);
                Reply::ok()
            }
            Request::SidebarClose => {
                if self.right.visible {
                    self.toggle_right_sidebar(cx);
                }
                Reply::ok()
            }
            Request::SidebarToggle => {
                self.toggle_right_sidebar(cx);
                Reply::ok()
            }
            Request::BrowserOpen { url, wait, tab } => {
                // Load the address first so a refused one does not leave the
                // sidebar open on an empty Browser.
                if let Err(message) = self.browser_ctl_open(url, *tab, false, cx) {
                    return Some(Reply::err(message));
                }
                self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                return self.browser_ctl_loaded(*wait, replier);
            }
            Request::TabFocus { index } => {
                if !tab_exists(&self.tabs, *index) {
                    return Some(Reply::err(format!("there is no tab {index}")));
                }
                self.focus_tab(*index, cx);
                Reply::ok()
            }
            // From before a tab belonged to whoever opened it. Nothing is asked
            // of the person now, so there is nothing to wait for.
            Request::BrowserControl { .. } => Reply::data(serde_json::json!({
                "granted": true,
                "note": "no permission is needed: open a tab with `browser new URL` and use it; \
                         tabs the person opened are theirs",
            })),
            Request::BrowserBorrow { tab, reason } => {
                // Up front, so the prompt is where the person is looking.
                self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                return self.browser_ctl_request(*tab, reason.as_deref(), replier, cx);
            }
            Request::BrowserRelease { tab } => self.browser_ctl_release(*tab, cx),
            Request::BrowserTabs => self.browser_ctl_tabs(),
            Request::BrowserNew {
                url: Some(url),
                wait,
            } => {
                if let Err(message) = self.browser_ctl_open(url, None, true, cx) {
                    return Some(Reply::err(message));
                }
                self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                return self.browser_ctl_loaded(*wait, replier);
            }
            Request::BrowserNew { url: None, .. } => {
                self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                self.browser_ctl_blank(cx)
            }
            Request::BrowserClose { tab } => self.browser_ctl_close(*tab, cx),
            Request::BrowserFocus { tab } => {
                self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                return self.browser_ctl_focus(*tab, replier, cx);
            }
            Request::BrowserViewport { tab, shape } => {
                self.browser_ctl_viewport(*tab, shape.as_deref(), cx)
            }
            Request::BrowserPage { tab, op } => {
                return self.browser_ctl_page(*tab, op, replier, cx);
            }
            Request::PortsList => return self.ports_ctl_list(replier),
            Request::PortsForward { port } => return self.ports_ctl_forward(*port, replier),
            Request::PortsStop { port } => self.ports_ctl_stop(*port),
        })
    }

    /// Open the right sidebar, on `tab` when one is named, as its buttons do.
    fn ctl_show_sidebar(&mut self, tab: Option<SidebarTab>, cx: &mut Context<Self>) {
        match tab {
            Some(SidebarTab::Sipil) => self.show_sipil(cx),
            Some(tab) => {
                if !self.right.visible {
                    self.toggle_right_sidebar(cx);
                }
                self.select_right_tab(tab, cx);
                // An empty Browser opens with the address bar ready, as its
                // button does.
                if tab == SidebarTab::Browser && !self.browser_has_content() {
                    self.browser_begin_edit(cx);
                }
            }
            None => {
                if !self.right.visible {
                    self.toggle_right_sidebar(cx);
                }
            }
        }
    }
}
