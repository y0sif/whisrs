//! The screen's frame: one rounded box in the faint role, a header line
//! inside it, and a line of key hints under it.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::{fit, width, wrap, Notice, Tone, Ui};

/// Below this the TUI shows one line saying so.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 18;

/// From this terminal width a screen shows two panes.
pub const TWO_PANES: u16 = 90;

/// The widest the frame gets, centred.
const MAX_WIDTH: u16 = 100;

/// Columns left free on each side of the frame.
const MARGIN: u16 = 3;

/// The width of the body inside the frame `place` puts in an `area` this
/// wide: the border and the padding (one column left, two right) off.
pub fn body_width(area_width: u16) -> u16 {
    (area_width.saturating_sub(2 * MARGIN))
        .min(MAX_WIDTH)
        .saturating_sub(5)
}

/// The most rows a body gets in `area`: what [`place`] leaves inside the
/// border, under the header, above the footer.
pub fn max_body(area: Rect) -> u16 {
    area.height.saturating_sub(5)
}

/// Where a screen goes.
pub struct Placed {
    pub frame: Rect,
    pub footer: Rect,
}

/// The frame for a body `body` rows tall (`None` for as tall as fits) in
/// `area`, centred, with the footer line under it. `None` when `area` is
/// below [`MIN_WIDTH`] by [`MIN_HEIGHT`].
pub fn place(area: Rect, body: Option<u16>) -> Option<Placed> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let width = (area.width - 2 * MARGIN).min(MAX_WIDTH);
    // Border, header and the blank line under it.
    let fits = area.height - 1;
    let height = body.map_or(fits, |body| (body + 4).min(fits));
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - (height + 1)) / 2;
    Some(Placed {
        frame: Rect::new(x, y, width, height),
        footer: Rect::new(x, y + height, width, 1),
    })
}

/// The one line shown on a terminal too small for the TUI.
pub fn too_small(f: &mut Frame, ui: &Ui) {
    let area = f.area();
    let text = format!(
        "whisrs needs {MIN_WIDTH}x{MIN_HEIGHT}, this terminal is {}x{}",
        area.width, area.height
    );
    let line = Line::from(Span::styled(
        fit(&text, usize::from(area.width), ui.glyphs.ellipsis),
        ui.theme.muted,
    ));
    let y = area.y + area.height / 2;
    let w = (line.width() as u16).min(area.width);
    let x = area.x + (area.width - w) / 2;
    f.render_widget(Paragraph::new(line), Rect::new(x, y, w, 1.min(area.height)));
}

/// What the header names: the screen, and whether the daemon is up
/// (`None` while not known yet).
pub struct Header<'a> {
    pub screen: &'a str,
    pub daemon: Option<bool>,
    /// The microphone's level, 0 to 1, while it is live: the waveform
    /// follows it.
    pub live: Option<f32>,
}

/// The header's five waveform cells: still, or after a live `level`.
fn wave(ui: &Ui, live: Option<f32>) -> Vec<Span<'static>> {
    let [low, mid, high] = ui.glyphs.wave;
    let Some(level) = live else {
        return vec![Span::styled(
            [low, mid, high, mid, low].concat(),
            ui.theme.muted,
        )];
    };
    [0.35, 0.7, 1.0, 0.7, 0.35]
        .into_iter()
        .map(|weight: f32| {
            let height = (level.clamp(0.0, 1.0) * weight * 3.0) as usize;
            Span::styled([low, mid, high][height.min(2)], ui.theme.accent)
        })
        .collect()
}

/// Draw the frame and header; the area for the body under them.
pub fn draw(f: &mut Frame, rect: Rect, ui: &Ui, header: Header) -> Rect {
    let block = Block::bordered()
        .border_set(ui.glyphs.frame)
        .border_style(ui.theme.faint);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    // One column in on the left, two on the right.
    let content = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(3),
        inner.height,
    );

    let mut title = vec![Span::raw("whisrs ")];
    title.extend(wave(ui, header.live));
    let title = Line::from(title);
    let (dot, dot_tone) = match header.daemon {
        Some(true) => (ui.glyphs.up, Tone::Ok),
        Some(false) => (ui.glyphs.down, Tone::Muted),
        None => (ui.glyphs.down, Tone::Faint),
    };
    let status = Line::from(vec![
        Span::styled(header.screen, ui.theme.muted),
        Span::styled(format!(" {} ", ui.glyphs.separator), ui.theme.faint),
        Span::styled(dot, dot_tone.style(ui.theme)),
    ]);
    let row = Rect::new(content.x, content.y, content.width, 1);
    f.render_widget(Paragraph::new(title), row);
    let w = (status.width() as u16).min(content.width);
    f.render_widget(
        Paragraph::new(status),
        Rect::new(content.x + content.width - w, content.y, w, 1),
    );

    Rect::new(
        content.x,
        content.y + 2,
        content.width,
        content.height.saturating_sub(2),
    )
}

/// The key hints under the frame: each key, then what it does, muted. The
/// hints that do not fit are left out, all but the last, which is the way
/// out or to the rest (`? help`, `q quit`).
pub fn footer(f: &mut Frame, rect: Rect, ui: &Ui, hints: &[(&str, &str)]) {
    let sep = format!(" {} ", ui.glyphs.separator);
    let cost = |(key, what): &(&str, &str)| width(key) + 1 + width(what);
    let room = usize::from(rect.width.saturating_sub(2));
    let mut shown: Vec<&(&str, &str)> = hints.iter().collect();
    let total = |shown: &[&(&str, &str)]| {
        shown.iter().map(|h| cost(h)).sum::<usize>() + width(&sep) * shown.len().saturating_sub(1)
    };
    while shown.len() > 1 && total(&shown) > room {
        shown.remove(shown.len() - 2);
    }
    let mut spans = vec![Span::raw("  ")];
    for (i, (key, what)) in shown.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep.clone(), ui.theme.faint));
        }
        spans.push(Span::raw(*key));
        spans.push(Span::styled(format!(" {what}"), ui.theme.muted));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// The most notices at the foot of a screen; past it, one line counts the
/// rest.
const MAX_NOTICES: usize = 4;

/// The most lines a notice or the status wraps to.
const MAX_STATUS: usize = 3;

/// The lines at the foot of a body, `max` columns wide: each notice, then
/// the status, wrapped (a warning's later lines under its text, past the
/// `!`). A warning leads with `!`, and an error says `error:` when there is
/// no colour to say it.
pub fn messages(
    notices: &[Notice],
    status: Option<&Notice>,
    ui: &Ui,
    max: usize,
) -> Vec<Line<'static>> {
    let text = |notice: &Notice| match notice.tone {
        Tone::Warn => format!("{} {}", ui.glyphs.warn, notice.text),
        Tone::Error if !ui.theme.color => format!("error: {}", notice.text),
        _ => notice.text.clone(),
    };
    let hang = |notice: &Notice| match notice.tone {
        Tone::Warn => width(ui.glyphs.warn) + 1,
        _ => 0,
    };
    let line = |text: String, tone: Tone| Line::from(Span::styled(text, tone.style(ui.theme)));
    let mut lines = Vec::new();
    let shown = if notices.len() > MAX_NOTICES {
        MAX_NOTICES - 1
    } else {
        notices.len()
    };
    for notice in &notices[..shown] {
        let wrapped = wrap_at_most(&text(notice), max, hang(notice), ui.glyphs.ellipsis);
        lines.extend(wrapped.into_iter().map(|text| line(text, notice.tone)));
    }
    if shown < notices.len() {
        let more = format!(
            "{} {} more: e opens config.toml",
            ui.glyphs.warn,
            notices.len() - shown
        );
        lines.push(line(fit(&more, max, ui.glyphs.ellipsis), Tone::Warn));
    }
    if let Some(status) = status {
        let wrapped = wrap_at_most(&text(status), max, hang(status), ui.glyphs.ellipsis);
        lines.extend(wrapped.into_iter().map(|text| line(text, status.tone)));
    }
    lines
}

/// `text` wrapped to `max` columns, its later lines indented by `hang`, in
/// at most [`MAX_STATUS`] lines, the last one cut with `ellipsis` when more
/// was left.
fn wrap_at_most(text: &str, max: usize, hang: usize, ellipsis: &str) -> Vec<String> {
    let mut lines = wrap_hanging(text, max, hang);
    if lines.len() > MAX_STATUS {
        lines.truncate(MAX_STATUS);
        let last = lines.pop().unwrap_or_default();
        lines.push(fit(&format!("{last} {ellipsis}"), max, ellipsis));
    }
    lines
}

/// [`wrap`], with every line after the first indented by `hang` columns.
fn wrap_hanging(text: &str, max: usize, hang: usize) -> Vec<String> {
    let plain = wrap(text, max);
    if plain.len() <= 1 || hang == 0 || max <= hang {
        return plain;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let head = &plain[0];
    let taken = head.split_whitespace().count();
    // A first word cut to fit is no whole word to carry on after.
    if words[..taken.min(words.len())].join(" ") != *head {
        return plain;
    }
    let indent = " ".repeat(hang);
    let mut lines = vec![head.clone()];
    lines.extend(
        wrap(&words[taken..].join(" "), max - hang)
            .into_iter()
            .map(|line| format!("{indent}{line}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_is_centred_and_at_most_100_wide() {
        let at = |w, h| place(Rect::new(0, 0, w, h), None).map(|p| p.frame);
        assert_eq!(at(59, 17), None);
        assert_eq!(at(60, 17), None);
        assert_eq!(at(59, 18), None);
        assert_eq!(at(60, 18), Some(Rect::new(3, 0, 54, 17)));
        assert_eq!(at(90, 30), Some(Rect::new(3, 0, 84, 29)));
        assert_eq!(at(200, 60), Some(Rect::new(50, 0, 100, 59)));
        // As tall as fits, the body has `max_body` rows.
        let tall = place(Rect::new(0, 0, 60, 18), None).unwrap();
        assert_eq!(tall.frame.height - 4, max_body(Rect::new(0, 0, 60, 18)));
        // A short body is centred vertically, with the footer under it.
        let placed = place(Rect::new(0, 0, 90, 30), Some(8)).unwrap();
        assert_eq!(placed.frame, Rect::new(3, 8, 84, 12));
        assert_eq!(placed.footer, Rect::new(3, 20, 84, 1));
    }

    /// A long warning wraps instead of being cut, its later lines under its
    /// text; past three lines it ends in an ellipsis.
    #[test]
    fn messages_wrap() {
        let theme = crate::tui::testing::theme();
        let glyphs = crate::tui::glyphs::Glyphs::UNICODE;
        let ui = Ui {
            theme: &theme,
            glyphs: &glyphs,
        };
        let warning = Notice::new(
            Tone::Warn,
            "[general] prompt is ignored with backend = \"deepgram-streaming\": the \
             Deepgram API takes no prompt",
        );
        let lines: Vec<String> = messages(&[warning], None, &ui, 40)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            lines,
            [
                "! [general] prompt is ignored with",
                "  backend = \"deepgram-streaming\": the",
                "  Deepgram API takes no prompt",
            ]
        );
        let long = Notice::new(Tone::Muted, "word ".repeat(40));
        let lines = messages(&[], Some(&long), &ui, 20);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].to_string().ends_with('\u{2026}'));
    }
}
