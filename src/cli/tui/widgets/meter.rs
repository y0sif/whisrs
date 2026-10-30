//! A level meter: a bar from -60 to 0 dBFS with the silence gate and the held
//! peak marked on it, the level in numbers beside it.

use ratatui::text::{Line, Span};

use super::{Tone, Ui};

/// The quietest level the bar shows.
pub const FLOOR_DBFS: f32 = -60.0;

/// What the numbers beside the bar take.
const VALUE: usize = 11;

/// The cell of `width` a level falls in.
fn cell(dbfs: f32, width: usize) -> usize {
    let at = ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0);
    ((at * width as f32) as usize).min(width.saturating_sub(1))
}

/// The bar for `rms` with the gate at `gate` and the held peak, `width`
/// columns in all. Over the gate the bar is in the accent: that is speech
/// getting through.
pub fn bar(rms: f32, gate: f32, held: Option<f32>, width: usize, ui: &Ui) -> Line<'static> {
    let cells = width.saturating_sub(VALUE).max(4);
    let filled = if rms <= FLOOR_DBFS {
        0
    } else {
        cell(rms, cells) + 1
    };
    let gate_at = cell(gate, cells);
    let peak_at = held.map(|peak| cell(peak, cells));
    let open = rms > gate;
    let mut spans = Vec::new();
    for i in 0..cells {
        let (text, tone) = if Some(i) == peak_at {
            let clipping = held.is_some_and(|peak| peak > -1.0);
            (
                ui.glyphs.peak,
                if clipping { Tone::Error } else { Tone::Plain },
            )
        } else if i == gate_at {
            (ui.glyphs.gate, Tone::Muted)
        } else if i < filled {
            (
                ui.glyphs.level,
                if open { Tone::Plain } else { Tone::Muted },
            )
        } else {
            (ui.glyphs.frame.horizontal_top, Tone::Faint)
        };
        let style = match tone {
            Tone::Plain => ui.theme.accent,
            tone => tone.style(ui.theme),
        };
        spans.push(Span::styled(text, style));
    }
    spans.push(Span::raw(format!(
        " {:>width$}",
        number(rms),
        width = VALUE - 1
    )));
    Line::from(spans)
}

/// A level as the meter writes it.
pub fn number(dbfs: f32) -> String {
    if dbfs <= crate::tui::screens::mic::analysis::MIN_DBFS {
        "silence".to_string()
    } else {
        format!("{:.0} dBFS", dbfs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_fall_in_their_cells() {
        assert_eq!(cell(-60.0, 30), 0);
        assert_eq!(cell(-90.0, 30), 0);
        assert_eq!(cell(-30.0, 30), 15);
        assert_eq!(cell(0.0, 30), 29);
        assert_eq!(cell(6.0, 30), 29);
    }

    #[test]
    fn the_bar_marks_the_gate_and_the_peak() {
        let theme = crate::tui::testing::theme();
        let glyphs = crate::tui::glyphs::Glyphs::ASCII;
        let ui = Ui {
            theme: &theme,
            glyphs: &glyphs,
        };
        let line = bar(-30.0, -50.0, Some(-12.0), 41, &ui).to_string();
        // 30 cells: the gate at 5, the level to 15, the peak at 24.
        assert_eq!(&line[..30], "#####:##########--------|-----");
        assert!(line.ends_with("-30 dBFS"), "{line}");
        let quiet = bar(-120.0, -50.0, None, 41, &ui).to_string();
        assert!(quiet.starts_with("-----:"), "{quiet}");
        assert!(quiet.ends_with("silence"), "{quiet}");
    }
}
