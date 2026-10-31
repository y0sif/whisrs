//! The hotkey dialog: the four modifiers as toggles and the key as a choice,
//! the result checked by `whisrs::hotkey::parse_hotkey` itself, and a
//! warning when another hotkey of the form is the same.
//!
//! It never captures a key press: compositor binds never reach the
//! terminal, and the daemon reads hotkeys from evdev, so a capture would
//! miss exactly the keys people bind.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use whisrs::hotkey::{hotkey_parts, KEY_NAMES, MODIFIER_NAMES};

use super::{dialog, wrap, Ui};
use crate::tui::keys::Action;

/// Where the dialog's focus is: a modifier, the key, or the result.
const KEY: usize = 4;
const RESULT: usize = 5;
const STOPS: usize = 6;

/// What a key in the dialog asks of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    None,
    /// Open the picker of key names.
    PickKey,
    /// Set the row to this hotkey (it parses).
    Save(String),
    /// It does not parse, and why.
    Invalid(String),
    /// Type it instead, starting from this text.
    Type(String),
    Unset,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct HotkeyDialog {
    pub title: String,
    mods: [bool; 4],
    /// Into [`KEY_NAMES`].
    key: Option<usize>,
    focus: usize,
    /// The value it opened on, kept as written when nothing changed.
    original: Option<String>,
    /// The other hotkey rows of the form: who, and their value.
    others: Vec<(String, String)>,
}

impl HotkeyDialog {
    /// The dialog for `value`; `None` when it is set but does not parse,
    /// which is typed instead.
    pub fn new(
        title: &str,
        value: Option<&str>,
        others: Vec<(String, String)>,
    ) -> Option<HotkeyDialog> {
        let value = value.map(str::trim).filter(|v| !v.is_empty());
        let (mods, key) = match value {
            None => ([false; 4], None),
            Some(value) => {
                let (names, key) = hotkey_parts(value)?;
                let mods = MODIFIER_NAMES.map(|m| names.contains(&m));
                (mods, KEY_NAMES.iter().position(|k| *k == key))
            }
        };
        Some(HotkeyDialog {
            title: title.to_string(),
            mods,
            key,
            focus: 0,
            original: value.map(str::to_string),
            others,
        })
    }

    /// The hotkey as built, `None` until it has a key.
    pub fn result(&self) -> Option<String> {
        let key = KEY_NAMES[self.key?];
        let mut parts: Vec<&str> = MODIFIER_NAMES
            .iter()
            .zip(self.mods)
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect();
        parts.push(key);
        Some(parts.join("+"))
    }

    /// The result if the parser takes it, else why not.
    fn checked(&self) -> Result<String, String> {
        let result = self.result().ok_or_else(|| "pick a key".to_string())?;
        whisrs::hotkey::parse_hotkey(&result)
            .map(|_| result)
            .map_err(|e| e.to_string())
    }

    /// Who else has this hotkey, if anyone.
    pub fn clash(&self) -> Option<&str> {
        let ours = hotkey_parts(&self.result()?)?;
        self.others
            .iter()
            .find(|(_, value)| hotkey_parts(value).as_ref() == Some(&ours))
            .map(|(who, _)| who.as_str())
    }

    /// Set the key to `name` (picked), and move to the result.
    pub fn set_key(&mut self, name: &str) {
        if let Some(at) = KEY_NAMES.iter().position(|k| *k == name) {
            self.key = Some(at);
            self.focus = RESULT;
        }
    }

    /// The key name now, for the picker's current mark.
    pub fn key_name(&self) -> Option<&'static str> {
        self.key.map(|at| KEY_NAMES[at])
    }

    pub fn key(&mut self, action: Action) -> Answer {
        match action {
            Action::Down => self.focus = (self.focus + 1) % STOPS,
            Action::Up => self.focus = (self.focus + STOPS - 1) % STOPS,
            Action::Toggle | Action::Left | Action::Right if self.focus < KEY => {
                self.mods[self.focus] = !self.mods[self.focus];
            }
            Action::Left | Action::Right if self.focus == KEY => {
                let len = KEY_NAMES.len();
                self.key = Some(match (self.key, action) {
                    (Some(at), Action::Right) => (at + 1) % len,
                    (Some(at), _) => (at + len - 1) % len,
                    (None, Action::Right) => 0,
                    (None, _) => len - 1,
                });
            }
            Action::Enter if self.focus == KEY => return Answer::PickKey,
            Action::Enter => return self.save(),
            Action::Type('t') => {
                let text = self.result().or_else(|| self.original.clone());
                return Answer::Type(text.unwrap_or_default());
            }
            Action::Reset => return Answer::Unset,
            Action::Back => return Answer::Cancel,
            _ => {}
        }
        Answer::None
    }

    /// The result, or the value as it was written when it means the same.
    fn save(&self) -> Answer {
        match self.checked() {
            Ok(result) => {
                let same = self
                    .original
                    .as_deref()
                    .is_some_and(|o| hotkey_parts(o) == hotkey_parts(&result));
                Answer::Save(match (&self.original, same) {
                    (Some(original), true) => original.clone(),
                    _ => result,
                })
            }
            Err(why) => Answer::Invalid(why),
        }
    }

    /// Draw it centred in `area`, over the dimmed screen.
    pub fn draw(&self, f: &mut Frame, area: Rect, ui: &Ui) {
        let theme = ui.theme;
        let label = |text: &str| Span::styled(format!("{text:<12}"), theme.muted);
        let gutter = |on: bool| {
            if on {
                Span::styled(format!("{} ", ui.glyphs.gutter), theme.accent)
            } else {
                Span::raw("  ")
            }
        };
        let focused = theme.accent.add_modifier(Modifier::BOLD);

        let mut mods = vec![gutter(self.focus < KEY), label("modifiers")];
        for (i, name) in MODIFIER_NAMES.iter().enumerate() {
            if i > 0 {
                mods.push(Span::raw(" "));
            }
            let mark = if self.mods[i] { "[x]" } else { "[ ]" };
            let style = if self.focus == i {
                focused
            } else {
                ratatui::style::Style::new()
            };
            mods.push(Span::styled(format!("{mark} {name}"), style));
        }

        let mut key = vec![gutter(self.focus == KEY), label("key")];
        let name = self.key_name().unwrap_or("none");
        if self.focus == KEY {
            key.push(Span::styled(format!("{} ", ui.glyphs.left), theme.accent));
            key.push(Span::styled(name, focused));
            key.push(Span::styled(format!(" {}", ui.glyphs.right), theme.accent));
        } else {
            key.push(Span::raw(name));
        }

        let checked = self.checked();
        let mut result = vec![gutter(self.focus == RESULT), label("result")];
        match (&checked, self.result()) {
            (Ok(text), _) => result.push(Span::styled(
                text.clone(),
                if self.focus == RESULT {
                    focused
                } else {
                    theme.accent
                },
            )),
            (Err(_), Some(text)) => result.push(Span::styled(text, theme.error)),
            (Err(_), None) => result.push(Span::styled("pick a key", theme.muted)),
        }

        let mut lines = vec![Line::from(mods), Line::from(key), Line::from(result)];
        let width = lines.iter().map(Line::width).max().unwrap_or(0).max(44);
        let say = |lines: &mut Vec<Line<'static>>, text: String, style| {
            for (i, part) in wrap(&text, width.saturating_sub(4)).into_iter().enumerate() {
                let lead = if i == 0 {
                    format!("{} ", ui.glyphs.warn)
                } else {
                    "  ".to_string()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{lead}{part}"), style),
                ]));
            }
        };
        match (&checked, self.clash()) {
            (Err(why), _) if self.key.is_some() => say(&mut lines, why.clone(), theme.error),
            (Ok(text), Some(who)) => {
                say(&mut lines, format!("{text} is already {who}"), theme.warn)
            }
            _ => {}
        }
        dialog::draw_titled(f, area, ui, &self.title, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(value: Option<&str>) -> HotkeyDialog {
        HotkeyDialog::new(
            "toggle hotkey",
            value,
            vec![("recording".into(), "Alt+Shift+M".into())],
        )
        .expect("parses")
    }

    #[test]
    fn modifiers_toggle_and_the_key_cycles() {
        let mut d = dialog(Some("Alt+D"));
        assert_eq!(d.result().as_deref(), Some("Alt+D"));
        // Super is the first stop.
        d.key(Action::Toggle);
        assert_eq!(d.result().as_deref(), Some("Super+Alt+D"));
        // h/l flip a modifier too.
        d.key(Action::Down);
        d.key(Action::Right);
        assert_eq!(d.result().as_deref(), Some("Super+Ctrl+Alt+D"));
        d.key(Action::Left);
        // On the key, h/l step through the names.
        d.key(Action::Down);
        d.key(Action::Down);
        d.key(Action::Down);
        d.key(Action::Right);
        assert_eq!(d.result().as_deref(), Some("Super+Alt+E"));
        assert_eq!(d.key(Action::Enter), Answer::PickKey);
        d.set_key("F9");
        assert_eq!(d.key(Action::Enter), Answer::Save("Super+Alt+F9".into()));
    }

    /// Only ScrollLock, Pause and F13 to F24 go alone: the parser says so.
    #[test]
    fn a_bare_key_is_refused_by_the_parser() {
        let mut d = dialog(None);
        d.set_key("D");
        match d.key(Action::Enter) {
            Answer::Invalid(why) => assert!(why.contains("ScrollLock"), "{why}"),
            other => panic!("saved {other:?}"),
        }
        d.set_key("F13");
        assert_eq!(d.key(Action::Enter), Answer::Save("F13".into()));
        // No key yet is no hotkey.
        assert_eq!(dialog(None).key(Action::Down), Answer::None);
        let mut empty = dialog(None);
        empty.focus = RESULT;
        assert_eq!(
            empty.key(Action::Enter),
            Answer::Invalid("pick a key".into())
        );
    }

    #[test]
    fn a_clash_with_another_row_is_named() {
        let mut d = dialog(Some("Alt+Shift+M"));
        assert_eq!(d.clash(), Some("recording"));
        // A clash is a warning, not a refusal: it still saves.
        d.focus = RESULT;
        assert_eq!(d.key(Action::Enter), Answer::Save("Alt+Shift+M".into()));
        assert_eq!(dialog(Some("shift+alt+m")).clash(), Some("recording"));
        assert_eq!(dialog(Some("Alt+M")).clash(), None);
    }

    /// Saved unchanged, a value keeps the way it was written.
    #[test]
    fn an_unchanged_value_keeps_its_spelling() {
        let mut d = dialog(Some("meta+shift+d"));
        d.focus = RESULT;
        assert_eq!(d.key(Action::Enter), Answer::Save("meta+shift+d".into()));
    }

    #[test]
    fn a_value_that_does_not_parse_is_typed_instead() {
        assert!(HotkeyDialog::new("t", Some("Super+Nope"), Vec::new()).is_none());
        let mut d = dialog(Some("Alt+D"));
        assert_eq!(d.key(Action::Type('t')), Answer::Type("Alt+D".into()));
        assert_eq!(d.key(Action::Reset), Answer::Unset);
        assert_eq!(d.key(Action::Back), Answer::Cancel);
    }
}
