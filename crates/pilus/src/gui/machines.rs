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
    App, Bounds, Context, Div, Entity, FocusHandle, Focusable, Font, KeyDownEvent, MouseButton,
    Pixels, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use pilus_client::{
    HOME_ENV, MachineInfo, find_installed_binary, is_behind, list_machines, luvus_binary,
    refresh_machines, restart_remote, stop_remote,
};

use crate::settings::Settings;

use super::style::{self, ButtonKind, RadiusExt};
use super::text_input::TextInput;
use super::theme::Theme;
use super::{CloseTab, CloseWindow, MachineSignal};

/// Width and height of the machines window.
const WINDOW_SIZE: (f32, f32) = (820.0, 600.0);
/// The smallest the window can be dragged to: below this its rows and
/// controls start to clip.
const WINDOW_MIN_SIZE: (f32, f32) = (640.0, 420.0);

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
struct Form {
    id: Entity<TextInput>,
    label: Entity<TextInput>,
    host: Entity<TextInput>,
    session: Entity<TextInput>,
    /// Whether an `--install` approval is attached to Add/Enable.
    install: bool,
}

impl Form {
    fn new(
        font: &Font,
        font_size: Pixels,
        theme: Theme,
        cx: &mut Context<MachineManagerView>,
    ) -> Self {
        let input = |cx: &mut Context<MachineManagerView>, placeholder: &'static str| {
            cx.new(|cx| {
                TextInput::new(
                    String::new(),
                    placeholder,
                    font.clone(),
                    font_size,
                    false,
                    theme,
                    cx,
                )
            })
        };
        Self {
            id: input(cx, "Profile id"),
            label: input(cx, "Label"),
            host: input(cx, "SSH host or alias"),
            session: input(cx, "Remote session"),
            install: false,
        }
    }

    fn entity(&self, field: Field) -> Entity<TextInput> {
        match field {
            Field::Id => self.id.clone(),
            Field::Label => self.label.clone(),
            Field::Host => self.host.clone(),
            Field::Session => self.session.clone(),
        }
    }

    /// The field that currently owns the keyboard, if any.
    fn focused(&self, window: &Window, cx: &App) -> Option<Field> {
        [Field::Id, Field::Label, Field::Host, Field::Session]
            .into_iter()
            .find(|field| self.entity(*field).read(cx).is_focused(window))
    }

    /// Move the keyboard to `field`.
    fn focus(&self, field: Field, window: &mut Window, cx: &mut App) {
        let handle = self.entity(field).focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn clear(&self, cx: &mut App) {
        for field in [Field::Id, Field::Label, Field::Host, Field::Session] {
            let entity = self.entity(field);
            entity.update(cx, |input, cx| input.clear(cx));
        }
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
    /// Bring this Mac's Luvus to the latest release, then re-provision the
    /// remote binary from it. `enable --install` ships the local binary, so the
    /// remote can only be as new as the local install.
    Update {
        id: String,
    },
    /// Restart the remote daemon so it loads the binary `Update` installed.
    Restart {
        id: String,
    },
    /// Stop an old remote daemon, then install. For a daemon too old to
    /// complete the `enable --install` handshake, which `Update` reports.
    StopAndUpdate {
        id: String,
    },
}

/// The outcome of one background CLI run.
enum OpOutcome {
    Ok(String),
    /// A new remote binary is in place but the old daemon is still running.
    Installed {
        id: String,
        message: String,
    },
    /// The running remote daemon rejected this client's protocol, so the
    /// install cannot finish until that daemon is stopped.
    NeedsStop {
        id: String,
        message: String,
    },
    Err(String),
}

/// The Remote Machines window root view.
pub struct MachineManagerView {
    root: PathBuf,
    signal: Rc<RefCell<MachineSignal>>,
    /// The app's live chrome palette, so this window matches the scheme.
    shared_theme: Rc<RefCell<Theme>>,
    /// Shared client settings, so the Interface font reaches this window.
    shared_settings: Rc<RefCell<Settings>>,
    machines: Vec<MachineInfo>,
    theme: Theme,
    form: Form,
    /// The machine id (or `add`) whose command is running.
    busy: Option<String>,
    /// A remove awaiting its second click.
    pending_remove: Option<String>,
    /// A machine whose daemon still runs the pre-upgrade binary.
    pending_restart: Option<String>,
    /// A machine whose old daemon must be stopped before it can be upgraded.
    pending_stop: Option<String>,
    /// A stop-and-upgrade awaiting its second click.
    confirming_stop: Option<String>,
    /// A restart awaiting its second click; it stops that session's panes.
    confirming_restart: Option<String>,
    /// The `luvus` version installed on this Mac, to flag outdated machines.
    local_version: Option<String>,
    version_rx: Receiver<Option<String>>,
    version_tx: Sender<Option<String>>,
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
    pub(super) fn new(
        root: PathBuf,
        signal: Rc<RefCell<MachineSignal>>,
        shared_theme: Rc<RefCell<Theme>>,
        shared_settings: Rc<RefCell<Settings>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (op_tx, op_rx) = std::sync::mpsc::channel();
        let (list_tx, list_rx) = std::sync::mpsc::channel();
        let (version_tx, version_rx) = std::sync::mpsc::channel();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let theme = *shared_theme.borrow();
        let fonts = style::Fonts::resolve(&shared_settings);
        let font = style::chrome_font(&shared_settings);

        let mut view = Self {
            root,
            signal,
            theme,
            shared_theme,
            shared_settings,
            machines: Vec::new(),
            form: Form::new(&font, px(fonts.body), theme, cx),
            busy: None,
            pending_remove: None,
            pending_restart: None,
            pending_stop: None,
            confirming_stop: None,
            confirming_restart: None,
            local_version: None,
            version_rx,
            version_tx,
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
        while let Ok(version) = self.version_rx.try_recv() {
            self.local_version = version;
            changed = true;
        }
        while let Ok(outcome) = self.op_rx.try_recv() {
            self.busy = None;
            match outcome {
                OpOutcome::NeedsStop { id, message } => {
                    self.pending_stop = Some(id);
                    self.confirming_stop = None;
                    self.status = message;
                }
                OpOutcome::Installed { id, message } => {
                    self.pending_stop = None;
                    self.confirming_stop = None;
                    self.pending_restart = Some(id);
                    self.status = message;
                    self.signal.borrow_mut().revision += 1;
                    self.reload();
                }
                OpOutcome::Ok(message) => {
                    self.pending_stop = None;
                    self.confirming_stop = None;
                    self.pending_restart = None;
                    self.confirming_restart = None;
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
            tx.send(list_machines(&root)).ok();
        });
    }

    /// Re-read the catalog with a parallel reachability probe.
    fn probe(&mut self) {
        let root = self.root.clone();
        let tx = self.list_tx.clone();
        self.refreshing = true;
        self.last_probe = Instant::now();
        let version_tx = self.version_tx.clone();
        std::thread::spawn(move || {
            version_tx
                .send(crate::luvus_update::installed_version().map(|v| v.to_string()))
                .ok();
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
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.form.id.read(cx).value().trim().to_string();
        let host = self.form.host.read(cx).value().trim().to_string();
        if id.is_empty() || host.is_empty() {
            self.status = "A profile id and an SSH host are required.".to_string();
            return;
        }
        let label = self.form.label.read(cx).value().trim().to_string();
        let session = self.form.session.read(cx).value().trim().to_string();
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
        self.form.clear(cx);
        let handle = self.focus_handle.clone();
        window.focus(&handle, cx);
    }

    /// Escape clears focus, then a pending removal, then closes the window.
    /// Tab walks the form; every other editing key belongs to the focused
    /// field, which consumes what it handles before it reaches here.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if self.form.focused(window, cx).is_some() {
                let handle = self.focus_handle.clone();
                window.focus(&handle, cx);
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
        let Some(field) = self.form.focused(window, cx) else {
            if key == "enter" {
                self.submit(window, cx);
            }
            cx.notify();
            return;
        };
        match key {
            "tab" => {
                let next = if event.keystroke.modifiers.shift {
                    field.prev()
                } else {
                    field.next()
                };
                self.form.focus(next, window, cx);
            }
            "enter" => self.submit(window, cx),
            _ => {}
        }
        cx.notify();
    }

    /// The window's type scale, resolved from the shared settings.
    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.shared_settings)
    }

    /// A labelled single-line text field.
    fn field_box(
        &self,
        field: Field,
        label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = self.fonts();
        let entity = self.form.entity(field);
        entity.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let focused = entity.read(cx).is_focused(window);
        style::field(
            format!("pilus-machine-field-{label}"),
            label,
            focused,
            theme,
            28.0,
            fonts.label,
            entity,
        )
        .flex_1()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.pending_remove = None;
                cx.notify();
            }),
        )
    }

    /// The add-machine form.
    fn add_form(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let installing = self.busy.as_deref() == Some("add");
        let checkbox = style::checkbox(
            "pilus-machine-install",
            self.form.install,
            "Install luvus if needed",
            theme,
            self.fonts().secondary,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.form.install = !this.form.install;
            this.pending_remove = None;
            cx.notify();
        }));
        let mut add = style::button(
            "pilus-machine-add",
            if installing {
                "Adding…"
            } else {
                "Add machine"
            },
            ButtonKind::Primary,
            theme,
            self.fonts().secondary,
        );
        if installing {
            add = add.bg(theme.panel_dim).text_color(theme.muted);
        }
        let add = add.on_click(cx.listener(|this, _, window, cx| {
            this.submit(window, cx);
            cx.notify();
        }));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pt_3()
            .child(style::section_label(
                theme,
                "ADD A MACHINE",
                self.fonts().label,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.field_box(Field::Id, "Profile id", window, cx))
                    .child(self.field_box(Field::Label, "Label", window, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.field_box(Field::Host, "SSH host or alias", window, cx))
                    .child(self.field_box(Field::Session, "Remote session", window, cx)),
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
        let mut toggle_button = style::small_button(
            format!("pilus-machine-toggle-{id}"),
            toggle.1,
            ButtonKind::Ghost,
            theme,
            self.fonts().label,
        );
        if busy {
            toggle_button = toggle_button.text_color(theme.muted);
        }
        let toggle_button = toggle_button.on_click(cx.listener(move |this, _, _, cx| {
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

        let behind = match (machine.version.as_deref(), self.local_version.as_deref()) {
            (Some(remote), Some(local)) => is_behind(remote, local),
            _ => false,
        };
        let restart_due = self.pending_restart.as_deref() == Some(machine.id.as_str());
        let restart_confirming = self.confirming_restart.as_deref() == Some(machine.id.as_str());
        let update_id = id.clone();
        let mut update_button = style::small_button(
            format!("pilus-machine-update-{id}"),
            if behind { "Upgrade" } else { "Update" },
            if behind {
                ButtonKind::Secondary
            } else {
                ButtonKind::Ghost
            },
            theme,
            self.fonts().label,
        );
        if busy {
            update_button = update_button.text_color(theme.muted);
        }
        let update_button = update_button.on_click(cx.listener(move |this, _, _, cx| {
            if this.busy.is_some() {
                return;
            }
            this.pending_remove = None;
            this.start_op(
                MachineOp::Update {
                    id: update_id.clone(),
                },
                &update_id,
            );
            cx.notify();
        }));

        let restart_id = id.clone();
        let restart_button = style::small_button(
            format!("pilus-machine-restart-{id}"),
            if restart_confirming {
                "Confirm restart"
            } else {
                "Restart"
            },
            ButtonKind::Ghost,
            theme,
            self.fonts().label,
        )
        .text_color(theme.alert)
        .on_click(cx.listener(move |this, _, _, cx| {
            if this.busy.is_some() {
                return;
            }
            if this.confirming_restart.as_deref() == Some(restart_id.as_str()) {
                this.confirming_restart = None;
                this.start_op(
                    MachineOp::Restart {
                        id: restart_id.clone(),
                    },
                    &restart_id,
                );
            } else {
                this.confirming_restart = Some(restart_id.clone());
                this.status = format!(
                    "Restart `{restart_id}`? This stops every pane in its session. \
                     Click Confirm restart to continue."
                );
            }
            cx.notify();
        }));

        let stop_due = self.pending_stop.as_deref() == Some(machine.id.as_str());
        let stop_confirming = self.confirming_stop.as_deref() == Some(machine.id.as_str());
        let stop_id = id.clone();
        let stop_button = style::small_button(
            format!("pilus-machine-stop-{id}"),
            if stop_confirming {
                "Confirm stop"
            } else {
                "Stop & upgrade"
            },
            ButtonKind::Ghost,
            theme,
            self.fonts().label,
        )
        .text_color(theme.alert)
        .on_click(cx.listener(move |this, _, _, cx| {
            if this.busy.is_some() {
                return;
            }
            if this.confirming_stop.as_deref() == Some(stop_id.as_str()) {
                this.confirming_stop = None;
                this.start_op(
                    MachineOp::StopAndUpdate {
                        id: stop_id.clone(),
                    },
                    &stop_id,
                );
            } else {
                this.confirming_stop = Some(stop_id.clone());
                this.status = format!(
                    "Stop `{stop_id}`'s daemon? Its panes end (the session is saved). \
                     Click Confirm stop to continue."
                );
            }
            cx.notify();
        }));

        let remove_id = id.clone();
        let remove_button = style::small_button(
            format!("pilus-machine-remove-{id}"),
            if confirming { "Confirm" } else { "Remove" },
            ButtonKind::Ghost,
            theme,
            self.fonts().label,
        )
        .text_color(if confirming { theme.alert } else { theme.muted })
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
                this.status = format!("Remove `{remove_id}`? Click Confirm to delete the profile.");
            }
            cx.notify();
        }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_2()
            .py_2()
            .radius_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
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
                                    .text_size(px(self.fonts().body))
                                    .text_color(theme.fg)
                                    .child(machine.display().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(self.fonts().label))
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
                            .text_size(px(self.fonts().label))
                            .text_color(theme.muted)
                            .child(machine.destination.clone())
                            .children(
                                machine
                                    .version
                                    .clone()
                                    .map(|version| div().child(format!("v{version}"))),
                            )
                            .children(behind.then(|| {
                                div().text_color(theme.alert).child(format!(
                                    "behind v{}",
                                    self.local_version.clone().unwrap_or_default()
                                ))
                            }))
                            .child(div().text_color(status_color).child(status)),
                    ),
            )
            .child(toggle_button)
            .children(stop_due.then_some(stop_button))
            .children((machine.enabled && restart_due).then_some(restart_button))
            .child(update_button)
            .child(remove_button)
    }

    /// The saved-machine list.
    fn machine_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div()
            .id("pilus-machine-list")
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .px_3()
            .pt_3()
            .pb_2()
            .overflow_y_scroll();
        list = list.child(style::section_label(theme, "SAVED MACHINES", self.fonts().label).pb_1());
        if self.machines.is_empty() {
            return list.child(
                div()
                    .text_size(px(self.fonts().secondary))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let refresh = style::small_button(
            "pilus-machine-refresh",
            if self.refreshing {
                "Refreshing…"
            } else {
                "Refresh"
            },
            ButtonKind::Secondary,
            theme,
            self.fonts().label,
        )
        .text_color(if self.refreshing {
            theme.muted
        } else {
            theme.fg
        })
        .on_click(cx.listener(|this, _, _, cx| {
            if !this.refreshing {
                this.probe();
            }
            cx.notify();
        }));

        let root = div()
            .id("pilus-machines-window")
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
                    .child(style::title(theme, "Remote Machines", self.fonts().title))
                    .child(refresh),
            )
            .child(self.add_form(window, cx))
            .child(self.machine_list(cx))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(self.fonts().label))
                    .text_color(theme.muted)
                    .child(self.status.clone()),
            );
        super::apply_chrome_font(root, &self.shared_settings)
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
    if let MachineOp::Restart { id } = &op {
        return restart_machine(root, id);
    }
    let stop_note = match &op {
        MachineOp::StopAndUpdate { id } => match stop_machine(root, id) {
            Ok(note) => note,
            Err(outcome) => return outcome,
        },
        _ => String::new(),
    };
    // Local first: the remote can only be as new as the binary that ships it.
    let local_note = match &op {
        MachineOp::Update { .. } => refresh_local_install(),
        MachineOp::StopAndUpdate { .. } => String::new(),
        _ => String::new(),
    };
    let Some(binary) = luvus_binary() else {
        return OpOutcome::Err("luvus binary not found on PATH".to_string());
    };
    if let MachineOp::StopAndUpdate { id } = &op {
        return reinstall_machine(&binary, root, id, &stop_note);
    }
    // A short confirmation instead of the CLI's JSON stdout.
    let success = match &op {
        MachineOp::Add { id, .. } => format!("Added `{id}`."),
        MachineOp::Enable { id, .. } => format!("Enabled `{id}`."),
        MachineOp::Disable { id } => format!("Disabled `{id}`."),
        MachineOp::Remove { id } => format!("Removed `{id}`."),
        MachineOp::Update { id } => format!(
            "{local_note}Installed on `{id}`. Restart it to load the new daemon \
             (this stops its panes)."
        ),
        MachineOp::StopAndUpdate { id } => format!(
            "Stopped the old daemon and installed on `{id}`; it starts on the new Luvus when \
             the machine reconnects."
        ),
        MachineOp::Restart { .. } => String::new(),
    };
    let mut args: Vec<String> = vec!["machine".to_string()];
    let mut update_id = None;
    // The machine to offer Stop & upgrade for when the CLI reports a protocol
    // mismatch; set for both Enable and Update, since either can hit it.
    let mut mismatch_id = None;
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
            mismatch_id = Some(id.clone());
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
        // `enable --install` re-copies the local release's binary and rewrites
        // the profile's versioned `remote_binary`, so it is the CLI's own
        // remote-update path rather than a second install mechanism.
        MachineOp::Update { id } => {
            update_id = Some(id.clone());
            mismatch_id = Some(id.clone());
            args.push("enable".to_string());
            args.push(id);
            args.push("--install".to_string());
        }
        MachineOp::StopAndUpdate { .. } | MachineOp::Restart { .. } => {
            return OpOutcome::Ok(String::new());
        }
    }
    match std::process::Command::new(binary)
        .args(&args)
        .env(HOME_ENV, root)
        .output()
    {
        Ok(output) if output.status.success() => match update_id {
            Some(id) => OpOutcome::Installed {
                id,
                message: success,
            },
            None => OpOutcome::Ok(success),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if let Some(id) = mismatch_id.filter(|_| is_protocol_mismatch(&stderr)) {
                return OpOutcome::NeedsStop {
                    message: format!(
                        "{local_note}`{id}` runs a Luvus too old for this client. Click Stop & \
                         upgrade to stop it (its session is saved) and install the new binary."
                    ),
                    id,
                };
            }
            OpOutcome::Err(if stderr.is_empty() {
                format!("{stop_note}The command failed.")
            } else {
                format!("{stop_note}{stderr}")
            })
        }
        Err(error) => OpOutcome::Err(error.to_string()),
    }
}

/// Whether CLI output says the remote speaks another protocol revision.
///
/// The CLI words this two ways: the handshake's "protocol version mismatch"
/// and the pre-flight "remote display protocol N does not match local M".
fn is_protocol_mismatch(stderr: &str) -> bool {
    stderr.contains("protocol version mismatch") || stderr.contains("does not match local")
}

/// Install the latest Luvus on this Mac when one is newer.
///
/// Returns a sentence for the status line. A failure (offline, or a package
/// manager owns the install) is reported but never stops the remote step: the
/// machine can still be provisioned from whatever is installed here.
fn refresh_local_install() -> String {
    let engine = crate::luvus_update::engine();
    match crate::luvus_update::check(&engine) {
        Ok(Some(release)) => match crate::luvus_update::install(&engine, &release, |_, _| {}) {
            Ok(_) => format!("Updated this Mac to v{}. ", release.version),
            Err(error) => format!("Could not update this Mac ({error}). "),
        },
        Ok(None) => String::new(),
        Err(error) => format!("Could not check for a newer Luvus ({error}). "),
    }
}

/// Stop one machine's old daemon so `enable --install` has nothing to reject.
///
/// An already-stopped or unreachable daemon is not an error here: the install
/// that follows reports any real connectivity problem itself.
fn stop_machine(root: &Path, id: &str) -> Result<String, OpOutcome> {
    let Some(machine) = list_machines(root).into_iter().find(|m| m.id == id) else {
        return Err(OpOutcome::Err(format!(
            "machine `{id}` is not in the catalog"
        )));
    };
    // Kept, not dropped: if the daemon survives, the install that follows
    // fails with the same mismatch, and this says why.
    Ok(match stop_remote(&machine) {
        Ok(output) if output.is_empty() => "Stopped the old daemon. ".to_string(),
        Ok(output) => format!("Stop said: {output}. "),
        Err(error) => format!("Could not stop the old daemon ({error}). "),
    })
}

/// Run one `luvus` command against `root`, returning its stderr on failure.
fn run_luvus(binary: &Path, root: &Path, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new(binary)
        .args(args)
        .env(HOME_ENV, root)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        "The command failed.".to_string()
    } else {
        stderr
    })
}

/// Replace a machine's profile so the new Luvus provisions it from scratch.
///
/// `machine enable --install` keeps the profile's saved `remote_binary` and
/// starts it for the closing handshake, so a pinned old binary rejects the
/// newer client even with its daemon stopped. Removing the profile (which
/// leaves the remote untouched) and adding it again with `--install` has
/// nothing pinned to reuse. If the add fails, the old profile is restored
/// disabled, pointing at its previous binary, so the machine is not lost.
fn reinstall_machine(binary: &Path, root: &Path, id: &str, stop_note: &str) -> OpOutcome {
    let Some(machine) = list_machines(root).into_iter().find(|m| m.id == id) else {
        return OpOutcome::Err(format!("machine `{id}` is not in the catalog"));
    };
    let mut add = vec!["machine", "add", id, "--host", machine.destination.as_str()];
    if !machine.display.is_empty() {
        add.extend(["--label", machine.display.as_str()]);
    }
    if let Some(session) = machine.preferred_session.as_deref() {
        add.extend(["--session", session]);
    }
    if let Err(error) = run_luvus(binary, root, &["machine", "remove", id]) {
        return OpOutcome::Err(format!("{stop_note}could not remove `{id}`: {error}"));
    }
    let mut install = add.clone();
    install.push("--install");
    let Err(error) = run_luvus(binary, root, &install) else {
        return OpOutcome::Ok(format!("{stop_note}Reinstalled `{id}` on the new Luvus."));
    };
    // The install can land the binary and still fail its closing handshake.
    // Point a fresh profile at that copy and enable it, which is the sequence
    // that works by hand.
    if let Some(version) = crate::luvus_update::installed_version().map(|v| v.to_string())
        && let Some(path) = find_installed_binary(&machine, &version)
    {
        let mut pinned = add.clone();
        pinned.extend(["--disabled", "--remote-binary", path.as_str()]);
        if run_luvus(binary, root, &pinned).is_ok() {
            return match run_luvus(binary, root, &["machine", "enable", id]) {
                Ok(()) => OpOutcome::Ok(format!(
                    "{stop_note}Reinstalled `{id}` on the new Luvus (install reported: {error})."
                )),
                Err(enable_error) => OpOutcome::Err(format!(
                    "{stop_note}Installed {path} but enabling `{id}` failed: {enable_error}. \
                     The profile is saved disabled."
                )),
            };
        }
    }
    let mut restore = add;
    restore.push("--disabled");
    if !machine.remote_binary.is_empty() {
        restore.extend(["--remote-binary", machine.remote_binary.as_str()]);
    }
    let restored = match run_luvus(binary, root, &restore) {
        Ok(()) => "The old profile was restored, disabled.".to_string(),
        Err(restore_error) => format!("Restoring the old profile also failed: {restore_error}"),
    };
    OpOutcome::Err(format!("{stop_note}{error}\n{restored}"))
}

/// Restart one machine's daemon through its freshly provisioned binary.
fn restart_machine(root: &Path, id: &str) -> OpOutcome {
    let Some(machine) = list_machines(root).into_iter().find(|m| m.id == id) else {
        return OpOutcome::Err(format!("machine `{id}` is not in the catalog"));
    };
    match restart_remote(&machine) {
        Ok(_) => OpOutcome::Ok(format!("Restarted `{id}` on the new Luvus.")),
        Err(error) => OpOutcome::Err(error.to_string()),
    }
}

/// Open the Remote Machines window.
pub fn open(
    root: PathBuf,
    signal: Rc<RefCell<MachineSignal>>,
    shared_theme: Rc<RefCell<Theme>>,
    shared_settings: Rc<RefCell<Settings>>,
    cx: &mut App,
) {
    let bounds = Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(WINDOW_MIN_SIZE.0), px(WINDOW_MIN_SIZE.1))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Remote Machines".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            cx.new(move |cx| {
                MachineManagerView::new(root, signal, shared_theme, shared_settings, window, cx)
            })
        },
    );
    if let Err(error) = opened {
        eprintln!("pilus: could not open the machines window: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::is_protocol_mismatch;

    #[test]
    fn both_cli_wordings_count_as_a_protocol_mismatch() {
        assert!(is_protocol_mismatch(
            "Error: remote Luvus rejected the client: protocol version mismatch"
        ));
        assert!(is_protocol_mismatch(
            "no remote changes made\n\nCaused by:\n    remote display protocol 18 does not match local 20"
        ));
        assert!(!is_protocol_mismatch("Host key verification failed."));
    }
}
