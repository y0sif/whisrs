//! One row of a list: an accent gutter on the focused row, a label at a
//! fixed indent, and a value. Only the focused row is bold, and only it
//! shows a choice's arrows, so nothing shifts as the focus moves.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::input::Input;
use super::{fit, width, Tone, Ui};

/// A value, in parts that each read in their own tone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Value {
    pub parts: Vec<(String, Tone)>,
}

impl Value {
    pub fn plain(text: impl Into<String>) -> Value {
        Value::of(text, Tone::Plain)
    }

    pub fn muted(text: impl Into<String>) -> Value {
        Value::of(text, Tone::Muted)
    }

    pub fn of(text: impl Into<String>, tone: Tone) -> Value {
        Value {
            parts: vec![(text.into(), tone)],
        }
    }

    /// Add a part after a space.
    pub fn then(mut self, text: impl Into<String>, tone: Tone) -> Value {
        self.parts.push((format!(" {}", text.into()), tone));
        self
    }

    /// Join `parts` with the separator glyph between them.
    pub fn joined(parts: Vec<(String, Tone)>, separator: &str) -> Value {
        let mut value = Value::default();
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                value.parts.push((format!(" {separator} "), Tone::Faint));
            }
            value.parts.push(part);
        }
        value
    }
}

/// One row to draw.
pub struct Row<'a> {
    pub label: &'a str,
    pub value: Value,
    pub focused: bool,
    /// The row picked in a list without the focus: plain rather than muted.
    pub selected: bool,
    /// A choice or a number: arrows either side of the value while focused.
    pub choice: bool,
    /// The value is at its lower or upper limit: that side's arrow dims.
    pub at_min: bool,
    pub at_max: bool,
    /// The value just changed: shown reversed for a moment.
    pub flash: bool,
    /// Being typed into.
    pub input: Option<&'a Input>,
    /// Shows the gutter column at all (the rows of a list do; info rows do
    /// not, but keep its indent).
    pub gutter: bool,
    /// A word at the row's right end, in its tone: whose value it is
    /// (`profile`, `base`).
    pub tag: Option<(&'a str, Tone)>,
}

impl Row<'_> {
    pub fn new(label: &str, value: Value) -> Row<'_> {
        Row {
            label,
            value,
            focused: false,
            selected: false,
            choice: false,
            at_min: false,
            at_max: false,
            flash: false,
            input: None,
            gutter: true,
            tag: None,
        }
    }

    /// Draw into the one-line `area`, the value at `label_width` columns
    /// after the label starts.
    pub fn draw(self, area: Rect, buf: &mut Buffer, ui: &Ui, label_width: usize) {
        let theme = ui.theme;
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut spans = Vec::new();
        if self.focused && self.gutter {
            spans.push(Span::styled(ui.glyphs.gutter, theme.accent));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(" "));

        let room = usize::from(area.width).saturating_sub(2);
        let label_room = label_width.min(room);
        let label = fit(self.label, label_room.saturating_sub(1), ui.glyphs.ellipsis);
        let label_style = if self.focused {
            bold
        } else if self.selected {
            Style::new()
        } else {
            theme.muted
        };
        let pad = label_room.saturating_sub(width(&label));
        spans.push(Span::styled(label, label_style));
        spans.push(Span::raw(" ".repeat(pad)));

        let mut room = room.saturating_sub(label_room);
        // The tag keeps its column, a space before it, ahead of the value.
        let tag = self
            .tag
            .filter(|(text, _)| !text.is_empty() && width(text) + 1 < room);
        if let Some((text, _)) = tag {
            room -= width(text) + 1;
        }
        if let Some(input) = self.input {
            spans.extend(input.spans(ui, room));
        } else {
            let arrows = self.focused && self.choice;
            let arrow = |end: bool| if end { theme.faint } else { theme.accent };
            if arrows {
                room = room.saturating_sub(4);
                spans.push(Span::styled(
                    format!("{} ", ui.glyphs.left),
                    arrow(self.at_min),
                ));
            }
            for (text, tone) in self.value.parts {
                if room == 0 {
                    break;
                }
                let text = fit(&text, room, ui.glyphs.ellipsis);
                room = room.saturating_sub(width(&text));
                let mut style = match (self.focused, tone) {
                    (true, Tone::Plain) => theme.accent.add_modifier(Modifier::BOLD),
                    (true, tone) => tone.style(theme).add_modifier(Modifier::BOLD),
                    (false, tone) => tone.style(theme),
                };
                if self.flash {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                spans.push(Span::styled(text, style));
            }
            if arrows {
                spans.push(Span::styled(
                    format!(" {}", ui.glyphs.right),
                    arrow(self.at_max),
                ));
            }
        }
        if let Some((text, tone)) = tag {
            let used: usize = spans.iter().map(Span::width).sum();
            let end = usize::from(area.width).saturating_sub(width(text));
            spans.push(Span::raw(" ".repeat(end.saturating_sub(used).max(1))));
            spans.push(Span::styled(text.to_string(), tone.style(theme)));
        }
        Line::from(spans).render(area, buf);
    }
}
