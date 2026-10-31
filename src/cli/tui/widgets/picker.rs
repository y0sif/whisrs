//! The picker: a dialog over the detail pane with a filter line and the
//! values a row offers, each with what it is. Letters filter, the arrows
//! move, `Enter` picks, `Esc` leaves with nothing changed.
//!
//! The filter keeps a value when every word of it starts a word of the
//! value or of its description, words split at spaces and `- _ . / :`: `ger`
//! finds `de German`, `turbo` finds `whisper-large-v3-turbo`. An exact value
//! comes first, then values it starts, then the list's own order.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::input::Input;
use super::{dialog, fit, scroll, width, Tone, Ui};
use crate::tui::keys::Action;
use crate::tui::options::{Loading, Options};

/// How far `PgUp` and `PgDn` move.
const PAGE: usize = 8;

/// The most rows a picker shows at once.
const MAX_ROWS: usize = 12;

/// The widest the value column gets.
const MAX_LABEL: usize = 34;

/// A row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Unset,
    /// Pick `index` of the options.
    Pick(usize),
    /// The filter text itself, for a row that takes any value.
    Use(String),
}

/// What was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Unset,
    /// A listed value.
    Value(String),
    /// A typed one, for the field's check.
    Typed(String),
}

/// What a key in the picker asks of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    None,
    Picked(Picked),
    /// That value cannot be picked here, and why.
    Refused(String),
    Closed,
    /// `Tab`: complete the filter as a path.
    Complete,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    pub options: Options,
    filter: Input,
    cursor: usize,
    /// The value set now (`None`: unset): marked, and the cursor starts on
    /// it.
    current: Option<String>,
    /// `Tab` completes the filter as a path.
    pub paths: bool,
}

impl Picker {
    pub fn new(title: &str, options: Options, current: Option<&str>, paths: bool) -> Picker {
        let mut picker = Picker {
            title: title.to_string(),
            options,
            filter: Input::new("", false),
            cursor: 0,
            current: current.map(str::to_string),
            paths,
        };
        picker.cursor = picker.current_row().unwrap_or(0);
        picker
    }

    pub fn filter(&self) -> String {
        self.filter.text()
    }

    /// Replace the filter (a completed path).
    pub fn set_filter(&mut self, text: &str) {
        self.filter = Input::new(text, false);
        self.cursor = 0;
    }

    /// New options (a worker's list came), keeping the filter; the cursor
    /// goes to the current value again while nothing is typed.
    pub fn set_options(&mut self, options: Options) {
        self.options = options;
        if self.filter.text().trim().is_empty() {
            self.cursor = self.current_row().unwrap_or(0);
        } else {
            self.cursor = self.cursor.min(self.rows().len().saturating_sub(1));
        }
    }

    /// Where the current value is among the rows.
    fn current_row(&self) -> Option<usize> {
        self.rows().iter().position(|row| match row {
            Row::Unset => self.current.is_none(),
            Row::Pick(i) => self.current.as_deref() == Some(self.options.picks[*i].value.as_str()),
            Row::Use(_) => false,
        })
    }

    /// The rows the filter keeps, in order.
    pub fn rows(&self) -> Vec<Row> {
        let filter = self.filter.text();
        let filter = filter.trim();
        let mut rows = Vec::new();
        if let Some(unset) = &self.options.unset {
            if filter.is_empty() || matches(filter, &["unset", unset]) {
                rows.push(Row::Unset);
            }
        }
        let lower = filter.to_lowercase();
        let mut picks: Vec<(u8, usize)> = self
            .options
            .picks
            .iter()
            .enumerate()
            .filter(|(_, p)| filter.is_empty() || matches(filter, &[&p.value, &p.label, &p.about]))
            .map(|(i, p)| {
                let (value, label) = (p.value.to_lowercase(), p.label.to_lowercase());
                let rank = if filter.is_empty() {
                    2
                } else if value == lower || label == lower {
                    0
                } else if value.starts_with(&lower) || label.starts_with(&lower) {
                    1
                } else {
                    2
                };
                (rank, i)
            })
            .collect();
        // Stable: within a rank, the list's own order.
        picks.sort_by_key(|(rank, _)| *rank);
        rows.extend(picks.into_iter().map(|(_, i)| Row::Pick(i)));
        let listed = self
            .options
            .picks
            .iter()
            .any(|p| p.value.eq_ignore_ascii_case(filter));
        if self.options.open && !filter.is_empty() && !listed {
            rows.push(Row::Use(filter.to_string()));
        }
        rows
    }

    pub fn key(&mut self, action: Action) -> Answer {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        match action {
            Action::Back => return Answer::Closed,
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(last),
            Action::PageUp => self.cursor = self.cursor.saturating_sub(PAGE),
            Action::PageDown => self.cursor = (self.cursor + PAGE).min(last),
            Action::First => self.cursor = 0,
            Action::Last => self.cursor = last,
            Action::Complete if self.paths => return Answer::Complete,
            Action::Enter => return self.pick(rows.get(self.cursor)),
            other => {
                let before = self.filter.text();
                if self.filter.apply(other) && self.filter.text() != before {
                    self.cursor = 0;
                }
            }
        }
        Answer::None
    }

    fn pick(&self, row: Option<&Row>) -> Answer {
        match row {
            Some(Row::Unset) => Answer::Picked(Picked::Unset),
            Some(Row::Pick(i)) => {
                let pick = &self.options.picks[*i];
                match &pick.refused {
                    Some((_, why)) => Answer::Refused(why.clone()),
                    None => Answer::Picked(Picked::Value(pick.value.clone())),
                }
            }
            Some(Row::Use(text)) => Answer::Picked(Picked::Typed(text.clone())),
            None => Answer::None,
        }
    }

    /// Draw over `area` (the detail pane): the box at its top, as tall as
    /// the rows need.
    pub fn draw(&self, f: &mut Frame, area: Rect, ui: &Ui) {
        let rows = self.rows();
        let status = match &self.options.loading {
            Loading::Done if rows.is_empty() => Some((
                if self.options.open {
                    "nothing listed: type a value"
                } else {
                    "nothing matches"
                }
                .to_string(),
                Tone::Muted,
            )),
            Loading::Done => None,
            Loading::Looking => Some(("looking...".to_string(), Tone::Muted)),
            Loading::Failed(why) => Some((format!("{} {why}", ui.glyphs.warn), Tone::Warn)),
        };
        let wanted = rows.len().min(MAX_ROWS) + usize::from(status.is_some());
        // Border, filter line.
        let height = (wanted as u16 + 3).min(area.height);
        let rect = Rect::new(area.x, area.y, area.width, height);
        let inner = dialog::boxed(f, rect, ui, &self.title);
        if inner.height == 0 {
            return;
        }

        let mut spans = vec![Span::styled(
            format!("{} ", ui.glyphs.prompt),
            ui.theme.accent,
        )];
        spans.extend(
            self.filter
                .spans(ui, usize::from(inner.width.saturating_sub(2))),
        );
        f.render_widget(Line::from(spans), Rect { height: 1, ..inner });

        let list_height = usize::from(inner.height.saturating_sub(1))
            .saturating_sub(usize::from(status.is_some()));
        let offset = scroll(rows.len(), self.cursor, list_height);
        let room = usize::from(inner.width);
        // The typed value's row is not lined up with the others.
        let label_width = rows
            .iter()
            .filter(|row| !matches!(row, Row::Use(_)))
            .map(|row| width(&self.label(row)))
            .max()
            .unwrap_or(0)
            .min(MAX_LABEL)
            .min(room / 2);
        let mut y = inner.y + 1;
        for (i, row) in rows.iter().enumerate().skip(offset).take(list_height) {
            let line = self.line(row, i == self.cursor, label_width, room, ui);
            f.render_widget(line, Rect::new(inner.x, y, inner.width, 1));
            y += 1;
        }
        if let Some((text, tone)) = status {
            if y < inner.bottom() {
                let text = fit(&text, room.saturating_sub(2), ui.glyphs.ellipsis);
                f.render_widget(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(text, tone.style(ui.theme)),
                    ]),
                    Rect::new(inner.x, y, inner.width, 1),
                );
            }
        }
    }

    fn label(&self, row: &Row) -> String {
        match row {
            Row::Unset => "unset".to_string(),
            Row::Pick(i) => self.options.picks[*i].label.clone(),
            Row::Use(text) => format!("use \"{text}\""),
        }
    }

    /// One row: the gutter on the cursor, the value, what it is (dim), and
    /// the mark on the current value at the right edge.
    fn line(
        &self,
        row: &Row,
        focused: bool,
        label_width: usize,
        room: usize,
        ui: &Ui,
    ) -> Line<'static> {
        let theme = ui.theme;
        let (about, note, refused, current) = match row {
            Row::Unset => (
                self.options.unset.clone().unwrap_or_default(),
                None,
                None,
                self.current.is_none(),
            ),
            Row::Pick(i) => {
                let pick = &self.options.picks[*i];
                (
                    pick.about.clone(),
                    pick.note.clone(),
                    pick.refused.as_ref().map(|(short, _)| short.clone()),
                    self.current.as_deref() == Some(pick.value.as_str()),
                )
            }
            Row::Use(_) => (String::new(), None, None, false),
        };
        let mut spans = vec![
            if focused {
                Span::styled(ui.glyphs.gutter, theme.accent)
            } else {
                Span::raw(" ")
            },
            Span::raw(" "),
        ];
        // The gutter, its space, and the mark with its space.
        let room = room.saturating_sub(4);
        // The typed value takes the whole row; the rest line up.
        let label_width = match row {
            Row::Use(_) => room,
            _ => label_width.min(room),
        };
        let label = fit(&self.label(row), label_width, ui.glyphs.ellipsis);
        let label_style = match (focused, refused.is_some()) {
            (_, true) => theme.faint,
            (true, false) => theme.accent.add_modifier(Modifier::BOLD),
            (false, false) => ratatui::style::Style::new(),
        };
        let pad = match row {
            Row::Use(_) => 0,
            _ => label_width.saturating_sub(width(&label)),
        };
        let used = width(&label) + pad;
        spans.push(Span::styled(label, label_style));
        spans.push(Span::raw(" ".repeat(pad)));
        let mut rest = room.saturating_sub(used);
        let mut push = |spans: &mut Vec<Span<'static>>, text: String, tone: Tone| {
            if rest < 3 || text.is_empty() {
                return;
            }
            let text = fit(&format!("  {text}"), rest, ui.glyphs.ellipsis);
            rest = rest.saturating_sub(width(&text));
            spans.push(Span::styled(text, tone.style(theme)));
        };
        match refused {
            // Why first: it is what the row is about now.
            Some(short) => {
                let text = if about.is_empty() {
                    short
                } else {
                    format!("{short} {} {about}", ui.glyphs.separator)
                };
                push(&mut spans, text, Tone::Faint);
            }
            None => {
                push(&mut spans, about, Tone::Muted);
                if let Some(note) = note {
                    push(&mut spans, note, Tone::Warn);
                }
            }
        }
        if current {
            spans.push(Span::raw(" ".repeat(rest + 1)));
            spans.push(Span::styled(ui.glyphs.current, theme.accent));
        }
        Line::from(spans)
    }
}

/// Whether every word of `filter` starts a word of one of `texts`, any
/// case.
pub fn matches(filter: &str, texts: &[&str]) -> bool {
    let words = |text: &str| -> Vec<String> {
        text.split([' ', '-', '_', '.', '/', ':'])
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let haystack: Vec<String> = texts.iter().flat_map(|t| words(t)).collect();
    words(filter)
        .iter()
        .all(|word| haystack.iter().any(|w| w.starts_with(word.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::options::Pick;

    fn options(list: &[(&str, &str)], unset: Option<&str>, open: bool) -> Options {
        Options {
            picks: list
                .iter()
                .map(|(v, a)| Pick {
                    value: v.to_string(),
                    label: v.to_string(),
                    about: a.to_string(),
                    note: None,
                    refused: None,
                })
                .collect(),
            unset: unset.map(str::to_string),
            open,
            loading: Loading::Done,
        }
    }

    fn type_in(picker: &mut Picker, text: &str) {
        for c in text.chars() {
            picker.key(Action::Type(c));
        }
    }

    fn shown(picker: &Picker) -> Vec<String> {
        picker.rows().iter().map(|row| picker.label(row)).collect()
    }

    #[test]
    fn words_match_by_prefix() {
        assert!(matches("ger", &["de", "German"]));
        assert!(matches("turbo", &["whisper-large-v3-turbo", ""]));
        assert!(matches("large tur", &["whisper-large-v3-turbo"]));
        assert!(matches("whisper-large", &["whisper-large-v3"]));
        assert!(matches("GER", &["de", "german"]));
        assert!(!matches("erman", &["de", "German"]));
        assert!(!matches("ger fr", &["de", "German"]));
        assert!(matches("bin", &["/m/ggml-base.en.bin"]));
    }

    /// An exact value first, then values the filter starts, then the
    /// list's order.
    #[test]
    fn exact_then_prefix_then_list_order() {
        let list = [
            ("ta", "Tamil"),
            ("de", "the German language"),
            ("en", "English"),
            ("te", "Telugu"),
            ("t", "a test"),
        ];
        let mut picker = Picker::new("language", options(&list, None, true), None, false);
        type_in(&mut picker, "t");
        assert_eq!(shown(&picker), ["t", "ta", "te", "de"]);
    }

    /// A typed value nothing lists is its own row, for an open set only.
    #[test]
    fn a_typed_value_is_offered_when_the_set_is_open() {
        let list = [("de", "German"), ("en", "English")];
        let mut open = Picker::new("language", options(&list, None, true), None, false);
        type_in(&mut open, "pt-BR");
        assert_eq!(shown(&open), ["use \"pt-BR\""]);
        assert_eq!(
            open.key(Action::Enter),
            Answer::Picked(Picked::Typed("pt-BR".into()))
        );
        // A listed value is not offered twice.
        let mut open = Picker::new("language", options(&list, None, true), None, false);
        type_in(&mut open, "DE");
        assert_eq!(shown(&open), ["de"]);

        let mut closed = Picker::new("theme", options(&list, None, false), None, false);
        type_in(&mut closed, "zz");
        assert!(closed.rows().is_empty());
        assert_eq!(closed.key(Action::Enter), Answer::None);
    }

    #[test]
    fn the_cursor_starts_on_the_current_value_and_unset_comes_first() {
        let list = [("de", "German"), ("en", "English"), ("fr", "French")];
        let picker = Picker::new(
            "language",
            options(&list, Some("[general] language (en)"), true),
            Some("fr"),
            false,
        );
        assert_eq!(shown(&picker), ["unset", "de", "en", "fr"]);
        assert_eq!(picker.cursor, 3);
        let mut unset = Picker::new(
            "language",
            options(&list, Some("[general] language (en)"), true),
            None,
            false,
        );
        assert_eq!(unset.cursor, 0);
        assert_eq!(unset.key(Action::Enter), Answer::Picked(Picked::Unset));
        // The arrows stop at the ends; Esc leaves.
        unset.key(Action::Up);
        assert_eq!(unset.cursor, 0);
        unset.key(Action::Last);
        unset.key(Action::Down);
        assert_eq!(unset.cursor, 3);
        assert_eq!(unset.key(Action::PageUp), Answer::None);
        assert_eq!(unset.cursor, 0);
        assert_eq!(unset.key(Action::Back), Answer::Closed);
    }

    #[test]
    fn a_refused_value_says_why_instead_of_being_picked() {
        let mut list = options(&[("groq", ""), ("local-vosk", "Vosk")], None, false);
        list.picks[1].refused = Some(("coming soon".into(), "not implemented yet".into()));
        let mut picker = Picker::new("backend", list, Some("groq"), false);
        picker.key(Action::Down);
        assert_eq!(
            picker.key(Action::Enter),
            Answer::Refused("not implemented yet".into())
        );
    }

    #[test]
    fn typing_resets_the_cursor_and_ctrl_u_clears() {
        let list = [("de", "German"), ("en", "English")];
        let mut picker = Picker::new("language", options(&list, None, true), Some("en"), false);
        assert_eq!(picker.cursor, 1);
        type_in(&mut picker, "e");
        assert_eq!(picker.cursor, 0);
        assert_eq!(shown(&picker), ["en", "use \"e\""]);
        picker.key(Action::Clear);
        assert_eq!(picker.filter(), "");
        assert_eq!(shown(&picker), ["de", "en"]);
    }
}
