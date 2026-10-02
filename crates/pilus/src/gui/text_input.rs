//! Native chrome text editing.
//!
//! The editable controls used to share a plain `TextField`: a `String` and a
//! char-index cursor. That split grapheme clusters, had no selection, and read
//! printable characters straight out of `KeyDownEvent`, so dead keys and IME
//! never reached the field. Here an entity owns the buffer and a selection in
//! UTF-8 byte offsets — the unit GPUI's shaper indexes glyphs by — implements
//! [`EntityInputHandler`] so the platform's input method drives composition, and
//! paints text, selection, and caret from the real shaped layout, so clicks and
//! carets land on glyphs instead of character counts.
//!
//! Windows keep one [`Entity<TextInput>`] per field and render it. Printable
//! keys are deliberately left unhandled in [`TextInput::handle_key`]: the
//! platform turns them into `replace_text_in_range`, which is what makes IME
//! and non-US layouts work. Navigation and editing keys are handled here and
//! stop propagation.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Font, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Render, ShapedLine, SharedString, TextAlign, TextRun, UTF16Selection, Window,
    canvas, div, fill, point, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::theme::Theme;

/// Multiplier from font size to painted line height, matching the chrome's
/// compact leading.
const LINE_LEADING: f32 = 1.45;
/// Caret width, in logical pixels.
const CARET_WIDTH: f32 = 1.5;
/// Half of the on/off caret blink cycle, in milliseconds.
const CARET_BLINK: Duration = Duration::from_millis(530);

/// The last shaped frame, kept so pointer hit-testing can map a window point to
/// a byte offset without re-shaping on every mouse move.
struct LastLayout {
    bounds: Bounds<Pixels>,
    offsets: Vec<usize>,
    layouts: Vec<ShapedLine>,
    line_height: Pixels,
}

impl LastLayout {
    /// The byte offset nearest `position` (window coordinates).
    fn byte_for_point(&self, position: Point<Pixels>) -> usize {
        let local_y = (position.y - self.bounds.origin.y).as_f32();
        let row = (local_y / self.line_height.as_f32())
            .floor()
            .max(0.0)
            .min(self.layouts.len().saturating_sub(1) as f32) as usize;
        let local_x = (position.x - self.bounds.origin.x).as_f32().max(0.0);
        let within = self.layouts[row].closest_index_for_x(px(local_x));
        self.offsets.get(row).copied().unwrap_or(0) + within
    }

    /// The x offset and row of `byte`, for caret and IME bounds.
    fn caret(&self, byte: usize) -> Option<(Pixels, usize)> {
        let row = row_for_offset(&self.offsets, byte);
        let line = self.layouts.get(row)?;
        let offset = byte.saturating_sub(*self.offsets.get(row)?);
        Some((line.x_for_index(offset), row))
    }
}

/// An editable field's buffer: text plus a selection, in byte offsets.
///
/// Every boundary helper snaps to a grapheme cluster, so arrow keys and
/// backspace never split an emoji or a combining sequence.
#[derive(Clone, Debug, Default)]
pub(super) struct Buffer {
    text: String,
    /// Where the selection was anchored.
    anchor: usize,
    /// The caret. The selection runs from `anchor` to `head`.
    head: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

/// One undo step.
#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    anchor: usize,
    head: usize,
}

impl Buffer {
    fn new(text: String) -> Self {
        let head = text.len();
        Self {
            text,
            anchor: head,
            head,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// The selection, low to high.
    fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    /// Whether the caret sits before the anchor.
    fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    fn has_selection(&self) -> bool {
        self.anchor != self.head
    }

    /// Move the caret to `byte`, extending the selection when `extend`.
    fn move_to(&mut self, byte: usize, extend: bool) {
        let at = self.clamp(self.text.floor_char_boundary(byte));
        self.head = at;
        if !extend {
            self.anchor = at;
        }
    }

    fn select_all(&mut self) {
        self.anchor = 0;
        self.head = self.text.len();
    }

    fn move_by_grapheme(&mut self, forward: bool, extend: bool) {
        // A collapsed move collapses an existing selection to its edge.
        if !extend && self.has_selection() {
            let edge = if forward {
                self.range().end
            } else {
                self.range().start
            };
            self.move_to(edge, false);
            return;
        }
        let from = self.head;
        let to = if forward {
            next_boundary(&self.text, from)
        } else {
            prev_boundary(&self.text, from)
        };
        self.move_to(to, extend);
    }

    fn move_by_word(&mut self, forward: bool, extend: bool) {
        let to = if forward {
            next_word(&self.text, self.head)
        } else {
            prev_word(&self.text, self.head)
        };
        self.move_to(to, extend);
    }

    fn move_line_edge(&mut self, start: bool, extend: bool) {
        let to = if start {
            line_start(&self.text, self.head)
        } else {
            line_end(&self.text, self.head)
        };
        self.move_to(to, extend);
    }

    fn move_vertical(&mut self, down: bool, extend: bool) {
        let at = self.head;
        let origin = line_start(&self.text, at);
        let column = at - origin;
        let target = if down {
            match self.text[origin..].find('\n') {
                Some(newline) => {
                    let next = origin + newline + 1;
                    let end = line_end(&self.text, next);
                    (next + column).min(end)
                }
                None => return,
            }
        } else {
            if origin == 0 {
                return;
            }
            let prev = origin - 1;
            let start = line_start(&self.text, prev);
            (start + column).min(prev)
        };
        self.move_to(target, extend);
    }

    /// Replace `range` with `text`, returning the inserted byte range.
    fn replace(&mut self, range: Range<usize>, text: &str, multiline: bool) -> Range<usize> {
        let inserted = if multiline {
            text.to_string()
        } else {
            text.replace('\n', "")
        };
        let start = self.clamp(self.text.floor_char_boundary(range.start));
        let end = self
            .clamp(self.text.ceil_char_boundary(range.end))
            .max(start);
        self.snapshot();
        self.text.replace_range(start..end, &inserted);
        let caret = start + inserted.len();
        self.head = caret;
        self.anchor = caret;
        self.redo.clear();
        start..caret
    }

    /// The range of one backspace or delete, so the edit is one grapheme.
    fn backspace(&mut self, word: bool) -> Range<usize> {
        if self.has_selection() {
            return self.range();
        }
        let end = self.head;
        let start = if word {
            prev_word(&self.text, end)
        } else {
            prev_boundary(&self.text, end)
        };
        start..end
    }

    fn delete(&mut self, word: bool) -> Range<usize> {
        if self.has_selection() {
            return self.range();
        }
        let start = self.head;
        let end = if word {
            next_word(&self.text, start)
        } else {
            next_boundary(&self.text, start)
        };
        start..end
    }

    fn snapshot(&mut self) {
        self.undo.push(Snapshot {
            text: self.text.clone(),
            anchor: self.anchor,
            head: self.head,
        });
        if self.undo.len() > 256 {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(snapshot) => {
                self.redo.push(self.current());
                self.restore(snapshot);
                true
            }
            None => false,
        }
    }

    fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(snapshot) => {
                self.undo.push(self.current());
                self.restore(snapshot);
                true
            }
            None => false,
        }
    }

    fn current(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            anchor: self.anchor,
            head: self.head,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.anchor = self.clamp(snapshot.anchor);
        self.head = self.clamp(snapshot.head);
    }

    fn clamp(&self, byte: usize) -> usize {
        byte.min(self.text.len())
    }
}

/// Snap `byte` to the start of the grapheme that contains it.
fn grapheme_start(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    if text.is_char_boundary(byte) {
        byte
    } else {
        text.floor_char_boundary(byte)
    }
}

/// The grapheme boundary before `byte`.
fn prev_boundary(text: &str, byte: usize) -> usize {
    let at = grapheme_start(text, byte);
    text[..at]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// The grapheme boundary after `byte`.
fn next_boundary(text: &str, byte: usize) -> usize {
    let at = grapheme_start(text, byte);
    text[at..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(index, _)| at + index)
        .unwrap_or(text.len())
}

/// Whether a cluster counts as a word character for word-wise movement.
fn is_word_char(cluster: &str) -> bool {
    cluster.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
}

/// The start of the word before `byte`.
fn prev_word(text: &str, byte: usize) -> usize {
    let mut at = grapheme_start(text, byte);
    // Skip the run of separators just before the caret.
    while at > 0 {
        let before = prev_boundary(text, at);
        if is_word_char(&text[before..at]) {
            break;
        }
        at = before;
    }
    while at > 0 {
        let before = prev_boundary(text, at);
        if !is_word_char(&text[before..at]) {
            break;
        }
        at = before;
    }
    at
}

/// The end of the word after `byte`.
fn next_word(text: &str, byte: usize) -> usize {
    let mut at = grapheme_start(text, byte);
    while at < text.len() {
        let after = next_boundary(text, at);
        if is_word_char(&text[at..after]) {
            break;
        }
        at = after;
    }
    while at < text.len() {
        let after = next_boundary(text, at);
        if !is_word_char(&text[at..after]) {
            break;
        }
        at = after;
    }
    at
}

/// The first byte of the line containing `byte`.
fn line_start(text: &str, byte: usize) -> usize {
    let at = grapheme_start(text, byte);
    text[..at]
        .rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(0)
}

/// The byte of the newline that ends the line containing `byte`.
fn line_end(text: &str, byte: usize) -> usize {
    let at = grapheme_start(text, byte);
    text[at..]
        .find('\n')
        .map(|newline| at + newline)
        .unwrap_or(text.len())
}

/// The first byte of each line of `text`.
fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

/// The row containing `byte`.
fn row_for_offset(offsets: &[usize], byte: usize) -> usize {
    offsets
        .iter()
        .rposition(|start| *start <= byte)
        .unwrap_or(0)
}

/// The UTF-16 length of `text` up to `byte`.
fn byte_to_utf16(text: &str, byte: usize) -> usize {
    let at = text.floor_char_boundary(byte.min(text.len()));
    text[..at].encode_utf16().count()
}

/// The byte offset of UTF-16 offset `utf16`.
fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= utf16 {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

/// A panel-owned handle to an editable buffer.
///
/// Panels keep their text as plain state; the window wraps the same buffer in
/// an [`Entity<TextInput>`] when it renders, so the field gets IME and
/// selection without threading a GPUI context through panel logic or its
/// tests. Two handles to the same field share one buffer.
#[derive(Clone, Debug, Default)]
pub(super) struct TextField(Rc<RefCell<Buffer>>);

impl TextField {
    /// The shared buffer an [`TextInput`] entity should render.
    pub(super) fn shared(&self) -> Rc<RefCell<Buffer>> {
        self.0.clone()
    }

    /// The current text.
    pub(super) fn value(&self) -> String {
        self.0.borrow().text.clone()
    }

    /// Replace the value in place, keeping any entity sharing this buffer.
    pub(super) fn set_value(&mut self, text: String) {
        *self.0.borrow_mut() = Buffer::new(text);
    }

    /// Clear the buffer and the undo history.
    pub(super) fn clear(&mut self) {
        *self.0.borrow_mut() = Buffer::new(String::new());
    }
}

/// A native chrome text field.
///
/// One entity per editable control. Windows render it through
/// [`Render`], which owns the focus and pointer wiring; the engine here stays
/// free of window state so it is testable on its own.
pub(super) struct TextInput {
    buffer: Rc<RefCell<Buffer>>,
    /// The IME composition range, underlined while the input method is active.
    marked: Option<Range<usize>>,
    multiline: bool,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    theme: Theme,
    /// Whether a pointer drag is extending the selection.
    dragging: bool,
    /// Whether a bare Enter should be left to the window (to send) instead of
    /// inserting a newline. Shift+Enter always inserts one.
    enter_submits: bool,
    /// Whether the caret is in its visible blink phase.
    caret_on: bool,
    /// Whether the field held focus at the last paint. The blink task idles
    /// while this is false.
    focused: bool,
    /// Whether the blink task has been started for this field.
    blink_started: bool,
    last: Rc<RefCell<Option<LastLayout>>>,
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    /// Build a field with `text` and the caret at the end.
    pub(super) fn new(
        text: String,
        placeholder: impl Into<SharedString>,
        font: Font,
        font_size: Pixels,
        multiline: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::from_shared(
            Rc::new(RefCell::new(Buffer::new(text))),
            placeholder,
            font,
            font_size,
            multiline,
            theme,
            cx,
        )
    }

    /// Build a field over a panel-owned buffer, so edits show up in both.
    pub(super) fn from_shared(
        buffer: Rc<RefCell<Buffer>>,
        placeholder: impl Into<SharedString>,
        font: Font,
        font_size: Pixels,
        multiline: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            buffer,
            marked: None,
            multiline,
            placeholder: placeholder.into(),
            focus_handle: cx.focus_handle(),
            font,
            font_size,
            line_height: px(font_size.as_f32() * LINE_LEADING),
            theme,
            dragging: false,
            enter_submits: false,
            caret_on: true,
            focused: false,
            blink_started: false,
            last: Rc::new(RefCell::new(None)),
        }
    }

    /// Leave a bare Enter to the window; Shift+Enter still breaks the line.
    pub(super) fn submit_on_enter(mut self) -> Self {
        self.enter_submits = true;
        self
    }

    /// Update the theme and type size to the window's current settings.
    pub(super) fn set_style(&mut self, theme: Theme, font: Font, font_size: Pixels) {
        self.theme = theme;
        self.font = font;
        self.font_size = font_size;
        self.line_height = px(font_size.as_f32() * LINE_LEADING);
    }

    /// The current value.
    pub(super) fn value(&self) -> String {
        self.buffer.borrow().text.clone()
    }

    /// Replace the value, clearing the selection and undo history.
    pub(super) fn set_value(&mut self, text: String, cx: &mut Context<Self>) {
        *self.buffer.borrow_mut() = Buffer::new(text);
        self.marked = None;
        cx.notify();
    }

    /// Empty the field.
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_value(String::new(), cx);
    }

    /// Whether this field currently owns the keyboard.
    pub(super) fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Handle a keystroke, returning whether it was consumed.
    ///
    /// Printable input is intentionally not consumed: leaving it to the
    /// platform routes it through the active input method.
    pub(super) fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let extend = modifiers.shift;
        let key = keystroke.key.as_str();

        if modifiers.platform {
            match key {
                "a" => {
                    self.buffer.borrow_mut().select_all();
                    self.changed(cx);
                    return true;
                }
                "c" => return self.copy(cx),
                "x" => return self.cut(cx),
                "v" => return self.paste(window, cx),
                "z" if modifiers.shift => {
                    if self.buffer.borrow_mut().redo() {
                        self.changed(cx);
                    }
                    return true;
                }
                "z" => {
                    if self.buffer.borrow_mut().undo() {
                        self.changed(cx);
                    }
                    return true;
                }
                "left" => {
                    self.buffer.borrow_mut().move_line_edge(true, extend);
                    self.changed(cx);
                    return true;
                }
                "right" => {
                    self.buffer.borrow_mut().move_line_edge(false, extend);
                    self.changed(cx);
                    return true;
                }
                "up" => {
                    self.buffer.borrow_mut().move_to(0, extend);
                    self.changed(cx);
                    return true;
                }
                "down" => {
                    let end = self.buffer.borrow().text.len();
                    self.buffer.borrow_mut().move_to(end, extend);
                    self.changed(cx);
                    return true;
                }
                "backspace" => {
                    let range = self.buffer.borrow_mut().backspace(true);
                    self.delete_range(range, cx);
                    return true;
                }
                _ => {}
            }
            return false;
        }

        if modifiers.control {
            return false;
        }

        let word = modifiers.alt;
        match key {
            "left" => {
                {
                    let mut buffer = self.buffer.borrow_mut();
                    if word {
                        buffer.move_by_word(false, extend);
                    } else {
                        buffer.move_by_grapheme(false, extend);
                    }
                }
                self.changed(cx);
                true
            }
            "right" => {
                {
                    let mut buffer = self.buffer.borrow_mut();
                    if word {
                        buffer.move_by_word(true, extend);
                    } else {
                        buffer.move_by_grapheme(true, extend);
                    }
                }
                self.changed(cx);
                true
            }
            "up" => {
                // Single-line fields leave vertical movement to the window,
                // which uses it to walk list rows.
                if !self.multiline {
                    return false;
                }
                self.buffer.borrow_mut().move_vertical(false, extend);
                self.changed(cx);
                true
            }
            "down" => {
                if !self.multiline {
                    return false;
                }
                self.buffer.borrow_mut().move_vertical(true, extend);
                self.changed(cx);
                true
            }
            "home" => {
                self.buffer.borrow_mut().move_line_edge(true, extend);
                self.changed(cx);
                true
            }
            "end" => {
                self.buffer.borrow_mut().move_line_edge(false, extend);
                self.changed(cx);
                true
            }
            "backspace" => {
                let range = self.buffer.borrow_mut().backspace(word);
                self.delete_range(range, cx);
                true
            }
            "delete" | "del" => {
                let range = self.buffer.borrow_mut().delete(word);
                self.delete_range(range, cx);
                true
            }
            "enter" if self.multiline => {
                if self.enter_submits && !modifiers.shift {
                    return false;
                }
                {
                    let mut buffer = self.buffer.borrow_mut();
                    let range = buffer.range();
                    buffer.replace(range, "\n", true);
                }
                self.changed(cx);
                true
            }
            _ => false,
        }
    }

    fn delete_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.start < range.end {
            self.buffer.borrow_mut().replace(range, "", self.multiline);
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let text = {
            let buffer = self.buffer.borrow();
            let range = buffer.range();
            if range.start == range.end {
                return false;
            }
            buffer.text[range].to_string()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn cut(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.copy(cx) {
            return false;
        }
        let range = self.buffer.borrow().range();
        self.delete_range(range, cx);
        true
    }

    fn paste(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return true;
        };
        {
            let mut buffer = self.buffer.borrow_mut();
            let range = buffer.range();
            buffer.replace(range, &text, self.multiline);
        }
        self.changed(cx);
        true
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = true;
        self.caret_from_point(event.position, event.modifiers.shift, window, cx);
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging {
            self.caret_from_point(event.position, true, window, cx);
        }
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    fn caret_from_point(
        &mut self,
        position: Point<Pixels>,
        extend: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte = {
            let last = self.last.borrow();
            let Some(last) = last.as_ref() else {
                return;
            };
            last.byte_for_point(position)
        };
        self.buffer.borrow_mut().move_to(byte, extend);
        cx.notify();
    }

    /// The text area's height for `lines` rows.
    fn height(&self, lines: usize) -> Pixels {
        px(self.line_height.as_f32() * lines.max(1) as f32)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let buffer = self.buffer.borrow();
        let start = buffer.clamp(utf16_to_byte(&buffer.text, range_utf16.start));
        let end = buffer.clamp(utf16_to_byte(&buffer.text, range_utf16.end));
        let start = buffer.text.floor_char_boundary(start);
        let end = buffer.text.ceil_char_boundary(end).max(start);
        let adjusted = byte_to_utf16(&buffer.text, start)..byte_to_utf16(&buffer.text, end);
        if adjusted != range_utf16 {
            adjusted_range.replace(adjusted);
        }
        Some(buffer.text[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let buffer = self.buffer.borrow();
        let range = buffer.range();
        Some(UTF16Selection {
            range: byte_to_utf16(&buffer.text, range.start)..byte_to_utf16(&buffer.text, range.end),
            reversed: buffer.reversed(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| {
            let buffer = self.buffer.borrow();
            byte_to_utf16(&buffer.text, range.start)..byte_to_utf16(&buffer.text, range.end)
        })
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.resolve_range(range_utf16);
        self.buffer
            .borrow_mut()
            .replace(range, text, self.multiline);
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.resolve_range(range_utf16);
        let inserted = self
            .buffer
            .borrow_mut()
            .replace(range, new_text, self.multiline);
        if new_text.is_empty() {
            self.marked = None;
        } else {
            self.marked = Some(inserted.clone());
        }
        if let Some(selected) = new_selected_range {
            let mut buffer = self.buffer.borrow_mut();
            let start = inserted.start + utf16_to_byte(new_text, selected.start);
            let end = inserted.start + utf16_to_byte(new_text, selected.end);
            buffer.head = buffer.clamp(buffer.text.floor_char_boundary(end));
            buffer.anchor = buffer.clamp(buffer.text.floor_char_boundary(start));
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let byte = {
            let buffer = self.buffer.borrow();
            utf16_to_byte(&buffer.text, range_utf16.start)
        };
        let last = self.last.borrow();
        let last = last.as_ref()?;
        let (x, row) = last.caret(byte)?;
        let origin = point(
            element_bounds.origin.x + x,
            element_bounds.origin.y + px(self.line_height.as_f32() * row as f32),
        );
        Some(Bounds {
            origin,
            size: size(px(CARET_WIDTH), self.line_height),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let byte = {
            let last = self.last.borrow();
            let last = last.as_ref()?;
            last.byte_for_point(point)
        };
        Some(byte_to_utf16(&self.buffer.borrow().text, byte))
    }
}

impl TextInput {
    /// The byte range an input-method call should replace: the given UTF-16
    /// range when present, otherwise the current selection.
    fn resolve_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        let buffer = self.buffer.borrow();
        match range_utf16 {
            Some(range) => {
                let start = buffer
                    .text
                    .floor_char_boundary(utf16_to_byte(&buffer.text, range.start));
                let end = buffer
                    .text
                    .ceil_char_boundary(utf16_to_byte(&buffer.text, range.end));
                start..end.max(start)
            }
            None => buffer.range(),
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        if focused != self.focused {
            // Start each focus span with a visible caret.
            self.focused = focused;
            self.caret_on = true;
        }
        // Blink the caret on a slow timer, repainting only while focused so an
        // idle field costs nothing. The task dies with the entity.
        if !self.blink_started {
            self.blink_started = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(CARET_BLINK).await;
                    let alive = this.update(cx, |input, cx| {
                        if input.focused {
                            input.caret_on = !input.caret_on;
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        let theme = self.theme;
        let font_size = self.font_size;
        let line_height = self.line_height;
        let (text, selection, head) = {
            let buffer = self.buffer.borrow();
            (buffer.text.clone(), buffer.range(), buffer.head)
        };
        let empty = text.is_empty();
        let show_placeholder = empty && !focused;
        let display = if empty {
            if focused {
                String::new()
            } else {
                self.placeholder.to_string()
            }
        } else {
            text.clone()
        };
        let color: Hsla = if show_placeholder {
            theme.muted.into()
        } else {
            theme.fg.into()
        };
        // Selected glyphs take the selection foreground so they stay readable
        // on the selection fill; the fill itself is painted under the text.
        let selection_color: Hsla = theme.selection_bg.into();
        let selection_fg: Hsla = theme.selection_fg.into();

        let mut layouts = Vec::new();
        let mut line_start = 0usize;
        for segment in display.split('\n') {
            let line_end = line_start + segment.len();
            let low = selection.start.max(line_start).min(line_end);
            let high = selection.end.max(line_start).min(line_end);
            let run = |len: usize, run_color: Hsla| TextRun {
                len,
                font: self.font.clone(),
                color: run_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = [
                run(low - line_start, color),
                run(high - low, selection_fg),
                run(line_end - high, color),
            ];
            let layout =
                window
                    .text_system()
                    .shape_line(SharedString::new(segment), font_size, &runs, None);
            layouts.push(layout);
            line_start = line_end + 1;
        }

        let offsets = line_offsets(&text);
        let marked = self.marked.clone();
        let caret_visible = focused && self.caret_on;
        let row_count = layouts.len();
        let height = self.height(row_count);

        // Store the shaped frame for pointer hit-testing and IME bounds.
        {
            let mut last = self.last.borrow_mut();
            *last = Some(LastLayout {
                bounds: Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(0.0), height),
                },
                offsets: offsets.clone(),
                layouts: layouts.clone(),
                line_height,
            });
        }

        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let last = self.last.clone();
        let accent: Hsla = theme.accent.into();

        div()
            .flex()
            .items_center()
            .w_full()
            .h(height)
            .track_focus(&self.focus_handle)
            // Text fields own the clipboard and selection shortcuts. The
            // window's grid bindings are scoped `!Input` so they stand down
            // while this element is focused.
            .key_context("Input")
            .on_key_down({
                let entity = entity.clone();
                move |event, window, cx| {
                    let handled = entity.update(cx, |this, cx| this.handle_key(event, window, cx));
                    if handled {
                        cx.stop_propagation();
                    }
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let entity = entity.clone();
                move |event, window, cx| {
                    let handle = entity.read(cx).focus_handle.clone();
                    window.focus(&handle, cx);
                    entity.update(cx, |this, cx| this.on_mouse_down(event, window, cx));
                }
            })
            .on_mouse_move({
                let entity = entity.clone();
                move |event, window, cx| {
                    entity.update(cx, |this, cx| this.on_mouse_move(event, window, cx));
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let entity = entity.clone();
                move |event, _window, cx| {
                    entity.update(cx, |this, cx| this.on_mouse_up(event, cx));
                }
            })
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        if let Some(last) = last.borrow_mut().as_mut() {
                            last.bounds = bounds;
                        }
                    },
                    move |bounds, (), window, cx| {
                        // Selection sits under the glyphs so the foreground
                        // colour chosen above stays legible on it.
                        if selection.start < selection.end {
                            paint_range(
                                window,
                                bounds,
                                &layouts,
                                &offsets,
                                selection.clone(),
                                line_height,
                                selection_color,
                                px(0.0),
                                line_height,
                            );
                        }

                        for (row, layout) in layouts.iter().enumerate() {
                            let origin = point(
                                bounds.origin.x,
                                bounds.origin.y + px(line_height.as_f32() * row as f32),
                            );
                            let _ = layout.paint(
                                origin,
                                line_height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }

                        if let Some(marked) = &marked {
                            paint_range(
                                window,
                                bounds,
                                &layouts,
                                &offsets,
                                marked.clone(),
                                line_height,
                                accent,
                                line_height - px(1.5),
                                px(1.0),
                            );
                        }

                        if caret_visible {
                            let row = row_for_offset(&offsets, head);
                            if let (Some(layout), Some(offset)) =
                                (layouts.get(row), offsets.get(row))
                            {
                                let x = layout.x_for_index(head.saturating_sub(*offset));
                                window.paint_quad(fill(
                                    Bounds {
                                        origin: point(
                                            bounds.origin.x + x,
                                            bounds.origin.y + px(line_height.as_f32() * row as f32),
                                        ),
                                        size: size(px(CARET_WIDTH), line_height),
                                    },
                                    accent,
                                ));
                            }
                        }

                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}

/// Paint a selection or composition range, clipped per line.
#[allow(clippy::too_many_arguments)]
fn paint_range(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    layouts: &[ShapedLine],
    offsets: &[usize],
    range: Range<usize>,
    line_height: Pixels,
    color: Hsla,
    y_offset: Pixels,
    height: Pixels,
) {
    for (row, layout) in layouts.iter().enumerate() {
        let start = offsets.get(row).copied().unwrap_or(0);
        let end = offsets
            .get(row + 1)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(usize::MAX);
        let low = range.start.max(start).min(end);
        let high = range.end.max(start).min(end);
        if low >= high {
            continue;
        }
        let x0 = layout.x_for_index(low.saturating_sub(start));
        let x1 = layout.x_for_index(high.saturating_sub(start));
        let y = bounds.origin.y + px(line_height.as_f32() * row as f32) + y_offset;
        window.paint_quad(fill(
            Bounds {
                origin: point(bounds.origin.x + x0, y),
                size: size(x1 - x0, height),
            },
            color,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backspace_keeps_grapheme_clusters_whole() {
        let mut buffer = Buffer::new("a👨‍👩‍👧‍👦".to_string());
        let range = buffer.backspace(false);
        assert_eq!(&buffer.text[range.clone()], "👨‍👩‍👧‍👦");
    }

    #[test]
    fn arrow_moves_over_a_whole_emoji() {
        let mut buffer = Buffer::new("a👨‍👩‍👧‍👦b".to_string());
        buffer.move_by_grapheme(false, false);
        assert_eq!(buffer.head, "a👨‍👩‍👧‍👦".len());
    }

    #[test]
    fn selection_replaces_on_insert() {
        let mut buffer = Buffer::new("hello world".to_string());
        buffer.move_to(0, false);
        buffer.move_to(5, true);
        buffer.replace(buffer.range(), "goodbye", false);
        assert_eq!(buffer.text, "goodbye world");
        assert_eq!(buffer.head, 7);
    }

    #[test]
    fn undo_restores_text_and_selection() {
        let mut buffer = Buffer::new("abc".to_string());
        buffer.move_to(3, false);
        buffer.replace(3..3, "d", false);
        assert_eq!(buffer.text, "abcd");
        assert!(buffer.undo());
        assert_eq!(buffer.text, "abc");
        assert!(buffer.redo());
        assert_eq!(buffer.text, "abcd");
    }

    #[test]
    fn single_line_drops_newlines() {
        let mut buffer = Buffer::new(String::new());
        buffer.replace(0..0, "a\nb", false);
        assert_eq!(buffer.text, "ab");
    }

    #[test]
    fn multiline_keeps_newlines() {
        let mut buffer = Buffer::new(String::new());
        buffer.replace(0..0, "a\nb", true);
        assert_eq!(buffer.text, "a\nb");
    }

    #[test]
    fn word_movement_crosses_runs() {
        let buffer = Buffer::new("hello brave  world".to_string());
        assert_eq!(prev_word(&buffer.text, buffer.text.len()), 13);
        assert_eq!(next_word(&buffer.text, 0), 5);
    }

    #[test]
    fn utf16_offsets_round_trip_across_astral_chars() {
        let text = "a😀b";
        let byte = text.find('b').unwrap_or(text.len());
        let utf16 = byte_to_utf16(text, byte);
        assert_eq!(utf16, 3);
        assert_eq!(utf16_to_byte(text, utf16), byte);
    }
}
