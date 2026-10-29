//! Keys to actions. Pure: which action a key is depends only on the key and
//! on whether a text input has it, so tests drive the screens without a
//! terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a key asks for. The screen decides what it means where the focus
/// is: `Left` changes a choice on a choice row and moves to the section
/// list elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    First,
    Last,
    Left,
    Right,
    /// `Tab`: the other pane.
    Pane,
    Enter,
    /// `Space`: flip a toggle.
    Toggle,
    /// `Esc`: back one level, or cancel an input.
    Back,
    Quit,
    Save,
    Editor,
    Filter,
    Help,
    /// `x`: the row back to its default.
    Reset,
    Add,
    Delete,
    Restart,
    // Text input.
    Type(char),
    Erase,
    EraseForward,
    Home,
    End,
    Clear,
}

/// Who has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// A text input: letters are text, `Enter` commits, `Esc` cancels.
    Text,
}

pub fn action(key: KeyEvent, mode: Mode) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl {
        return match key.code {
            KeyCode::Char('c') => Some(Action::Quit),
            KeyCode::Char('u') if mode == Mode::Text => Some(Action::Clear),
            _ => None,
        };
    }
    let common = match key.code {
        KeyCode::Enter => Some(Action::Enter),
        KeyCode::Esc => Some(Action::Back),
        KeyCode::Left => Some(Action::Left),
        KeyCode::Right => Some(Action::Right),
        KeyCode::Up => Some(Action::Up),
        KeyCode::Down => Some(Action::Down),
        _ => None,
    };
    if common.is_some() {
        return common;
    }
    match mode {
        Mode::Text => match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => Some(Action::Type(c)),
            KeyCode::Backspace => Some(Action::Erase),
            KeyCode::Delete => Some(Action::EraseForward),
            KeyCode::Home => Some(Action::Home),
            KeyCode::End => Some(Action::End),
            _ => None,
        },
        Mode::Browse => match key.code {
            KeyCode::Char('j') => Some(Action::Down),
            KeyCode::Char('k') => Some(Action::Up),
            KeyCode::Char('g') | KeyCode::Home => Some(Action::First),
            KeyCode::Char('G') | KeyCode::End => Some(Action::Last),
            KeyCode::Char('h') => Some(Action::Left),
            KeyCode::Char('l') => Some(Action::Right),
            KeyCode::Tab | KeyCode::BackTab => Some(Action::Pane),
            KeyCode::Char(' ') => Some(Action::Toggle),
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('w') => Some(Action::Save),
            KeyCode::Char('e') => Some(Action::Editor),
            KeyCode::Char('/') => Some(Action::Filter),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char('x') => Some(Action::Reset),
            KeyCode::Char('a') => Some(Action::Add),
            KeyCode::Char('d') => Some(Action::Delete),
            KeyCode::Char('r') => Some(Action::Restart),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn letters_are_commands_when_browsing_and_text_when_typing() {
        assert_eq!(
            action(press(KeyCode::Char('q')), Mode::Browse),
            Some(Action::Quit)
        );
        assert_eq!(
            action(press(KeyCode::Char('q')), Mode::Text),
            Some(Action::Type('q'))
        );
        assert_eq!(
            action(press(KeyCode::Char('j')), Mode::Browse),
            Some(Action::Down)
        );
        assert_eq!(
            action(press(KeyCode::Char('G')), Mode::Browse),
            Some(Action::Last)
        );
        assert_eq!(
            action(press(KeyCode::Char(' ')), Mode::Text),
            Some(Action::Type(' '))
        );
    }

    #[test]
    fn enter_esc_and_arrows_mean_the_same_in_both_modes() {
        for mode in [Mode::Browse, Mode::Text] {
            assert_eq!(action(press(KeyCode::Enter), mode), Some(Action::Enter));
            assert_eq!(action(press(KeyCode::Esc), mode), Some(Action::Back));
            assert_eq!(action(press(KeyCode::Left), mode), Some(Action::Left));
        }
    }

    #[test]
    fn ctrl_c_always_quits_and_ctrl_u_clears_an_input() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(action(ctrl('c'), Mode::Browse), Some(Action::Quit));
        assert_eq!(action(ctrl('c'), Mode::Text), Some(Action::Quit));
        assert_eq!(action(ctrl('u'), Mode::Text), Some(Action::Clear));
        assert_eq!(action(ctrl('u'), Mode::Browse), None);
        assert_eq!(action(ctrl('w'), Mode::Browse), None);
    }
}
