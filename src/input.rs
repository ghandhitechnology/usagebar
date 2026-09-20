//! A single-line text field for the secret and number entry in the overlays.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    value: String,
    /// Cursor as a grapheme index, so a visible character is edited as one unit.
    cursor: usize,
    pub masked: bool,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_value(text: impl Into<String>) -> Self {
        let value = text.into();
        let cursor = value.graphemes(true).count();
        Self {
            value,
            cursor,
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
        self.cursor = self.value.graphemes(true).count();
    }

    fn byte_at(&self, grapheme_index: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .nth(grapheme_index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, ch: char) {
        self.paste(&ch.to_string());
    }

    /// Pasted text arrives in one piece; newlines would break a one-line field.
    pub fn paste(&mut self, text: &str) {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        let at = self.byte_at(self.cursor);
        self.value.insert_str(at, &text);
        // Inserting a combining mark or a joiner can merge adjacent graphemes.
        self.cursor = self
            .value
            .grapheme_indices(true)
            .take_while(|(byte, _)| *byte < at + text.len())
            .count();
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
        if self.cursor >= self.value.graphemes(true).count() {
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
        self.cursor = (self.cursor + 1).min(self.value.graphemes(true).count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.graphemes(true).count();
    }

    /// Returns true when the value changed, so callers can tell an edit from a cursor move.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+R reveals a masked secret long enough to check which account it belongs to.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
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

    /// The visible slice and cursor column, measured in terminal cells.
    pub fn display(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let graphemes: Vec<&str> = self
            .value
            .graphemes(true)
            .map(|text| if self.masked { "•" } else { text })
            .collect();
        let cursor = self.cursor.min(graphemes.len());
        let cursor_width = graphemes.get(cursor).map_or(0, |text| text.width().max(1));
        let budget = width.saturating_sub(cursor_width);
        let mut scroll = cursor;
        let mut column = 0;
        while scroll > 0 && column + graphemes[scroll - 1].width() <= budget {
            scroll -= 1;
            column += graphemes[scroll].width();
        }
        let mut shown = String::new();
        let mut used = 0;
        let mut last_column = 0;
        for text in &graphemes[scroll..] {
            if used + text.width() > width {
                break;
            }
            last_column = used;
            used += text.width();
            shown.push_str(text);
        }
        (shown, if column >= width { last_column } else { column })
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

    #[test]
    fn edits_combining_marks_and_joined_emoji_as_one_character() {
        let mut input = TextInput::with_value("Ae\u{301}👩‍💻한");
        input.left();
        input.backspace();
        assert_eq!(input.value(), "Ae\u{301}한");
        input.left();
        input.delete();
        assert_eq!(input.value(), "A한");
        input.home();
        input.right();
        input.insert('\u{301}');
        input.backspace();
        assert_eq!(input.value(), "한");
    }

    #[test]
    fn display_uses_cells_and_keeps_wide_graphemes_whole() {
        let mut input = TextInput::with_value("/한글/e\u{301}👩‍💻");
        let (shown, cursor) = input.display(5);
        assert_eq!(shown, "/e\u{301}👩‍💻");
        assert_eq!(cursor, 4);
        input.left();
        assert_eq!(input.display(4), ("/e\u{301}👩‍💻".into(), 2));
        input.home();
        input.right();
        assert_eq!(input.display(2), ("한".into(), 0));
        assert_eq!(input.display(1), (String::new(), 0));
        assert_eq!(input.display(0), (String::new(), 0));
        let input = TextInput::with_value("가나다");
        assert_eq!(input.display(4), ("나다".into(), 2));
        let input = TextInput::with_value("e\u{301}👩‍💻한").secret();
        assert_eq!(input.display(8), ("•••".into(), 3));
    }
}
