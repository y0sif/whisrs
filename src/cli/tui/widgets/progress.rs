//! A download's progress: a bar, the percent, and how much of how much.

use ratatui::text::{Line, Span};

use super::Ui;
use crate::tui::options::size;

/// How many cells the bar takes.
const CELLS: usize = 20;

/// `done` bytes of `total` (when known).
pub fn line(done: u64, total: Option<u64>, ui: &Ui) -> Line<'static> {
    let Some(total) = total.filter(|t| *t > 0) else {
        return Line::from(Span::styled(size(done), ui.theme.accent));
    };
    let share = (done as f64 / total as f64).clamp(0.0, 1.0);
    let on = (share * CELLS as f64).round() as usize;
    Line::from(vec![
        Span::styled(ui.glyphs.bar_on.repeat(on), ui.theme.accent),
        Span::styled(ui.glyphs.bar_off.repeat(CELLS - on), ui.theme.faint),
        Span::raw(format!(" {:.0}%", share * 100.0)),
        Span::styled(
            format!(" {} {} of {}", ui.glyphs.separator, size(done), size(total)),
            ui.theme.muted,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_reads_as_a_bar_and_sizes() {
        let theme = crate::tui::testing::theme();
        let glyphs = crate::tui::glyphs::Glyphs::ASCII;
        let ui = Ui {
            theme: &theme,
            glyphs: &glyphs,
        };
        assert_eq!(
            line(74_000_000, Some(148_000_000), &ui).to_string(),
            "##########---------- 50% | 74 MB of 148 MB"
        );
        assert_eq!(line(5_000_000, None, &ui).to_string(), "5 MB");
    }
}
