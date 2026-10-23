//! The native Ports tab: what the active machine is listening on, and a
//! button to reach any of it from this Mac.
//!
//! The daemon knows nothing of ports, so the tab asks the machine itself
//! (`pilus_client::list_ports`, over the saved SSH profile) while it is on
//! screen, off the GUI thread. A forward is an `ssh -N -L` process owned by a
//! `PortForward` in [`PortsPanel::forwards`]. Forwards are kept by machine, not
//! by what the tab shows, so one keeps running while another machine is
//! selected; dropping the entry (Stop, or the window closing) ends it.
//!
//! This Mac needs no forwarding: its ports are already at `localhost`, so its
//! rows only open.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px};
use pilus_client::{
    ListeningPort, PortForward, SshTarget, free_local_port, kill_process, list_ports,
};

use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::toast::ToastKind;
use super::{EndpointId, PilusView, PromptAction, icon};

/// How often the open tab asks the machine what it is listening on.
const POLL: Duration = Duration::from_millis(3000);

/// What a background job reports back to the tab.
pub(super) enum PortsMsg {
    Scan {
        endpoint: EndpointId,
        result: Result<Vec<ListeningPort>, String>,
    },
    Forwarded {
        machine: String,
        port: u16,
        result: Result<PortForward, String>,
    },
    Killed {
        /// The machine the process ran on; `None` for this Mac.
        machine: Option<String>,
        port: u16,
        name: String,
        result: Result<(), String>,
    },
}

/// Where one remote port's forward stands.
enum Forward {
    /// The SSH handshake is in flight.
    Starting,
    Active(PortForward),
    Failed(String),
}

/// A forward is told apart by its machine and the remote port it reaches.
type ForwardKey = (String, u16);

/// The tab's own state.
pub(super) struct PortsPanel {
    endpoint: Option<EndpointId>,
    /// `None` until the first scan of this machine lands.
    ports: Option<Vec<ListeningPort>>,
    forwards: HashMap<ForwardKey, Forward>,
    /// The row showing its process, by port.
    expanded: Option<u16>,
    error: Option<String>,
    polling: bool,
    last_poll: Option<Instant>,
    tx: Sender<PortsMsg>,
    rx: Receiver<PortsMsg>,
}

impl PortsPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            endpoint: None,
            ports: None,
            forwards: HashMap::new(),
            expanded: None,
            error: None,
            polling: false,
            last_poll: None,
            tx,
            rx,
        }
    }

    /// Ask for a fresh scan the next time the tab polls.
    pub(super) fn mark_stale(&mut self) {
        self.last_poll = None;
    }

    /// Drop what was read from the previous machine; its forwards stay.
    fn reset(&mut self) {
        self.ports = None;
        self.expanded = None;
        self.error = None;
        self.last_poll = None;
    }

    /// Fold a finished scan in; true when what is drawn changed.
    fn apply(&mut self, result: Result<Vec<ListeningPort>, String>) -> bool {
        match result {
            Ok(ports) => {
                let changed = self.error.take().is_some() || self.ports.as_ref() != Some(&ports);
                self.ports = Some(ports);
                changed
            }
            Err(error) => {
                let changed = self.error.as_ref() != Some(&error);
                self.error = Some(error);
                changed
            }
        }
    }
}

/// One row to draw: a port the machine is listening on, or one it was
/// forwarded from and has since stopped listening on.
struct Row {
    port: ListeningPort,
    listening: bool,
}

impl PilusView {
    /// Fold finished jobs in, end forwards whose SSH process died, and poll the
    /// open tab. Returns whether anything changed.
    pub(super) fn ports_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.ports_panel.rx.try_recv() {
            match message {
                PortsMsg::Scan { endpoint, result } => {
                    self.ports_panel.polling = false;
                    if self.ports_panel.endpoint.as_ref() == Some(&endpoint) {
                        changed |= self.ports_panel.apply(result);
                    }
                }
                PortsMsg::Forwarded {
                    machine,
                    port,
                    result,
                } => {
                    let state = match result {
                        Ok(forward) => {
                            let local = forward.local_port();
                            self.toast(
                                ToastKind::Success,
                                format!("Forwarded port {port} to localhost:{local}"),
                            );
                            Forward::Active(forward)
                        }
                        Err(error) => Forward::Failed(error),
                    };
                    self.ports_panel.forwards.insert((machine, port), state);
                    changed = true;
                }
                PortsMsg::Killed {
                    machine,
                    port,
                    name,
                    result,
                } => {
                    match result {
                        Ok(()) => {
                            self.toast(
                                ToastKind::Success,
                                format!("Asked {name} on port {port} to stop"),
                            );
                            // A forward to a port nothing holds is dead weight.
                            if let Some(machine) = machine {
                                self.ports_panel.forwards.remove(&(machine, port));
                            }
                        }
                        Err(error) => self.toast(ToastKind::Error, error),
                    }
                    self.ports_panel.mark_stale();
                    changed = true;
                }
            }
        }
        for state in self.ports_panel.forwards.values_mut() {
            if let Forward::Active(forward) = state
                && forward.has_exited()
            {
                *state = Forward::Failed("The connection closed".to_string());
                changed = true;
            }
        }
        if !self.ports_visible() {
            return changed;
        }
        if self.ports_panel.endpoint.as_ref() != Some(&self.active) {
            self.ports_panel.reset();
            self.ports_panel.endpoint = Some(self.active.clone());
            changed = true;
        }
        if self.ports_panel.polling
            || self
                .ports_panel
                .last_poll
                .is_some_and(|at| at.elapsed() < super::activity::scaled(POLL))
        {
            return changed;
        }
        let target = match self.ssh_target_for_active() {
            Ok(target) => target,
            Err(error) => {
                changed |= self.ports_panel.apply(Err(error));
                return changed;
            }
        };
        let panel = &mut self.ports_panel;
        panel.polling = true;
        panel.last_poll = Some(Instant::now());
        let tx = panel.tx.clone();
        let endpoint = self.active.clone();
        std::thread::spawn(move || {
            let result = list_ports(target.as_ref());
            tx.send(PortsMsg::Scan { endpoint, result }).ok();
        });
        changed
    }

    /// How many forwards of ours are running, on any machine.
    pub(super) fn ports_forwarded_count(&self) -> usize {
        self.ports_panel
            .forwards
            .values()
            .filter(|state| matches!(state, Forward::Active(_)))
            .count()
    }

    pub(super) fn ports_forwarding(&self) -> bool {
        self.ports_forwarded_count() > 0
    }

    /// End every forward: a quit does not reliably drop the view, and an
    /// `ssh -L` left running would keep the local port taken.
    pub(super) fn ports_stop_all(&mut self) {
        self.ports_panel.forwards.clear();
    }

    /// How to run commands on the active machine: `None` for this Mac.
    fn ssh_target_for_active(&self) -> Result<Option<SshTarget>, String> {
        match &self.active_machine {
            None => Ok(None),
            Some(machine) => machine.ssh_target().map(Some).ok_or_else(|| {
                "Ports are only available on this Mac and on SSH machines".to_string()
            }),
        }
    }

    /// Forward `port` of the active machine to this Mac.
    fn ports_forward(&mut self, port: &ListeningPort) {
        let Some(machine) = self.active_machine.as_ref() else {
            return;
        };
        let Some(target) = machine.ssh_target() else {
            self.toast(
                ToastKind::Error,
                "This machine has no SSH profile to forward through",
            );
            return;
        };
        let key = (machine.id.clone(), port.port);
        if matches!(
            self.ports_panel.forwards.get(&key),
            Some(Forward::Starting | Forward::Active(_))
        ) {
            return;
        }
        self.ports_panel
            .forwards
            .insert(key.clone(), Forward::Starting);
        let tx = self.ports_panel.tx.clone();
        let host = port.host.clone();
        std::thread::spawn(move || {
            let (machine, port) = key;
            let result = free_local_port(port)
                .ok_or_else(|| "no free port on this Mac".to_string())
                .and_then(|local| PortForward::start(&target, local, &host, port));
            tx.send(PortsMsg::Forwarded {
                machine,
                port,
                result,
            })
            .ok();
        });
    }

    /// End the forward of `port` on the active machine.
    fn ports_stop(&mut self, port: u16) {
        if let Some(machine) = &self.active_machine {
            self.ports_panel
                .forwards
                .remove(&(machine.id.clone(), port));
        }
    }

    /// Ask before stopping the process behind `port`.
    fn ports_confirm_kill(&mut self, port: &ListeningPort, cx: &mut Context<Self>) {
        let Some(pid) = port.pid else {
            return;
        };
        let label = if port.command.is_empty() {
            format!("pid {pid} · {}", port.process)
        } else {
            format!("pid {pid} · {}", port.command)
        };
        self.open_menu_prompt(
            PromptAction::KillProcess {
                endpoint: self.active.clone(),
                pid,
                port: port.port,
                name: port.process.clone(),
                label,
            },
            "Kill process",
            "",
            "",
            cx,
        );
    }

    /// Stop `pid` on `endpoint`'s machine, off the GUI thread.
    pub(super) fn ports_kill(
        &mut self,
        endpoint: &EndpointId,
        pid: u32,
        port: u16,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let target = match endpoint {
            EndpointId::Local => Ok(None),
            EndpointId::Machine(id) => self
                .machines
                .iter()
                .find(|machine| &machine.id == id)
                .and_then(|machine| machine.ssh_target())
                .map(Some)
                .ok_or("This machine has no SSH profile to reach it through"),
        };
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                return;
            }
        };
        let machine = endpoint.machine_id().map(str::to_string);
        let tx = self.ports_panel.tx.clone();
        std::thread::spawn(move || {
            let result = kill_process(target.as_ref(), pid, port);
            tx.send(PortsMsg::Killed {
                machine,
                port,
                name,
                result,
            })
            .ok();
        });
        cx.notify();
    }

    /// The forward state of `port` on the active machine, if one was asked for.
    fn forward_of(&self, port: u16) -> Option<&Forward> {
        let machine = self.active_machine.as_ref()?;
        self.ports_panel.forwards.get(&(machine.id.clone(), port))
    }

    /// The rows: what the machine listens on, then forwards it no longer
    /// backs, so each still has its Stop.
    fn port_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .ports_panel
            .ports
            .iter()
            .flatten()
            .map(|port| Row {
                port: port.clone(),
                listening: true,
            })
            .collect();
        if let Some(machine) = &self.active_machine {
            let mut gone: Vec<u16> = self
                .ports_panel
                .forwards
                .keys()
                .filter(|(id, port)| {
                    *id == machine.id && !rows.iter().any(|row| row.port.port == *port)
                })
                .map(|(_, port)| *port)
                .collect();
            gone.sort_unstable();
            rows.extend(gone.into_iter().map(|port| Row {
                port: ListeningPort {
                    port,
                    process: String::new(),
                    pid: None,
                    command: String::new(),
                    host: "localhost".to_string(),
                },
                listening: false,
            }));
        }
        rows
    }
}

// Rendering.
impl PilusView {
    pub(super) fn ports_page(&mut self, cx: &mut Context<Self>) -> Div {
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.ports_header(cx));
        if let (None, Some(error)) = (&self.ports_panel.ports, &self.ports_panel.error) {
            return page.child(self.diff_message(error.clone()));
        }
        if self.ports_panel.ports.is_none() {
            return page.child(self.diff_message("Looking for listening ports…"));
        }
        let rows = self.port_rows();
        if rows.is_empty() {
            return page.child(self.diff_message(
                "Nothing is listening yet. Start a dev server and it appears here.",
            ));
        }
        let mut list = div().flex().flex_col().p_2();
        for row in &rows {
            list = list.child(self.port_row(row, cx));
        }
        page.child(
            div()
                .id("pilus-ports-list")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .child(list),
        )
    }

    /// One row: where the machine is, how many ports, and the refresh control.
    fn ports_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let name = match &self.active_machine {
            Some(machine) => machine.display().to_string(),
            None => "this Mac".to_string(),
        };
        let refresh = self
            .diff_icon_button(
                "pilus-ports-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.ports_panel.mark_stale();
                cx.notify();
            }));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(icon(
                include_bytes!("../../assets/icons/server.svg"),
                fonts.body,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("Listening on {name}")),
            )
            .child(refresh)
    }

    fn port_row(&self, row: &Row, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let port = row.port.port;
        let forward = self.forward_of(port);
        let remote = self.active_machine.is_some();
        let open = self.ports_panel.expanded == Some(port);

        let mut title = div().flex().flex_row().items_center().gap_2().child(
            div()
                .flex_shrink_0()
                .font_family(SharedString::from(style::mono_family(
                    &self.shared_settings,
                )))
                .text_size(px(fonts.body))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.panel_fg)
                .child(port.to_string()),
        );
        let process = if row.listening {
            row.port.process.clone()
        } else {
            "no longer listening".to_string()
        };
        if !process.is_empty() {
            title = title.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(process),
            );
        }

        let mut text = div().flex().flex_col().gap(px(2.0)).flex_1().min_w(px(0.0));
        text = text.child(title);
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0();
        match forward {
            _ if !remote => {
                actions = actions.child(self.port_open_button(port, port, cx));
            }
            Some(Forward::Starting) => {
                actions = actions.child(style::busy_dot(14.0, theme.muted));
                text = text.child(self.port_note("Forwarding…", theme.muted));
            }
            Some(Forward::Active(active)) => {
                let local = active.local_port();
                text = text.child(self.port_note(&format!("→ localhost:{local}"), theme.ok));
                actions =
                    actions
                        .child(self.port_open_button(port, local, cx))
                        .child(self.port_action(
                            format!("pilus-ports-stop-{port}"),
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            "Stop forwarding",
                            cx,
                            move |this, _| this.ports_stop(port),
                        ));
            }
            failed => {
                if let Some(Forward::Failed(error)) = failed {
                    text = text.child(self.port_note(error, theme.alert));
                }
                actions = actions.child(if row.listening {
                    let target = row.port.clone();
                    self.port_action(
                        format!("pilus-ports-forward-{port}"),
                        include_bytes!("../../assets/icons/forward.svg"),
                        ButtonKind::Secondary,
                        "Forward to this Mac",
                        cx,
                        move |this, _| this.ports_forward(&target),
                    )
                } else {
                    self.port_action(
                        format!("pilus-ports-dismiss-{port}"),
                        include_bytes!("../../assets/icons/x.svg"),
                        ButtonKind::Ghost,
                        "Dismiss",
                        cx,
                        move |this, _| this.ports_stop(port),
                    )
                });
            }
        }
        if row.listening && row.port.pid.is_some() {
            let target = row.port.clone();
            actions = actions.child(self.port_action(
                format!("pilus-ports-kill-{port}"),
                include_bytes!("../../assets/icons/trash-2.svg"),
                ButtonKind::Danger,
                "Kill process",
                cx,
                move |this, cx| this.ports_confirm_kill(&target, cx),
            ));
        }

        let mut card = div()
            .id(format!("pilus-ports-row-{port}"))
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py(px(6.0))
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(text)
                    .child(actions),
            );
        if open {
            card = card.child(self.port_detail(&row.port));
        }
        card.on_click(cx.listener(move |this, _, _, cx| {
            let panel = &mut this.ports_panel;
            panel.expanded = if panel.expanded == Some(port) {
                None
            } else {
                Some(port)
            };
            cx.notify();
        }))
    }

    /// What is behind a port: its pid and the command that started it.
    fn port_detail(&self, port: &ListeningPort) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut detail = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .min_w(px(0.0))
            .text_size(px(fonts.secondary));
        if let Some(pid) = port.pid {
            detail = detail.child(div().text_color(theme.muted).child(format!("pid {pid}")));
        }
        detail.child(
            div()
                .font_family(SharedString::from(style::mono_family(
                    &self.shared_settings,
                )))
                .text_color(if port.command.is_empty() {
                    theme.muted
                } else {
                    theme.panel_fg
                })
                .child(if port.command.is_empty() {
                    "The host didn't report a command".to_string()
                } else {
                    port.command.clone()
                }),
        )
    }

    /// A line under a row's title: a forward's target or why it failed.
    fn port_note(&self, text: &str, color: gpui::Rgba) -> Div {
        div()
            .text_size(px(style::Fonts::resolve(&self.shared_settings).secondary))
            .text_color(color)
            .child(text.to_string())
    }

    /// An icon-only row action. It takes the click itself, so the row behind
    /// it does not also expand.
    fn port_action(
        &self,
        id: String,
        glyph: &'static [u8],
        kind: ButtonKind,
        tip: &str,
        cx: &mut Context<Self>,
        run: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let theme = self.theme;
        style::icon_button(
            id,
            glyph,
            kind,
            theme,
            style::scaled(&self.shared_settings, 24.0),
        )
        .tooltip(super::row_tooltip(
            tip.to_string(),
            theme.panel_fg,
            Vec::new(),
            theme,
        ))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            run(this, cx);
            cx.notify();
        }))
    }

    /// Open `localhost:local` in the browser.
    fn port_open_button(&self, port: u16, local: u16, cx: &mut Context<Self>) -> Stateful<Div> {
        self.port_action(
            format!("pilus-ports-open-{port}"),
            include_bytes!("../../assets/icons/external-link.svg"),
            ButtonKind::Ghost,
            "Open in browser",
            cx,
            move |_, cx| cx.open_url(&format!("http://localhost:{local}")),
        )
    }
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// A machine with a dev server, a database, and a forward in each state
    /// short of a live one, which needs a real SSH process.
    pub(super) fn ports_show_sample(&mut self, cx: &mut Context<Self>) {
        use super::right_sidebar::SidebarTab;

        let port = |port: u16, process: &str, pid: u32, command: &str| ListeningPort {
            port,
            process: process.to_string(),
            pid: Some(pid),
            command: command.to_string(),
            host: "localhost".to_string(),
        };
        self.active_machine = Some(pilus_client::MachineInfo {
            id: "mini".to_string(),
            display: "mac-mini".to_string(),
            ..Default::default()
        });
        self.right.visible = true;
        self.right.select_tab(SidebarTab::Ports);
        let panel = &mut self.ports_panel;
        panel.endpoint = Some(self.active.clone());
        panel.ports = Some(vec![
            port(
                3000,
                "next-server",
                4121,
                "node /app/node_modules/.bin/next dev",
            ),
            port(
                5173,
                "node",
                4188,
                "node /app/node_modules/.bin/vite --port 5173",
            ),
            port(
                5432,
                "postgres",
                611,
                "/opt/homebrew/opt/postgresql@16/bin/postgres -D /opt/homebrew/var/postgresql@16",
            ),
            port(8080, "python3", 5302, "python3 -m http.server 8080"),
        ]);
        panel.expanded = Some(8080);
        panel
            .forwards
            .insert(("mini".to_string(), 3000), Forward::Starting);
        if let Ok(child) = std::process::Command::new("sleep").arg("600").spawn() {
            panel.forwards.insert(
                ("mini".to_string(), 5173),
                Forward::Active(PortForward::adopt(child, 5173)),
            );
        }
        panel.forwards.insert(
            ("mini".to_string(), 5432),
            Forward::Failed("bind [127.0.0.1]:5432: Address already in use".to_string()),
        );
        panel.polling = true;
        cx.notify();
    }

    pub(super) fn ports_clear_sample(&mut self, cx: &mut Context<Self>) {
        self.active_machine = None;
        self.ports_panel.forwards.clear();
        self.ports_panel.ports = None;
        self.ports_panel.polling = false;
        cx.notify();
    }
}
