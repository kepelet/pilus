//! Local echo for slow links, after mosh's speculative echo.
//!
//! On a remote endpoint a typed key shows up only after a full round trip:
//! the key travels to the daemon, the program in the pane echoes it, and the
//! daemon's next diff carries the echo back. On a slow link every keystroke
//! waits that long. The window instead guesses where a plain printable key
//! will land and draws it at once; the daemon's frames then confirm or refute
//! each guess. This module only keeps the guesses. The window feeds it keys and
//! frames and paints [`Predictor::overlay`] in place of the daemon's frame.
//!
//! The protocol carries no acknowledgement of input, so a guess is judged by
//! the server's caret: once the caret sits past the guessed cell on the same
//! row, the program has handled that key, and the cell either holds the
//! guessed character or it does not. A guess the caret never passes expires.
//!
//! As in mosh, any input that is not plain text (Enter, arrows, a control
//! chord, a paste, a click, a resize) starts a new epoch, and an epoch's
//! guesses stay hidden until the daemon confirms one of them. A password
//! prompt never echoes, so its epoch is never trusted and nothing is drawn.
//!
//! After such a boundary the window cannot know where the caret will be once
//! the daemon has handled it, so keys typed in the meantime are kept as a
//! blind run. When a frame shows the start of that run echoed just left of a
//! caret that has moved, the run is anchored there and the rest of it becomes
//! guesses. Typing straight on after Enter is therefore predicted from the
//! first echo, without waiting for a pause.
//!
//! Guesses are drawn only while the measured echo time is long enough to be
//! felt, and underlined once it is long enough that a wrong one could mislead.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use pilus_protocol::{CellData, ClientMessage, FrameData};

/// Echo time above which guesses are drawn. Mosh uses 30 ms of network round
/// trip; this is measured from key to drained frame, which also counts the
/// daemon's render cadence and the window's burst hold, so it sits higher.
const SHOW_ABOVE: Duration = Duration::from_millis(50);
/// Echo time below which guesses stop being drawn. The gap to [`SHOW_ABOVE`]
/// keeps a link near the threshold from flickering between the two.
const HIDE_BELOW: Duration = Duration::from_millis(30);
/// Echo time above which drawn guesses are underlined.
const FLAG_ABOVE: Duration = Duration::from_millis(100);
/// Echo time below which drawn guesses lose the underline.
const UNFLAG_BELOW: Duration = Duration::from_millis(70);
/// How long to assume input already sent takes to land before anything is
/// measured.
const DEFAULT_SETTLE: Duration = Duration::from_millis(250);
/// The longest a guess may wait for its echo when nothing is measured yet.
const DEFAULT_PATIENCE: Duration = Duration::from_secs(1);
/// The longest a guess may wait for its echo on any link.
const MAX_PATIENCE: Duration = Duration::from_secs(3);
/// Guesses and blind keys held at once. A program that never echoes would
/// otherwise grow the list for as long as the person types.
const MAX_PENDING: usize = 256;

/// Ratatui's `Modifier::UNDERLINED`, as the daemon packs cell attributes.
const UNDERLINED: u16 = 1 << 3;
/// Ratatui's `Modifier::REVERSED`. A guess borrows the style of the cell to its
/// left, and a reversed one there is usually a drawn caret, not text.
const REVERSED: u16 = 1 << 6;

/// One printable key drawn ahead of its echo.
#[derive(Clone, Copy, Debug)]
struct Guess {
    column: u16,
    row: u16,
    symbol: char,
    sent: Instant,
}

/// A printable key typed before the epoch knew where keys land.
#[derive(Clone, Copy, Debug)]
struct Blind {
    symbol: char,
    sent: Instant,
}

/// Speculative echo for one endpoint. Replace it when the window switches
/// endpoints: its timing and guesses belong to one link.
#[derive(Debug, Default)]
pub struct Predictor {
    /// Guesses in typing order, all on one row.
    guesses: Vec<Guess>,
    /// Where the next guess lands, once the epoch knows.
    anchor: Option<(u16, u16)>,
    /// Keys typed while the anchor was unknown.
    blind: Vec<Blind>,
    /// The daemon's caret and the text of its row when the epoch began. A
    /// blind run is only matched once one of them has changed, so text that
    /// was already on screen is not read as an echo.
    boundary: Option<((u16, u16), String)>,
    /// Whether the daemon has echoed a key of this epoch as guessed.
    trusted: bool,
    /// Every guess so far was confirmed and nothing typed since, so the
    /// daemon's caret is where the next key lands.
    caught_up: bool,
    /// When the last input of any kind was sent.
    last_input: Option<Instant>,
    /// Smoothed echo time.
    srtt: Option<Duration>,
    /// Whether the echo time is long enough to draw guesses.
    showing: bool,
    /// Whether the echo time is long enough to underline them.
    flagging: bool,
    /// The grid size guesses were made against.
    size: Option<(u16, u16)>,
}

impl Predictor {
    /// A predictor with no measurements yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The smoothed time from a key leaving to its echo being drained, once a
    /// guess has been confirmed.
    pub fn echo_time(&self) -> Option<Duration> {
        self.srtt
    }

    /// Note a key the window is about to send. `frame` is the daemon's grid as
    /// the window holds it. Returns whether the drawn overlay changed.
    pub fn key(&mut self, key: &KeyEvent, frame: Option<&FrameData>, now: Instant) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let Some(symbol) = printable(key) else {
            return self.interrupt(frame, now);
        };
        let Some(frame) = frame else {
            return self.interrupt(None, now);
        };
        self.size = Some((frame.width, frame.height));
        if self.guesses.len() + self.blind.len() >= MAX_PENDING {
            return self.interrupt(Some(frame), now);
        }
        let quiet = self
            .last_input
            .is_none_or(|last| now.saturating_duration_since(last) >= self.settle());
        self.last_input = Some(now);
        if self.anchor.is_none() && self.blind.is_empty() && (self.caught_up || quiet) {
            // Nothing is in flight, so the daemon's caret is where this lands.
            self.anchor = caret(frame);
        }
        let Some((column, row)) = self.anchor else {
            self.blind.push(Blind { symbol, sent: now });
            self.caught_up = false;
            return false;
        };
        // A key in the last column wraps, which this does not model.
        if column.saturating_add(1) >= frame.width || row >= frame.height {
            return self.interrupt(Some(frame), now);
        }
        self.guesses.push(Guess {
            column,
            row,
            symbol,
            sent: now,
        });
        self.anchor = Some((column + 1, row));
        self.caught_up = false;
        self.drawn()
    }

    /// Start a new epoch: input that is not plain text was sent, so drop every
    /// guess until the daemon shows where keys land again. Returns whether the
    /// drawn overlay changed.
    pub fn interrupt(&mut self, frame: Option<&FrameData>, now: Instant) -> bool {
        let was = self.drawn();
        self.guesses.clear();
        self.blind.clear();
        self.anchor = None;
        self.trusted = false;
        self.caught_up = false;
        self.last_input = Some(now);
        self.boundary = frame.and_then(|frame| caret(frame).map(|at| (at, row_text(frame, at.1))));
        was
    }

    /// Judge the guesses against a frame the daemon sent. Returns whether the
    /// drawn overlay changed.
    pub fn observe(&mut self, frame: Option<&FrameData>, now: Instant) -> bool {
        let Some(frame) = frame else {
            return self.interrupt(None, now);
        };
        let size = (frame.width, frame.height);
        if self.size.is_some_and(|known| known != size) {
            self.size = Some(size);
            return self.interrupt(Some(frame), now);
        }
        self.size = Some(size);
        let was = self.drawn();
        self.anchor_blind(frame, now);
        let mut confirmed = None;
        for (index, guess) in self.guesses.iter().enumerate() {
            match judge(guess, frame) {
                Verdict::Pending => {}
                Verdict::Confirmed => confirmed = Some(index),
                Verdict::Wrong => {
                    self.interrupt(Some(frame), now);
                    return was;
                }
            }
        }
        if let Some(index) = confirmed {
            if let Some(guess) = self.guesses.get(index) {
                self.sample(now.saturating_duration_since(guess.sent));
            }
            self.trusted = true;
            self.guesses.drain(..=index);
            if self.guesses.is_empty() {
                self.anchor = None;
                self.caught_up = true;
            }
        }
        was || self.drawn()
    }

    /// Drop guesses and blind keys whose echo is overdue. Called on the
    /// window's tick, so a guess the daemon never answers does not linger.
    /// Returns whether the drawn overlay changed.
    pub fn expire(&mut self, frame: Option<&FrameData>, now: Instant) -> bool {
        let patience = self.patience();
        let overdue = |sent: Instant| now.saturating_duration_since(sent) > patience;
        let stale = self
            .guesses
            .first()
            .is_some_and(|guess| overdue(guess.sent))
            || self.blind.first().is_some_and(|blind| overdue(blind.sent));
        if stale {
            return self.interrupt(frame, now);
        }
        false
    }

    /// The daemon's frame with the drawn guesses laid over it, or `None` when
    /// nothing is drawn and the frame should be painted as it is.
    pub fn overlay(&self, frame: &FrameData) -> Option<FrameData> {
        if !self.drawn() || self.size != Some((frame.width, frame.height)) {
            return None;
        }
        let mut out = frame.clone();
        let width = usize::from(frame.width);
        let mut caret = None;
        for guess in &self.guesses {
            let index = usize::from(guess.row) * width + usize::from(guess.column);
            let style_index = if guess.column > 0 { index - 1 } else { index };
            let style = out.cells.get(style_index)?.clone();
            let mut mods = style.mods & !(UNDERLINED | REVERSED);
            if self.flagging {
                mods |= UNDERLINED;
            }
            let cell = out.cells.get_mut(index)?;
            *cell = CellData {
                symbol: guess.symbol.to_string(),
                fg: style.fg,
                bg: style.bg,
                mods,
            };
            caret = Some((guess.column + 1, guess.row));
        }
        if caret.is_some() {
            out.cursor = caret;
            out.cursor_visible = true;
        }
        Some(out)
    }

    /// Whether any guess is painted right now.
    fn drawn(&self) -> bool {
        self.showing && self.trusted && !self.guesses.is_empty()
    }

    /// Find the blind run echoed just left of the daemon's caret, and turn the
    /// part not yet echoed into guesses from there.
    fn anchor_blind(&mut self, frame: &FrameData, now: Instant) {
        if self.blind.is_empty() {
            return;
        }
        let Some((column, row)) = caret(frame) else {
            return;
        };
        // Neither the caret nor its row changed since the boundary, so the
        // daemon has not handled it yet and any match would be old text.
        if self
            .boundary
            .as_ref()
            .is_some_and(|(at, text)| *at == (column, row) && *text == row_text(frame, row))
        {
            return;
        }
        let Some(echoed) = echoed_prefix(frame, column, row, &self.blind) else {
            return;
        };
        if let Some(last) = echoed.checked_sub(1).and_then(|at| self.blind.get(at)) {
            self.sample(now.saturating_duration_since(last.sent));
        }
        let rest: Vec<Blind> = self.blind.drain(..).skip(echoed).collect();
        self.trusted = true;
        let mut at = column;
        for blind in rest {
            if at.saturating_add(1) >= frame.width {
                self.interrupt(Some(frame), now);
                return;
            }
            self.guesses.push(Guess {
                column: at,
                row,
                symbol: blind.symbol,
                sent: blind.sent,
            });
            at += 1;
        }
        self.anchor = Some((at, row));
        self.caught_up = self.guesses.is_empty();
    }

    /// Fold one echo time into the smoothed estimate, as TCP does, and move the
    /// draw and underline switches across their thresholds.
    fn sample(&mut self, echo: Duration) {
        let srtt = match self.srtt {
            Some(srtt) => (srtt * 7 + echo) / 8,
            None => echo,
        };
        self.srtt = Some(srtt);
        if srtt > SHOW_ABOVE {
            self.showing = true;
        } else if srtt < HIDE_BELOW {
            self.showing = false;
        }
        if srtt > FLAG_ABOVE {
            self.flagging = true;
        } else if srtt < UNFLAG_BELOW {
            self.flagging = false;
        }
    }

    /// How long input already sent is assumed to take to land.
    fn settle(&self) -> Duration {
        self.srtt.map_or(DEFAULT_SETTLE, |srtt| {
            (srtt * 3 / 2).clamp(Duration::from_millis(50), Duration::from_secs(2))
        })
    }

    /// How long a guess may wait for its echo before it is given up.
    fn patience(&self) -> Duration {
        self.srtt.map_or(DEFAULT_PATIENCE, |srtt| {
            (srtt * 3 + Duration::from_millis(250)).min(MAX_PATIENCE)
        })
    }
}

/// Whether sending `message` moves the screen in a way guesses cannot follow.
///
/// Keys are judged by [`Predictor::key`] itself. Pointer moves only hover,
/// and the bookkeeping messages draw nothing.
pub fn interrupts_echo(message: &ClientMessage) -> bool {
    match message {
        ClientMessage::Key(_)
        | ClientMessage::HealthCheck { .. }
        | ClientMessage::ClipboardSucceeded { .. }
        | ClientMessage::TerminalColors(_)
        | ClientMessage::CellPixels { .. } => false,
        ClientMessage::Mouse(event) => !matches!(event.kind, MouseEventKind::Moved),
        _ => true,
    }
}

/// The character a key types, when it is plain printable ASCII.
///
/// Anything else (a chord, a named key, a non-ASCII glyph whose width this
/// does not model) is a boundary.
fn printable(key: &KeyEvent) -> Option<char> {
    if !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Char(symbol) if symbol.is_ascii_graphic() || symbol == ' ' => Some(symbol),
        _ => None,
    }
}

/// The daemon's caret, when it is shown.
fn caret(frame: &FrameData) -> Option<(u16, u16)> {
    frame.cursor.filter(|_| frame.cursor_visible)
}

/// The symbol in a cell, if the cell exists.
fn symbol_at(frame: &FrameData, column: u16, row: u16) -> Option<&str> {
    if column >= frame.width || row >= frame.height {
        return None;
    }
    let index = usize::from(row) * usize::from(frame.width) + usize::from(column);
    frame.cells.get(index).map(|cell| cell.symbol.as_str())
}

/// A row's symbols joined, for telling whether it changed.
fn row_text(frame: &FrameData, row: u16) -> String {
    let width = usize::from(frame.width);
    let start = usize::from(row) * width;
    frame
        .cells
        .get(start..start + width)
        .unwrap_or_default()
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

/// How many keys of `blind`, from the start, sit echoed just left of the caret.
///
/// The longest such run wins. A run of spaces alone is not taken as an echo,
/// since blank cells sit left of most carets.
fn echoed_prefix(frame: &FrameData, column: u16, row: u16, blind: &[Blind]) -> Option<usize> {
    let most = blind.len().min(usize::from(column));
    (1..=most).rev().find(|&count| {
        let run = blind.get(..count).unwrap_or_default();
        if run.iter().all(|key| key.symbol == ' ') {
            return false;
        }
        let start = column - count as u16;
        run.iter().enumerate().all(|(offset, key)| {
            let mut buffer = [0; 4];
            symbol_at(frame, start + offset as u16, row)
                == Some(key.symbol.encode_utf8(&mut buffer) as &str)
        })
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The daemon has not handled the key yet.
    Pending,
    /// The daemon put the guessed character where it was guessed.
    Confirmed,
    /// The daemon handled the key and the screen disagrees.
    Wrong,
}

fn judge(guess: &Guess, frame: &FrameData) -> Verdict {
    let Some((column, row)) = caret(frame) else {
        return Verdict::Pending;
    };
    if row != guess.row {
        return Verdict::Wrong;
    }
    if column <= guess.column {
        return Verdict::Pending;
    }
    let mut buffer = [0; 4];
    if symbol_at(frame, guess.column, guess.row)
        == Some(guess.symbol.encode_utf8(&mut buffer) as &str)
    {
        Verdict::Confirmed
    } else {
        Verdict::Wrong
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u16 = 20;

    /// A one-row screen holding `text`, with the caret at `caret`.
    fn screen(text: &str, caret: u16) -> FrameData {
        let mut cells: Vec<CellData> = (0..WIDTH).map(|_| CellData::blank(7, 0)).collect();
        for (cell, symbol) in cells.iter_mut().zip(text.chars()) {
            cell.symbol = symbol.to_string();
        }
        FrameData {
            width: WIDTH,
            height: 1,
            cells,
            hyperlinks: Vec::new(),
            cursor: Some((caret, 0)),
            cursor_visible: true,
        }
    }

    fn press(symbol: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(symbol), KeyModifiers::NONE)
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn drawn_text(predictor: &Predictor, frame: &FrameData) -> Option<String> {
        predictor.overlay(frame).map(|out| {
            out.cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
    }

    /// A predictor that has measured a slow link and trusts its epoch: `a`
    /// typed at `$ ` and echoed after 200 ms.
    fn slow_and_trusted(start: Instant) -> (Predictor, Instant) {
        let mut predictor = Predictor::new();
        predictor.key(&press('a'), Some(&screen("$ ", 2)), start);
        let later = start + Duration::from_millis(200);
        predictor.observe(Some(&screen("$ a", 3)), later);
        assert!(predictor.trusted && predictor.showing);
        (predictor, later)
    }

    #[test]
    fn a_confirmed_epoch_draws_keys_before_their_echo() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        let frame = screen("$ a", 3);
        assert!(predictor.key(&press('b'), Some(&frame), now));
        predictor.key(&press('c'), Some(&frame), now);
        assert_eq!(drawn_text(&predictor, &frame).as_deref(), Some("$ abc"));
        let caret = predictor.overlay(&frame).and_then(|out| out.cursor);
        assert_eq!(caret, Some((5, 0)));
        // The underline marks a guess on a link this slow.
        let out = predictor.overlay(&frame).expect("drawn");
        assert_ne!(out.cells[3].mods & UNDERLINED, 0);
    }

    #[test]
    fn nothing_is_drawn_until_the_epoch_is_confirmed() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        let prompt = screen("Password: ", 10);
        predictor.key(&enter(), Some(&screen("$ a", 3)), now);
        let later = now + Duration::from_secs(1);
        predictor.key(&press('s'), Some(&prompt), later);
        predictor.key(&press('e'), Some(&prompt), later);
        assert_eq!(drawn_text(&predictor, &prompt), None);
        // The prompt never echoes; the keys expire without being drawn.
        assert!(!predictor.observe(Some(&prompt), later + Duration::from_millis(400)));
        predictor.expire(Some(&prompt), later + Duration::from_secs(5));
        assert!(predictor.guesses.is_empty() && predictor.blind.is_empty());
    }

    #[test]
    fn a_wrong_guess_is_dropped_once_the_caret_passes_it() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        predictor.key(&press('b'), Some(&screen("$ a", 3)), now);
        assert!(predictor.drawn());
        // The program put something else there, so the epoch ends.
        assert!(predictor.observe(Some(&screen("$ aX", 4)), now));
        assert_eq!(drawn_text(&predictor, &screen("$ aX", 4)), None);
        assert!(!predictor.trusted);
    }

    #[test]
    fn a_pending_guess_survives_frames_from_before_its_key() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        let frame = screen("$ a", 3);
        predictor.key(&press('b'), Some(&frame), now);
        predictor.observe(Some(&frame), now + Duration::from_millis(10));
        assert_eq!(drawn_text(&predictor, &frame).as_deref(), Some("$ ab"));
    }

    #[test]
    fn keys_typed_straight_after_enter_anchor_on_the_first_echo() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        let old = screen("$ a", 3);
        predictor.key(&enter(), Some(&old), now);
        // `l` and `s` go out before the daemon has drawn the new prompt.
        predictor.key(&press('l'), Some(&old), now + Duration::from_millis(20));
        predictor.key(&press('s'), Some(&old), now + Duration::from_millis(40));
        assert_eq!(drawn_text(&predictor, &old), None);
        // The new prompt shows `l` echoed; `s` is still on its way.
        let echoed = screen("> l", 3);
        predictor.observe(Some(&echoed), now + Duration::from_millis(220));
        assert_eq!(drawn_text(&predictor, &echoed).as_deref(), Some("> ls"));
    }

    #[test]
    fn old_text_left_of_an_unmoved_caret_is_not_an_echo() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        let frame = screen("$ make al", 9);
        predictor.key(&enter(), Some(&frame), now);
        predictor.key(&press('l'), Some(&frame), now + Duration::from_millis(10));
        predictor.key(&press('s'), Some(&frame), now + Duration::from_millis(20));
        predictor.observe(Some(&frame), now + Duration::from_millis(30));
        assert!(!predictor.trusted);
        assert_eq!(drawn_text(&predictor, &frame), None);
    }

    #[test]
    fn a_fast_link_draws_nothing() {
        let start = Instant::now();
        let mut predictor = Predictor::new();
        let mut now = start;
        let mut text = String::from("$ ");
        for symbol in "abcdef".chars() {
            let frame = screen(&text, text.len() as u16);
            predictor.key(&press(symbol), Some(&frame), now);
            now += Duration::from_millis(5);
            text.push(symbol);
            predictor.observe(Some(&screen(&text, text.len() as u16)), now);
            now += Duration::from_millis(100);
        }
        assert!(!predictor.showing);
        predictor.key(&press('g'), Some(&screen(&text, 8)), now);
        assert_eq!(drawn_text(&predictor, &screen(&text, 8)), None);
    }

    #[test]
    fn a_resize_ends_the_epoch() {
        let start = Instant::now();
        let (mut predictor, now) = slow_and_trusted(start);
        predictor.key(&press('b'), Some(&screen("$ a", 3)), now);
        let mut wider = screen("$ a", 3);
        wider.width = WIDTH + 1;
        wider.cells.push(CellData::blank(7, 0));
        predictor.observe(Some(&wider), now);
        assert!(predictor.guesses.is_empty());
    }

    #[test]
    fn hover_does_not_interrupt_but_a_click_does() {
        use crossterm::event::{MouseButton, MouseEvent};
        let at = |kind| {
            ClientMessage::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(!interrupts_echo(&at(MouseEventKind::Moved)));
        assert!(interrupts_echo(&at(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(interrupts_echo(&ClientMessage::Paste("x".into())));
        assert!(!interrupts_echo(&ClientMessage::HealthCheck { nonce: 1 }));
    }

    #[test]
    fn chords_and_non_ascii_are_not_printable() {
        assert_eq!(printable(&press('a')), Some('a'));
        assert_eq!(
            printable(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
        assert_eq!(
            printable(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(printable(&press('é')), None);
        assert_eq!(printable(&enter()), None);
    }
}
