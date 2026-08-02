//! Shared overlay rendering: theme, animation state and frame drawing.

use std::sync::mpsc;
use std::time::Instant;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use tracing::warn;

use crate::{OverlayConfig, State};

use super::service::OverlayError;

pub(super) const BOTTOM_MARGIN: i32 = 16;

// Per-frame sleep matching the draw loop. ~16 ms ≈ 60 fps for visibly
// smoother motion. Spawn animation progress is wall-clock-driven (see
// `Overlay::spawn_t`), so this only caps the redraw rate.
pub(super) const FRAME_MS: u64 = 16;

// Spawn animation: the pill "draws out" from a 4-px sliver anchored at the
// bottom of the surface up to its full configured height. Slight overshoot
// for life. Going away is shorter and accelerated — feels intentional.
const SPAWN_IN_MS: f32 = 220.0;
const SPAWN_OUT_MS: f32 = 140.0;
/// Initial pill height during the appear animation, in px.
const SPAWN_PILL_MIN_H: f32 = 4.0;
/// `easeOutBack` overshoot constant. 0.4 ⇒ peak ~3 % over target before
/// settling — barely perceptible but adds a "physical arrival" feel.
const SPAWN_OVERSHOOT_C: f32 = 0.4;

/// While the pill is still growing, bars stay fully invisible for this many
/// ms after appearance — then they fade in over `BARS_FADE_MS` while the
/// pill finishes settling. After both have elapsed, audio reactivity
/// unlocks.
const BARS_GRACE_MS: f32 = 80.0;
const BARS_FADE_MS: f32 = 80.0;

// Bar layout. 7 bars × 3 px + 6 gaps × 2 px = 33 px wide, centered in
// the pill. More, thinner bars means motion reads as a continuous
// equalizer ripple instead of a few chunky blocks. Max bar height =
// HEIGHT − 2·BAR_VPAD (e.g. 28 px on the default 40 px pill). Bar height
// is purely level-driven — no per-bar phase animation — so each bar
// stays anchored at the pill center and expands symmetrically up and
// down with the audio amplitude.
const BAR_COUNT: u32 = 7;
const BAR_W: f32 = 3.0;
const BAR_GAP: f32 = 2.0;
const BAR_PITCH: f32 = BAR_W + BAR_GAP;
const BAR_BLOCK_W: f32 = BAR_COUNT as f32 * BAR_W + (BAR_COUNT - 1) as f32 * BAR_GAP;
const BAR_BASELINE: f32 = 6.0;
const BAR_VPAD: f32 = 6.0;

/// Color palette for one overlay theme. Bytes are stored as `[A, R, G, B]`,
/// matching the canvas pixel layout used by [`blend_pixel`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Theme {
    bg: [u8; 4],
    ring: [u8; 4],
    rec_bar: [u8; 4],
    trans_bar: [u8; 4],
    /// Read-aloud "speaking" bar color — a distinct hue from `rec_bar` so the
    /// user can tell dictation from read-aloud at a glance.
    speak_bar: [u8; 4],
    glow: [u8; 4],
}

impl Theme {
    /// Default palette — warm "tally light" amber on near-black slate.
    const fn ember() -> Self {
        Self {
            bg: [235, 14, 14, 16],           // #0E0E10 @ 92%
            ring: [64, 249, 115, 22],        // #F97316 @ 25%
            rec_bar: [255, 249, 115, 22],    // #F97316
            trans_bar: [255, 240, 237, 245], // #F0EDF5
            speak_bar: [255, 52, 211, 153],  // #34D399 emerald — distinct from amber
            glow: [60, 249, 115, 22],
        }
    }

    /// Monochrome terminal palette — subdued, never distracting.
    const fn carbon() -> Self {
        Self {
            bg: [235, 14, 14, 16],
            ring: [80, 58, 58, 64],          // hairline gray
            rec_bar: [255, 240, 237, 245],   // soft white
            trans_bar: [255, 156, 163, 175], // warm gray
            speak_bar: [255, 45, 212, 191],  // #2DD4BF teal — pops against the grays
            glow: [40, 240, 237, 245],
        }
    }

    /// Cool electric-blue palette — audio-equipment vibe.
    const fn cyan() -> Self {
        Self {
            bg: [235, 10, 15, 20],
            ring: [64, 34, 211, 238], // #22D3EE @ 25%
            rec_bar: [255, 34, 211, 238],
            trans_bar: [255, 56, 189, 248], // #38BDF8
            speak_bar: [255, 74, 222, 128], // #4ADE80 green — distinct from cyan
            glow: [50, 34, 211, 238],
        }
    }

    pub(super) fn from_config(cfg: &OverlayConfig) -> Self {
        let base = match cfg.theme.as_str() {
            "carbon" => Self::carbon(),
            "cyan" => Self::cyan(),
            "ember" | "custom" => Self::ember(),
            other => {
                warn!("unknown overlay theme {other:?}, falling back to ember");
                Self::ember()
            }
        };
        if cfg.theme != "custom" {
            return base;
        }
        let Some(c) = cfg.colors.as_ref() else {
            return base;
        };
        Self {
            bg: c
                .background
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.bg),
            ring: c
                .ring
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.ring),
            rec_bar: c
                .recording
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.rec_bar),
            trans_bar: c
                .transcribing
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.trans_bar),
            speak_bar: c
                .speaking
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.speak_bar),
            glow: c
                .glow
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.glow),
        }
    }
}

pub(super) struct OverlayRenderer {
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pixmap: Pixmap,
    target_state: State,
    visible_state: State,
    /// Wall-clock instant when the current spawn animation started. The
    /// animation progress `t` is derived from `(now - spawn_started) /
    /// duration`, so the timing is honest regardless of how often the
    /// dispatch loop ticks. (Previously this was a per-call increment that
    /// over-advanced when the loop fired multiple times per rendered
    /// frame, making the animation visually pop instead of ease.)
    spawn_started: Instant,
    /// `true` while transitioning into a visible state, `false` while
    /// transitioning out. Determines easing direction and duration.
    spawn_in: bool,
    /// Set once either input channel returns `Disconnected` — the daemon's
    /// forwarding task has exited, so the overlay should wind down cleanly
    /// instead of leaking the thread.
    pub(super) disconnected: bool,
    frame: u32,
    /// Smoothed audio level driving bar heights. Advanced toward
    /// `level_target` by a critically-damped spring stepped with the
    /// real elapsed `dt` between calls — frame-rate-independent.
    level: f32,
    level_target: f32,
    level_velocity: f32,
    /// Wall-clock instant of the previous spring step.
    last_update: Instant,
    theme: Theme,
}

/// Per-frame animation state computed from `spawn_t` + `spawn_in`. The
/// spawn animation drives a bottom-anchored height morph (with a small
/// overshoot on appear) instead of a slide+scale; the pill literally draws
/// itself out of the screen edge.
#[derive(Debug, Clone, Copy)]
struct AnimState {
    /// Currently displayed pill height in px (bottom-anchored — the pill's
    /// bottom edge stays glued to the surface bottom regardless of this
    /// value).
    pill_height: f32,
    /// Pill alpha 0..=1. Eases in faster than the height grows so the pill
    /// is solid before it stops moving.
    pill_alpha: f32,
    /// Bar alpha 0..=1. Stays at 0 during the initial grow, then fades in.
    bar_alpha: f32,
    /// `true` while audio reactivity should be gated to baseline. Honored
    /// by the recording draw — silenced bars rise from baseline once this
    /// goes false.
    bars_locked: bool,
}

impl OverlayRenderer {
    pub(super) fn new(
        state_rx: mpsc::Receiver<State>,
        level_rx: mpsc::Receiver<f32>,
        width: u32,
        height: u32,
        theme: Theme,
    ) -> Result<Self, OverlayError> {
        let pixmap = Pixmap::new(width, height).ok_or(OverlayError::Pixmap(width, height))?;
        Ok(Self {
            state_rx,
            level_rx,
            width,
            height,
            pixmap,
            target_state: State::Idle,
            visible_state: State::Idle,
            spawn_started: Instant::now(),
            spawn_in: false,
            disconnected: false,
            frame: 0,
            level: 0.0,
            level_target: 0.0,
            level_velocity: 0.0,
            last_update: Instant::now(),
            theme,
        })
    }

    pub(super) fn apply_state_updates(&mut self) {
        loop {
            match self.state_rx.try_recv() {
                Ok(state) => {
                    let was_idle = self.target_state == State::Idle;
                    let now_idle = state == State::Idle;
                    self.target_state = state;

                    if !now_idle {
                        self.visible_state = state;
                    }

                    // Trigger spawn / despawn only on the boundary between
                    // idle and visible. Recording ↔ Transcribing keeps the
                    // pill steady.
                    if was_idle && !now_idle {
                        self.spawn_in = true;
                        self.spawn_started = Instant::now();
                    } else if !was_idle && now_idle {
                        self.spawn_in = false;
                        self.spawn_started = Instant::now();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        // Drain incoming audio levels — keep only the latest as the
        // spring target. The spring is stepped below using real elapsed
        // `dt`, so it doesn't matter how many samples we drained.
        loop {
            match self.level_rx.try_recv() {
                Ok(new) => self.level_target = new.clamp(0.0, 1.0),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }

        // Critically-damped-ish spring on the displayed level. Stepped
        // with wall-clock dt so the time constants are real-world ms,
        // not "per dispatch tick". `STIFFNESS` and `DAMPING` are tuned
        // to settle in ~150–200 ms with no perceptible overshoot — feels
        // like the bars *track* the voice rather than chase it.
        const STIFFNESS: f32 = 360.0;
        const DAMPING: f32 = 32.0;
        let now = Instant::now();
        let dt = now.duration_since(self.last_update).as_secs_f32().min(0.1);
        self.last_update = now;
        if dt > 0.0 {
            let force = (self.level_target - self.level) * STIFFNESS;
            let drag = self.level_velocity * DAMPING;
            self.level_velocity += (force - drag) * dt;
            self.level = (self.level + self.level_velocity * dt).clamp(0.0, 1.0);
        }

        // Once the despawn finishes, snap the renderer to Idle so the next
        // appearance starts from a clean state.
        if !self.spawn_in && self.spawn_t() >= 1.0 {
            self.visible_state = State::Idle;
        }
    }

    /// Wall-clock progress through the current spawn animation, 0..=1.
    fn spawn_t(&self) -> f32 {
        let duration = if self.spawn_in {
            SPAWN_IN_MS
        } else {
            SPAWN_OUT_MS
        };
        let elapsed_ms = self.spawn_started.elapsed().as_secs_f32() * 1000.0;
        (elapsed_ms / duration).clamp(0.0, 1.0)
    }

    /// Compute the current animated transform.
    ///
    /// **Appear** (`spawn_in == true`, 220 ms):
    /// - Pill height grows `SPAWN_PILL_MIN_H → full_height` with an
    ///   `easeOutBack` curve, so it overshoots ~3 % then settles —
    ///   gives the arrival a small "physical pop".
    /// - Pill alpha eases in over the first ~64 % of the duration (so
    ///   the pill is solid before it stops moving — Material 3
    ///   "emphasized" pattern).
    /// - Bars stay invisible for the first 80 ms, then fade in over the
    ///   next 80 ms while the pill is still finishing its grow. After
    ///   that window they unlock and react to audio.
    ///
    /// **Disappear** (`spawn_in == false`, 140 ms):
    /// - Pill height shrinks back to `SPAWN_PILL_MIN_H` with an
    ///   `easeInCubic` accelerate — sharper exit than entry.
    /// - Pill and bar alpha fade in lockstep with the height collapse.
    fn anim(&self, full_height: f32) -> AnimState {
        let t = self.spawn_t();
        if self.spawn_in {
            let h_curve = ease_out_back(t, SPAWN_OVERSHOOT_C).clamp(0.0, 1.4);
            let pill_height = SPAWN_PILL_MIN_H + h_curve * (full_height - SPAWN_PILL_MIN_H);

            // Alpha finishes well before the height — so the pill is fully
            // opaque while it's still growing. ease-out-quad over first 64%.
            let alpha_t = (t / 0.64).clamp(0.0, 1.0);
            let pill_alpha = 1.0 - (1.0 - alpha_t) * (1.0 - alpha_t);

            // Bar grace + fade. Convert ms to t-fractions of total duration.
            let grace_t = BARS_GRACE_MS / SPAWN_IN_MS;
            let fade_t = BARS_FADE_MS / SPAWN_IN_MS;
            let bar_t = ((t - grace_t) / fade_t).clamp(0.0, 1.0);
            let bar_alpha = 1.0 - (1.0 - bar_t) * (1.0 - bar_t);
            let bars_locked = t < grace_t + fade_t;

            AnimState {
                pill_height,
                pill_alpha,
                bar_alpha,
                bars_locked,
            }
        } else {
            let e = ease_in_cubic(t);
            let pill_height = full_height - e * (full_height - SPAWN_PILL_MIN_H);
            let pill_alpha = 1.0 - e;
            // Bars fade out a bit faster than the pill collapses.
            let bar_t = (t / 0.7).clamp(0.0, 1.0);
            let bar_alpha = 1.0 - bar_t * bar_t;
            AnimState {
                pill_height,
                pill_alpha,
                bar_alpha,
                bars_locked: true,
            }
        }
    }

    pub(super) fn draw_frame(&mut self) {
        self.apply_state_updates();

        let anim = self.anim(self.height as f32);
        let level_gated = if anim.bars_locked { 0.0 } else { self.level };
        draw_overlay(
            &mut self.pixmap,
            self.visible_state,
            self.frame,
            level_gated,
            anim,
            &self.theme,
        );
        self.frame = self.frame.wrapping_add(1);
    }
}

fn ease_in_cubic(t: f32) -> f32 {
    t * t * t
}

/// "Back" easing — overshoots the target by `c × peak %` before settling.
/// `c = 0.4` ⇒ ~3 % overshoot; `c = 1.7` is the standard CSS easeOutBack
/// (~10 %). Output range is approximately [0, 1+c/something], typically
/// peaking around `t ≈ 0.85`.
fn ease_out_back(t: f32, c: f32) -> f32 {
    let t1 = t - 1.0;
    1.0 + (c + 1.0) * t1 * t1 * t1 + c * t1 * t1
}

/// Render one frame of the overlay into `pixmap` using tiny-skia. The
/// pixmap is the same size as the surface; the pill is drawn at the bottom
/// of the surface (its bottom edge glued to the surface bottom) so the
/// height-morph animation reads as the pill *growing out of the screen
/// edge* instead of inflating from its center.
fn draw_overlay(
    pixmap: &mut Pixmap,
    state: State,
    frame: u32,
    level: f32,
    anim: AnimState,
    theme: &Theme,
) {
    pixmap.fill(Color::TRANSPARENT);

    if anim.pill_alpha <= 0.0 || state == State::Idle {
        return;
    }

    let surface_w = pixmap.width() as f32;
    let surface_h = pixmap.height() as f32;
    let pill_h = anim.pill_height.clamp(SPAWN_PILL_MIN_H, surface_h);
    let pill_y = surface_h - pill_h; // bottom-anchored
    let pill_w = surface_w;

    // Outer pill — drawn in the *ring* color first. We then paint a 1 px
    // inset pill in the *bg* color so the visible result is a perfectly
    // even 1 px ring with no stroke-math seams. This avoids the corner
    // join artifacts that a tiny-skia stroke produces on tight pills.
    let outer = build_stadium(0.0, pill_y, pill_w, pill_h);
    if let Some(path) = &outer {
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        paint.set_color(theme_color(theme.ring, anim.pill_alpha));
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    if pill_w > 2.0 && pill_h > 2.0 {
        let inner = build_stadium(1.0, pill_y + 1.0, pill_w - 2.0, pill_h - 2.0);
        if let Some(path) = &inner {
            let mut paint = Paint {
                anti_alias: true,
                ..Default::default()
            };
            paint.set_color(theme_color(theme.bg, anim.pill_alpha));
            pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    if anim.bar_alpha <= 0.0 {
        return;
    }

    // Bars sit at the *final* pill center — the surface midpoint — not at
    // the currently animating pill_cy. While the pill is still growing
    // upward from the bottom edge, this keeps the bars planted at one fixed
    // y so they read as "expanding amplitude" rather than "translating up
    // with the pill". The pill's bottom-anchored grow still happens
    // visually; the bars just don't follow its center-of-mass.
    let pill_cy = surface_h / 2.0;
    match state {
        State::Recording => draw_bars(pixmap, theme, theme.rec_bar, level, anim, pill_cy),
        // Read-aloud playback reuses the equalizer bars in a distinct hue,
        // driven by the spoken audio amplitude.
        State::Speaking => draw_bars(pixmap, theme, theme.speak_bar, level, anim, pill_cy),
        // Synthesizing reuses the transcribing "working on it" shimmer.
        State::Transcribing | State::Synthesizing => {
            draw_sweep(pixmap, theme, frame, anim, pill_cy)
        }
        State::Idle => {}
    }
    // NOTE: the external GNOME Shell extension (not in this repo) only renders
    // recording/transcribing today; it would need a separate update to draw
    // the synthesizing/speaking states. The native Wayland/X11 paths above
    // handle them fully.
}

/// Build a stadium / pill path: rectangle + two end-cap circles, joined by
/// the `Winding` fill rule. This is geometrically exact — no cubic-bezier
/// approximation — so the silhouette has no minor inward dents at the
/// corner joins, which were visible against colored rings.
///
/// Handles both orientations: horizontal pills (`w > h`) get end caps on
/// the left/right; vertical pills (`h > w`) get end caps on the top/bottom.
/// Square (or near-square) input collapses to a single circle. Returns
/// `None` for non-positive dimensions.
fn build_stadium(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    if w >= h {
        let r = h / 2.0;
        if (w - 2.0 * r).abs() < 0.01 {
            return PathBuilder::from_circle(x + r, y + r, r);
        }
        let mut pb = PathBuilder::new();
        if let Some(rect) = Rect::from_xywh(x + r, y, w - 2.0 * r, h) {
            pb.push_rect(rect);
        }
        if let Some(cap) = PathBuilder::from_circle(x + r, y + r, r) {
            pb.push_path(&cap);
        }
        if let Some(cap) = PathBuilder::from_circle(x + w - r, y + r, r) {
            pb.push_path(&cap);
        }
        pb.finish()
    } else {
        let r = w / 2.0;
        let mut pb = PathBuilder::new();
        if let Some(rect) = Rect::from_xywh(x, y + r, w, h - 2.0 * r) {
            pb.push_rect(rect);
        }
        if let Some(cap) = PathBuilder::from_circle(x + r, y + r, r) {
            pb.push_path(&cap);
        }
        if let Some(cap) = PathBuilder::from_circle(x + r, y + h - r, r) {
            pb.push_path(&cap);
        }
        pb.finish()
    }
}

/// Convert a `[A, R, G, B]` byte array to a tiny-skia non-premultiplied
/// `Color`, with the alpha channel further scaled by `extra_alpha`.
fn theme_color(bytes: [u8; 4], extra_alpha: f32) -> Color {
    let a = (bytes[0] as f32 / 255.0 * extra_alpha.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    Color::from_rgba(
        bytes[1] as f32 / 255.0,
        bytes[2] as f32 / 255.0,
        bytes[3] as f32 / 255.0,
        a,
    )
    .unwrap_or(Color::TRANSPARENT)
}

/// Wavy taper across the bar row — center bar at ~100 %, with a cosine
/// modulation so adjacent bars alternate between "taller" and "shorter"
/// inside a gaussian envelope. Reads as an equalizer pattern instead of
/// a smooth bell:
///
/// ```text
///   index:    0     1     2     3     4     5     6
///   factor:  .20   .64   .45  1.00   .45   .64   .20
///            short  tall  mid  PEAK  mid  tall  short
/// ```
fn taper_factor(i: u32, count: u32) -> f32 {
    if count <= 1 {
        return 1.0;
    }
    let center = (count as f32 - 1.0) / 2.0;
    let d = (i as f32 - center) / center; // -1..=1
    let envelope = (-d * d).exp(); // exp(-1) ≈ 0.367 at edges
                                   // For odd `count`, (i - center) is integer ⇒ cos is ±1, giving a
                                   // strict alternation. For even `count` the cos collapses to 0 and the
                                   // factor is just the gaussian envelope, which is fine.
    let wave = 0.75 + 0.25 * (std::f32::consts::PI * (i as f32 - center)).cos();
    envelope * wave
}

/// Recording bars: react to audio level, gaussian taper across the row,
/// soft glow halo behind each bar at higher amplitudes. The audio level is
/// the dominant driver — only ~15 % of the bar height comes from the
/// per-bar phase animation, so silence reads as actually quiet and loud
/// speech reaches near the pill edge.
fn draw_bars(
    pixmap: &mut Pixmap,
    theme: &Theme,
    bar_color: [u8; 4],
    level: f32,
    anim: AnimState,
    pill_cy: f32,
) {
    let surface_w = pixmap.width() as f32;
    // Track the *currently displayed* pill height so bars stay within the
    // pill while it's still growing during the spawn animation.
    let max_h = (anim.pill_height - BAR_VPAD * 2.0).max(BAR_BASELINE + 2.0);
    let bar_x_start = (surface_w - BAR_BLOCK_W) / 2.0;

    for i in 0..BAR_COUNT {
        let taper = taper_factor(i, BAR_COUNT);
        // Pure level-driven height. Each bar's center is anchored to
        // `pill_cy` and the bar grows symmetrically up and down. No
        // per-bar phase animation — that creates the illusion of bars
        // translating instead of expanding, which reads as "moving up
        // and down" rather than "amplitude".
        let effective = (level * taper).clamp(0.0, 1.0);
        let h = (BAR_BASELINE + effective * (max_h - BAR_BASELINE)).max(BAR_BASELINE);
        let bx = bar_x_start + i as f32 * BAR_PITCH;
        let by = pill_cy - h / 2.0;

        // Glow halo behind the bar — only visible above a small threshold.
        if effective > 0.02 {
            let glow_intensity = (effective * 0.9 + 0.1).clamp(0.0, 1.0);
            let glow_a = theme.glow[0] as f32 / 255.0 * glow_intensity * anim.bar_alpha;
            let glow_color = Color::from_rgba(
                theme.glow[1] as f32 / 255.0,
                theme.glow[2] as f32 / 255.0,
                theme.glow[3] as f32 / 255.0,
                glow_a.clamp(0.0, 1.0),
            )
            .unwrap_or(Color::TRANSPARENT);
            let glow_w = BAR_W + 2.0;
            let glow_h = (h + 2.0).max(BAR_BASELINE + 2.0);
            if let Some(path) = build_stadium(bx - 1.0, pill_cy - glow_h / 2.0, glow_w, glow_h) {
                let mut paint = Paint {
                    anti_alias: true,
                    ..Default::default()
                };
                paint.set_color(glow_color);
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        if let Some(path) = build_stadium(bx, by, BAR_W, h) {
            let mut paint = Paint {
                anti_alias: true,
                ..Default::default()
            };
            paint.set_color(theme_color(bar_color, anim.bar_alpha));
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

/// Transcribing state: no audio level, just a center-out shimmer that
/// travels across the bar row to communicate "working on it" without
/// flat staticness.
fn draw_sweep(pixmap: &mut Pixmap, theme: &Theme, frame: u32, anim: AnimState, pill_cy: f32) {
    let surface_w = pixmap.width() as f32;
    let max_h = (anim.pill_height - BAR_VPAD * 2.0).max(BAR_BASELINE + 2.0);
    let bar_x_start = (surface_w - BAR_BLOCK_W) / 2.0;

    // Sliding focus point that pings back and forth across the row.
    let cycle = (BAR_COUNT as i32) * 2 - 2;
    let pos = ((frame / 3) as i32) % cycle.max(1);
    let active = if pos < BAR_COUNT as i32 {
        pos as f32
    } else {
        (cycle - pos) as f32
    };

    for i in 0..BAR_COUNT {
        let taper = taper_factor(i, BAR_COUNT);
        let dist = (i as f32 - active).abs();
        // Bell-shaped intensity centered on `active`, ~3 bars wide.
        let intensity = (-dist * dist / 4.0).exp().max(0.15);
        let dynamic = intensity * taper;
        let h = (BAR_BASELINE + dynamic * (max_h - BAR_BASELINE) * 0.85).max(BAR_BASELINE);
        let bx = bar_x_start + i as f32 * BAR_PITCH;
        let by = pill_cy - h / 2.0;

        let bar_a = theme.trans_bar[0] as f32 / 255.0 * (0.3 + 0.7 * intensity) * anim.bar_alpha;
        let bar_color = Color::from_rgba(
            theme.trans_bar[1] as f32 / 255.0,
            theme.trans_bar[2] as f32 / 255.0,
            theme.trans_bar[3] as f32 / 255.0,
            bar_a.clamp(0.0, 1.0),
        )
        .unwrap_or(Color::TRANSPARENT);

        if let Some(path) = build_stadium(bx, by, BAR_W, h) {
            let mut paint = Paint {
                anti_alias: true,
                ..Default::default()
            };
            paint.set_color(bar_color);
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 100;
    const H: u32 = 64;

    fn fresh_pixmap() -> Pixmap {
        Pixmap::new(W, H).unwrap()
    }

    fn shown() -> AnimState {
        AnimState {
            pill_height: H as f32,
            pill_alpha: 1.0,
            bar_alpha: 1.0,
            bars_locked: false,
        }
    }
    fn hidden() -> AnimState {
        AnimState {
            pill_height: SPAWN_PILL_MIN_H,
            pill_alpha: 0.0,
            bar_alpha: 0.0,
            bars_locked: true,
        }
    }

    #[test]
    fn idle_draw_is_transparent() {
        let mut pm = fresh_pixmap();
        let t = Theme::ember();
        draw_overlay(&mut pm, State::Idle, 0, 0.0, hidden(), &t);
        assert!(pm.data().iter().all(|b| *b == 0));
    }

    #[test]
    fn faded_out_draw_is_transparent() {
        let mut pm = fresh_pixmap();
        let t = Theme::ember();
        draw_overlay(&mut pm, State::Recording, 0, 1.0, hidden(), &t);
        assert!(pm.data().iter().all(|b| *b == 0));
    }

    #[test]
    fn active_draw_has_visible_pixels() {
        let mut pm = fresh_pixmap();
        let t = Theme::ember();
        draw_overlay(&mut pm, State::Recording, 0, 1.0, shown(), &t);
        // tiny-skia stores premultiplied RGBA; alpha lives in the 4th byte.
        assert!(pm.data().chunks_exact(4).any(|px| px[3] != 0));
    }

    #[test]
    fn speaking_uses_distinct_bar_color_from_recording() {
        // Recording bars are amber (#F97316: high R, mid G, low B); speaking
        // bars are emerald (#34D399: low R, high G, mid B). Confirm the
        // speaking frame paints green-dominant bar pixels that the recording
        // frame does not.
        fn green_dominant(data: &[u8]) -> usize {
            data.chunks_exact(4)
                .filter(|px| px[1] > 150 && px[0] < 120 && px[1] > px[0])
                .count()
        }

        let t = Theme::ember();
        let mut rec = fresh_pixmap();
        let mut spk = fresh_pixmap();
        draw_overlay(&mut rec, State::Recording, 0, 1.0, shown(), &t);
        draw_overlay(&mut spk, State::Speaking, 0, 1.0, shown(), &t);
        let rec_green = green_dominant(rec.data());
        let spk_green = green_dominant(spk.data());
        assert!(
            spk_green > rec_green,
            "speaking should paint more green-dominant pixels than recording \
             (recording={rec_green}, speaking={spk_green})"
        );
    }

    #[test]
    fn synthesizing_draws_visible_pixels() {
        let mut pm = fresh_pixmap();
        let t = Theme::ember();
        draw_overlay(&mut pm, State::Synthesizing, 0, 0.0, shown(), &t);
        assert!(pm.data().chunks_exact(4).any(|px| px[3] != 0));
    }

    #[test]
    fn taper_is_strongest_in_center() {
        let center = taper_factor(BAR_COUNT / 2, BAR_COUNT);
        let edge_left = taper_factor(0, BAR_COUNT);
        let edge_right = taper_factor(BAR_COUNT - 1, BAR_COUNT);
        assert!(center > edge_left);
        assert!(center > edge_right);
        assert!(edge_left < 0.5);
        assert!(edge_right < 0.5);
    }

    #[test]
    fn ease_curves_hit_endpoints() {
        assert!((ease_in_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_in_cubic(1.0) - 1.0).abs() < 1e-6);
        assert!((ease_out_back(0.0, 0.4) - 0.0).abs() < 1e-6);
        assert!((ease_out_back(1.0, 0.4) - 1.0).abs() < 1e-6);
        // The "back" curve is supposed to peak above 1 in the middle.
        assert!(ease_out_back(0.85, 0.4) > 1.0);
    }

    #[test]
    fn silence_draws_minimal_baseline() {
        // Recording bars in the ember theme are amber (#F97316). tiny-skia
        // pixmap pixels are premultiplied RGBA in memory order [R, G, B, A];
        // count amber-dominant pixels (high R, mid G, low B) to measure
        // bar area independent of the bg pill.
        fn amber_pixels(data: &[u8]) -> usize {
            data.chunks_exact(4)
                .filter(|px| px[0] > 200 && px[1] > 70 && px[1] < 180 && px[2] < 60)
                .count()
        }

        let t = Theme::ember();
        let mut quiet = fresh_pixmap();
        let mut loud = fresh_pixmap();
        draw_overlay(&mut quiet, State::Recording, 0, 0.0, shown(), &t);
        draw_overlay(&mut loud, State::Recording, 0, 1.0, shown(), &t);
        let count_quiet = amber_pixels(quiet.data());
        let count_loud = amber_pixels(loud.data());
        assert!(
            count_loud > count_quiet,
            "loud audio should fill more bar area than silence (silence={count_quiet}, loud={count_loud})"
        );
    }
}
