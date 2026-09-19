//! A single-line text field for the secret and number entry in the overlays.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    value: String,
    /// Cursor as a character index, not a byte offset: pasted secrets can hold anything.
    cursor: usize,
    /// First visible character, so a long token can still be edited in a narrow pane.
    scroll: usize,
    pub masked: bool,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_value(text: impl Into<String>) -> Self {
        let value = text.into();
        let cursor = value.chars().count();
        Self {
            value,
            cursor,
            scroll: 0,
            masked: false,
        }
    }

    pub fn secret(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Does the field hold anything a user would recognize as input?
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.value = text.into();
        self.cursor = self.value.chars().count();
        self.scroll = 0;
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.scroll = 0;
    }

    fn byte_at(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, ch: char) {
        if ch == '\n' || ch == '\r' || ch == '\t' {
            return;
        }
        let at = self.byte_at(self.cursor);
        self.value.insert(at, ch);
        self.cursor += 1;
    }

    /// Pasted text arrives in one piece; newlines would break a one-line field.
    pub fn paste(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert(ch);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let from = self.byte_at(self.cursor - 1);
        let to = self.byte_at(self.cursor);
        self.value.replace_range(from..to, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        let from = self.byte_at(self.cursor);
        let to = self.byte_at(self.cursor + 1);
        self.value.replace_range(from..to, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    /// Returns true when the value changed, so callers can tell an edit from a cursor move.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+R reveals a masked secret long enough to check which account it belongs to.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('r'))
        {
            self.masked = !self.masked;
            return false;
        }
        match key.code {
            KeyCode::Char(ch) => {
                self.insert(ch);
                true
            }
            KeyCode::Backspace => {
                let was = self.value.len();
                self.backspace();
                was != self.value.len()
            }
            KeyCode::Delete => {
                let was = self.value.len();
                self.delete();
                was != self.value.len()
            }
            KeyCode::Left => {
                self.left();
                false
            }
            KeyCode::Right => {
                self.right();
                false
            }
            KeyCode::Home => {
                self.home();
                false
            }
            KeyCode::End => {
                self.end();
                false
            }
            _ => false,
        }
    }

    /// The visible slice and where the cursor lands inside it, both in characters.
    pub fn display(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut scroll = self.scroll;
        if self.cursor < scroll {
            scroll = self.cursor;
        }
        if self.cursor >= scroll + width {
            // Keep the cursor at the right edge, except at the end of the value where
            // the last full window is more useful than an empty cell.
            scroll = if self.cursor >= chars.len() {
                chars.len().saturating_sub(width)
            } else {
                (self.cursor + 1).saturating_sub(width)
            };
        }
        let shown: String = chars
            .iter()
            .skip(scroll)
            .take(width)
            .map(|ch| if self.masked { '•' } else { *ch })
            .collect();
        let cursor_column = self.cursor.saturating_sub(scroll).min(width.saturating_sub(1));
        (shown, cursor_column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_happen_at_the_cursor() {
        let mut input = TextInput::new();
        input.paste("abc");
        input.left();
        input.insert('X');
        assert_eq!(input.value(), "abXc");
        input.backspace();
        assert_eq!(input.value(), "abc");
        input.home();
        input.delete();
        assert_eq!(input.value(), "bc");
        input.end();
        input.insert('!');
        assert_eq!(input.value(), "bc!");
    }

    #[test]
    fn paste_keeps_one_line() {
        let mut input = TextInput::new();
        input.paste("sk-ant-oat01\nabcdef\tgh");
        assert_eq!(input.value(), "sk-ant-oat01abcdefgh");
    }

    #[test]
    fn unicode_is_edited_by_character() {
        let mut input = TextInput::new();
        input.paste("비밀키");
        input.backspace();
        assert_eq!(input.value(), "비밀");
        // Multi-byte text must not panic on a byte-oriented slice.
        input.left();
        input.insert('x');
        assert_eq!(input.value(), "비x밀");
    }

    #[test]
    fn display_scrolls_to_keep_the_cursor_visible() {
        let mut input = TextInput::new();
        input.paste("0123456789");
        let (shown, cursor) = input.display(4);
        assert_eq!(shown, "6789");
        assert_eq!(cursor, 3);
        // In the middle the cursor rides the right edge of the window.
        input.home();
        for _ in 0..5 {
            input.right();
        }
        let (shown, cursor) = input.display(4);
        assert_eq!(shown, "2345");
        assert_eq!(cursor, 3);
        input.home();
        let (shown, cursor) = input.display(4);
        assert_eq!(shown, "0123");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn masked_value_renders_dots_but_keeps_the_secret() {
        let input = TextInput::with_value("sk-secret").secret();
        let (shown, _) = input.display(20);
        assert_eq!(shown, "•••••••••");
        assert_eq!(input.value(), "sk-secret");
        let mut input = input;
        assert!(input.handle_key(key(KeyCode::Char('!'))));
        assert_eq!(input.value(), "sk-secret!");
    }

    #[test]
    fn cursor_keys_do_not_count_as_edits() {
        let mut input = TextInput::with_value("abc");
        assert!(!input.handle_key(key(KeyCode::Left)));
        assert!(input.handle_key(key(KeyCode::Char('z'))));
        assert_eq!(input.value(), "abzc");
    }
}
