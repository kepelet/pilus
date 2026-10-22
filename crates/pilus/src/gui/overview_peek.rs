//! The agent panel of the Home page: click an idle agent and its row opens to
//! what it last said, a field to answer it, and buttons to stop it or go to it.
//!
//! "Last said" is the agent's own newest reply, drawn as Markdown:
//!
//! - A terminal agent (Claude, Pi) keeps its replies in a transcript. With the
//!   same permission Search needs (`Settings::index_agents`, this Mac ticked)
//!   the panel finds the agent's session in the index (by the session id the
//!   agent reported, else the newest one in its folder) and reads the end of
//!   that file. It is read on demand, shown and kept nowhere. Without the
//!   permission, for a machine over SSH, or for an agent that is waiting on a
//!   question (which is on its screen, not in its transcript), the panel shows
//!   the tail of the screen (`agent.read`) instead.
//! - A sipil agent's is its newest line in the copy this pilus keeps of the
//!   conversation, which is there once the conversation has been opened.
//!
//! A reply goes where the sidebar's rows would take you: `agent.prompt` for a
//! terminal agent, the conversation's own `send` for a sipil agent, which queues
//! it when the daemon is away. Stop closes the agent's pane (the daemon's
//! `pane.close`), after a second press; a sipil agent has no pane of this
//! window's to close, so it has no Stop.
//!
//! Every call runs on a worker thread and answers through the page's channel;
//! the GUI thread only builds the request.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{Context, Div, KeyDownEvent, MouseButton, div, prelude::*, px};
use pilus_index::{Agent, Index, last_reply_local};

use super::overview::{Listed, Msg, Target};
use super::sipil::{self, Watch};
use super::sipil_social;
use super::style::{self, ButtonKind, Fonts, RadiusExt};
use super::tasks::{age_label, unix_now};
use super::{PilusView, activity};

/// Lines of the agent's screen asked for.
const READ_LINES: u16 = 60;
/// Lines of them shown, from the end.
const SHOW_LINES: usize = 12;
/// How often an open pane agent's panel is read again.
const READ_EVERY: Duration = Duration::from_secs(3);
/// The machine key of this Mac in the index.
const LOCAL: &str = "local";

/// Where a terminal agent's transcript is found: what the index needs to name
/// its session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Lookup {
    /// `local`, or the saved machine's id.
    pub(super) machine: String,
    /// The daemon's kind id: `claude`, `pi`, ...
    pub(super) kind: String,
    pub(super) cwd: String,
    /// The session id the agent reported, when its integration does.
    pub(super) session: Option<String>,
}

/// What a read of a pane agent found.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Peeked {
    /// The agent's newest reply, from its transcript.
    Reply { text: String, at: i64 },
    /// The tail of its screen.
    Screen(String),
}

/// What the open agent last said, as far as the page knows.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) enum Peek {
    #[default]
    Nothing,
    Reading,
    /// The tail of a terminal agent's screen.
    Output(String),
    /// A reply, with when (0 when undated).
    Said {
        text: String,
        at: u64,
    },
    Failed(String),
}

/// The agent opened under its row.
#[derive(Default)]
pub(super) struct AgentPanel {
    pub(super) open: Option<Target>,
    name: String,
    lookup: Option<Lookup>,
    /// Its status says it is waiting on something, which is on its screen.
    waiting: bool,
    /// The reply field has the keyboard.
    pub(super) focused: bool,
    pub(super) peek: Peek,
    /// Stop was pressed once and awaits its second press.
    confirm_stop: bool,
    read_at: Option<Instant>,
    reading: bool,
}

impl AgentPanel {
    /// A panel already open on `target` showing `peek`, for a capture.
    #[cfg(feature = "visual-test")]
    pub(super) fn sample(target: Target, name: &str, peek: Peek) -> Self {
        Self {
            open: Some(target),
            name: name.to_string(),
            focused: true,
            peek,
            ..Self::default()
        }
    }
}

/// The last [`SHOW_LINES`] lines of a screen, without the blank margins a
/// terminal pads it with.
fn tail(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let start = lines.iter().position(|line| !line.is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |index| index + 1);
    let kept = &lines[start.min(end)..end];
    kept[kept.len().saturating_sub(SHOW_LINES)..].join("\n")
}

/// The newest reply of the agent `lookup` names, from its transcript on this
/// Mac. `None` when the index has no session for it or the file has no reply.
fn transcript_reply(index: &Path, lookup: &Lookup) -> Option<(String, i64)> {
    let agent = Agent::from_kind(&lookup.kind)?;
    let session = Index::open(index)
        .ok()?
        .session_for(
            &lookup.machine,
            agent,
            &lookup.cwd,
            lookup.session.as_deref(),
        )
        .ok()??;
    let reply = last_reply_local(&session.path, agent).ok()??;
    Some((reply.text, reply.at))
}

impl PilusView {
    /// Open `agent`'s panel, or fold it when it is the one open.
    pub(super) fn overview_toggle_agent(&mut self, agent: Listed, cx: &mut Context<Self>) {
        if self.overview.agent.open.as_ref() == Some(&agent.target) {
            self.overview.agent = AgentPanel::default();
        } else {
            self.overview.agent = AgentPanel {
                open: Some(agent.target),
                name: agent.name,
                lookup: agent.lookup,
                waiting: agent.status == "blocked",
                focused: true,
                peek: Peek::Reading,
                ..AgentPanel::default()
            };
            self.overview_reply.update(cx, |input, cx| input.clear(cx));
            self.overview_peek_refresh();
        }
        cx.notify();
    }

    /// Read what the open agent last said again.
    fn overview_peek_refresh(&mut self) {
        let Some(target) = self.overview.agent.open.clone() else {
            return;
        };
        self.overview.agent.read_at = Some(Instant::now());
        match &target {
            Target::Pane { endpoint, pane } => {
                if self.overview.agent.reading {
                    return;
                }
                let Some(mut client) = self.control_client_for_endpoint(endpoint) else {
                    self.overview.agent.peek =
                        Peek::Failed("Couldn't reach that machine's daemon.".to_string());
                    return;
                };
                // The transcript is read only with the permission Search has,
                // for this Mac, and not for an agent waiting on a question.
                let allowed = self.settings.index_agents
                    && self.settings.index_machines.iter().any(|key| key == LOCAL);
                let lookup = self
                    .overview
                    .agent
                    .lookup
                    .clone()
                    .filter(|lookup| lookup.machine == LOCAL && allowed)
                    .filter(|_| !self.overview.agent.waiting);
                let index = self.search_index_path();
                self.overview.agent.reading = true;
                let tx = self.overview.tx.clone();
                let pane = pane.clone();
                std::thread::spawn(move || {
                    let found = lookup
                        .and_then(|lookup| transcript_reply(&index, &lookup))
                        .map(|(text, at)| Peeked::Reply { text, at });
                    let found = match found {
                        Some(found) => Ok(found),
                        None => client
                            .read_agent(&pane, READ_LINES)
                            .map(|screen| Peeked::Screen(tail(&screen)))
                            .map_err(|error| error.to_string()),
                    };
                    tx.send(Msg::Peek(target, found)).ok();
                });
            }
            Target::Chat(Watch::On(daemon, conversation)) => {
                self.overview.agent.peek = match sipil_social::last_said(daemon, conversation) {
                    Some((text, at)) => Peek::Said { text, at },
                    None => Peek::Failed(
                        "Nothing remembered of this conversation yet. Open it once to load it."
                            .to_string(),
                    ),
                };
            }
            Target::Chat(_) => {}
        }
    }

    /// A pane agent's read arrived. An answer for an agent that has since been
    /// folded away is dropped.
    pub(super) fn overview_take_peek(&mut self, target: Target, found: Result<Peeked, String>) {
        let panel = &mut self.overview.agent;
        panel.reading = false;
        if panel.open.as_ref() != Some(&target) {
            return;
        }
        panel.peek = match found {
            Ok(Peeked::Reply { text, at }) => Peek::Said {
                text,
                at: u64::try_from(at).unwrap_or(0),
            },
            Ok(Peeked::Screen(screen)) => Peek::Output(screen),
            // A failed re-read keeps what is already on the panel.
            Err(_) if matches!(panel.peek, Peek::Output(_) | Peek::Said { .. }) => return,
            Err(error) => Peek::Failed(error),
        };
    }

    /// Keep the open panel honest: fold it when its agent is no longer idle
    /// (the reply started it working, or it went away), and re-read it on a
    /// beat. Returns whether the page changed.
    pub(super) fn overview_agent_tick(&mut self) -> bool {
        let Some(open) = self.overview.agent.open.clone() else {
            return false;
        };
        let due = self
            .overview
            .agent
            .read_at
            .is_none_or(|at| at.elapsed() >= activity::scaled(READ_EVERY));
        if !due {
            return false;
        }
        let idle = self
            .overview_agents()
            .iter()
            .any(|agent| agent.target == open && agent.status != "working");
        if !idle {
            self.overview.agent = AgentPanel::default();
            return true;
        }
        // What is on the panel stays up while the next read is out.
        if !matches!(
            self.overview.agent.peek,
            Peek::Output(_) | Peek::Said { .. }
        ) {
            self.overview.agent.peek = Peek::Reading;
        }
        self.overview_peek_refresh();
        true
    }

    /// Go to the agent: its pane, or its conversation.
    pub(super) fn overview_open_agent(&mut self, target: &Target, cx: &mut Context<Self>) {
        match target {
            Target::Pane { endpoint, pane } => {
                self.leave_settings_page();
                self.activate_endpoint(endpoint.clone(), None, Some(pane.clone()), cx);
            }
            Target::Chat(watch) => self.open_sipil_chat(watch.clone(), cx),
        }
    }

    /// Send the reply field's text to the open agent.
    fn overview_reply_send(&mut self, cx: &mut Context<Self>) {
        let text = self.overview_reply.read(cx).value().trim().to_string();
        let Some(target) = self.overview.agent.open.clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        match target {
            Target::Pane { endpoint, pane } => {
                let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
                    self.overview.agent.peek =
                        Peek::Failed("Couldn't reach that machine's daemon.".to_string());
                    cx.notify();
                    return;
                };
                let tx = self.overview.tx.clone();
                std::thread::spawn(move || {
                    let result = client
                        .prompt_agent(&pane, &text)
                        .map(|()| "Reply sent".to_string())
                        .map_err(|error| format!("Couldn't send the reply: {error}"));
                    tx.send(Msg::Done(result)).ok();
                });
            }
            Target::Chat(watch) => {
                sipil::send_in(watch, text, None);
                self.toast(super::toast::ToastKind::Info, "Reply sent");
            }
        }
        // It is working on it now, so the panel has done its job.
        self.overview.agent = AgentPanel::default();
        cx.notify();
    }

    /// Stop the open agent: close its pane. The first press asks, the second
    /// does it.
    fn overview_stop(&mut self, cx: &mut Context<Self>) {
        let Some(Target::Pane { endpoint, pane }) = self.overview.agent.open.clone() else {
            return;
        };
        if !self.overview.agent.confirm_stop {
            self.overview.agent.confirm_stop = true;
            cx.notify();
            return;
        }
        let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
            self.overview.agent.confirm_stop = false;
            self.overview.agent.peek =
                Peek::Failed("Couldn't reach that machine's daemon.".to_string());
            cx.notify();
            return;
        };
        let name = std::mem::take(&mut self.overview.agent.name);
        let tx = self.overview.tx.clone();
        std::thread::spawn(move || {
            let result = client
                .close_pane(&pane)
                .map(|()| format!("Stopped {name}"))
                .map_err(|error| format!("Couldn't stop it: {error}"));
            tx.send(Msg::Done(result)).ok();
        });
        self.overview.agent = AgentPanel::default();
        cx.notify();
    }

    /// Keys while the reply field has the keyboard: Enter sends, Escape takes
    /// back a pending Stop and then folds the panel, Tab goes back to the
    /// composer. Typing is the field's own.
    pub(super) fn overview_reply_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let mods = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "escape" if self.overview.agent.confirm_stop => {
                self.overview.agent.confirm_stop = false;
            }
            "escape" => self.overview.agent = AgentPanel::default(),
            "enter" if !mods.shift => return self.overview_reply_send(cx),
            "tab" => self.overview.blur_reply(),
            _ => return,
        }
        cx.notify();
    }

    /// The panel under an idle agent's row.
    pub(super) fn overview_agent_panel(&self, agent: &Listed, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let (label, body) = match &self.overview.agent.peek {
            Peek::Nothing | Peek::Reading => (
                "LATEST MESSAGE".to_string(),
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("Reading…"),
            ),
            Peek::Output(screen) => {
                let mut lines = div()
                    .flex()
                    .flex_col()
                    .font_family(style::mono_family(&self.shared_settings))
                    .text_size(px(fonts.label))
                    .text_color(theme.panel_fg);
                for line in screen.lines() {
                    // A blank line keeps its height.
                    let line = if line.is_empty() { "\u{a0}" } else { line };
                    lines = lines.child(div().truncate().child(line.to_string()));
                }
                ("ON SCREEN".to_string(), lines)
            }
            Peek::Said { text, at } => {
                let label = if *at > 0 {
                    format!(
                        "LATEST MESSAGE · {}",
                        age_label(unix_now(), *at).to_uppercase()
                    )
                } else {
                    "LATEST MESSAGE".to_string()
                };
                (label, self.overview_message(text, fonts))
            }
            Peek::Failed(error) => (
                "LATEST MESSAGE".to_string(),
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(error.clone()),
            ),
        };
        self.overview_agent_frame(agent, &label, body, cx)
    }

    /// The panel's frame around its `body`: the label, the body, and the row of
    /// reply field and buttons.
    fn overview_agent_frame(
        &self,
        agent: &Listed,
        label: &str,
        body: Div,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let input = self.overview_reply.clone();
        input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let focused = self.overview.reply_focused();
        let target = agent.target.clone();
        let field = div()
            .id("pilus-overview-reply")
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(0.0))
            .h(px(style::scaled(
                &self.shared_settings,
                style::CONTROL_HEIGHT,
            )))
            .px_2()
            .radius_md()
            .border_1()
            .border_color(if focused {
                theme.border_focus
            } else {
                theme.border
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.overview.agent.focused = true;
                    this.overview.agent.confirm_stop = false;
                    cx.notify();
                }),
            )
            .child(input);
        let mut buttons = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(field)
            .child(
                style::small_button(
                    "pilus-overview-reply-send",
                    "Send",
                    ButtonKind::Primary,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(|this, _, _, cx| this.overview_reply_send(cx))),
            );
        // Only a terminal agent has a pane to close.
        if matches!(agent.target, Target::Pane { .. }) {
            let sure = self.overview.agent.confirm_stop;
            buttons = buttons.child(
                style::small_button(
                    "pilus-overview-reply-stop",
                    if sure { "Close it?" } else { "Stop" },
                    if sure {
                        ButtonKind::Danger
                    } else {
                        ButtonKind::Secondary
                    },
                    theme,
                    fonts.label,
                )
                .tooltip(super::row_tooltip(
                    "Close this agent's pane".to_string(),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.overview_stop(cx))),
            );
        }
        buttons = buttons.child(
            style::small_button(
                "pilus-overview-reply-open",
                "Open",
                ButtonKind::Ghost,
                theme,
                fonts.label,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.overview_open_agent(&target, cx);
            })),
        );
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mx_2()
            .mb_1()
            .p_3()
            .radius_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(style::section_label(theme, label.to_string(), fonts.label))
            .child(body)
            .child(buttons)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_screen_loses_its_blank_margins_and_keeps_its_last_lines() {
        let screen = "\n\n  one  \n\ntwo\n\n\n";
        assert_eq!(tail(screen), "  one\n\ntwo");
        let long: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let shown = tail(&long);
        assert_eq!(shown.lines().count(), SHOW_LINES);
        assert!(shown.ends_with("line 30"));
        assert_eq!(tail("\n\n"), "");
    }

    #[test]
    fn an_agent_with_no_session_in_the_index_has_no_transcript_reply() {
        let lookup = Lookup {
            machine: LOCAL.to_string(),
            kind: "claude".to_string(),
            cwd: "/nowhere".to_string(),
            session: None,
        };
        assert_eq!(
            transcript_reply(&PathBuf::from("/no/such/index.db"), &lookup),
            None
        );
        let unknown = Lookup {
            kind: "codex".to_string(),
            ..lookup
        };
        assert_eq!(transcript_reply(&PathBuf::from("/x"), &unknown), None);
    }
}
