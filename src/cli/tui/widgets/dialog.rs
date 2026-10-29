//! Dialogs: the only thick-bordered box, centred, with the screen behind
//! it dimmed.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::Ui;

/// Dim everything already drawn in `area`: faint, and SGR 2 for a terminal
/// without the colour.
pub fn dim(buf: &mut Buffer, area: Rect, ui: &Ui) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.modifier.insert(Modifier::DIM);
            cell.modifier.remove(Modifier::BOLD);
            if let Some(faint) = ui.theme.faint.fg {
                cell.fg = faint;
            }
        }
    }
}

/// Draw `lines` in a dialog centred in `area`, over the dimmed screen.
pub fn draw(f: &mut Frame, area: Rect, ui: &Ui, lines: Vec<Line<'static>>) {
    dim(f.buffer_mut(), area, ui);
    let inner_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (inner_width + 4).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let block = Block::bordered().border_set(ui.glyphs.dialog);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let inner = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    f.render_widget(Paragraph::new(lines), inner);
}
