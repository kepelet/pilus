//! The native Remote Machines window.
//!
//! It is a thin, owner-local manager over the `luvus machine` CLI: the daemon
//! exposes no machine method, so listing, adding, enabling, disabling, and
//! removing profiles all shell out with `LUVUS_HOME` pinned to this client's
//! state root. Every mutation bumps a shared revision so open main windows
//! reload the catalog immediately.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, Div, FocusHandle, KeyDownEvent, MouseButton, Render, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use l3s_client::{HOME_ENV, MachineInfo, luvus_binary, refresh_machines};

use super::text_field::TextField;
use super::theme::Theme;
use super::{CloseTab, CloseWindow, MachineSignal};

/// Width and height of the machines window.
const WINDOW_SIZE: (f32, f32) = (820.0, 600.0);

/// One editable field in the add form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    /// Stable profile id.
    Id,
    /// Display label.
    Label,
    /// OpenSSH destination or alias.
    Host,
    /// Remote named session.
    Session,
}

impl Field {
    /// The next field, wrapping.
    fn next(self) -> Self {
        match self {
            Self::Id => Self::Label,
            Self::Label => Self::Host,
            Self::Host => Self::Session,
            Self::Session => Self::Id,
        }
    }

    /// The previous field, wrapping.
    fn prev(self) -> Self {
        match self {
            Self::Id => Self::Session,
            Self::Label => Self::Id,
            Self::Host => Self::Label,
            Self::Session => Self::Host,
        }
    }
}

/// The add-machine form.
#[derive(Default)]
struct Form {
    id: TextField,
    label: TextField,
    host: TextField,
    session: TextField,
    /// Whether an `--install` approval is attached to Add/Enable.
    install: bool,
}

impl Form {
    fn field(&self, field: Field) -> &TextField {
        match field {
            Field::Id => &self.id,
            Field::Label => &self.label,
            Field::Host => &self.host,
            Field::Session => &self.session,
        }
    }

    fn field_mut(&mut self, field: Field) -> &mut TextField {
        match field {
            Field::Id => &mut self.id,
            Field::Label => &mut self.label,
            Field::Host => &mut self.host,
            Field::Session => &mut self.session,
        }
    }

    fn clear(&mut self) {
        self.id.clear();
        self.label.clear();
        self.host.clear();
        self.session.clear();
    }
}

/// One CLI mutation requested by the window.
enum MachineOp {
    Add {
        id: String,
        host: String,
        label: String,
        session: String,
        install: bool,
    },
    Enable {
        id: String,
        install: bool,
    },
    Disable {
        id: String,
    },
    Remove {
        id: String,
    },
}

/// The outcome of one background CLI run.
enum OpOutcome {
    Ok(String),
    Err(String),
}

/// The Remote Machines window root view.
pub struct MachineManagerView {
    root: PathBuf,
    signal: Rc<RefCell<MachineSignal>>,
    /// The app's live chrome palette, so this window matches the scheme.
    shared_theme: Rc<RefCell<Theme>>,
    machines: Vec<MachineInfo>,
    theme: Theme,
    form: Form,
    focus: Option<Field>,
    /// The machine id (or `add`) whose command is running.
    busy: Option<String>,
    /// A remove awaiting its second click.
    pending_remove: Option<String>,
    status: String,
    refreshing: bool,
    last_probe: Instant,
    op_rx: Receiver<OpOutcome>,
    op_tx: Sender<OpOutcome>,
    list_rx: Receiver<Vec<MachineInfo>>,
    list_tx: Sender<Vec<MachineInfo>>,
    focus_handle: FocusHandle,
}

impl MachineManagerView {
    fn new(
        root: PathBuf,
        signal: Rc<RefCell<MachineSignal>>,
        shared_theme: Rc<RefCell<Theme>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (op_tx, op_rx) = std::sync::mpsc::channel();
        let (list_tx, list_rx) = std::sync::mpsc::channel();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let theme = *shared_theme.borrow();

        let mut view = Self {
            root,
            signal,
            theme,
            shared_theme,
            machines: Vec::new(),
            form: Form::default(),
            focus: None,
            busy: None,
            pending_remove: None,
            status: "Loading machines…".to_string(),
            refreshing: false,
            last_probe: Instant::now(),
            op_rx,
            op_tx,
            list_rx,
            list_tx,
            focus_handle,
        };
        view.probe();

        cx.spawn(async move |this, cx| {
            loop {
                let updated = this.update(cx, |view, cx| {
                    if view.poll() {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();

        view
    }

    /// Drain finished background work. Returns whether a repaint is needed.
    fn poll(&mut self) -> bool {
        let mut changed = false;
        let theme = *self.shared_theme.borrow();
        if theme != self.theme {
            self.theme = theme;
            changed = true;
        }
        while let Ok(machines) = self.list_rx.try_recv() {
            self.machines = machines;
            self.refreshing = false;
            // Clear the initial placeholder once the first catalog lands.
            if self.status == "Loading machines…" {
                self.status.clear();
            }
            changed = true;
        }
        while let Ok(outcome) = self.op_rx.try_recv() {
            self.busy = None;
            match outcome {
                OpOutcome::Ok(message) => {
                    self.status = if message.is_empty() {
                        "Done.".to_string()
                    } else {
                        message
                    };
                    // Tell every open main window to reload the catalog.
                    self.signal.borrow_mut().revision += 1;
                    self.reload();
                }
                OpOutcome::Err(message) => {
                    self.status = if message.is_empty() {
                        "The command failed.".to_string()
                    } else {
                        message
                    };
                }
            }
            changed = true;
        }
        // Reachability drifts; probe on a slow tick without blocking the window.
        if !self.refreshing && self.last_probe.elapsed() >= Duration::from_secs(10) {
            self.probe();
        }
        changed
    }

    /// Re-read the catalog cheaply, without probing.
    fn reload(&mut self) {
        let root = self.root.clone();
        let tx = self.list_tx.clone();
        self.refreshing = true;
        std::thread::spawn(move || {
            tx.send(l3s_client::list_machines(&root)).ok();
        });
    }

    /// Re-read the catalog with a parallel reachability probe.
    fn probe(&mut self) {
        let root = self.root.clone();
        let tx = self.list_tx.clone();
        self.refreshing = true;
        self.last_probe = Instant::now();
        std::thread::spawn(move || {
            tx.send(refresh_machines(&root)).ok();
        });
    }

    /// Run one CLI mutation on a background thread.
    fn start_op(&mut self, op: MachineOp, label: &str) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label.to_string());
        self.status = format!("Running {label}…");
        let root = self.root.clone();
        let tx = self.op_tx.clone();
        std::thread::spawn(move || {
            tx.send(run_op(&root, op)).ok();
        });
    }

    /// Submit the add form.
    fn submit(&mut self) {
        let id = self.form.id.value.trim().to_string();
        let host = self.form.host.value.trim().to_string();
        if id.is_empty() || host.is_empty() {
            self.status = "A profile id and an SSH host are required.".to_string();
            return;
        }
        let label = self.form.label.value.trim().to_string();
        let session = self.form.session.value.trim().to_string();
        let install = self.form.install;
        self.start_op(
            MachineOp::Add {
                id,
                host,
                label,
                session,
                install,
            },
            "add",
        );
        self.form.clear();
        self.focus = None;
    }

    /// Escape clears focus, then a pending removal, then closes the window.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if self.focus.is_some() {
                self.focus = None;
            } else if self.pending_remove.is_some() {
                self.pending_remove = None;
            } else {
                window.remove_window();
            }
            cx.notify();
            return;
        }
        if self.pending_remove.is_some() && key != "shift" {
            self.pending_remove = None;
            cx.notify();
        }
        let Some(field) = self.focus else {
            if key == "enter" {
                self.submit();
            }
            cx.notify();
            return;
        };
        match key {
            "tab" => {
                self.focus = Some(if event.keystroke.modifiers.shift {
                    field.prev()
                } else {
                    field.next()
                });
            }
            "enter" => self.submit(),
            "backspace" => self.form.field_mut(field).backspace(),
            "delete" | "del" => self.form.field_mut(field).delete(),
            "left" => self.form.field_mut(field).left(),
            "right" => self.form.field_mut(field).right(),
            "home" => self.form.field_mut(field).home(),
            "end" => self.form.field_mut(field).end(),
            _ => {
                if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
                    return;
                }
                if let Some(text) = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|t| !t.is_empty())
                {
                    self.form.field_mut(field).insert(text);
                } else if event.keystroke.key.chars().count() == 1 {
                    self.form.field_mut(field).insert(&event.keystroke.key);
                }
            }
        }
        cx.notify();
    }

    /// A labelled single-line text field.
    fn field_box(&self, field: Field, label: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let focused = self.focus == Some(field);
        let text = self.form.field(field).value.clone();
        let cursor = self.form.field(field).cursor;
        let mut line = div().flex().flex_row().items_center().min_h(px(18.0));
        if text.is_empty() && !focused {
            line = line.child(div().text_color(theme.muted).child(label.to_string()));
        } else {
            let before: String = text.chars().take(cursor).collect();
            let after: String = text.chars().skip(cursor).collect();
            line = line.child(div().text_color(theme.fg).child(before));
            if focused {
                line = line.child(div().w(px(1.5)).h(px(14.0)).bg(theme.accent));
            }
            line = line.child(div().text_color(theme.fg).child(after));
        }
        div()
            .id(format!("l3s-machine-field-{label}"))
            .flex_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(28.0))
                    .px_2()
                    .rounded_md()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(if focused { theme.accent } else { theme.border })
                    .cursor_pointer()
                    .child(line),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.focus = Some(field);
                    this.form.field_mut(field).end();
                    this.pending_remove = None;
                    cx.notify();
                }),
            )
    }

    /// The add-machine form.
    fn add_form(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let installing = self.busy.as_deref() == Some("add");
        let check = div()
            .flex()
            .items_center()
            .justify_center()
            .w(px(14.0))
            .h(px(14.0))
            .rounded_sm()
            .border_1()
            .border_color(if self.form.install {
                theme.accent
            } else {
                theme.muted
            })
            .bg(if self.form.install {
                theme.accent
            } else {
                theme.panel_dim
            })
            .child(if self.form.install {
                div().w(px(6.0)).h(px(6.0)).rounded_sm().bg(theme.window_bg)
            } else {
                div()
            });
        let checkbox = div()
            .id("l3s-machine-install")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(theme.fg)
            .child(check)
            .child("Install luvus if needed")
            .on_click(cx.listener(|this, _, _, cx| {
                this.form.install = !this.form.install;
                this.pending_remove = None;
                cx.notify();
            }));
        let add = div()
            .id("l3s-machine-add")
            .flex()
            .items_center()
            .justify_center()
            .h(px(28.0))
            .px_3()
            .rounded_md()
            .bg(if installing {
                theme.panel_dim
            } else {
                theme.accent
            })
            .text_color(if installing {
                theme.muted
            } else {
                theme.window_bg
            })
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::BOLD)
            .cursor_pointer()
            .child(if installing {
                "Adding…"
            } else {
                "Add machine"
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.submit();
                cx.notify();
            }));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pt_3()
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.fg)
                    .child("Add a machine"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.field_box(Field::Id, "Profile id", cx))
                    .child(self.field_box(Field::Label, "Label", cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.field_box(Field::Host, "SSH host or alias", cx))
                    .child(self.field_box(Field::Session, "Remote session", cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(checkbox)
                    .child(div().flex_1())
                    .child(add),
            )
    }

    /// One saved machine row with its connection controls.
    fn machine_row(&self, machine: &MachineInfo, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let id = machine.id.clone();
        let busy = self.busy.as_deref() == Some(machine.id.as_str());
        let confirming = self.pending_remove.as_deref() == Some(machine.id.as_str());
        let (status, status_color) = machine_status(machine, theme);
        let installing = self.form.install;

        let toggle = if machine.enabled {
            (false, "Disable")
        } else {
            (true, "Enable")
        };
        let toggle_id = id.clone();
        let toggle_button = div()
            .id(format!("l3s-machine-toggle-{id}"))
            .flex()
            .items_center()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .bg(theme.panel)
            .text_size(px(12.0))
            .text_color(if busy { theme.muted } else { theme.fg })
            .cursor_pointer()
            .hover(|style| style.bg(theme.border))
            .child(toggle.1)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.busy.is_some() {
                    return;
                }
                this.pending_remove = None;
                let op = if toggle.0 {
                    MachineOp::Enable {
                        id: toggle_id.clone(),
                        install: installing,
                    }
                } else {
                    MachineOp::Disable {
                        id: toggle_id.clone(),
                    }
                };
                this.start_op(op, &toggle_id);
                cx.notify();
            }));

        let remove_id = id.clone();
        let remove_button = div()
            .id(format!("l3s-machine-remove-{id}"))
            .flex()
            .items_center()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .text_size(px(12.0))
            .text_color(if confirming { theme.alert } else { theme.muted })
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .child(if confirming { "Confirm" } else { "Remove" })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.busy.is_some() {
                    return;
                }
                if this.pending_remove.as_deref() == Some(remove_id.as_str()) {
                    this.pending_remove = None;
                    this.start_op(
                        MachineOp::Remove {
                            id: remove_id.clone(),
                        },
                        &remove_id,
                    );
                } else {
                    this.pending_remove = Some(remove_id.clone());
                    this.status =
                        format!("Remove `{remove_id}`? Click Confirm to delete the profile.");
                }
                cx.notify();
            }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .hover(|style| style.bg(theme.panel_dim))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme.fg)
                                    .child(machine.display().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.muted)
                                    .child(machine.id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.0))
                            .text_color(theme.muted)
                            .child(machine.destination.clone())
                            .child(div().text_color(status_color).child(status)),
                    ),
            )
            .child(toggle_button)
            .child(remove_button)
    }

    /// The saved-machine list.
    fn machine_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div()
            .id("l3s-machine-list")
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .px_3()
            .pt_3()
            .pb_2()
            .overflow_y_scroll();
        list = list.child(
            div()
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.fg)
                .pb_1()
                .child("Saved machines"),
        );
        if self.machines.is_empty() {
            return list.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child("No machines yet. Add one above."),
            );
        }
        for machine in &self.machines {
            if machine.id == "local" {
                continue;
            }
            list = list.child(self.machine_row(machine, cx));
        }
        list
    }
}

impl Render for MachineManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let refresh = div()
            .id("l3s-machine-refresh")
            .flex()
            .items_center()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .text_size(px(12.0))
            .text_color(if self.refreshing {
                theme.muted
            } else {
                theme.fg
            })
            .cursor_pointer()
            .hover(|style| style.bg(theme.panel))
            .child(if self.refreshing {
                "Refreshing…"
            } else {
                "Refresh"
            })
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.refreshing {
                    this.probe();
                }
                cx.notify();
            }));

        div()
            .id("l3s-machines-window")
            .relative()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window_bg)
            .text_color(theme.fg)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &CloseTab, window, _| window.remove_window()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_3()
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("Remote Machines"),
                    )
                    .child(refresh),
            )
            .child(self.add_form(cx))
            .child(self.machine_list(cx))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .child(self.status.clone()),
            )
    }
}

/// The status label and color for a saved machine.
fn machine_status(machine: &MachineInfo, theme: Theme) -> (String, gpui::Rgba) {
    if !machine.enabled {
        return ("disabled".to_string(), theme.muted);
    }
    match (machine.online, machine.latency_ms) {
        (Some(true), Some(ms)) => (format!("online · {ms} ms"), theme.ok),
        (Some(true), None) => ("online".to_string(), theme.ok),
        (Some(false), _) => ("offline".to_string(), theme.alert),
        (None, _) => ("probing…".to_string(), theme.muted),
    }
}

/// Run one `luvus machine …` mutation and capture its result.
fn run_op(root: &Path, op: MachineOp) -> OpOutcome {
    let Some(binary) = luvus_binary() else {
        return OpOutcome::Err("luvus binary not found on PATH".to_string());
    };
    // A short confirmation instead of the CLI's JSON stdout.
    let success = match &op {
        MachineOp::Add { id, .. } => format!("Added `{id}`."),
        MachineOp::Enable { id, .. } => format!("Enabled `{id}`."),
        MachineOp::Disable { id } => format!("Disabled `{id}`."),
        MachineOp::Remove { id } => format!("Removed `{id}`."),
    };
    let mut args: Vec<String> = vec!["machine".to_string()];
    match op {
        MachineOp::Add {
            id,
            host,
            label,
            session,
            install,
        } => {
            args.push("add".to_string());
            args.push(id);
            args.push("--host".to_string());
            args.push(host);
            if !label.is_empty() {
                args.push("--label".to_string());
                args.push(label);
            }
            if !session.is_empty() {
                args.push("--session".to_string());
                args.push(session);
            }
            if install {
                args.push("--install".to_string());
            }
        }
        MachineOp::Enable { id, install } => {
            args.push("enable".to_string());
            args.push(id);
            if install {
                args.push("--install".to_string());
            }
        }
        MachineOp::Disable { id } => {
            args.push("disable".to_string());
            args.push(id);
        }
        MachineOp::Remove { id } => {
            args.push("remove".to_string());
            args.push(id);
        }
    }
    match std::process::Command::new(binary)
        .args(&args)
        .env(HOME_ENV, root)
        .output()
    {
        Ok(output) if output.status.success() => OpOutcome::Ok(success),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            OpOutcome::Err(if stderr.is_empty() {
                "The command failed.".to_string()
            } else {
                stderr
            })
        }
        Err(error) => OpOutcome::Err(error.to_string()),
    }
}

/// Open the Remote Machines window.
pub fn open(
    root: PathBuf,
    signal: Rc<RefCell<MachineSignal>>,
    shared_theme: Rc<RefCell<Theme>>,
    cx: &mut App,
) {
    let bounds = Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Remote Machines".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            cx.new(move |cx| MachineManagerView::new(root, signal, shared_theme, window, cx))
        },
    );
    if let Err(error) = opened {
        eprintln!("l3s: could not open the machines window: {error}");
    }
}
