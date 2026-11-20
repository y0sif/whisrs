//! Agent overlay rendering: the rows of [`crate::agent::display`] drawn
//! into a pixmap, headless (the Wayland and X11 hosts only copy it out).
//!
//! The canvas is a fixed logical [`CANVAS_WIDTH`] x [`CANVAS_HEIGHT`] with
//! room for [`MAX_ROWS`](crate::agent::display::MAX_ROWS) rows. A row is a
//! 44 px shape (an orb or a face, [`super::agent_orb`],
//! [`super::agent_face`]) on the screen-edge side of a 212 x 44 card: the
//! agent's name and state word, then what it does and for how long. Rows
//! stack away from the corner's edge, the first in config order nearest
//! it, and a "+N more" chip takes the last slot when agents do not fit.
//!
//! Everything is drawn at `scale` (pixmap px per logical px): shapes
//! through a transform, text rasterized at the physical size, card edges
//! snapped to whole pixels, so 2x is crisp rather than upscaled.
//!
//! [`AgentScene`] remembers, per shown agent, when its row appeared and when
//! its state or activity last changed: a new row fades and slides in over
//! [`ENTRANCE`] once, and a change crossfades instead of popping. An
//! entry's own `since` cannot carry that: it restarts with every voice
//! state, and for a Done row it is when the turn started.

use std::time::{Duration, Instant};

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, StrokeDash,
    Transform,
};

use crate::agent::display::{clock, more_label, AgentOverlayEntry, AgentVisual, Rows};
use crate::{AgentOverlayCorner, AgentOverlayStyle};

use super::text::{self, TextStyle, Weight};
use super::{agent_face, agent_orb};

/// Logical canvas width: padding, card, gap, shape, padding.
pub const CANVAS_WIDTH: u32 = 300;
/// Logical canvas height: four rows and the gaps between them, padded.
pub const CANVAS_HEIGHT: u32 = 236;

/// How long a new row takes to fade and slide in, and a change to
/// crossfade.
pub const ENTRANCE: Duration = Duration::from_millis(180);

/// Room around the rows for what a shape draws past its box (ripples,
/// a working face's dots) and for anti-aliasing.
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 12.0;
const ROW_H: f32 = 44.0;
const ROW_GAP: f32 = 12.0;
/// The shape's box, square.
pub(super) const SHAPE: f32 = 44.0;
const CARD_W: f32 = 212.0;
const CARD_GAP: f32 = 12.0;
const CARD_RADIUS: f32 = 10.0;
/// Text inset from the card's left and right edges.
const CARD_PAD: f32 = 12.0;
/// Baselines inside the card, from its top: caps of line 1 (13 px) and
/// line 2 (12 px) sit at an even inset from the card edges.
const LINE1_BASELINE: f32 = 19.0;
const LINE2_BASELINE: f32 = 34.0;
/// Gap between the activity and the timer on line 2.
const TIMER_GAP: f32 = 8.0;
/// A new row starts this far toward the screen edge.
const SLIDE: f32 = 10.0;
const MORE_H: f32 = 24.0;
const MORE_PAD: f32 = 10.0;
/// The card's background opacity: the desktop shows through a little.
const CARD_ALPHA: f32 = 0.94;

const NAME: TextStyle = TextStyle::new(Weight::SemiBold, 13.0);
const BODY: TextStyle = TextStyle::new(Weight::Regular, 12.0);
const SEPARATOR: &str = " \u{b7} ";

pub(super) const LISTENING: [u8; 3] = [0x4F, 0xC3, 0xF7];
pub(super) const TRANSCRIBING: [u8; 3] = [0x8E, 0xA6, 0xFF];
pub(super) const WORKING: [u8; 3] = [0xF2, 0xB3, 0x4B];
pub(super) const SPEAKING: [u8; 3] = [0x4D, 0xD9, 0xA7];
pub(super) const NEEDS_YOU: [u8; 3] = [0xFF, 0x8A, 0x3D];
pub(super) const DONE: [u8; 3] = [0x4D, 0xD9, 0xA7];
pub(super) const ERROR: [u8; 3] = [0xF2, 0x6B, 0x6B];
const CARD_BG: [u8; 3] = [0x17, 0x1A, 0x21];
const CARD_BORDER: [u8; 3] = [0x26, 0x2B, 0x35];
const TEXT: [u8; 3] = [0xE8, 0xEA, 0xED];
const MUTED: [u8; 3] = [0x9A, 0xA0, 0xA6];

/// A state's color: the shape, the state word, a waiting card's border.
pub(super) fn state_color(visual: AgentVisual) -> [u8; 3] {
    match visual {
        AgentVisual::Listening => LISTENING,
        AgentVisual::Transcribing => TRANSCRIBING,
        AgentVisual::Working => WORKING,
        AgentVisual::Speaking => SPEAKING,
        AgentVisual::NeedsYou => NEEDS_YOU,
        AgentVisual::Done => DONE,
        AgentVisual::Error => ERROR,
    }
}

/// The pixmap size for `scale`: the logical canvas, rounded up.
pub fn canvas_size(scale: f32) -> (u32, u32) {
    let px = |logical: u32| (logical as f32 * scale).ceil() as u32;
    (px(CANVAS_WIDTH), px(CANVAS_HEIGHT))
}

/// Whether the host has to draw again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// A row is shown or still arriving: draw the next frame.
    Animating,
    /// Nothing is shown and the pixmap is fully transparent: no frames
    /// until the rows change.
    Idle,
}

/// One shape to draw: where, how big, how opaque, and how far into its
/// state.
#[derive(Debug, Clone, Copy)]
pub(super) struct ShapeFrame {
    /// Top-left of the [`SHAPE`] box, logical px.
    pub(super) origin: (f32, f32),
    pub(super) scale: f32,
    pub(super) alpha: f32,
    /// Seconds since the row entered this state.
    pub(super) t: f32,
    /// The voice level, 0 to 1: the mic while listening, the reply while
    /// speaking.
    pub(super) level: f32,
}

impl ShapeFrame {
    /// Maps the shape's box coordinates (0 to [`SHAPE`]) to pixmap px.
    pub(super) fn transform(&self) -> Transform {
        Transform::from_row(
            self.scale,
            0.0,
            0.0,
            self.scale,
            self.origin.0 * self.scale,
            self.origin.1 * self.scale,
        )
    }

    /// A point of the shape's box in logical canvas px (for text).
    pub(super) fn at(&self, x: f32, y: f32) -> (f32, f32) {
        (self.origin.0 + x, self.origin.1 + y)
    }
}

/// What a row last showed, to notice a change.
#[derive(Debug, Clone)]
struct Seen {
    name: String,
    appeared: Instant,
    visual: AgentVisual,
    visual_since: Instant,
    prev_visual: Option<AgentVisual>,
    activity: Option<String>,
    activity_since: Instant,
    prev_activity: Option<Option<String>>,
}

/// The agent overlay's drawing, for one surface: its style and corner,
/// plus what each row showed last (see the module docs).
#[derive(Debug, Clone)]
pub struct AgentScene {
    style: AgentOverlayStyle,
    corner: AgentOverlayCorner,
    seen: Vec<Seen>,
}

impl AgentScene {
    pub fn new(style: AgentOverlayStyle, corner: AgentOverlayCorner) -> Self {
        Self {
            style,
            corner,
            seen: Vec::new(),
        }
    }

    /// Draw `rows` as of `now` into `pixmap` (cleared first; sized
    /// [`canvas_size`]`(scale)`). `level` is the voice level, 0 to 1, best
    /// smoothed by the caller. `scale` is pixmap px per logical px: a
    /// Wayland buffer scale, or `Xft.dpi / 96` on X11 (need not be whole).
    /// Says whether to keep drawing.
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        rows: Rows<'_>,
        now: Instant,
        level: f32,
        scale: f32,
    ) -> Pace {
        pixmap.fill(Color::TRANSPARENT);
        self.observe(rows.shown, now);
        let level = level.clamp(0.0, 1.0);
        let mut pace = Pace::Idle;
        for (slot, entry) in rows.shown.iter().enumerate() {
            let Some(seen) = self.seen.iter().find(|s| s.name == entry.name) else {
                continue;
            };
            let alpha = entry.alpha(now);
            if alpha <= 0.0 {
                continue;
            }
            pace = Pace::Animating;
            let row = Row {
                y: self.slot_y(slot),
                entry,
                seen,
                alpha,
                now,
                level,
                scale,
            };
            self.draw_row(pixmap, &row);
        }
        if rows.more > 0 {
            pace = Pace::Animating;
            self.draw_more(pixmap, rows.more, self.slot_y(rows.shown.len()), scale);
        }
        pace
    }

    /// Track appearances and changes; forget agents no longer shown.
    fn observe(&mut self, entries: &[AgentOverlayEntry], now: Instant) {
        self.seen
            .retain(|s| entries.iter().any(|e| e.name == s.name));
        for entry in entries {
            match self.seen.iter_mut().find(|s| s.name == entry.name) {
                None => self.seen.push(Seen {
                    name: entry.name.clone(),
                    appeared: now,
                    visual: entry.visual,
                    visual_since: now,
                    prev_visual: None,
                    activity: entry.activity.clone(),
                    activity_since: now,
                    prev_activity: None,
                }),
                Some(seen) => {
                    if seen.visual != entry.visual {
                        seen.prev_visual = Some(seen.visual);
                        seen.visual = entry.visual;
                        seen.visual_since = now;
                    }
                    if seen.activity != entry.activity {
                        let old = std::mem::replace(&mut seen.activity, entry.activity.clone());
                        seen.prev_activity = Some(old);
                        seen.activity_since = now;
                    }
                }
            }
        }
    }

    /// Top of slot `i` (0 nearest the corner's edge), logical px.
    fn slot_y(&self, i: usize) -> f32 {
        let step = i as f32 * (ROW_H + ROW_GAP);
        if self.corner.is_top() {
            PAD_Y + step
        } else {
            CANVAS_HEIGHT as f32 - PAD_Y - ROW_H - step
        }
    }

    /// Left edges of the card and the shape, logical px: the shape on the
    /// screen-edge side.
    fn columns(&self) -> (f32, f32) {
        if self.corner.is_right() {
            (PAD_X, PAD_X + CARD_W + CARD_GAP)
        } else {
            (PAD_X + SHAPE + CARD_GAP, PAD_X)
        }
    }

    fn draw_row(&self, pixmap: &mut Pixmap, row: &Row<'_>) {
        let since_appeared = secs(row.now, row.seen.appeared);
        let enter = ease_out_cubic(fraction(since_appeared, ENTRANCE));
        let alpha = row.alpha * enter;
        if alpha <= 0.0 {
            return;
        }
        let toward_edge = if self.corner.is_right() { 1.0 } else { -1.0 };
        let dx = (1.0 - enter) * SLIDE * toward_edge;
        let (card_x, shape_x) = self.columns();
        let visual = row.entry.visual;

        // A state change crossfades: the old state's opacity, the new one's.
        let x = fraction(secs(row.now, row.seen.visual_since), ENTRANCE);
        let prev = row.seen.prev_visual.filter(|_| x < 1.0);
        let (fade_out, fade_in) = if prev.is_some() {
            crossfade(x)
        } else {
            (0.0, 1.0)
        };

        let shape = |t, alpha| ShapeFrame {
            origin: (shape_x + dx, row.y),
            scale: row.scale,
            alpha,
            t,
            level: row.level,
        };
        let t = secs(row.now, row.seen.visual_since);
        if let Some(old) = prev {
            // The old shape is long into its state, so a one-shot draw-in
            // (Done's check) shows complete while it fades.
            self.draw_shape(pixmap, old, &shape(t + 10.0, alpha * fade_out));
        }
        self.draw_shape(pixmap, visual, &shape(t, alpha * fade_in));

        let card = Card {
            x: card_x + dx,
            y: row.y,
            scale: row.scale,
            alpha,
        };
        let border = if visual == AgentVisual::NeedsYou {
            NEEDS_YOU
        } else {
            CARD_BORDER
        };
        card.draw_box(pixmap, border);

        // Line 1: name, separator, state word. The word crossfades.
        let inner = CARD_W - 2.0 * CARD_PAD;
        let word_room = [Some(visual), prev]
            .into_iter()
            .flatten()
            .map(|v| text::measure(v.word(), NAME))
            .fold(0.0, f32::max);
        let name_room = inner - text::measure(SEPARATOR, NAME) - word_room;
        let name = text::ellipsize(&row.entry.name, NAME, name_room.max(0.0));
        let mut pen = card.text(pixmap, &name, NAME, (0.0, LINE1_BASELINE), TEXT, 1.0);
        pen += card.text(pixmap, SEPARATOR, NAME, (pen, LINE1_BASELINE), MUTED, 1.0);
        let at = (pen, LINE1_BASELINE);
        if let Some(old) = prev {
            card.text(pixmap, old.word(), NAME, at, state_color(old), fade_out);
        }
        card.text(
            pixmap,
            visual.word(),
            NAME,
            at,
            state_color(visual),
            fade_in,
        );

        // Line 2: the activity, and the timer at the right. A finished
        // row's activity already says how long it took; its timer would
        // keep counting from the turn's start.
        let timer_style = BODY.tabular();
        let timer = clock(row.now.saturating_duration_since(row.entry.since));
        let timer_w = text::measure(&timer, timer_style);
        let timer_alpha = |v: AgentVisual| if shows_timer(v) { 1.0 } else { 0.0 };
        let timer_now = match prev {
            Some(old) => (timer_alpha(old) * fade_out + timer_alpha(visual) * fade_in).min(1.0),
            None => timer_alpha(visual),
        };
        if timer_now > 0.0 {
            let at = (inner - timer_w, LINE2_BASELINE);
            card.text(pixmap, &timer, timer_style, at, MUTED, timer_now);
        }
        let activity_room = if timer_now > 0.0 {
            inner - timer_w - TIMER_GAP
        } else {
            inner
        };
        let ax = fraction(secs(row.now, row.seen.activity_since), ENTRANCE);
        let mut activity = |text: Option<&str>, a: f32| {
            if let Some(text) = text.filter(|t| !t.is_empty()) {
                let text = text::ellipsize(text, BODY, activity_room.max(0.0));
                card.text(pixmap, &text, BODY, (0.0, LINE2_BASELINE), MUTED, a);
            }
        };
        match row.seen.prev_activity.as_ref().filter(|_| ax < 1.0) {
            Some(old) => {
                let (out, inn) = crossfade(ax);
                activity(old.as_deref(), out);
                activity(row.entry.activity.as_deref(), inn);
            }
            None => activity(row.entry.activity.as_deref(), 1.0),
        }
    }

    fn draw_shape(&self, pixmap: &mut Pixmap, visual: AgentVisual, frame: &ShapeFrame) {
        if frame.alpha <= 0.0 {
            return;
        }
        match self.style {
            AgentOverlayStyle::Orb => agent_orb::draw(pixmap, visual, frame),
            AgentOverlayStyle::Face => agent_face::draw(pixmap, visual, frame),
        }
    }

    /// The "+N more" chip: a small card at the card column's screen-edge
    /// end, against the rows it continues.
    fn draw_more(&self, pixmap: &mut Pixmap, more: usize, slot_y: f32, scale: f32) {
        let label = more_label(more);
        let w = (text::measure(&label, BODY) + 2.0 * MORE_PAD).ceil();
        let (card_x, _) = self.columns();
        let x = if self.corner.is_right() {
            card_x + CARD_W - w
        } else {
            card_x
        };
        let y = if self.corner.is_top() {
            slot_y
        } else {
            slot_y + ROW_H - MORE_H
        };
        let chip = Card {
            x,
            y,
            scale,
            alpha: 1.0,
        };
        chip.draw_rounded(pixmap, w, MORE_H, MORE_H / 2.0, CARD_BORDER);
        chip.text(
            pixmap,
            &label,
            BODY,
            (MORE_PAD - CARD_PAD, 16.5),
            MUTED,
            1.0,
        );
    }
}

/// One row being drawn.
struct Row<'a> {
    /// Top of its slot, logical px.
    y: f32,
    entry: &'a AgentOverlayEntry,
    seen: &'a Seen,
    /// The entry's own opacity (a fading Done row).
    alpha: f32,
    now: Instant,
    level: f32,
    scale: f32,
}

/// Whether the card shows a running timer: not for a finished turn.
fn shows_timer(visual: AgentVisual) -> bool {
    !matches!(visual, AgentVisual::Done | AgentVisual::Error)
}

/// A card being drawn: its top-left (logical px), scale and opacity.
#[derive(Debug, Clone, Copy)]
struct Card {
    x: f32,
    y: f32,
    scale: f32,
    alpha: f32,
}

impl Card {
    fn draw_box(&self, pixmap: &mut Pixmap, border: [u8; 3]) {
        self.draw_rounded(pixmap, CARD_W, ROW_H, CARD_RADIUS, border);
    }

    /// A rounded box with a one logical px border, edges snapped to whole
    /// pixmap pixels. The border is a ring, so the translucent background
    /// never shows it through.
    fn draw_rounded(&self, pixmap: &mut Pixmap, w: f32, h: f32, radius: f32, border: [u8; 3]) {
        let s = self.scale;
        let snap = |v: f32| (v * s).round();
        let (x0, y0) = (snap(self.x), snap(self.y));
        let (x1, y1) = (snap(self.x + w), snap(self.y + h));
        let b = s.round().max(1.0);
        let r = radius * s;
        let outer = rounded_rect(x0, y0, x1 - x0, y1 - y0, r);
        let inner = rounded_rect(x0 + b, y0 + b, x1 - x0 - 2.0 * b, y1 - y0 - 2.0 * b, r - b);
        let (Some(outer), Some(inner)) = (outer, inner) else {
            return;
        };
        fill(
            pixmap,
            &inner,
            CARD_BG,
            self.alpha * CARD_ALPHA,
            Transform::identity(),
        );
        let mut ring = PathBuilder::new();
        ring.push_path(&outer);
        ring.push_path(&inner);
        if let Some(ring) = ring.finish() {
            let paint = paint(border, self.alpha);
            pixmap.fill_path(
                &ring,
                &paint,
                FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
        }
    }

    /// Draw text at `at.0` from the card's text inset with its baseline
    /// `at.1` below the card's top. Returns its width.
    fn text(
        &self,
        pixmap: &mut Pixmap,
        s: &str,
        style: TextStyle,
        at: (f32, f32),
        rgb: [u8; 3],
        alpha: f32,
    ) -> f32 {
        let at = (self.x + CARD_PAD + at.0, self.y + at.1);
        text::draw(pixmap, s, style, at, rgb, self.alpha * alpha, self.scale)
    }
}

/// Seconds from `from` to `now`, 0 if `now` is earlier.
fn secs(now: Instant, from: Instant) -> f32 {
    now.saturating_duration_since(from).as_secs_f32()
}

/// How far `elapsed` seconds are into `span`, 0 to 1.
fn fraction(elapsed: f32, span: Duration) -> f32 {
    (elapsed / span.as_secs_f32()).clamp(0.0, 1.0)
}

/// The old and the new opacity `x` (0 to 1) into a crossfade. Staggered:
/// two different strings at half opacity each read as a smudge, so the old
/// one is mostly gone before the new one comes up.
fn crossfade(x: f32) -> (f32, f32) {
    let out = (1.0 - x / 0.6).clamp(0.0, 1.0);
    let inn = ((x - 0.4) / 0.6).clamp(0.0, 1.0);
    (out, inn)
}

pub(super) fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// 0 to 1 and back over `period` seconds, smoothly, 0 at `t = 0`.
pub(super) fn wave(t: f32, period: f32) -> f32 {
    0.5 - 0.5 * (std::f32::consts::TAU * t / period).cos()
}

pub(super) fn paint(rgb: [u8; 3], alpha: f32) -> Paint<'static> {
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(
        rgb[0],
        rgb[1],
        rgb[2],
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    paint
}

pub(super) fn fill(pixmap: &mut Pixmap, path: &Path, rgb: [u8; 3], alpha: f32, ts: Transform) {
    if alpha > 0.0 {
        pixmap.fill_path(path, &paint(rgb, alpha), FillRule::Winding, ts, None);
    }
}

/// Stroke with round caps and joins, `width` in the transform's units.
pub(super) fn stroke(
    pixmap: &mut Pixmap,
    path: &Path,
    rgb: [u8; 3],
    alpha: f32,
    width: f32,
    ts: Transform,
) {
    stroke_dashed(pixmap, path, rgb, alpha, width, None, ts);
}

pub(super) fn stroke_dashed(
    pixmap: &mut Pixmap,
    path: &Path,
    rgb: [u8; 3],
    alpha: f32,
    width: f32,
    dash: Option<StrokeDash>,
    ts: Transform,
) {
    if alpha <= 0.0 {
        return;
    }
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        dash,
        ..Default::default()
    };
    pixmap.stroke_path(path, &paint(rgb, alpha), &stroke, ts, None);
}

pub(super) fn circle(cx: f32, cy: f32, r: f32) -> Option<Path> {
    PathBuilder::from_circle(cx, cy, r.max(0.01))
}

/// A circular arc from `start` sweeping `sweep` radians (clockwise on
/// screen for a positive sweep, 0 pointing right), as cubic segments.
pub(super) fn arc(cx: f32, cy: f32, r: f32, start: f32, sweep: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    push_arc(&mut pb, cx, cy, r, start, sweep, true);
    pb.finish()
}

/// Append an arc to `pb`, starting a new subpath when `move_first`.
pub(super) fn push_arc(
    pb: &mut PathBuilder,
    cx: f32,
    cy: f32,
    r: f32,
    start: f32,
    sweep: f32,
    move_first: bool,
) {
    let segments = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / segments as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan() * r;
    let point = |a: f32| (cx + r * a.cos(), cy + r * a.sin());
    let (x, y) = point(start);
    if move_first {
        pb.move_to(x, y);
    } else {
        pb.line_to(x, y);
    }
    for i in 0..segments {
        let a0 = start + step * i as f32;
        let a1 = a0 + step;
        let (x0, y0) = point(a0);
        let (x1, y1) = point(a1);
        pb.cubic_to(
            x0 - k * a0.sin(),
            y0 + k * a0.cos(),
            x1 + k * a1.sin(),
            y1 - k * a1.cos(),
            x1,
            y1,
        );
    }
}

pub(super) fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.clamp(0.0, w.min(h) / 2.0);
    let mut pb = PathBuilder::new();
    use std::f32::consts::{FRAC_PI_2, PI};
    push_arc(&mut pb, x + w - r, y + r, r, -FRAC_PI_2, FRAC_PI_2, true);
    push_arc(&mut pb, x + w - r, y + h - r, r, 0.0, FRAC_PI_2, false);
    push_arc(&mut pb, x + r, y + h - r, r, FRAC_PI_2, FRAC_PI_2, false);
    push_arc(&mut pb, x + r, y + r, r, PI, FRAC_PI_2, false);
    pb.close();
    pb.finish()
}

/// A straight segment.
pub(super) fn line(x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::display::{rows, Board, DONE_FADE, DONE_HOLD};

    const ALL: [AgentVisual; 7] = [
        AgentVisual::Listening,
        AgentVisual::Transcribing,
        AgentVisual::Working,
        AgentVisual::Speaking,
        AgentVisual::NeedsYou,
        AgentVisual::Done,
        AgentVisual::Error,
    ];
    const CORNERS: [AgentOverlayCorner; 4] = [
        AgentOverlayCorner::BottomRight,
        AgentOverlayCorner::BottomLeft,
        AgentOverlayCorner::TopRight,
        AgentOverlayCorner::TopLeft,
    ];
    const STYLES: [AgentOverlayStyle; 2] = [AgentOverlayStyle::Orb, AgentOverlayStyle::Face];

    fn entry(name: &str, visual: AgentVisual, since: Instant) -> AgentOverlayEntry {
        AgentOverlayEntry {
            name: name.to_string(),
            visual,
            activity: Some("Reading your inbox".to_string()),
            since,
            fading_from: None,
        }
    }

    /// A scene whose rows appeared a second before `now`, drawn at `now`.
    fn settled(
        style: AgentOverlayStyle,
        corner: AgentOverlayCorner,
        entries: &[AgentOverlayEntry],
        now: Instant,
        scale: f32,
    ) -> (Pixmap, Pace) {
        let (w, h) = canvas_size(scale);
        let mut pm = Pixmap::new(w, h).unwrap();
        let mut scene = AgentScene::new(style, corner);
        scene.draw(
            &mut pm,
            rows(entries),
            now - Duration::from_secs(1),
            0.5,
            scale,
        );
        let pace = scene.draw(&mut pm, rows(entries), now, 0.5, scale);
        (pm, pace)
    }

    /// Bounding box `(x0, y0, x1, y1)` of the pixels with any alpha in
    /// `[x0, x1) x [y0, y1)`.
    fn ink_box(pm: &Pixmap, area: (u32, u32, u32, u32)) -> Option<(u32, u32, u32, u32)> {
        let mut found: Option<(u32, u32, u32, u32)> = None;
        for y in area.1..area.3 {
            for x in area.0..area.2 {
                if pm.pixel(x, y).is_some_and(|p| p.alpha() > 0) {
                    let b = found.get_or_insert((x, y, x, y));
                    *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
        }
        found
    }

    fn whole(pm: &Pixmap) -> (u32, u32, u32, u32) {
        (0, 0, pm.width(), pm.height())
    }

    /// The most common bright, opaque color in `area`, quantized to 16
    /// levels per channel: what the shape is drawn in.
    fn dominant(pm: &Pixmap, area: (u32, u32, u32, u32)) -> Option<[u8; 3]> {
        let mut counts = std::collections::HashMap::<[u8; 3], usize>::new();
        for y in area.1..area.3 {
            for x in area.0..area.2 {
                let Some(p) = pm.pixel(x, y) else { continue };
                let c = p.demultiply();
                let rgb = [c.red(), c.green(), c.blue()];
                if c.alpha() > 200 && rgb.iter().any(|v| *v > 90) {
                    *counts.entry(rgb.map(|v| v / 16)).or_default() += 1;
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(q, _)| q.map(|v| v * 16 + 8))
    }

    fn close(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tolerance)
    }

    /// The shape box of the bottom-right corner's first row, pixmap px.
    fn shape_area(scale: f32) -> (u32, u32, u32, u32) {
        let x0 = PAD_X + CARD_W + CARD_GAP;
        let y0 = CANVAS_HEIGHT as f32 - PAD_Y - ROW_H;
        let px = |v: f32| (v * scale) as u32;
        (px(x0), px(y0), px(x0 + SHAPE), px(y0 + ROW_H))
    }

    fn card_area(scale: f32) -> (u32, u32, u32, u32) {
        let y0 = CANVAS_HEIGHT as f32 - PAD_Y - ROW_H;
        let px = |v: f32| (v * scale) as u32;
        (px(PAD_X), px(y0), px(PAD_X + CARD_W), px(y0 + ROW_H))
    }

    #[test]
    fn every_style_and_state_draws_in_its_color() {
        // At 2x: the face's features are 1.4 px lines, which at 1x are
        // mostly edge pixels blended with the face's background.
        let now = Instant::now();
        for style in STYLES {
            for visual in ALL {
                let (pm, pace) = settled(
                    style,
                    AgentOverlayCorner::BottomRight,
                    &[entry("jarvis", visual, now)],
                    now,
                    2.0,
                );
                assert_eq!(pace, Pace::Animating);
                let got = dominant(&pm, shape_area(2.0));
                let want = state_color(visual);
                assert!(
                    got.is_some_and(|got| close(got, want, 24)),
                    "{style:?} {visual:?}: {got:?} vs {want:?}"
                );
            }
        }
    }

    #[test]
    fn the_card_carries_text() {
        let now = Instant::now();
        let (pm, _) = settled(
            AgentOverlayStyle::Orb,
            AgentOverlayCorner::BottomRight,
            &[entry("jarvis", AgentVisual::Working, now)],
            now,
            1.0,
        );
        let (x0, y0, x1, y1) = card_area(1.0);
        let mut text = 0;
        let mut muted = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                let c = pm.pixel(x, y).unwrap().demultiply();
                let rgb = [c.red(), c.green(), c.blue()];
                text += usize::from(c.alpha() == 255 && close(rgb, TEXT, 12));
                muted += usize::from(c.alpha() == 255 && close(rgb, MUTED, 12));
            }
        }
        assert!(text > 20, "name pixels: {text}");
        assert!(muted > 20, "activity pixels: {muted}");
        // The state word in the state color, inside the card.
        assert!(dominant(&pm, (x0 + 60, y0, x1, y0 + 22)).is_some_and(|c| close(c, WORKING, 24)));
    }

    #[test]
    fn rows_sit_in_their_corner() {
        let now = Instant::now();
        let (w, h) = (CANVAS_WIDTH, CANVAS_HEIGHT);
        for corner in CORNERS {
            let (pm, _) = settled(
                AgentOverlayStyle::Orb,
                corner,
                &[entry("jarvis", AgentVisual::Working, now)],
                now,
                1.0,
            );
            let (_, y0, _, y1) = ink_box(&pm, whole(&pm)).unwrap();
            if corner.is_top() {
                assert!(y1 < h / 3, "{corner:?}: {y0}..{y1}");
            } else {
                assert!(y0 > h * 2 / 3, "{corner:?}: {y0}..{y1}");
            }
            // The shape (amber) on the screen-edge side of the card.
            let amber = |x0: u32, x1: u32| dominant(&pm, (x0, 0, x1, h));
            let (edge, inner) = if corner.is_right() {
                (amber(w - 70, w), amber(0, 70))
            } else {
                (amber(0, 70), amber(w - 70, w))
            };
            assert!(
                edge.is_some_and(|c| close(c, WORKING, 24)),
                "{corner:?}: {edge:?}"
            );
            assert!(
                !inner.is_some_and(|c| close(c, WORKING, 24)),
                "{corner:?}: {inner:?}"
            );
        }
    }

    #[test]
    fn rows_stack_away_from_the_edge_and_more_takes_the_last_slot() {
        let now = Instant::now();
        let names = ["a", "b", "c", "d", "e", "f"];
        let entries: Vec<_> = names
            .iter()
            .map(|n| entry(n, AgentVisual::Working, now))
            .collect();
        for corner in CORNERS {
            let (pm, _) = settled(AgentOverlayStyle::Face, corner, &entries[..2], now, 1.0);
            let (_, y0, _, y1) = ink_box(&pm, whole(&pm)).unwrap();
            assert!(
                y1 - y0 > 2 * ROW_H as u32,
                "{corner:?}: two rows {y0}..{y1}"
            );
            let (four, _) = settled(AgentOverlayStyle::Face, corner, &entries, now, 1.0);
            let (_, y0, _, y1) = ink_box(&four, whole(&four)).unwrap();
            assert!(
                y1 - y0 > 3 * ROW_H as u32,
                "{corner:?}: four slots {y0}..{y1}"
            );
        }
    }

    #[test]
    fn nothing_touches_the_canvas_edge() {
        // Every state at its widest phases, four rows, in every corner and
        // style: the outermost pixels stay clear (nothing is clipped).
        let now = Instant::now();
        for style in STYLES {
            for corner in CORNERS {
                for phase in [0.0, 0.3, 0.45, 0.8, 0.9, 1.2, 1.6] {
                    let mut scene = AgentScene::new(style, corner);
                    let mut pm = Pixmap::new(CANVAS_WIDTH, CANVAS_HEIGHT).unwrap();
                    for chunk in ALL.chunks(4) {
                        let entries: Vec<_> = chunk
                            .iter()
                            .enumerate()
                            .map(|(i, v)| entry(&format!("agent-{i}"), *v, now))
                            .collect();
                        let at = now + Duration::from_secs_f32(phase);
                        scene.draw(
                            &mut pm,
                            rows(&entries),
                            now - Duration::from_secs(1),
                            1.0,
                            1.0,
                        );
                        scene.draw(&mut pm, rows(&entries), at, 1.0, 1.0);
                        let (x0, y0, x1, y1) = ink_box(&pm, whole(&pm)).unwrap();
                        assert!(
                            x0 > 0 && y0 > 0 && x1 < CANVAS_WIDTH - 1 && y1 < CANVAS_HEIGHT - 1,
                            "{style:?} {corner:?} {chunk:?} at {phase}: {x0},{y0} {x1},{y1}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn scale_two_doubles_the_extent() {
        let now = Instant::now();
        let entries = [
            entry("jarvis", AgentVisual::Done, now),
            entry("friday", AgentVisual::NeedsYou, now),
        ];
        for style in STYLES {
            let (one, _) = settled(style, AgentOverlayCorner::BottomLeft, &entries, now, 1.0);
            let (two, _) = settled(style, AgentOverlayCorner::BottomLeft, &entries, now, 2.0);
            assert_eq!(
                (two.width(), two.height()),
                (2 * CANVAS_WIDTH, 2 * CANVAS_HEIGHT)
            );
            let a = ink_box(&one, whole(&one)).unwrap();
            let b = ink_box(&two, whole(&two)).unwrap();
            let pairs = [
                (a.0, b.0),
                (a.1, b.1),
                (a.2 + 1, b.2 + 1),
                (a.3 + 1, b.3 + 1),
            ];
            for (single, double) in pairs {
                assert!(
                    (single as i64 * 2 - double as i64).abs() <= 2,
                    "{style:?}: {a:?} at 1x vs {b:?} at 2x"
                );
            }
        }
    }

    #[test]
    fn a_fractional_scale_stays_inside_its_canvas() {
        // X11's Xft.dpi / 96 need not be whole.
        let now = Instant::now();
        let entries = [entry("jarvis", AgentVisual::Speaking, now)];
        let (pm, pace) = settled(
            AgentOverlayStyle::Orb,
            AgentOverlayCorner::TopLeft,
            &entries,
            now,
            1.5,
        );
        assert_eq!((pm.width(), pm.height()), (450, 354));
        assert_eq!(pace, Pace::Animating);
        let (x0, y0, x1, y1) = ink_box(&pm, whole(&pm)).unwrap();
        assert!(
            x0 > 0 && y0 > 0 && x1 < pm.width() - 1 && y1 < pm.height() / 3,
            "{x0},{y0} {x1},{y1}"
        );
    }

    #[test]
    fn a_done_row_is_gone_after_its_hold_and_fade() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", t0);
        board.turn_ended("jarvis", true, t0 + Duration::from_secs(5));
        let ended = t0 + Duration::from_secs(5);
        let mut scene = AgentScene::new(AgentOverlayStyle::Orb, AgentOverlayCorner::BottomRight);
        let mut pm = Pixmap::new(CANVAS_WIDTH, CANVAS_HEIGHT).unwrap();

        let shown = board.compose(ended);
        assert_eq!(shown[0].visual, AgentVisual::Done);
        assert_eq!(
            scene.draw(&mut pm, rows(&shown), ended, 0.0, 1.0),
            Pace::Animating
        );
        let held = ended + DONE_HOLD;
        assert_eq!(
            scene.draw(&mut pm, rows(&shown), held, 0.0, 1.0),
            Pace::Animating
        );
        assert!(ink_box(&pm, whole(&pm)).is_some());

        // The same entry (a host that has not recomposed yet) draws nothing
        // once the fade is over, and the scene goes idle.
        let gone = ended + DONE_HOLD + DONE_FADE;
        assert_eq!(
            scene.draw(&mut pm, rows(&shown), gone, 0.0, 1.0),
            Pace::Idle
        );
        assert!(pm.data().iter().all(|b| *b == 0));
        // Recomposed, the row is gone.
        let none = board.compose(gone);
        assert!(none.is_empty());
        assert_eq!(scene.draw(&mut pm, rows(&none), gone, 0.0, 1.0), Pace::Idle);
        assert!(pm.data().iter().all(|b| *b == 0));
    }

    #[test]
    fn a_new_row_fades_in_once() {
        let t0 = Instant::now();
        let mut scene = AgentScene::new(AgentOverlayStyle::Orb, AgentOverlayCorner::BottomRight);
        let mut pm = Pixmap::new(CANVAS_WIDTH, CANVAS_HEIGHT).unwrap();
        let opaque = |pm: &Pixmap| pm.pixels().iter().filter(|p| p.alpha() == 255).count();
        let listening = [entry("jarvis", AgentVisual::Listening, t0)];
        // Just appeared: nothing drawn yet, but still animating.
        assert_eq!(
            scene.draw(&mut pm, rows(&listening), t0, 0.0, 1.0),
            Pace::Animating
        );
        assert_eq!(opaque(&pm), 0);
        scene.draw(&mut pm, rows(&listening), t0 + ENTRANCE / 2, 0.0, 1.0);
        let half = opaque(&pm);
        scene.draw(&mut pm, rows(&listening), t0 + ENTRANCE, 0.0, 1.0);
        let full = opaque(&pm);
        assert!(half < full, "{half} vs {full}");

        // A state change restarts the entry's `since` but not the entrance:
        // the card stays fully drawn through the crossfade.
        let later = t0 + Duration::from_secs(2);
        let transcribing = [entry("jarvis", AgentVisual::Transcribing, later)];
        scene.draw(&mut pm, rows(&transcribing), later, 0.0, 1.0);
        let card = card_area(1.0);
        let bg = pm.pixel(card.0 + 100, card.1 + 4).unwrap().demultiply();
        assert_eq!(bg.alpha(), (CARD_ALPHA * 255.0).round() as u8);
    }
}
