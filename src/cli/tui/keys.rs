//! Keys to actions. Pure: which action a key is depends only on the key and
//! on who has the keys (a row list, a text input, a picker, a dialog), so
//! tests drive the screens without a terminal.

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
    /// `H`, `L`, `Shift+Left`, `Shift+Right`: a number by its big step.
    BigLeft,
    BigRight,
    PageUp,
    PageDown,
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
    /// `t`: test the key (or model) of the row.
    Test,
    /// `s`: skip an optional wizard step.
    Skip,
    /// `Backspace` on the rows: the wizard's previous step.
    Previous,
    // Text input.
    Type(char),
    Erase,
    EraseForward,
    Home,
    End,
    Clear,
    /// `Tab` in a path input: complete it.
    Complete,
}

/// Who has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// A text input: letters are text, `Enter` commits, `Esc` cancels.
    Text,
    /// A dialog: a letter is the letter its line names (`w save`), in any
    /// case.
    Dialog,
    /// A picker: letters filter, the arrows, `Ctrl+P`/`Ctrl+N`, `PgUp`/
    /// `PgDn` and `Home`/`End` move, `Enter` picks, `Esc` leaves.
    Picker,
    /// The hotkey dialog: `Tab` and `j`/`k` move between its parts, `Space`
    /// and `h`/`l` change one, `t` types the hotkey instead, `x` unsets it.
    Hotkey,
}

pub fn action(key: KeyEvent, mode: Mode) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if ctrl {
        return match key.code {
            KeyCode::Char('c') => Some(Action::Quit),
            KeyCode::Char('u') if matches!(mode, Mode::Text | Mode::Picker) => Some(Action::Clear),
            KeyCode::Char('p') if mode == Mode::Picker => Some(Action::Up),
            KeyCode::Char('n') if mode == Mode::Picker => Some(Action::Down),
            _ => None,
        };
    }
    let common = match key.code {
        KeyCode::Enter => Some(Action::Enter),
        KeyCode::Esc => Some(Action::Back),
        KeyCode::Left if shift && mode == Mode::Browse => Some(Action::BigLeft),
        KeyCode::Right if shift && mode == Mode::Browse => Some(Action::BigRight),
        KeyCode::Left => Some(Action::Left),
        KeyCode::Right => Some(Action::Right),
        KeyCode::Up => Some(Action::Up),
        KeyCode::Down => Some(Action::Down),
        // Only where a list moves: in a dialog a page key is no answer.
        KeyCode::PageUp if matches!(mode, Mode::Browse | Mode::Picker) => Some(Action::PageUp),
        KeyCode::PageDown if matches!(mode, Mode::Browse | Mode::Picker) => Some(Action::PageDown),
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
            KeyCode::Tab => Some(Action::Complete),
            _ => None,
        },
        Mode::Picker => match key.code {
            // No value has a `?`: it is the picker's help.
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => Some(Action::Type(c)),
            KeyCode::Backspace => Some(Action::Erase),
            KeyCode::Home => Some(Action::First),
            KeyCode::End => Some(Action::Last),
            KeyCode::Tab => Some(Action::Complete),
            _ => None,
        },
        Mode::Hotkey => match key.code {
            KeyCode::Char('j') | KeyCode::Tab => Some(Action::Down),
            KeyCode::Char('k') | KeyCode::BackTab => Some(Action::Up),
            KeyCode::Char('h') => Some(Action::Left),
            KeyCode::Char('l') => Some(Action::Right),
            KeyCode::Char(' ') => Some(Action::Toggle),
            KeyCode::Char('x') => Some(Action::Reset),
            KeyCode::Char('t') => Some(Action::Type('t')),
            KeyCode::Char('?') => Some(Action::Help),
            _ => None,
        },
        Mode::Dialog => match key.code {
            KeyCode::Char(c) => Some(Action::Type(c.to_ascii_lowercase())),
            _ => None,
        },
        Mode::Browse => match key.code {
            KeyCode::Char('j') => Some(Action::Down),
            KeyCode::Char('k') => Some(Action::Up),
            KeyCode::Char('g') | KeyCode::Home => Some(Action::First),
            KeyCode::Char('G') | KeyCode::End => Some(Action::Last),
            KeyCode::Char('h') => Some(Action::Left),
            KeyCode::Char('l') => Some(Action::Right),
            KeyCode::Char('H') => Some(Action::BigLeft),
            KeyCode::Char('L') => Some(Action::BigRight),
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
            KeyCode::Char('t') => Some(Action::Test),
            KeyCode::Char('s') => Some(Action::Skip),
            KeyCode::Backspace => Some(Action::Previous),
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

    /// A dialog's letters are the ones it names, even those that move
    /// elsewhere (`l`, `d`).
    #[test]
    fn a_dialog_gets_its_letters() {
        assert_eq!(
            action(press(KeyCode::Char('l')), Mode::Dialog),
            Some(Action::Type('l'))
        );
        assert_eq!(
            action(press(KeyCode::Char('Y')), Mode::Dialog),
            Some(Action::Type('y'))
        );
        assert_eq!(
            action(press(KeyCode::Esc), Mode::Dialog),
            Some(Action::Back)
        );
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

    /// A picker's letters filter, `q` and `j` included; it moves with the
    /// arrows, `Ctrl+P`/`Ctrl+N`, the page keys and `Home`/`End`.
    #[test]
    fn a_picker_types_letters_and_moves_with_keys() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for c in ['q', 'j', 'G', ' ', '/'] {
            assert_eq!(
                action(press(KeyCode::Char(c)), Mode::Picker),
                Some(Action::Type(c))
            );
        }
        assert_eq!(action(ctrl('p'), Mode::Picker), Some(Action::Up));
        assert_eq!(action(ctrl('n'), Mode::Picker), Some(Action::Down));
        assert_eq!(action(ctrl('u'), Mode::Picker), Some(Action::Clear));
        assert_eq!(action(ctrl('c'), Mode::Picker), Some(Action::Quit));
        assert_eq!(
            action(press(KeyCode::PageDown), Mode::Picker),
            Some(Action::PageDown)
        );
        assert_eq!(
            action(press(KeyCode::Home), Mode::Picker),
            Some(Action::First)
        );
        assert_eq!(
            action(press(KeyCode::End), Mode::Picker),
            Some(Action::Last)
        );
        assert_eq!(
            action(press(KeyCode::Backspace), Mode::Picker),
            Some(Action::Erase)
        );
        assert_eq!(
            action(press(KeyCode::Tab), Mode::Picker),
            Some(Action::Complete)
        );
        // Ctrl+P is nothing while browsing.
        assert_eq!(action(ctrl('p'), Mode::Browse), None);
        assert_eq!(
            action(press(KeyCode::Char('?')), Mode::Picker),
            Some(Action::Help)
        );
    }

    /// The page keys move lists, and mean nothing in a dialog or an input.
    #[test]
    fn page_keys_only_move_lists() {
        for mode in [Mode::Browse, Mode::Picker] {
            assert_eq!(
                action(press(KeyCode::PageUp), mode),
                Some(Action::PageUp),
                "{mode:?}"
            );
        }
        for mode in [Mode::Dialog, Mode::Text, Mode::Hotkey] {
            assert_eq!(action(press(KeyCode::PageUp), mode), None, "{mode:?}");
            assert_eq!(action(press(KeyCode::PageDown), mode), None, "{mode:?}");
        }
    }

    /// `H`/`L` and Shift with an arrow step by the big step.
    #[test]
    fn big_steps() {
        assert_eq!(
            action(press(KeyCode::Char('L')), Mode::Browse),
            Some(Action::BigRight)
        );
        assert_eq!(
            action(press(KeyCode::Char('H')), Mode::Browse),
            Some(Action::BigLeft)
        );
        let shifted = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        assert_eq!(
            action(shifted(KeyCode::Right), Mode::Browse),
            Some(Action::BigRight)
        );
        assert_eq!(
            action(shifted(KeyCode::Left), Mode::Browse),
            Some(Action::BigLeft)
        );
        // In an input, Shift+Left is only Left.
        assert_eq!(
            action(shifted(KeyCode::Left), Mode::Text),
            Some(Action::Left)
        );
        assert_eq!(
            action(press(KeyCode::Tab), Mode::Text),
            Some(Action::Complete)
        );
    }

    /// `t`, `s` and `Backspace` are the wizard's on the rows, and text in an
    /// input.
    #[test]
    fn test_skip_and_previous() {
        assert_eq!(
            action(press(KeyCode::Char('t')), Mode::Browse),
            Some(Action::Test)
        );
        assert_eq!(
            action(press(KeyCode::Char('s')), Mode::Browse),
            Some(Action::Skip)
        );
        assert_eq!(
            action(press(KeyCode::Backspace), Mode::Browse),
            Some(Action::Previous)
        );
        assert_eq!(
            action(press(KeyCode::Char('t')), Mode::Text),
            Some(Action::Type('t'))
        );
        assert_eq!(
            action(press(KeyCode::Backspace), Mode::Text),
            Some(Action::Erase)
        );
    }

    #[test]
    fn the_hotkey_dialog_keys() {
        let keys = [
            (KeyCode::Tab, Action::Down),
            (KeyCode::BackTab, Action::Up),
            (KeyCode::Char('j'), Action::Down),
            (KeyCode::Char('h'), Action::Left),
            (KeyCode::Char('l'), Action::Right),
            (KeyCode::Char(' '), Action::Toggle),
            (KeyCode::Char('x'), Action::Reset),
            (KeyCode::Char('t'), Action::Type('t')),
            (KeyCode::Enter, Action::Enter),
            (KeyCode::Esc, Action::Back),
            (KeyCode::Char('?'), Action::Help),
        ];
        for (code, want) in keys {
            assert_eq!(action(press(code), Mode::Hotkey), Some(want), "{code:?}");
        }
        // No quit or save by letter while it is up.
        assert_eq!(action(press(KeyCode::Char('q')), Mode::Hotkey), None);
        assert_eq!(action(press(KeyCode::Char('w')), Mode::Hotkey), None);
    }
}
