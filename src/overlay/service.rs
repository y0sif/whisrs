//! Wayland layer-shell overlay shown while recording or transcribing.

use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
enum OverlayError {
    #[error("Wayland connection error: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland globals error: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
    #[error("smithay bind error: {0}")]
    Bind(#[from] wayland_client::globals::BindError),
    #[error("smithay shm create error: {0}")]
    Shm(#[from] smithay_client_toolkit::shm::CreatePoolError),
    #[error("Wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    #[error("D-Bus signal error: {0}")]
    DBusSignal(#[from] zbus::fdo::Error),
    #[error("tiny-skia pixmap allocation failed for {0}x{1}")]
    Pixmap(u32, u32),
}

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PremultipliedColorU8, Rect, Transform,
};
use tokio::sync::watch;
use tracing::{info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

use crate::{OverlayConfig, State};

const BOTTOM_MARGIN: i32 = 16;

// Per-frame sleep matching the draw loop. ~16 ms ≈ 60 fps for visibly
// smoother motion. Spawn animation progress is wall-clock-driven (see
// `Overlay::spawn_t`), so this only caps the redraw rate.
const FRAME_MS: f32 = 16.0;

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
struct Theme {
    bg: [u8; 4],
    ring: [u8; 4],
    rec_bar: [u8; 4],
    trans_bar: [u8; 4],
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
            glow: [50, 34, 211, 238],
        }
    }

    fn from_config(cfg: &OverlayConfig) -> Self {
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
            glow: c
                .glow
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.glow),
        }
    }
}

/// Spawn the bottom recording overlay.
///
/// The Wayland event loop runs on a dedicated OS thread because it is a
/// blocking client loop. A small Tokio task forwards daemon state changes into
/// that thread.
pub async fn spawn_overlay(
    mut state_rx: watch::Receiver<State>,
    mut level_rx: watch::Receiver<f32>,
    config: OverlayConfig,
) {
    let gnome_state_rx = state_rx.clone();
    let gnome_level_rx = level_rx.clone();
    let gnome_theme = config.theme.clone();
    tokio::spawn(async move {
        if let Err(e) = run_gnome_broadcaster(gnome_state_rx, gnome_level_rx, gnome_theme).await {
            warn!("GNOME overlay D-Bus broadcaster unavailable: {e:#}");
        }
    });

    let (tx, rx) = mpsc::channel::<State>();
    let (level_tx, level_rx_thread) = mpsc::channel::<f32>();

    let overlay_config = config;
    std::thread::Builder::new()
        .name("whisrs-overlay".to_string())
        .spawn(move || {
            if let Err(e) = run_overlay(rx, level_rx_thread, overlay_config) {
                warn!("overlay unavailable: {e:#}");
            }
        })
        .map_err(|e| warn!("failed to spawn overlay thread: {e}"))
        .ok();

    tokio::spawn(async move {
        let _ = tx.send(*state_rx.borrow());
        let _ = level_tx.send(*level_rx.borrow());
        loop {
            tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() { break; }
                    if tx.send(*state_rx.borrow()).is_err() { break; }
                }
                changed = level_rx.changed() => {
                    if changed.is_err() { break; }
                    let _ = level_tx.send(*level_rx.borrow());
                }
            }
        }
    });
}

async fn run_gnome_broadcaster(
    mut state_rx: watch::Receiver<State>,
    level_rx: watch::Receiver<f32>,
    theme: String,
) -> Result<(), OverlayError> {
    // Custom themes don't sync over D-Bus for v1 — the GNOME extension only
    // knows the named themes it ships. Fall back to "ember" so the bar
    // colors remain sensible.
    let advertised_theme = match theme.as_str() {
        "carbon" | "cyan" | "ember" => theme.clone(),
        _ => "ember".to_string(),
    };

    let conn = zbus::connection::Builder::session()?
        .serve_at("/org/whisrs/Overlay", GnomeOverlayBus)?
        .name("org.whisrs.Overlay")?
        .build()
        .await?;

    info!("GNOME overlay D-Bus broadcaster started");
    emit_gnome_theme(&conn, &advertised_theme).await?;
    let initial_state = *state_rx.borrow();
    emit_gnome_state(&conn, initial_state).await?;
    let initial_level = *level_rx.borrow();
    emit_gnome_level(&conn, initial_level).await?;

    // Emit level at a steady ~30 Hz so the GNOME extension always has fresh
    // data, regardless of how the watch channel coalesces updates.
    let mut level_interval = tokio::time::interval(Duration::from_millis(33));
    level_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = *state_rx.borrow();
                emit_gnome_state(&conn, state).await?;
            }
            _ = level_interval.tick() => {
                let level = *level_rx.borrow();
                emit_gnome_level(&conn, level).await?;
            }
        }
    }

    Ok(())
}

async fn emit_gnome_state(conn: &zbus::Connection, state: State) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "StateChanged",
        &(state.to_string()),
    )
    .await
}

async fn emit_gnome_level(conn: &zbus::Connection, level: f32) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "LevelChanged",
        &level.clamp(0.0, 1.0),
    )
    .await
}

async fn emit_gnome_theme(conn: &zbus::Connection, theme: &str) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "ThemeChanged",
        &theme,
    )
    .await
}

struct GnomeOverlayBus;

#[zbus::interface(name = "org.whisrs.Overlay")]
impl GnomeOverlayBus {
    fn ping(&self) -> &'static str {
        "ok"
    }
}

fn run_overlay(
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    config: OverlayConfig,
) -> Result<(), OverlayError> {
    let width = config.clamped_width();
    let height = config.clamped_height();
    let theme = Theme::from_config(&config);

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("whisrs"), None);
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_margin(0, 0, BOTTOM_MARGIN, 0);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);

    // Make the transparent overlay non-interactive so it never blocks clicks.
    let input_region = compositor.wl_compositor().create_region(&qh, ());
    layer.set_input_region(Some(&input_region));
    input_region.destroy();

    layer.commit();

    let pool = SlotPool::new((width * height * 4) as usize, &shm)?;
    let pixmap = Pixmap::new(width, height).ok_or(OverlayError::Pixmap(width, height))?;
    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        pixmap,
        layer,
        state_rx,
        level_rx,
        exit: false,
        first_configure: true,
        width,
        height,
        target_state: State::Idle,
        visible_state: State::Idle,
        spawn_started: Instant::now(),
        spawn_in: false,
        frame: 0,
        level: 0.0,
        level_target: 0.0,
        level_velocity: 0.0,
        last_update: Instant::now(),
        theme,
    };

    info!("recording overlay started");
    while !overlay.exit {
        overlay.apply_state_updates();
        event_queue.blocking_dispatch(&mut overlay)?;
    }

    Ok(())
}

struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    pixmap: Pixmap,
    layer: LayerSurface,
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    exit: bool,
    first_configure: bool,
    width: u32,
    height: u32,
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

impl Overlay {
    fn apply_state_updates(&mut self) {
        while let Ok(state) = self.state_rx.try_recv() {
            let was_idle = self.target_state == State::Idle;
            let now_idle = state == State::Idle;
            self.target_state = state;

            if !now_idle {
                self.visible_state = state;
            }

            // Trigger spawn / despawn only on the boundary between idle and
            // visible. Recording ↔ Transcribing keeps the pill steady.
            if was_idle && !now_idle {
                self.spawn_in = true;
                self.spawn_started = Instant::now();
            } else if !was_idle && now_idle {
                self.spawn_in = false;
                self.spawn_started = Instant::now();
            }
        }
        // Drain incoming audio levels — keep only the latest as the
        // spring target. The spring is stepped below using real elapsed
        // `dt`, so it doesn't matter how many samples we drained.
        while let Ok(new) = self.level_rx.try_recv() {
            self.level_target = new.clamp(0.0, 1.0);
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

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        self.apply_state_updates();

        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

        // Render into our owned pixmap first so the buffer borrow on the
        // shm pool doesn't conflict with the pixmap borrow.
        let anim = self.anim(height as f32);
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

        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            warn!("failed to allocate overlay buffer");
            return;
        };
        copy_pixmap_to_argb8888(&self.pixmap, canvas);

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
        if let Err(e) = buffer.attach_to(self.layer.wl_surface()) {
            warn!("failed to attach overlay buffer: {e}");
            return;
        }
        self.layer.commit();

        std::thread::sleep(Duration::from_millis(FRAME_MS as u64));
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

/// tiny-skia stores premultiplied RGBA bytes; the wl_shm Argb8888 format on
/// little-endian systems is BGRA in memory. Convert by swapping R and B.
/// Both formats use premultiplied alpha so no math is needed beyond the swap.
fn copy_pixmap_to_argb8888(pixmap: &Pixmap, canvas: &mut [u8]) {
    let src = pixmap.pixels();
    debug_assert_eq!(src.len() * 4, canvas.len());
    for (i, px) in src.iter().enumerate() {
        let dst = &mut canvas[i * 4..i * 4 + 4];
        let pre: PremultipliedColorU8 = *px;
        dst[0] = pre.blue();
        dst[1] = pre.green();
        dst[2] = pre.red();
        dst[3] = pre.alpha();
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.width = configure.new_size.0.max(self.width);
        self.height = configure.new_size.1.max(self.height);

        if self.first_configure {
            self.first_configure = false;
            self.draw(qh);
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<wl_region::WlRegion, ()> for Overlay {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_layer!(Overlay);
delegate_registry!(Overlay);

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
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
        State::Recording => draw_bars(pixmap, theme, level, anim, pill_cy),
        State::Transcribing => draw_sweep(pixmap, theme, frame, anim, pill_cy),
        State::Idle => {}
    }
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
fn draw_bars(pixmap: &mut Pixmap, theme: &Theme, level: f32, anim: AnimState, pill_cy: f32) {
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
            paint.set_color(theme_color(theme.rec_bar, anim.bar_alpha));
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
