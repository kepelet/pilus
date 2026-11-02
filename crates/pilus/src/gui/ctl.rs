//! The window's half of `pilus ctl`: take a [`Call`] from the control socket
//! and do what a click would.
//!
//! `crate::ctl` parses the request on the listener's thread and sends it down
//! a channel. A task on the app's executor, parked until a call arrives, picks
//! it up here, finds the window and runs it on the GUI thread, so no view is
//! ever touched from the listener. Each verb calls the same method as the
//! button or key that does it by hand.

use std::path::Path;

use futures::StreamExt;
use gpui::{App, Context, Window};

use super::right_sidebar::SidebarTab;
use super::{PilusView, tab_exists};
use crate::ctl::{self, Reply, Request, Tab};

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
            let reply = cx.update(|cx| apply(&call.request, cx));
            call.respond(reply);
        }
    })
    .detach();
}

/// Run `request` on the window the person is using.
fn apply(request: &Request, cx: &mut App) -> Reply {
    if matches!(request, Request::Ping) {
        return Reply::ok();
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
        return Reply::err("no pilus window is open");
    };
    handle
        .update(cx, |view, window, cx| view.apply_ctl(request, window, cx))
        .unwrap_or_else(|error| Reply::err(format!("the window is busy: {error}")))
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Reply {
        match request {
            Request::Ping => Reply::ok(),
            Request::State => {
                let (tabs, url) = self.browser_summary();
                ctl::state_reply(
                    self.right.visible,
                    &self.right.tab.label().to_ascii_lowercase(),
                    tabs,
                    &url,
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
            Request::BrowserOpen { url } => {
                // Load the address first so a refused one does not leave the
                // sidebar open on an empty Browser.
                match self.browser_open_url(url, cx) {
                    Ok(()) => {
                        self.ctl_show_sidebar(Some(SidebarTab::Browser), cx);
                        Reply::ok()
                    }
                    Err(message) => Reply::err(message),
                }
            }
            Request::TabFocus { index } => {
                if !tab_exists(&self.tabs, *index) {
                    return Reply::err(format!("there is no tab {index}"));
                }
                self.focus_tab(*index, cx);
                Reply::ok()
            }
        }
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
