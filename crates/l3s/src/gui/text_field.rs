//! A minimal single-line edit buffer with a caret.
//!
//! GPUI has no built-in text input here, so the native native-chrome windows
//! share this model: they hold the string, a char-index cursor, and apply
//! insert/backspace/arrow operations from key events.

/// A single-line edit buffer with a caret.
#[derive(Default)]
pub struct TextField {
    /// The current text.
    pub value: String,
    /// Caret position in characters from the start.
    pub cursor: usize,
}

impl TextField {
    /// Begin editing with `value` and the caret at the end.
    pub fn with_value(value: String) -> Self {
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn chars(&self) -> Vec<char> {
        self.value.chars().collect()
    }

    /// Insert `text` at the caret, dropping control characters.
    pub fn insert(&mut self, text: &str) {
        let clean: Vec<char> = text.chars().filter(|ch| !ch.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        let mut chars = self.chars();
        let at = self.cursor.min(chars.len());
        for (offset, ch) in clean.iter().enumerate() {
            chars.insert(at + offset, *ch);
        }
        self.cursor = at + clean.len();
        self.value = chars.into_iter().collect();
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        let mut chars = self.chars();
        let at = self.cursor.min(chars.len());
        if at == 0 {
            return;
        }
        chars.remove(at - 1);
        self.cursor = at - 1;
        self.value = chars.into_iter().collect();
    }

    /// Delete the character after the caret.
    pub fn delete(&mut self) {
        let mut chars = self.chars();
        let at = self.cursor.min(chars.len());
        if at < chars.len() {
            chars.remove(at);
        }
        self.value = chars.into_iter().collect();
    }

    /// Move the caret one character left.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the caret one character right.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }

    /// Move the caret to the start.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move the caret to the end.
    pub fn end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }
}
