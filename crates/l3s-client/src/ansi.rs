//! Parses the styled text of a UHP `terminal.frame` into screen rows.
//!
//! `terminal.backend.observe` does not stream raw PTY bytes. It sends the
//! already-rendered screen as one complete replacement per frame: rows joined
//! by `\n`, with colour expressed only through SGR (`ESC [ … m`) sequences.
//! There is no cursor addressing, no scrolling, and no `\r`, so no VT emulator
//! is needed — this module folds the sequence of styled runs straight into a
//! grid of cells.

/// A colour selected by an SGR sequence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Color {
    /// The terminal's own default.
    #[default]
    Default,
    /// One of the 256 palette slots.
    Indexed(u8),
    /// A direct 24-bit colour.
    Rgb(u8, u8, u8),
}

/// The attributes in force for a run of characters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Bold or increased intensity.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Reverse video (swap foreground and background).
    pub reversed: bool,
}

/// A maximal run of identically styled characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Run {
    /// The run's characters, escape sequences removed.
    pub text: String,
    /// The style applied to all of `text`.
    pub style: Style,
}

/// A parsed frame: one `Vec<Run>` per screen row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Screen {
    /// Rows top to bottom, each a left-to-right list of styled runs.
    pub rows: Vec<Vec<Run>>,
}

impl Screen {
    /// The text of each row with all styling dropped, for logging and probes.
    pub fn plain(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| row.iter().map(|run| run.text.as_str()).collect())
            .collect()
    }
}

/// Parse one frame's styled text into exactly `lines` rows.
pub fn parse(text: &str, lines: u16) -> Screen {
    let mut rows: Vec<Vec<Run>> = Vec::new();
    let mut current: Vec<Run> = Vec::new();
    let mut run = String::new();
    let mut style = Style::default();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' if chars.peek() == Some(&'[') => {
                chars.next();
                let mut params = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        if next == 'm' {
                            // Only split when the state actually changes. Btop
                            // re-emits the same SGR per cell; splitting on every
                            // sequence would multiply the spans per frame.
                            let mut updated = style;
                            apply_sgr(&params, &mut updated);
                            if updated != style {
                                push_run(&mut current, &mut run, style);
                                style = updated;
                            }
                        }
                        break;
                    }
                    params.push(next);
                }
            }
            '\n' => {
                push_run(&mut current, &mut run, style);
                rows.push(std::mem::take(&mut current));
            }
            '\r' => {}
            _ => run.push(ch),
        }
    }
    push_run(&mut current, &mut run, style);
    rows.push(current);
    rows.truncate(usize::from(lines));
    while rows.len() < usize::from(lines) {
        rows.push(Vec::new());
    }
    Screen { rows }
}

fn push_run(row: &mut Vec<Run>, run: &mut String, style: Style) {
    if !run.is_empty() {
        row.push(Run {
            text: std::mem::take(run),
            style,
        });
    }
}

/// Apply one SGR parameter list to the running style.
fn apply_sgr(params: &str, style: &mut Style) {
    let numbers: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut index = 0;
    while index < numbers.len() {
        let code = numbers[index];
        match code {
            0 => *style = Style::default(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 => style.underline = true,
            7 => style.reversed = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => style.underline = false,
            27 => style.reversed = false,
            39 => style.fg = Color::Default,
            49 => style.bg = Color::Default,
            30..=37 => style.fg = Color::Indexed((code - 30) as u8),
            90..=97 => style.fg = Color::Indexed((code - 82) as u8),
            40..=47 => style.bg = Color::Indexed((code - 40) as u8),
            100..=107 => style.bg = Color::Indexed((code - 92) as u8),
            38 | 48 => {
                let color = match numbers.get(index + 1) {
                    Some(5) => {
                        index += 2;
                        Color::Indexed(numbers.get(index).copied().unwrap_or(0) as u8)
                    }
                    Some(2) => {
                        let r = numbers.get(index + 2).copied().unwrap_or(0) as u8;
                        let g = numbers.get(index + 3).copied().unwrap_or(0) as u8;
                        let b = numbers.get(index + 4).copied().unwrap_or(0) as u8;
                        index += 4;
                        Color::Rgb(r, g, b)
                    }
                    _ => Color::Default,
                };
                if code == 38 {
                    style.fg = color;
                } else {
                    style.bg = color;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_sgr_does_not_split_runs() {
        // Btop emits the same colour per cell; that must stay one run or the
        // pane renderer builds a span per cell.
        let screen = parse("\u{1b}[31ma\u{1b}[31mb\u{1b}[31mc", 1);
        assert_eq!(screen.rows[0].len(), 1);
        assert_eq!(screen.rows[0][0].text, "abc");
        assert_eq!(screen.rows[0][0].style.fg, Color::Indexed(1));
    }

    #[test]
    fn splits_rows_and_strips_sgr() {
        let screen = parse("\n\x1b[0;38;5;250mhi\x1b[0m\n\n", 3);
        assert_eq!(screen.plain(), vec!["", "hi", ""]);
        assert_eq!(screen.rows[1][0].style.fg, Color::Indexed(250));
    }

    #[test]
    fn parses_truecolor_and_attributes() {
        let screen = parse("\x1b[0;1;38;2;10;20;30;48;2;1;2;3mx", 1);
        let style = screen.rows[0][0].style;
        assert_eq!(style.fg, Color::Rgb(10, 20, 30));
        assert_eq!(style.bg, Color::Rgb(1, 2, 3));
        assert!(style.bold);
    }

    #[test]
    fn pads_to_the_requested_line_count() {
        assert_eq!(parse("one", 4).rows.len(), 4);
        assert_eq!(parse("a\nb", 1).rows.len(), 1);
    }

    #[test]
    fn coordinates_are_not_interpreted() {
        // A literal `\r` never appears in a frame, but drop it if it does
        // rather than starting a new row.
        let screen = parse("a\rb", 1);
        assert_eq!(screen.plain(), vec!["ab"]);
    }
}
