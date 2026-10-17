//! The note flow over a rendered markdown document.
//!
//! A selection offers **Copy** and **Add note**. Adding a note opens a small
//! composer over the foot of the document, quoting the excerpt; saving keeps
//! it (see `md_notes`). A **Notes** pill at the top right opens the list of the
//! file's notes, where each one can be ticked, edited, or deleted, and the
//! ticked ones sent to a running agent as a single prompt. The overlays are
//! drawn by `md_view` inside the document's own surface, so they work the same
//! on the Files tab and on the Diff's rendered view.
//!
//! The composer's field is one entity for the whole window, focused while a
//! draft is open, as the menu prompt's is.

use std::collections::HashSet;
use std::path::Path;

use gpui::{AnyElement, Context, Div, KeyDownEvent, MouseButton, Stateful, div, prelude::*, px};

use super::md_notes::{ExcerptNote, NewNote, NoteBook, notes_prompt};
use super::md_view::Page;
use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::toast::ToastKind;
use super::{PilusView, agent_icon, icon};

/// A note being written: the excerpt it quotes, or the note being reworded.
pub(super) struct Draft {
    page: Page,
    lines: Option<(u32, u32)>,
    quote: String,
    editing: Option<u64>,
}

/// The window's notes: the saved book, the draft, and which are ticked.
pub(super) struct MdNotes {
    book: NoteBook,
    draft: Option<Draft>,
    /// The list of the open file's notes is showing.
    panel: bool,
    /// Notes ticked to send.
    ticked: HashSet<u64>,
}

impl MdNotes {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            book: NoteBook::load(root),
            draft: None,
            panel: false,
            ticked: HashSet::new(),
        }
    }
}

/// A note panel's surface: the floating card the composer, the list, and the
/// selection bar share. It sits over the document, which would start a new
/// selection on a press, so it swallows the press.
fn surface(theme: super::theme::Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_2()
        .radius_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

impl PilusView {
    /// Whether the note field holds the keyboard: a draft is open on a
    /// document that is on screen.
    pub(super) fn md_composing(&self) -> bool {
        self.md_notes
            .draft
            .as_ref()
            .is_some_and(|draft| match draft.page {
                Page::Files => self.files_visible() && self.files_panel.doc_path().is_some(),
                Page::Diff => self.diff_visible() && self.diff_panel.md_view().is_some(),
            })
    }

    /// Keys for the composer, after its field has had its turn.
    pub(super) fn md_note_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.md_notes.draft = None;
                cx.notify();
            }
            "enter" if !event.keystroke.modifiers.shift => self.md_save_note(false, cx),
            _ => {}
        }
    }

    /// The machine and path that identify `page`'s open document.
    fn md_doc_key(&self, page: Page) -> Option<(String, String)> {
        Some((self.active.key(), self.md_path(page)?))
    }

    /// Everything the notes flow floats over the document: the composer or the
    /// selection bar, and the notes pill or list.
    pub(super) fn md_overlays(&self, page: Page, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        let Some((endpoint, path)) = self.md_doc_key(page) else {
            return overlays;
        };
        let count = self.md_notes.book.for_doc(&endpoint, &path).count();
        let composing = self
            .md_notes
            .draft
            .as_ref()
            .is_some_and(|draft| draft.page == page);
        if composing {
            overlays.push(self.md_composer(page, cx));
        } else if let Some(bar) = self.md_selection_bar(page, cx) {
            overlays.push(bar);
        }
        if count > 0 {
            overlays.push(if self.md_notes.panel {
                self.md_notes_list(page, &endpoint, &path, cx)
            } else {
                self.md_notes_pill(page, count, cx)
            });
        }
        overlays
    }

    /// What a selection can do: copy it, or write a note on it. Nothing shows
    /// while the pointer is still selecting.
    fn md_selection_bar(&self, page: Page, cx: &mut Context<Self>) -> Option<AnyElement> {
        let words = self.md_selected_words(page)?;
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let id = match page {
            Page::Files => "pilus-md-bar-files",
            Page::Diff => "pilus-md-bar-diff",
        };
        let bar = surface(theme).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .px_1()
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .child(format!(
                            "{words} {}",
                            if words == 1 { "word" } else { "words" }
                        )),
                )
                .child(
                    style::small_button(
                        format!("{id}-copy"),
                        "Copy",
                        ButtonKind::Ghost,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.md_copy(page, cx))),
                )
                .child(
                    style::small_button(
                        format!("{id}-note"),
                        "Add note",
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.md_begin_note(page, cx))),
                ),
        );
        Some(self.md_foot(id, bar, 420.0))
    }

    /// `card` centered over the document's foot.
    fn md_foot(&self, id: &'static str, card: Div, width: f32) -> AnyElement {
        div()
            .absolute()
            .left(px(12.0))
            .right(px(12.0))
            .bottom(px(10.0))
            .flex()
            .justify_center()
            .child(style::enter(
                id,
                card.max_w(px(style::scaled(&self.shared_settings, width))),
            ))
            .into_any_element()
    }

    /// The form that turns the quoted excerpt into a note.
    fn md_composer(&self, page: Page, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        self.md_note_input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let Some(draft) = &self.md_notes.draft else {
            return div().into_any_element();
        };
        let place = match draft.lines {
            Some((first, last)) if first != last => format!("LINES {first}–{last}"),
            Some((line, _)) => format!("LINE {line}"),
            None => "EXCERPT".to_string(),
        };
        let heading = if draft.editing.is_some() {
            format!("EDIT NOTE · {place}")
        } else {
            format!("NOTE · {place}")
        };
        let card = surface(theme)
            .w_full()
            .p_3()
            .border_color(theme.border_focus)
            .child(style::section_label(theme, heading, fonts.label))
            .child(
                div()
                    .pl_2()
                    .border_l_2()
                    .border_color(theme.border)
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .line_clamp(2)
                    .whitespace_normal()
                    .child(draft.quote.clone()),
            )
            .child(
                div()
                    .min_h(px(style::scaled(&self.shared_settings, 56.0)))
                    .radius_md()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(theme.border)
                    .px_2()
                    .py_1()
                    .child(self.md_note_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(
                        style::small_button(
                            "pilus-md-note-cancel",
                            "Cancel",
                            ButtonKind::Ghost,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.md_notes.draft = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        style::small_button(
                            "pilus-md-note-save",
                            "Save",
                            ButtonKind::Secondary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.md_save_note(false, cx))),
                    )
                    .child(
                        style::small_button(
                            "pilus-md-note-save-send",
                            "Save & send…",
                            ButtonKind::Primary,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.md_save_note(true, cx))),
                    ),
            );
        let _ = page;
        self.md_foot("pilus-md-composer", card, 480.0)
    }

    /// The pill at the document's top right that opens the notes list.
    fn md_notes_pill(&self, page: Page, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let id = match page {
            Page::Files => "pilus-md-pill-files",
            Page::Diff => "pilus-md-pill-diff",
        };
        div()
            .absolute()
            .top(px(10.0))
            .right(px(14.0))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                style::small_button(
                    id,
                    format!("Notes · {count}"),
                    ButtonKind::Secondary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.md_notes.panel = true;
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    /// The file's notes, each tickable, and the agents to send the ticked ones to.
    fn md_notes_list(
        &self,
        page: Page,
        endpoint: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let notes: Vec<&ExcerptNote> = self.md_notes.book.for_doc(endpoint, path).collect();
        let ticked = notes
            .iter()
            .filter(|note| self.md_notes.ticked.contains(&note.id))
            .count();
        let mut rows = div().flex().flex_col().gap_2();
        for note in &notes {
            let id = note.id;
            let on = self.md_notes.ticked.contains(&id);
            let place = match note.lines {
                Some((first, last)) if first != last => format!("lines {first}–{last}"),
                Some((line, _)) => format!("line {line}"),
                None => "excerpt".to_string(),
            };
            let status = match &note.sent_to {
                Some(agent) => format!("{place} · sent to {agent}"),
                None => place,
            };
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .p_2()
                    .radius_md()
                    .bg(theme.panel_dim)
                    .child(
                        div().pt(px(2.0)).child(
                            style::checkbox(
                                format!("pilus-md-tick-{id}"),
                                on,
                                "",
                                theme,
                                fonts.body,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.md_toggle_note(id);
                                    cx.notify();
                                },
                            )),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(fonts.label))
                                    .text_color(theme.muted)
                                    .child(status),
                            )
                            .child(
                                div()
                                    .pl_2()
                                    .border_l_2()
                                    .border_color(theme.border)
                                    .text_size(px(fonts.label))
                                    .text_color(theme.muted)
                                    .line_clamp(2)
                                    .whitespace_normal()
                                    .child(note.quote.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.secondary))
                                    .text_color(theme.panel_fg)
                                    .whitespace_normal()
                                    .child(note.body.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(self.md_note_action(
                                format!("pilus-md-edit-{id}"),
                                include_bytes!("../../assets/icons/pencil.svg"),
                                ButtonKind::Ghost,
                                cx.listener(move |this, _, _, cx| this.md_edit_note(page, id, cx)),
                            ))
                            .child(self.md_note_action(
                                format!("pilus-md-delete-{id}"),
                                include_bytes!("../../assets/icons/trash-2.svg"),
                                ButtonKind::Danger,
                                cx.listener(move |this, _, _, cx| this.md_delete_note(id, cx)),
                            )),
                    ),
            );
        }
        let all_ticked = ticked == notes.len();
        let controls = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("{ticked} of {} to send", notes.len())),
            )
            .child(
                style::small_button(
                    "pilus-md-tick-all",
                    if all_ticked { "Untick all" } else { "Tick all" },
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.md_tick_all(page, !all_ticked);
                    cx.notify();
                })),
            );
        let card = surface(theme)
            .w_full()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(style::section_label(
                        theme,
                        format!("NOTES · {}", notes.len()),
                        fonts.label,
                    ))
                    .child(
                        style::small_button(
                            "pilus-md-notes-close",
                            "Close",
                            ButtonKind::Ghost,
                            theme,
                            fonts.secondary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.md_notes.panel = false;
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .id("pilus-md-notes-rows")
                    .max_h(px(style::scaled(&self.shared_settings, 260.0)))
                    .overflow_y_scroll()
                    .child(rows),
            )
            .child(controls)
            .child(style::section_label(theme, "SEND TO", fonts.label))
            .child(self.md_agent_rows(page, ticked > 0, cx));
        div()
            .absolute()
            .top(px(10.0))
            .left(px(12.0))
            .right(px(14.0))
            .flex()
            .justify_end()
            .child(style::enter(
                "pilus-md-notes-list",
                card.max_w(px(style::scaled(&self.shared_settings, 360.0))),
            ))
            .into_any_element()
    }

    /// A small icon button on a note's card.
    fn md_note_action(
        &self,
        id: String,
        glyph: &'static [u8],
        kind: ButtonKind,
        on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> Stateful<Div> {
        style::icon_button(
            gpui::SharedString::from(id),
            glyph,
            kind,
            self.theme,
            style::scaled(&self.shared_settings, 22.0),
        )
        .on_click(on_click)
    }

    /// The live agents as rows; a click sends the ticked notes to that agent.
    /// With nothing ticked they are shown but do nothing.
    fn md_agent_rows(&self, page: Page, enabled: bool, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let workspace = self.active_workspace_id();
        let agents = self.diff_agents();
        let mut column = div().flex().flex_col().gap_1();
        for (index, agent) in agents.iter().enumerate() {
            let mut row = div()
                .id(("pilus-md-agent", index))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(style::scaled(&self.shared_settings, 28.0)))
                .px_2()
                .radius_md()
                .opacity(if enabled { 1.0 } else { 0.5 });
            if enabled {
                row = row
                    .cursor_pointer()
                    .hover_fill(theme.panel_dim, style::RADIUS_MD)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.md_send_notes(page, index, cx)),
                    );
            }
            column = column.child(
                row.child(icon(agent_icon(&agent.agent), fonts.body, theme.muted))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(fonts.body))
                            .text_color(theme.panel_fg)
                            .child(agent.label().to_string()),
                    )
                    .children(
                        (!agent.workspace_name.is_empty()
                            && Some(agent.workspace_id.as_str()) != workspace.as_deref())
                        .then(|| {
                            div()
                                .text_size(px(fonts.label))
                                .text_color(theme.muted)
                                .child(agent.workspace_name.clone())
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(agent.status.clone()),
                    ),
            );
        }
        if agents.is_empty() {
            column = column.child(
                div()
                    .px_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("No live agent. Start one in a pane, then send."),
            );
        } else if !enabled {
            column = column.child(
                div()
                    .px_2()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child("Tick the notes to send first."),
            );
        }
        column
    }

    /// Start a note on the selection, quoting it.
    fn md_begin_note(&mut self, page: Page, cx: &mut Context<Self>) {
        let Some((quote, lines)) = self.md_selected_excerpt(page) else {
            return;
        };
        self.md_notes.draft = Some(Draft {
            page,
            lines,
            quote,
            editing: None,
        });
        self.md_note_input.update(cx, |input, cx| input.clear(cx));
        if let Some(doc) = self.md_doc_mut(page) {
            doc.clear_selection();
        }
        cx.notify();
    }

    fn md_edit_note(&mut self, page: Page, id: u64, cx: &mut Context<Self>) {
        let Some(note) = self.md_notes.book.get(id) else {
            return;
        };
        let (quote, lines, body) = (note.quote.clone(), note.lines, note.body.clone());
        self.md_notes.draft = Some(Draft {
            page,
            lines,
            quote,
            editing: Some(id),
        });
        self.md_note_input
            .update(cx, |input, cx| input.set_value(body, cx));
        cx.notify();
    }

    /// Keep the draft as a note. With `send`, go on to the list with only
    /// that note ticked, ready for an agent.
    fn md_save_note(&mut self, send: bool, cx: &mut Context<Self>) {
        let body = self.md_note_input.read(cx).value().trim().to_string();
        if body.is_empty() {
            self.toast(ToastKind::Info, "A note needs some text");
            cx.notify();
            return;
        }
        let Some(draft) = self.md_notes.draft.take() else {
            return;
        };
        let id = match draft.editing {
            Some(id) => {
                self.md_notes.book.edit(id, body);
                id
            }
            None => {
                let Some((endpoint, path)) = self.md_doc_key(draft.page) else {
                    return;
                };
                self.md_notes.book.add(NewNote {
                    endpoint,
                    path,
                    lines: draft.lines,
                    quote: draft.quote,
                    body,
                })
            }
        };
        let ticked = &mut self.md_notes.ticked;
        if send {
            ticked.clear();
        }
        ticked.insert(id);
        if send {
            self.md_notes.panel = true;
        }
        self.md_persist_notes();
        cx.notify();
    }

    fn md_delete_note(&mut self, id: u64, cx: &mut Context<Self>) {
        self.md_notes.book.remove(id);
        self.md_notes.ticked.remove(&id);
        self.md_persist_notes();
        cx.notify();
    }

    fn md_toggle_note(&mut self, id: u64) {
        let ticked = &mut self.md_notes.ticked;
        if !ticked.remove(&id) {
            ticked.insert(id);
        }
    }

    fn md_tick_all(&mut self, page: Page, on: bool) {
        let Some((endpoint, path)) = self.md_doc_key(page) else {
            return;
        };
        let ids: Vec<u64> = self
            .md_notes
            .book
            .for_doc(&endpoint, &path)
            .map(|note| note.id)
            .collect();
        for id in ids {
            if on {
                self.md_notes.ticked.insert(id);
            } else {
                self.md_notes.ticked.remove(&id);
            }
        }
    }

    fn md_persist_notes(&mut self) {
        if let Err(error) = self.md_notes.book.save() {
            self.toast(ToastKind::Error, format!("Could not save notes: {error}"));
        }
    }

    /// Send the ticked notes of `page`'s file to the `pick`th live agent, as
    /// one prompt, and mark them sent once the daemon has taken it.
    fn md_send_notes(&mut self, page: Page, pick: usize, cx: &mut Context<Self>) {
        let agents = self.diff_agents();
        let Some(agent) = agents.get(pick) else {
            self.toast(ToastKind::Error, "No agent to send to");
            cx.notify();
            return;
        };
        let Some((endpoint, path)) = self.md_doc_key(page) else {
            return;
        };
        let book = &self.md_notes.book;
        let notes: Vec<&ExcerptNote> = book
            .for_doc(&endpoint, &path)
            .filter(|note| self.md_notes.ticked.contains(&note.id))
            .collect();
        if notes.is_empty() {
            self.toast(ToastKind::Info, "Tick the notes to send first");
            cx.notify();
            return;
        }
        let ids: Vec<u64> = notes.iter().map(|note| note.id).collect();
        let prompt = notes_prompt(&path, &notes);
        let (target, label) = (agent.pane.clone(), agent.label().to_string());
        let client = self.control_client_for_endpoint(&self.active);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match client {
                        Some(mut client) => client
                            .prompt_agent(&target, &prompt)
                            .map_err(|error| error.to_string()),
                        None => Err("Could not reach the daemon".to_string()),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        let count = ids.len();
                        this.md_notes.book.mark_sent(&ids, &label);
                        for id in &ids {
                            this.md_notes.ticked.remove(id);
                        }
                        this.md_persist_notes();
                        let noun = if count == 1 { "note" } else { "notes" };
                        this.toast(
                            ToastKind::Success,
                            format!("Sent {count} {noun} to {label}"),
                        );
                    }
                    Err(error) => this.toast(ToastKind::Error, error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.toast(ToastKind::Info, "Sending…");
        cx.notify();
    }
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// Shot harness: three notes on the open Files document, kept in memory.
    pub(super) fn md_notes_show_sample(
        &mut self,
        panel: bool,
        composing: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((endpoint, path)) = self.md_doc_key(Page::Files) else {
            return;
        };
        let mut book = NoteBook::in_memory();
        let mut add = |lines: (u32, u32), quote: &str, body: &str| {
            book.add(NewNote {
                endpoint: endpoint.clone(),
                path: path.clone(),
                lines: Some(lines),
                quote: quote.to_string(),
                body: body.to_string(),
            })
        };
        let first = add(
            (10, 11),
            "Notes stay in the daemon, so an agent can pick them up later.",
            "Say where the daemon keeps them.",
        );
        let second = add((17, 17), "Run `cargo test`", "Add the flag we use in CI.");
        let _ = add(
            (31, 33),
            "A note is anchored to source lines, not to the text it was written on.",
            "Is this still true after a rebase?",
        );
        book.mark_sent(&[second], "orca");
        // Adding a note drops the selection it was made from.
        if let Some(doc) = self.md_doc_mut(Page::Files) {
            doc.clear_selection();
        }
        self.md_notes.book = book;
        self.md_notes.ticked = HashSet::from([first]);
        self.md_notes.panel = panel;
        self.md_notes.draft = composing.then(|| Draft {
            page: Page::Files,
            lines: Some((10, 11)),
            quote: "Notes stay in the daemon, so an agent can pick them up later.".to_string(),
            editing: None,
        });
        cx.notify();
    }
}
