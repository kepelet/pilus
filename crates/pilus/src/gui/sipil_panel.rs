//! The sipil tab of the right sidebar: this device's ID, and the people and
//! sipil daemons it talks to, and the channels.
//!
//! The panel holds no connection of its own. [`sipil`] owns the daemons'
//! state; this reads a snapshot to paint and sends it intent. A daemon is
//! added from its own machine (`sipil connect`), which shows up here as a
//! request to confirm, and removed here. Provider keys and the model are not
//! here: each daemon takes them from its own environment.
//!
//! People come first because every pilus is a sipil: this device's ID is what
//! another pilus adds to talk to it, and what a daemon's `npm run connect` names.
//! Adding a contact is a promise to listen to them, so it is mutual by hand: they
//! add this device too, or their messages are refused here and ours there.
//!
//! Any number of daemons can be connected, a Researcher and a Chief of Staff
//! say. Who one is shared with is set on its own page in the main window, the
//! way who is in a channel is.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, ClipboardItem, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    Render, Window, div, prelude::*, px,
};
use pilus_client::pilus_config_dir;

use crate::settings::Settings;

use super::sipil::{self, Daemon, Phase, Reach, SipilState};
use super::style::{self, ButtonKind, RadiusExt};
use super::text_input::TextInput;
use super::theme::Theme;
/// Side of the status dot.
const DOT: f32 = 8.0;
/// The widest a contact's name is drawn before it is cut short.
const NAME_MAX: f32 = 150.0;
/// Width of the column a row's dot or spinner sits in, so the names line up.
const LEAD: f32 = 12.0;

/// One input the keyboard can sit in.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Slot {
    /// A new contact's device ID or ticket.
    ContactId,
    /// A new contact's name.
    ContactName,
    /// The new name of the contact being renamed.
    Rename,
    /// A new channel's name.
    ChannelName,
}

/// What a row of the contacts list is made of.
struct Row<'a> {
    /// The device ID, which keys the row's buttons.
    id: &'a str,
    name: &'a str,
    /// The dot or spinner at its head.
    lead: gpui::AnyElement,
    /// How they are doing, said after the start of the ID.
    status: &'a str,
    /// Lines under the row: what they share, or what went wrong.
    notes: Vec<(String, gpui::Rgba)>,
    /// Forget them.
    remove: Box<dyn Fn(&mut SipilPanel)>,
}

/// What the panel tells the window around it.
pub enum PanelEvent {
    /// Escape was pressed with no field to leave: hand the keyboard back.
    Leave,
}

/// The sipil tab's view.
pub struct SipilPanel {
    /// The app's live chrome palette, so this panel matches the scheme.
    shared_theme: Rc<RefCell<Theme>>,
    /// Shared client settings: the address to remember and the Interface font.
    shared_settings: Rc<RefCell<Settings>>,
    theme: Theme,
    state: Arc<SipilState>,
    /// The new contact's device ID or ticket.
    contact_id: Entity<TextInput>,
    /// The new contact's name.
    contact_name: Entity<TextInput>,
    /// The new name being typed for the contact being renamed.
    rename_input: Entity<TextInput>,
    /// The contact being renamed, by device ID.
    renaming: Option<String>,
    /// A new channel's name.
    channel_name: Entity<TextInput>,
    /// The contacts picked for a new channel, by device ID.
    channel_members: BTreeSet<String>,
    /// How many contacts had been added when the form was last cleared.
    added_seen: u64,
    /// A problem with what was typed, shown in place of sipil's own message.
    local_error: Option<String>,
    /// What was last copied.
    copied: Option<String>,
    focus_handle: FocusHandle,
}

impl SipilPanel {
    pub(super) fn new(
        shared_theme: Rc<RefCell<Theme>>,
        shared_settings: Rc<RefCell<Settings>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let theme = *shared_theme.borrow();
        let fonts = style::Fonts::resolve(&shared_settings);
        let font = style::chrome_font(&shared_settings);
        // The identity is made here, not at launch, so a person who never opens
        // sipil never gets a key file. The shots must not write the real one.
        #[cfg(not(feature = "visual-test"))]
        sipil::prepare();
        let make = |value: String, placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextInput::new(
                    value,
                    placeholder,
                    font.clone(),
                    px(fonts.body),
                    false,
                    theme,
                    cx,
                )
            })
        };
        let contact_id = make(String::new(), "Paste it here", cx);
        let contact_name = make(String::new(), "What to call them", cx);
        let rename_input = make(String::new(), "Name", cx);
        let channel_name = make(String::new(), "project-x", cx);
        let view = Self {
            shared_theme,
            shared_settings,
            theme,
            state: sipil::snapshot(),
            contact_id,
            contact_name,
            rename_input,
            renaming: None,
            channel_name,
            channel_members: BTreeSet::new(),
            added_seen: 0,
            local_error: None,
            copied: None,
            focus_handle,
        };

        cx.spawn(async move |this, cx| {
            loop {
                let updated = this.update(cx, |view, cx| {
                    if view.poll(cx) {
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

    /// Take what sipil last said. Returns whether a repaint is needed.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let theme = *self.shared_theme.borrow();
        if theme != self.theme {
            self.theme = theme;
            changed = true;
        }
        let state = sipil::snapshot();
        if Arc::ptr_eq(&state, &self.state) {
            return changed;
        }
        self.state = state;
        if self.state.me.added != self.added_seen {
            self.added_seen = self.state.me.added;
            self.contact_id.update(cx, |input, cx| input.clear(cx));
            self.contact_name.update(cx, |input, cx| input.clear(cx));
            self.channel_name.update(cx, |input, cx| input.clear(cx));
            self.channel_members.clear();
        }
        true
    }

    /// Every input in the order Tab walks them.
    fn slots(&self) -> Vec<Slot> {
        let mut slots = vec![Slot::ContactId, Slot::ContactName, Slot::ChannelName];
        if self.renaming.is_some() {
            slots.insert(0, Slot::Rename);
        }
        slots
    }

    fn entity(&self, slot: &Slot) -> Option<Entity<TextInput>> {
        match slot {
            Slot::ContactId => Some(self.contact_id.clone()),
            Slot::ContactName => Some(self.contact_name.clone()),
            Slot::Rename => Some(self.rename_input.clone()),
            Slot::ChannelName => Some(self.channel_name.clone()),
        }
    }

    /// The input that currently owns the keyboard, if any.
    fn focused(&self, window: &Window, cx: &App) -> Option<Slot> {
        self.slots().into_iter().find(|slot| {
            self.entity(slot)
                .is_some_and(|entity| entity.read(cx).is_focused(window))
        })
    }

    fn focus(&self, slot: &Slot, window: &mut Window, cx: &mut App) {
        if let Some(entity) = self.entity(slot) {
            let handle = entity.focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    // Intent

    fn add_contact(&mut self, cx: &mut Context<Self>) {
        let id = self.contact_id.read(cx).value();
        let name = self.contact_name.read(cx).value();
        self.copied = None;
        self.local_error = sipil::add_contact(&id, &name).err();
    }

    /// Start renaming a contact: the name becomes a field, filled with the old one.
    fn start_rename(&mut self, id: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(id.to_string());
        self.rename_input
            .update(cx, |input, cx| input.set_value(name.to_string(), cx));
        self.focus(&Slot::Rename, window, cx);
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.renaming.take() {
            let name = self.rename_input.read(cx).value();
            sipil::rename_contact(&id, &name);
        }
    }

    fn create_channel(&mut self, cx: &mut Context<Self>) {
        let name = self.channel_name.read(cx).value();
        let members: Vec<String> = self.channel_members.iter().cloned().collect();
        self.copied = None;
        self.local_error = sipil::create_channel(&name, members).err();
    }

    /// Put `text` on the clipboard and say so in the status line.
    fn copy(&mut self, text: String, what: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.local_error = None;
        self.copied = Some(format!("Copied {what}."));
    }

    fn set_reachable(&mut self, on: bool) {
        sipil::set_reachable(on);
        let mut settings = self.shared_settings.borrow_mut();
        settings.sipil_online = on;
        settings.save(&pilus_config_dir());
    }

    fn remove_daemon(&mut self, id: &str) {
        self.local_error = None;
        sipil::remove_daemon(id);
    }

    /// Escape leaves the focused input, then hands the keyboard back. Tab walks the
    /// inputs; Enter acts on the one it is in. Every other editing key belongs
    /// to the focused input, which consumes what it handles before it reaches
    /// here.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let focused = self.focused(window, cx);
        if key == "escape" && focused == Some(Slot::Rename) {
            self.renaming = None;
            let handle = self.focus_handle.clone();
            window.focus(&handle, cx);
            cx.notify();
            return;
        }
        if key == "escape" {
            if focused.is_some() {
                let handle = self.focus_handle.clone();
                window.focus(&handle, cx);
            } else {
                cx.emit(PanelEvent::Leave);
            }
            cx.notify();
            return;
        }
        let Some(slot) = focused else {
            return;
        };
        match key {
            "tab" => {
                let slots = self.slots();
                let at = slots.iter().position(|candidate| *candidate == slot);
                let count = slots.len();
                let next = match (at, event.keystroke.modifiers.shift) {
                    (Some(at), false) => (at + 1) % count,
                    (Some(at), true) => (at + count - 1) % count,
                    (None, _) => 0,
                };
                if let Some(next) = slots.get(next) {
                    self.focus(next, window, cx);
                }
            }
            "enter" => match &slot {
                Slot::ContactId | Slot::ContactName => self.add_contact(cx),
                Slot::Rename => self.commit_rename(cx),
                Slot::ChannelName => self.create_channel(cx),
            },
            _ => {}
        }
        cx.notify();
    }

    // Painting

    fn fonts(&self) -> style::Fonts {
        style::Fonts::resolve(&self.shared_settings)
    }

    /// A labelled single-line input, restyled to the current scheme.
    fn input_box(
        &self,
        id: String,
        label: &str,
        entity: Entity<TextInput>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        entity.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let focused = entity.read(cx).is_focused(window);
        div().flex_1().min_w_0().child(style::field(
            id,
            label,
            focused,
            theme,
            28.0,
            fonts.label,
            entity,
        ))
    }

    /// One daemon, in the same row a person has: its name (renamable), the start
    /// of its ID and how it is doing, then Rename and Remove. What it last
    /// complained of goes under the row.
    fn daemon_block(&self, daemon: &Daemon, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let (lead, status, detail) = match (daemon.phase, &daemon.error) {
            (Phase::Connected, _) => (dot(theme.ok).into_any_element(), "online", None),
            (Phase::Connecting, error) => (
                style::spinner(
                    format!("pilus-sipil-connecting-{}", daemon.id),
                    fonts.label,
                    theme.muted,
                )
                .into_any_element(),
                if error.is_some() {
                    "reconnecting"
                } else {
                    "connecting…"
                },
                error.clone().map(|error| (error, theme.muted)),
            ),
            (Phase::Off, Some(error)) => (
                dot(theme.alert).into_any_element(),
                "offline",
                Some((error.clone(), theme.alert)),
            ),
            (Phase::Off, None) => (dot(theme.muted).into_any_element(), "not connected", None),
        };
        let id = daemon.id.clone();
        let mut block = self.contact_row(
            Row {
                id: &daemon.id,
                name: &daemon.name(),
                lead,
                status,
                notes: detail.into_iter().collect(),
                remove: Box::new(move |this| this.remove_daemon(&id)),
            },
            window,
            cx,
        );
        // Whatever the daemon said, the fix for "not allowed" is the same
        // command, so it gets a button of its own.
        let refused = daemon
            .error
            .as_deref()
            .is_some_and(|error| error.contains("npm run connect"));
        if let (true, Some(id)) = (refused && !daemon.connected(), &self.state.me.device_id) {
            let command = format!("npm run connect -- --device-id {id}");
            block = block.child(
                div()
                    .pl(px(LEAD + 8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        style::small_button(
                            format!("pilus-sipil-copy-allow-{}", daemon.id),
                            "Copy command",
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(command.clone(), "the command", cx);
                            cx.notify();
                        })),
                    ),
            );
        }
        block
    }

    /// Whether others may reach this pilus, and when they may, the ID they add.
    fn device_section(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let me = &self.state.me;
        let id = me.device_id.clone().unwrap_or_default();
        let copy_id = {
            let id = id.clone();
            style::small_button(
                "pilus-sipil-copy-id",
                "Copy ID",
                ButtonKind::Secondary,
                theme,
                fonts.label,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.copy(id.clone(), "your device ID", cx);
                cx.notify();
            }))
        };
        let on = me.reachable;
        let toggle = style::toggle("pilus-sipil-reachable", on, theme).on_click(cx.listener(
            move |this, _, _, cx| {
                this.set_reachable(!on);
                cx.notify();
            },
        ));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.fg)
                            .child("Let contacts reach this pilus"),
                    )
                    .child(toggle),
            )
            .when(!on, |section| {
                section.child(hint(
                    theme,
                    fonts,
                    "Turn this on to get a device ID, add contacts and connect sipil agents.",
                ))
            })
            .when(on, |section| {
                section
                    .child(div().pt_2().child(style::section_label(
                        theme,
                        "THIS DEVICE",
                        fonts.label,
                    )))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("pilus-sipil-device-id")
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(style::mono_family(&self.shared_settings))
                                    .text_size(px(fonts.label))
                                    .text_color(if id.is_empty() { theme.muted } else { theme.fg })
                                    // The middle goes, and the tooltip has all of it.
                                    .child(if id.is_empty() {
                                        "No identity yet".to_string()
                                    } else {
                                        shorten_id(&id)
                                    })
                                    .when(!id.is_empty(), |text| {
                                        text.tooltip(super::row_tooltip(
                                            id.clone(),
                                            theme.panel_fg,
                                            Vec::new(),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(copy_id),
                    )
                    .child(hint(
                        theme,
                        fonts,
                        "Your device ID is how others find you, and how a sipil agent knows \
                         it is you. Only people you add can message you.",
                    ))
            })
            .when_some(me.error.clone(), |section, error| {
                section.child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.alert)
                        .child(error),
                )
            })
    }

    /// The channels: those this pilus is in, and a form to make one.
    fn channels_section(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let me = &self.state.me;
        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .child(style::section_label(theme, "CHANNELS", fonts.label));
        if me.channels.is_empty() {
            section = section.child(hint(
                theme,
                fonts,
                "No channels yet. A channel is a named room for several of your contacts; \
                 mention an agent in it with @name to bring it in.",
            ));
        }
        for row in &me.channels {
            let id = row.channel.id.clone();
            let names: Vec<String> = row
                .channel
                .members
                .iter()
                .map(|member| {
                    me.people
                        .iter()
                        .find(|person| person.contact.id == *member)
                        .map_or_else(
                            || member.chars().take(8).collect(),
                            |person| person.contact.name.clone(),
                        )
                })
                .collect();
            let members = if names.is_empty() {
                "just you".to_string()
            } else {
                names.join(", ")
            };
            section = section.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_size(px(fonts.secondary))
                            .text_color(theme.fg)
                            .child(format!("#{}", row.channel.name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(members),
                    )
                    .child(
                        style::small_button(
                            format!("pilus-sipil-leave-{id}"),
                            "Leave",
                            ButtonKind::Ghost,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            sipil::leave_channel(&id);
                            cx.notify();
                        })),
                    ),
            );
        }
        let name_input = self.input_box(
            "pilus-sipil-channel-name".to_string(),
            "New channel",
            self.channel_name.clone(),
            window,
            cx,
        );
        let mut picks = div().flex().flex_row().flex_wrap().gap_3();
        for person in &me.people {
            let id = person.contact.id.clone();
            let on = self.channel_members.contains(&id);
            picks = picks.child(
                style::checkbox(
                    format!("pilus-sipil-channel-member-{id}"),
                    on,
                    person.contact.name.clone(),
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.channel_members.remove(&id) {
                        this.channel_members.insert(id.clone());
                    }
                    cx.notify();
                })),
            );
        }
        section
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap_2()
                    .child(name_input)
                    .child(
                        style::button(
                            "pilus-sipil-create-channel",
                            "Create",
                            ButtonKind::Primary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.create_channel(cx);
                            cx.notify();
                        })),
                    ),
            )
            .when(!me.people.is_empty(), |section| section.child(picks))
    }

    /// One contact: their name (renamable) and whether they are there.
    fn person_block(&self, person: &sipil::Person, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let (color, status) = match person.reach {
            Reach::Online => (theme.ok, "online"),
            Reach::Waiting => (theme.muted, "has not added you yet"),
            Reach::Offline => (theme.muted, "offline"),
            Reach::Unknown => (theme.muted, ""),
        };
        let id = person.contact.id.clone();
        self.contact_row(
            Row {
                id: &person.contact.id,
                name: &person.contact.name,
                lead: dot(color).into_any_element(),
                status,
                notes: Vec::new(),
                remove: Box::new(move |_| sipil::remove_contact(&id)),
            },
            window,
            cx,
        )
    }

    /// A row of the contacts list, the same for a person and a sipil daemon so
    /// they line up: a status mark, the name, the start of the ID with how they
    /// are doing, and the Rename and Remove buttons. A name being edited becomes
    /// a field with Save and Cancel.
    fn contact_row(&self, row: Row, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let Row {
            id,
            name,
            lead,
            status,
            notes,
            remove,
        } = row;
        let short: String = id.chars().take(8).collect();
        let renaming = self.renaming.as_deref() == Some(id);
        let name_view: gpui::AnyElement = if renaming {
            let entity = self.rename_input.clone();
            self.input_box(format!("pilus-sipil-rename-{id}"), "", entity, window, cx)
                .into_any_element()
        } else {
            // The name keeps its room; the status after it is what gives way.
            div()
                .flex_shrink_0()
                .max_w(px(NAME_MAX))
                .truncate()
                .text_size(px(fonts.secondary))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.fg)
                .child(name.to_string())
                .into_any_element()
        };
        let actions = if renaming {
            div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .gap_1()
                .child(
                    style::small_button(
                        format!("pilus-sipil-rename-save-{id}"),
                        "Save",
                        ButtonKind::Primary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.commit_rename(cx);
                        cx.notify();
                    })),
                )
                .child(
                    style::small_button(
                        format!("pilus-sipil-rename-cancel-{id}"),
                        "Cancel",
                        ButtonKind::Ghost,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.renaming = None;
                        cx.notify();
                    })),
                )
        } else {
            let (rename_id, rename_name) = (id.to_string(), name.to_string());
            div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .gap_1()
                .child(
                    style::small_button(
                        format!("pilus-sipil-rename-{id}-start"),
                        "Rename",
                        ButtonKind::Ghost,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_rename(&rename_id, &rename_name, window, cx);
                        cx.notify();
                    })),
                )
                .child(
                    style::small_button(
                        format!("pilus-sipil-remove-contact-{id}"),
                        "Remove",
                        ButtonKind::Ghost,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        remove(this);
                        cx.notify();
                    })),
                )
        };
        let top = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(LEAD))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .child(lead),
            )
            .child(name_view)
            .when(!renaming, |top| {
                top.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_right()
                        .font_family(style::mono_family(&self.shared_settings))
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(format!("{short}… {status}")),
                )
            })
            .child(actions);

        let mut block = div().flex().flex_col().gap_1().child(top);
        for (text, color) in notes {
            block = block.child(
                div()
                    .pl(px(LEAD + 8.0))
                    .text_size(px(fonts.label))
                    .text_color(color)
                    .child(text),
            );
        }
        block
    }

    /// The people: who asked to talk, who is added, and the form to add more.
    fn contacts_section(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = self.fonts();
        let me = &self.state.me;
        let mut section = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .child(style::section_label(theme, "CONTACTS", fonts.label));

        for request in &me.requests {
            let id = request.id.clone();
            let (accept_id, ignore_id) = (id.clone(), id.clone());
            let short: String = id.chars().take(8).collect();
            let (what, button) = if request.daemon {
                let name = request.label.clone().unwrap_or_else(|| short.clone());
                (
                    format!("{name}, a sipil agent, wants to connect"),
                    "Connect",
                )
            } else {
                (format!("{short}… wants to talk to you"), "Add")
            };
            section = section.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .radius_md()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.fg)
                            .child(what),
                    )
                    .child(
                        style::small_button(
                            format!("pilus-sipil-accept-{id}"),
                            button,
                            ButtonKind::Primary,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            sipil::accept_request(&accept_id);
                            cx.notify();
                        })),
                    )
                    .child(
                        style::small_button(
                            format!("pilus-sipil-ignore-{id}"),
                            "Ignore",
                            ButtonKind::Ghost,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            sipil::dismiss_request(&ignore_id);
                            cx.notify();
                        })),
                    ),
            );
        }

        if me.people.is_empty() && me.requests.is_empty() && self.state.daemons.is_empty() {
            section = section.child(hint(
                theme,
                fonts,
                "No contacts yet. Add someone by the device ID they send you, and send \
                 them yours: you each add the other.",
            ));
        }
        for daemon in &self.state.daemons {
            section = section.child(self.daemon_block(daemon, window, cx));
        }
        for person in &me.people {
            section = section.child(self.person_block(person, window, cx));
        }

        section = section.child(
            div()
                .pt_2()
                .text_size(px(fonts.secondary))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.fg)
                .child("Add sipil contact"),
        );
        let id_input = self.input_box(
            "pilus-sipil-contact-id".to_string(),
            "Device ID",
            self.contact_id.clone(),
            window,
            cx,
        );
        let name_input = self.input_box(
            "pilus-sipil-contact-name".to_string(),
            "Name",
            self.contact_name.clone(),
            window,
            cx,
        );
        section.child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap_2()
                .child(id_input)
                .child(div().w(px(140.0)).flex_shrink_0().flex().child(name_input))
                .child(
                    style::button(
                        "pilus-sipil-add-contact",
                        "Add",
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_contact(cx);
                        cx.notify();
                    })),
                ),
        )
    }

    /// The last thing worth saying: a typing problem, a refusal, or a
    /// confirmation.
    fn status_line(&self) -> Div {
        let theme = self.theme;
        let me = &self.state.me;
        let daemon_error = self
            .state
            .daemons
            .iter()
            .find(|daemon| daemon.connected() && daemon.error.is_some())
            .and_then(|daemon| daemon.error.clone());
        let (text, color) = if let Some(error) = self
            .local_error
            .clone()
            .or_else(|| me.contact_error.clone())
            .or_else(|| me.daemon_error.clone())
            .or(daemon_error)
        {
            (error, theme.alert)
        } else if let Some(copied) = self.copied.clone() {
            (copied, theme.muted)
        } else {
            (String::new(), theme.muted)
        };
        div()
            .px_3()
            .py_2()
            .min_h(px(self.fonts().label * 2.4))
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(self.fonts().label))
            .text_color(color)
            .child(text)
    }
}

/// A device ID with its middle left out, so it fits one line and can still be
/// told from another by both ends.
fn shorten_id(id: &str) -> String {
    if id.chars().count() <= 20 {
        return id.to_string();
    }
    let head: String = id.chars().take(10).collect();
    let tail: String = id
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

/// A line of explanation under a control.
fn hint(theme: Theme, fonts: style::Fonts, text: &str) -> Div {
    div()
        .text_size(px(fonts.label))
        .text_color(theme.muted)
        .child(text.to_string())
}

/// A filled status dot.
fn dot(color: gpui::Rgba) -> Div {
    div()
        .w(px(DOT))
        .h(px(DOT))
        .flex_shrink_0()
        .radius_full()
        .bg(color)
}

impl Render for SipilPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let fonts = self.fonts();
        let root = div()
            .id("pilus-sipil-panel")
            .relative()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .text_color(theme.fg)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .pt_3()
                    .child(style::title(theme, "sipil", fonts.title))
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(
                                "Every pilus is a sipil. Talk to the people you add, and \
                                 to a durable sipil agent that keeps its conversations \
                                 across restarts.",
                            ),
                    ),
            )
            .child(
                div()
                    .id("pilus-sipil-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .pt_4()
                    .pb_3()
                    .child(self.device_section(cx))
                    // Nothing else is of use to a pilus nobody can reach.
                    .when(self.state.me.reachable, |panel| {
                        panel
                            .child(self.contacts_section(window, cx))
                            .child(self.channels_section(window, cx))
                    }),
            )
            .child(self.status_line());
        super::apply_chrome_font(root, &self.shared_settings)
    }
}

impl EventEmitter<PanelEvent> for SipilPanel {}

impl Focusable for SipilPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
