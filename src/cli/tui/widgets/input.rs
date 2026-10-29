//! A one-line text input. `Enter` commits and `Esc` cancels (the screen
//! does both); this edits the text. A secret shows a bullet per character.

use ratatui::style::Modifier;
use ratatui::text::Span;

use super::Ui;
use crate::tui::keys::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    chars: Vec<char>,
    /// Where the next character goes, in characters.
    cursor: usize,
    secret: bool,
}

impl Input {
    /// An input holding `text`, the cursor at its end.
    pub fn new(text: &str, secret: bool) -> Input {
        let chars: Vec<char> = text.chars().collect();
        Input {
            cursor: chars.len(),
            chars,
            secret,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Apply an editing action; `false` for one that is not editing.
    pub fn apply(&mut self, action: Action) -> bool {
        match action {
            Action::Type(c) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            Action::Erase if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            Action::EraseForward if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            Action::Left => self.cursor = self.cursor.saturating_sub(1),
            Action::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Action::Home => self.cursor = 0,
            Action::End => self.cursor = self.chars.len(),
            Action::Clear => {
                self.chars.clear();
                self.cursor = 0;
            }
            Action::Erase | Action::EraseForward => {}
            _ => return false,
        }
        true
    }

    /// The input in `room` columns: the text (bullets for a secret), the
    /// cursor cell reversed, scrolled so the cursor stays in view.
    pub fn spans(&self, ui: &Ui, room: usize) -> Vec<Span<'static>> {
        let shown: Vec<String> = self
            .chars
            .iter()
            .map(|c| {
                if self.secret {
                    ui.glyphs.bullet.to_string()
                } else {
                    c.to_string()
                }
            })
            .collect();
        let room = room.max(1);
        let start = (self.cursor + 1).saturating_sub(room);
        let end = (start + room).min(shown.len());
        let before: String = shown[start..self.cursor].concat();
        let at = shown
            .get(self.cursor)
            .cloned()
            .unwrap_or_else(|| " ".into());
        let after: String = shown
            .get(self.cursor + 1..end.max(self.cursor + 1))
            .map(|s| s.concat())
            .unwrap_or_default();
        let style = ui.theme.accent;
        vec![
            Span::styled(before, style),
            Span::styled(at, style.add_modifier(Modifier::REVERSED)),
            Span::styled(after, style),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_moving_and_erasing() {
        let mut input = Input::new("nova", false);
        for c in "-3".chars() {
            input.apply(Action::Type(c));
        }
        assert_eq!(input.text(), "nova-3");
        input.apply(Action::Home);
        input.apply(Action::EraseForward);
        input.apply(Action::Type('N'));
        assert_eq!(input.text(), "Nova-3");
        input.apply(Action::End);
        input.apply(Action::Erase);
        input.apply(Action::Erase);
        assert_eq!(input.text(), "Nova");
        input.apply(Action::Clear);
        assert_eq!(input.text(), "");
        // Erasing an empty input is still an edit, and does nothing.
        assert!(input.apply(Action::Erase));
        assert!(!input.apply(Action::Up));
    }

    #[test]
    fn a_multibyte_text_edits_by_character() {
        let mut input = Input::new("caf\u{e9}", false);
        input.apply(Action::Erase);
        assert_eq!(input.text(), "caf");
    }
}
