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

/// Draw `lines` in a dialog centred in `area`, over the dimmed screen,
/// `title` (if any) in its top border.
pub fn draw_titled(f: &mut Frame, area: Rect, ui: &Ui, title: &str, lines: Vec<Line<'static>>) {
    dim(f.buffer_mut(), area, ui);
    let inner_width = lines
        .iter()
        .map(Line::width)
        .chain([title.chars().count() + 2])
        .max()
        .unwrap_or(0) as u16;
    let rect = centred(area, inner_width + 4, lines.len() as u16 + 2);
    let inner = boxed(f, rect, ui, title);
    f.render_widget(Paragraph::new(lines), inner);
}

/// A box `width` by `height` (border included) centred in `area`, no
/// bigger than it.
pub fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Clear `rect` and draw the thick box there, `title` in its top border;
/// the area inside, one column in on each side.
pub fn boxed(f: &mut Frame, rect: Rect, ui: &Ui, title: &str) -> Rect {
    f.render_widget(Clear, rect);
    let mut block = Block::bordered().border_set(ui.glyphs.dialog);
    if !title.is_empty() {
        block = block.title(format!(" {title} "));
    }
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    )
}
