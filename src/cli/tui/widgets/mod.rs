//! The pieces every screen draws with: the frame and footer, rows, the text
//! input, scrolling, and dialogs.

pub mod dialog;
pub mod frame;
pub mod input;
pub mod row;

use ratatui::style::Style;
use ratatui::text::Span;

use super::glyphs::Glyphs;
use super::theme::Theme;

/// What a frame is drawn with.
#[derive(Clone, Copy)]
pub struct Ui<'a> {
    pub theme: &'a Theme,
    pub glyphs: &'a Glyphs,
}

/// How a piece of text reads, by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Faint,
    Ok,
    Warn,
    Error,
}

impl Tone {
    pub fn style(self, theme: &Theme) -> Style {
        match self {
            Tone::Plain => Style::new(),
            Tone::Muted => theme.muted,
            Tone::Faint => theme.faint,
            Tone::Ok => theme.ok,
            Tone::Warn => theme.warn,
            Tone::Error => theme.error,
        }
    }
}

/// A line at the foot of a screen: what the load found, or what the last
/// action did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub tone: Tone,
    pub text: String,
}

impl Notice {
    pub fn new(tone: Tone, text: impl Into<String>) -> Notice {
        Notice {
            tone,
            text: text.into(),
        }
    }
}

/// The columns `text` takes on screen.
pub fn width(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` cut to `max` columns, ending in `ellipsis` when it was longer.
pub fn fit(text: &str, max: usize, ellipsis: &str) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let mut buf = [0u8; 4];
        let w = width(c.encode_utf8(&mut buf));
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push_str(ellipsis);
    }
    out
}

/// `text` wrapped at word boundaries to `max` columns; a word longer than
/// a line is cut.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let fits = if line.is_empty() {
            width(word) <= max
        } else {
            width(&line) + 1 + width(word) <= max
        };
        if !fits && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while width(&line) > max && max > 0 {
            let head: String = line.chars().take(max).collect();
            let rest: String = line.chars().skip(max).collect();
            lines.push(head);
            line = rest;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The first row to show of `len` rows, `height` at a time, with `cursor`
/// in view.
pub fn scroll(len: usize, cursor: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        0
    } else {
        cursor.saturating_sub(height - 1).min(len - height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_with_an_ellipsis() {
        assert_eq!(fit("deepgram", 8, "~"), "deepgram");
        assert_eq!(fit("deepgram-streaming", 8, "~"), "deepgra~");
        assert_eq!(fit("deepgram", 0, "~"), "");
    }

    #[test]
    fn wrap_breaks_at_words() {
        assert_eq!(
            wrap("API key for the deepgram backends.", 16),
            vec!["API key for the", "deepgram", "backends."]
        );
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert!(wrap("", 10).is_empty());
    }

    #[test]
    fn scroll_keeps_the_cursor_in_view() {
        assert_eq!(scroll(5, 4, 10), 0);
        assert_eq!(scroll(20, 0, 5), 0);
        assert_eq!(scroll(20, 4, 5), 0);
        assert_eq!(scroll(20, 5, 5), 1);
        assert_eq!(scroll(20, 19, 5), 15);
        assert_eq!(scroll(20, 3, 0), 0);
    }
}
