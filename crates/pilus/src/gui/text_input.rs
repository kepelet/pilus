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
//!
//! A field can be given a file name ([`TextInput::set_syntax`]) and then colors
//! its text by that file's grammar, as the Files tab's editor does. The spans are
//! reparsed when the text changes, not on every frame, and recolor with the
//! theme for free because they name roles, not colors. A multiline field can
//! also show a gutter of line numbers ([`TextInput::set_line_numbers`]): one
//! per logical line, on its first display row, so a wrapped line is numbered
//! once.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Font, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Render, ShapedLine, SharedString, TextAlign, TextRun, UTF16Selection, Window,
    canvas, div, fill, point, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::highlight::{self, Span};
use super::theme::Theme;

/// Multiplier from font size to painted line height, matching the chrome's
/// compact leading.
const LINE_LEADING: f32 = 1.45;
/// Caret width, in logical pixels.
const CARET_WIDTH: f32 = 1.5;
/// Space between the line numbers and the text.
const LINE_NUMBER_GAP: f32 = 14.0;
/// The fewest digits a gutter makes room for, so it does not jump from nine
/// lines to ten.
const MIN_GUTTER_DIGITS: usize = 2;
/// How long after a Command chord the field refuses incoming text. The glyph
/// of an unclaimed chord lands within a frame or two; typing cannot follow
/// a chord that quickly.
const COMMAND_CHORD_WINDOW: Duration = Duration::from_millis(150);
/// Half of the on/off caret blink cycle, in milliseconds.
const CARET_BLINK: Duration = Duration::from_millis(530);

/// One shaped display row: part (or all) of one logical line. Wrapping a
/// long line yields several pieces; an unwrapped line is exactly one.
#[derive(Clone)]
struct Piece {
    /// Absolute byte offset where this piece starts.
    byte_start: usize,
    layout: ShapedLine,
}

/// The last shaped frame, kept so pointer hit-testing can map a window point to
/// a byte offset without re-shaping on every mouse move.
struct LastLayout {
    bounds: Bounds<Pixels>,
    pieces: Vec<Piece>,
    line_height: Pixels,
    /// How far a single-line field's text is shifted left to keep the caret
    /// in view; always zero for a wrapping field.
    scroll: Pixels,
}

impl LastLayout {
    /// The byte offset nearest `position` (window coordinates).
    fn byte_for_point(&self, position: Point<Pixels>) -> usize {
        let local_y = (position.y - self.bounds.origin.y).as_f32();
        let row = (local_y / self.line_height.as_f32())
            .floor()
            .max(0.0)
            .min(self.pieces.len().saturating_sub(1) as f32) as usize;
        let local_x = (position.x - self.bounds.origin.x + self.scroll)
            .as_f32()
            .max(0.0);
        let piece = &self.pieces[row];
        let within = piece.layout.closest_index_for_x(px(local_x));
        piece.byte_start + within
    }

    /// The x offset and display row of `byte`, for caret and IME bounds.
    fn caret(&self, byte: usize) -> Option<(Pixels, usize)> {
        let row = self
            .pieces
            .iter()
            .rposition(|piece| piece.byte_start <= byte)?;
        let piece = &self.pieces[row];
        Some((
            piece
                .layout
                .x_for_index(byte.saturating_sub(piece.byte_start))
                - self.scroll,
            row,
        ))
    }
}

/// One run of a display row's text: how many bytes, and how they are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RunStyle {
    len: usize,
    color: Hsla,
    italic: bool,
    bold: bool,
}

/// The runs that tile `piece`, a display row's byte range: the base color,
/// each of `spans` where it overlaps, and the selection's foreground wherever
/// it covers text. `spans` are sorted and do not overlap, as the highlighter
/// reports them; `look` resolves a span to its color and slant.
fn row_runs(
    piece: Range<usize>,
    selection: Range<usize>,
    spans: &[Span],
    look: &dyn Fn(&Span) -> (Hsla, bool, bool),
    base: Hsla,
    selected: Hsla,
) -> Vec<RunStyle> {
    let mut cuts = vec![piece.start, piece.end];
    cuts.extend(
        [selection.start, selection.end]
            .into_iter()
            .filter(|cut| piece.contains(cut)),
    );
    // The first span that can reach this row; earlier ones end before it.
    let first = spans.partition_point(|span| span.range.end <= piece.start);
    let reaching: Vec<&Span> = spans[first..]
        .iter()
        .take_while(|span| span.range.start < piece.end)
        .collect();
    for span in &reaching {
        cuts.extend(
            [span.range.start, span.range.end]
                .into_iter()
                .filter(|cut| piece.contains(cut)),
        );
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut runs: Vec<RunStyle> = Vec::new();
    for pair in cuts.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let span = reaching
            .iter()
            .find(|span| span.range.start <= from && to <= span.range.end);
        let (mut color, italic, bold) = span.map_or((base, false, false), |span| look(span));
        if selection.start <= from && to <= selection.end {
            color = selected;
        }
        match runs.last_mut() {
            Some(last) if (last.color, last.italic, last.bold) == (color, italic, bold) => {
                last.len += to - from;
            }
            _ => runs.push(RunStyle {
                len: to - from,
                color,
                italic,
                bold,
            }),
        }
    }
    if runs.is_empty() {
        // An empty row still shapes, from one empty run.
        runs.push(RunStyle {
            len: 0,
            color: base,
            italic: false,
            bold: false,
        });
    }
    runs
}

/// Split one logical line (no `\n`) into display pieces that fit `max_width`,
/// breaking at spaces and, for overlong words, mid-word. Returns absolute
/// byte ranges tiling `[line_start, line_end)` contiguously.
fn wrap_pieces(
    line: &str,
    line_start: usize,
    max_width: Pixels,
    measure: &dyn Fn(&str) -> Pixels,
) -> Vec<(usize, usize)> {
    // Words tile the line; each keeps its absolute start for byte math.
    let mut words: Vec<(usize, &str)> = Vec::new();
    let mut cursor = line_start;
    for word in line.split_inclusive(' ') {
        words.push((cursor, word));
        cursor += word.len();
    }
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let (start, _) = words[index];
        let mut end = start;
        let mut width = px(0.0);
        while index < words.len() {
            let (word_start, word) = words[index];
            let word_width = measure(word);
            if word_width > max_width {
                break; // overlong word: split below
            }
            if end > start && width + word_width > max_width {
                break; // full: start a fresh piece at this word
            }
            width += word_width;
            end = word_start + word.len();
            index += 1;
        }
        if end == start {
            // The next word alone overflows: split it mid-word.
            let (word_start, word) = words[index];
            pieces.extend(split_long_word(word, word_start, max_width, measure));
            index += 1;
        } else {
            pieces.push((start, end));
        }
    }
    if pieces.is_empty() {
        // Empty line: one empty piece so the row still paints.
        pieces.push((line_start, line_start));
    }
    pieces
}

/// Split an overlong word into pieces that fit, near the middle.
fn split_long_word(
    word: &str,
    word_start: usize,
    max_width: Pixels,
    measure: &dyn Fn(&str) -> Pixels,
) -> Vec<(usize, usize)> {
    if measure(word) <= max_width || word.chars().count() <= 1 {
        return vec![(word_start, word_start + word.len())];
    }
    // Mid byte index, snapped to a char boundary; a degenerate split emits
    // the word whole (overflowing slightly) rather than recursing forever.
    let at = word.floor_char_boundary(word.len() / 2);
    if at == 0 || at >= word.len() {
        return vec![(word_start, word_start + word.len())];
    }
    let (head, tail) = word.split_at(at);
    let mut pieces = split_long_word(head, word_start, max_width, measure);
    pieces.extend(split_long_word(tail, word_start + at, max_width, measure));
    pieces
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

/// `text` with every character drawn as asterisks, one per byte.
///
/// Matching the byte length is what lets the caret, the selection and a click
/// keep using offsets into the real text, with no mapping between the two.
fn mask(text: &str) -> String {
    "*".repeat(text.len())
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
    /// Draw the text as asterisks and keep it off the clipboard: an API key.
    secret: bool,
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
    /// The file name whose grammar colors the text, if any.
    syntax: Option<String>,
    /// Whether a gutter of line numbers shows beside the text.
    line_numbers: bool,
    /// The text the spans below were parsed from, so a frame only reparses
    /// when the text changed.
    highlighted: Option<(String, Vec<Span>)>,
    /// When a Command chord last reached this field's key handler. The
    /// platform delivers an unclaimed chord's glyph a moment later as text,
    /// by which time the modifier state can already be stale.
    command_chord_at: Option<Instant>,
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
            command_chord_at: None,
            syntax: None,
            line_numbers: false,
            highlighted: None,
            multiline,
            secret: false,
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

    /// Mask the text and refuse to copy it, for a secret such as an API key.
    pub(super) fn secret(mut self) -> Self {
        self.secret = true;
        self
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

    /// Show or hide the gutter of line numbers. Only a multiline field has one.
    pub(super) fn set_line_numbers(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.line_numbers != on {
            self.line_numbers = on;
            cx.notify();
        }
    }

    /// Color the text by the grammar of the file at `path`; `None` paints it
    /// plain.
    pub(super) fn set_syntax(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.syntax != path {
            self.syntax = path;
            self.highlighted = None;
            cx.notify();
        }
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

    /// Replace the value with the caret at the start, so a long document opens
    /// at its top.
    pub(super) fn set_value_at_start(&mut self, text: String, cx: &mut Context<Self>) {
        let mut buffer = Buffer::new(text);
        buffer.move_to(0, false);
        *self.buffer.borrow_mut() = buffer;
        self.marked = None;
        cx.notify();
    }

    /// Whether the value equals `text`, without copying a long document out.
    pub(super) fn holds(&self, text: &str) -> bool {
        self.buffer.borrow().text == text
    }

    /// The caret's byte offset.
    pub(super) fn caret_byte(&self) -> usize {
        self.buffer.borrow().head
    }

    /// The caret row's top and height within the field, as of the last paint,
    /// so a scrolling parent can keep it in view.
    pub(super) fn caret_row(&self) -> Option<(Pixels, Pixels)> {
        let last = self.last.borrow();
        let last = last.as_ref()?;
        let (_, row) = last.caret(self.buffer.borrow().head)?;
        Some((px(last.line_height.as_f32() * row as f32), last.line_height))
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
            self.command_chord_at = Some(Instant::now());
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
        if self.secret {
            return false;
        }
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
        let range = self.buffer.borrow().range();
        if self.secret {
            // Cut still removes the selection; it just keeps it off the clipboard.
            if range.start == range.end {
                return false;
            }
            self.delete_range(range, cx);
            return true;
        }
        if !self.copy(cx) {
            return false;
        }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A Command chord the window did not claim (Cmd+Shift+. in a window
        // without that action, Cmd+` for the OS window switch) still reaches
        // the input method, which would type its glyph. Command never
        // composes text, so drop it here and let the chord stay a shortcut.
        let chord_pending = self
            .command_chord_at
            .is_some_and(|at| at.elapsed() < COMMAND_CHORD_WINDOW);
        if window.modifiers().platform || chord_pending {
            return;
        }
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
        if let Some(path) = &self.syntax {
            if self
                .highlighted
                .as_ref()
                .is_none_or(|(held, _)| *held != text)
            {
                self.highlighted = Some((text.clone(), highlight::source(path, &text)));
            }
        } else {
            self.highlighted = None;
        }
        let spans: &[Span] = self
            .highlighted
            .as_ref()
            .map_or(&[], |(_, spans)| spans.as_slice());
        let display = if empty {
            if focused {
                String::new()
            } else {
                self.placeholder.to_string()
            }
        } else if self.secret {
            mask(&text)
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

        let field_width = self
            .last
            .borrow()
            .as_ref()
            .map_or(px(0.0), |last| last.bounds.size.width);
        let mut pieces: Vec<Piece> = Vec::new();
        // Multiline fields soft-wrap to the last laid-out width (unknown on
        // the first frame, so it paints unwrapped once). Single-line fields
        // never wrap: their content is one row by construction.
        let wrap_width: Option<Pixels> = if self.multiline {
            let last = self.last.borrow();
            let width = last
                .as_ref()
                .map(|last| last.bounds.size.width)
                .unwrap_or(px(0.0));
            (width > px(0.0)).then_some(width)
        } else {
            None
        };
        let font = self.font.clone();
        let measure = |text: &str| {
            window
                .text_system()
                .shape_line(
                    SharedString::new(text),
                    font_size,
                    &[TextRun {
                        len: text.len(),
                        font: font.clone(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width
        };
        let mut line_start = 0usize;
        // The logical line each display row opens, for the gutter.
        let mut numbers: Vec<Option<usize>> = Vec::new();
        for (line_index, segment) in display.split('\n').enumerate() {
            let line_end = line_start + segment.len();
            let ranges: Vec<(usize, usize)> = match wrap_width {
                Some(max) => wrap_pieces(segment, line_start, max, &measure),
                None => vec![(line_start, line_end)],
            };
            for (nth, (piece_start, piece_end)) in ranges.into_iter().enumerate() {
                numbers.push((nth == 0).then_some(line_index + 1));
                let look = |span: &Span| {
                    let look = highlight::look(span.token, theme);
                    (Hsla::from(look.color), look.italic, look.bold)
                };
                let runs: Vec<TextRun> = row_runs(
                    piece_start..piece_end,
                    selection.clone(),
                    spans,
                    &look,
                    color,
                    selection_fg,
                )
                .into_iter()
                .map(|run| {
                    let mut font = self.font.clone();
                    if run.italic {
                        font.style = gpui::FontStyle::Italic;
                    }
                    if run.bold {
                        font.weight = gpui::FontWeight::BOLD;
                    }
                    TextRun {
                        len: run.len,
                        font,
                        color: run.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }
                })
                .collect();
                let relative = &segment[piece_start - line_start..piece_end - line_start];
                let layout = window.text_system().shape_line(
                    SharedString::new(relative),
                    font_size,
                    &runs,
                    None,
                );
                pieces.push(Piece {
                    byte_start: piece_start,
                    layout,
                });
            }
            line_start = line_end + 1;
        }

        let gutter = (self.line_numbers && self.multiline).then(|| {
            let total = numbers.iter().flatten().last().copied().unwrap_or(1);
            let digits = total.to_string().len().max(MIN_GUTTER_DIGITS);
            let caret_line = text[..head.min(text.len())].matches('\n').count() + 1;
            let shape = |number: usize| {
                let label = number.to_string();
                let color: Hsla = if number == caret_line {
                    theme.fg.into()
                } else {
                    theme.muted.into()
                };
                window.text_system().shape_line(
                    SharedString::new(label.clone()),
                    font_size,
                    &[TextRun {
                        len: label.len(),
                        font: self.font.clone(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
            };
            Gutter {
                width: measure("0") * digits as f32 + px(LINE_NUMBER_GAP),
                rows: numbers.iter().map(|number| number.map(shape)).collect(),
            }
        });

        let marked = self.marked.clone();
        let caret_visible = focused && self.caret_on;
        let row_count = pieces.len();
        let height = self.height(row_count);

        // A single-line field never wraps, so text longer than the box slides
        // left to keep the caret on screen. The width is last frame's.
        let scroll = match (self.multiline, field_width) {
            (false, width) if width > px(0.0) => pieces
                .first()
                .map(|piece| {
                    let caret = piece.layout.x_for_index(head.min(piece.layout.len()));
                    (caret + px(CARET_WIDTH) - width).max(px(0.0))
                })
                .unwrap_or(px(0.0)),
            _ => px(0.0),
        };

        // Store the shaped frame for pointer hit-testing and IME bounds.
        {
            let mut last = self.last.borrow_mut();
            *last = Some(LastLayout {
                bounds: Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(0.0), height),
                },
                pieces: pieces.clone(),
                line_height,
                scroll,
            });
        }

        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let last = self.last.clone();
        let accent: Hsla = theme.accent.into();

        let root = div()
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
            });
        let text_canvas = canvas(
            move |bounds, _window, _cx| {
                if let Some(last) = last.borrow_mut().as_mut() {
                    last.bounds = bounds;
                }
            },
            move |field, (), window, cx| {
                // The text slides under the field's own edges, so
                // what scrolls out is clipped rather than painted
                // over the chrome beside it.
                let bounds = Bounds {
                    origin: point(field.origin.x - scroll, field.origin.y),
                    size: field.size,
                };
                window.with_content_mask(Some(gpui::ContentMask { bounds: field }), |window| {
                    // Selection sits under the glyphs so the foreground
                    // colour chosen above stays legible on it.
                    if selection.start < selection.end {
                        paint_range(
                            window,
                            bounds,
                            &pieces,
                            selection.clone(),
                            line_height,
                            selection_color,
                            px(0.0),
                            line_height,
                        );
                    }

                    for (row, piece) in pieces.iter().enumerate() {
                        let origin = point(
                            bounds.origin.x,
                            bounds.origin.y + px(line_height.as_f32() * row as f32),
                        );
                        let _ = piece.layout.paint(
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
                            &pieces,
                            marked.clone(),
                            line_height,
                            accent,
                            line_height - px(1.5),
                            px(1.0),
                        );
                    }

                    if caret_visible {
                        let piece_at = pieces.iter().rposition(|piece| piece.byte_start <= head);
                        if let Some(row) = piece_at {
                            let piece = &pieces[row];
                            let x = piece
                                .layout
                                .x_for_index(head.saturating_sub(piece.byte_start));
                            window.paint_quad(
                                fill(
                                    Bounds {
                                        origin: point(
                                            bounds.origin.x + x,
                                            bounds.origin.y + px(line_height.as_f32() * row as f32),
                                        ),
                                        size: size(px(CARET_WIDTH), line_height),
                                    },
                                    accent,
                                )
                                .corner_radii(px(2.0)),
                            );
                        }
                    }
                });

                window.handle_input(
                    &focus_handle,
                    ElementInputHandler::new(field, entity.clone()),
                    cx,
                );
            },
        )
        .size_full();
        match gutter {
            None => root.child(text_canvas),
            Some(gutter) => root
                .child(gutter.element(line_height, height))
                .child(div().flex_1().min_w(px(0.0)).h(height).child(text_canvas)),
        }
    }
}

/// The line numbers beside a multiline field, shaped for this frame.
struct Gutter {
    width: Pixels,
    /// A number on each display row that opens a logical line.
    rows: Vec<Option<ShapedLine>>,
}

impl Gutter {
    fn element(self, line_height: Pixels, height: Pixels) -> impl IntoElement {
        let Self { width, rows } = self;
        div().flex_shrink_0().w(width).h(height).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, cx| {
                    for (row, line) in rows.iter().enumerate() {
                        let Some(line) = line else {
                            continue;
                        };
                        // Right-aligned, so the digits line up by place.
                        let origin = point(
                            bounds.origin.x + width - px(LINE_NUMBER_GAP) - line.width,
                            bounds.origin.y + px(line_height.as_f32() * row as f32),
                        );
                        let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
                    }
                },
            )
            .size_full(),
        )
    }
}

/// Paint a selection or composition range, clipped per display row.
#[allow(clippy::too_many_arguments)]
fn paint_range(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    pieces: &[Piece],
    range: Range<usize>,
    line_height: Pixels,
    color: Hsla,
    y_offset: Pixels,
    height: Pixels,
) {
    for (row, piece) in pieces.iter().enumerate() {
        let start = piece.byte_start;
        let end = pieces
            .get(row + 1)
            .map(|next| next.byte_start)
            .unwrap_or(usize::MAX);
        let low = range.start.max(start).min(end);
        let high = range.end.max(start).min(end);
        if low >= high {
            continue;
        }
        let x0 = piece.layout.x_for_index(low.saturating_sub(start));
        let x1 = piece.layout.x_for_index(high.saturating_sub(start));
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
    fn wrap_pieces_break_at_spaces_first() {
        // Widths in columns through a counting measure.
        let measure = |text: &str| px(text.len() as f32);
        assert_eq!(
            wrap_pieces("aa bb cc", 0, px(5.0), &measure),
            vec![(0, 3), (3, 8)]
        );
        // One line, no breaks needed.
        assert_eq!(wrap_pieces("hi", 10, px(10.0), &measure), vec![(10, 12)]);
        // Empty lines still paint one row.
        assert_eq!(wrap_pieces("", 4, px(10.0), &measure), vec![(4, 4)]);
    }

    #[test]
    fn wrap_pieces_splits_overlong_words_mid_word() {
        let measure = |text: &str| px(text.len() as f32);
        assert_eq!(
            wrap_pieces("abcdef", 0, px(3.0), &measure),
            vec![(0, 3), (3, 6)]
        );
        // Ranges tile contiguously, so display rows map back to bytes. A
        // word wider than the field splits mid-word: `aa | bcdef | gh`
        // becomes `aa | bcd | ef | gh`.
        let pieces = wrap_pieces("aa bcdef gh", 0, px(4.0), &measure);
        assert_eq!(pieces, vec![(0, 3), (3, 6), (6, 9), (9, 11)]);
        let mut cursor = 0;
        for (start, end) in &pieces {
            assert_eq!(*start, cursor);
            cursor = *end;
        }
        assert_eq!(cursor, 11);
    }

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
    fn a_masked_field_keeps_every_byte_offset() {
        let text = "sk-é😀";
        let masked = mask(text);
        assert_eq!(masked.len(), text.len());
        assert!(masked.chars().all(|ch| ch == '*'));
        for (at, _) in text.char_indices() {
            assert!(masked.is_char_boundary(at));
        }
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

    fn color(value: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l: value,
            a: 1.0,
        }
    }

    fn span(range: Range<usize>, token: highlight::Token) -> Span {
        Span { range, token }
    }

    fn runs(piece: Range<usize>, selection: Range<usize>, spans: &[Span]) -> Vec<RunStyle> {
        let look = |span: &Span| match span.token {
            highlight::Token::Comment => (color(0.3), true, false),
            _ => (color(0.6), false, true),
        };
        row_runs(piece, selection, spans, &look, color(0.9), color(1.0))
    }

    fn lens(runs: &[RunStyle]) -> Vec<usize> {
        runs.iter().map(|run| run.len).collect()
    }

    #[test]
    fn a_row_without_spans_or_selection_is_one_run() {
        let found = runs(10..20, 0..0, &[]);
        assert_eq!(lens(&found), [10]);
        assert_eq!(found[0].color, color(0.9));
    }

    #[test]
    fn spans_split_a_row_and_tile_it() {
        let spans = [
            span(2..5, highlight::Token::Keyword),
            span(8..30, highlight::Token::Comment),
        ];
        let found = runs(0..12, 0..0, &spans);
        assert_eq!(lens(&found), [2, 3, 3, 4]);
        assert_eq!(found.iter().map(|run| run.len).sum::<usize>(), 12);
        assert!(found[1].bold && !found[1].italic);
        assert!(found[3].italic, "a span running past the row is cut to it");
    }

    #[test]
    fn a_span_that_began_on_an_earlier_row_still_colors_this_one() {
        let spans = [span(0..40, highlight::Token::Comment)];
        let found = runs(20..30, 0..0, &spans);
        assert_eq!(lens(&found), [10]);
        assert!(found[0].italic);
    }

    #[test]
    fn the_selection_overrides_color_but_keeps_the_slant() {
        let spans = [span(0..10, highlight::Token::Comment)];
        let found = runs(0..10, 4..6, &spans);
        assert_eq!(lens(&found), [4, 2, 4]);
        assert_eq!(found[1].color, color(1.0));
        assert!(found[1].italic);
        assert_eq!(found[0].color, color(0.3));
    }

    #[test]
    fn an_empty_row_still_has_a_run() {
        let found = runs(5..5, 0..0, &[]);
        assert_eq!(lens(&found), [0]);
    }
}
