//! Wayland layer-shell overlay shown while recording or transcribing.

use std::sync::mpsc;
use std::time::Duration;

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
    Color, FillRule, Paint, PathBuilder, Pixmap, PremultipliedColorU8, Rect, Stroke, Transform,
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

// Per-frame sleep matching the draw loop. ~24 ms ≈ 41 fps. The spawn
// animation timings below are expressed in milliseconds and converted to
// per-frame steps using this constant.
const FRAME_MS: f32 = 24.0;

// Spawn animation durations (ms). Slightly faster going away than coming in
// — the asymmetry reads as "intentional dismiss" rather than a glitch.
const SPAWN_IN_MS: f32 = 180.0;
const SPAWN_OUT_MS: f32 = 140.0;

// On appear, hold the bars at their baseline for this many milliseconds so
// the audio reactivity doesn't fire while the pill is still flying in.
const BARS_GRACE_MS: f32 = 80.0;

// Slide-up offset (px) and scale start used by the spawn animation.
const SPAWN_SLIDE_PX: f32 = 8.0;
const SPAWN_SCALE_FROM: f32 = 0.92;

// Bar layout — fixed for visual consistency.
// 5 bars × 6 px + 4 gaps × 4 px = 46 px wide, centered in the pill.
const BAR_COUNT: u32 = 5;
const BAR_W: f32 = 6.0;
const BAR_GAP: f32 = 4.0;
const BAR_PITCH: f32 = BAR_W + BAR_GAP;
const BAR_BLOCK_W: f32 = BAR_COUNT as f32 * BAR_W + (BAR_COUNT - 1) as f32 * BAR_GAP;
const BAR_BASELINE: f32 = 3.0;

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
        spawn_t: 0.0,
        spawn_in: false,
        bars_grace_ms: 0.0,
        frame: 0,
        level: 0.0,
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
    /// Progress through the current spawn animation, 0..=1. Hits 1 when the
    /// animation finishes; reset to 0 each time `target_state` flips between
    /// idle and active.
    spawn_t: f32,
    /// `true` while transitioning into a visible state, `false` while
    /// transitioning out. Determines easing direction and duration.
    spawn_in: bool,
    /// Remaining grace period (ms) before bars unlock from baseline. Set
    /// when the pill is appearing so audio reactivity doesn't fire while
    /// the pill is still flying in.
    bars_grace_ms: f32,
    frame: u32,
    level: f32,
    theme: Theme,
}

#[derive(Debug, Clone, Copy)]
struct AnimState {
    /// 0..=1
    alpha: f32,
    /// Vertical offset in px. Positive = drawn lower than rest position.
    slide_y: f32,
    /// Linear scale factor centered on the pill.
    scale: f32,
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
                self.spawn_t = 0.0;
                self.bars_grace_ms = BARS_GRACE_MS;
            } else if !was_idle && now_idle {
                self.spawn_in = false;
                self.spawn_t = 0.0;
            }
        }
        while let Ok(level) = self.level_rx.try_recv() {
            self.level = level.clamp(0.0, 1.0);
        }
        self.level = (self.level * 0.85).max(0.0);

        // Advance the spawn animation. `spawn_t` saturates at 1.0; at that
        // point the pill is fully shown (`spawn_in = true`) or fully hidden
        // (`spawn_in = false`).
        let duration = if self.spawn_in {
            SPAWN_IN_MS
        } else {
            SPAWN_OUT_MS
        };
        if self.spawn_t < 1.0 {
            self.spawn_t = (self.spawn_t + FRAME_MS / duration).min(1.0);
        }

        if !self.spawn_in && self.spawn_t >= 1.0 {
            self.visible_state = State::Idle;
        }

        if self.bars_grace_ms > 0.0 {
            self.bars_grace_ms = (self.bars_grace_ms - FRAME_MS).max(0.0);
        }
    }

    /// Compute the current animated transform: alpha for the cross-fade,
    /// slide_y in px, and the centered scale factor. Uses `ease_out_cubic`
    /// for the appear path so the pill decelerates into place, and
    /// `ease_in_cubic` on the way out for a faster, more committed dismiss.
    fn anim(&self) -> AnimState {
        let t = self.spawn_t.clamp(0.0, 1.0);
        if self.spawn_in {
            let e = ease_out_cubic(t);
            AnimState {
                alpha: e,
                slide_y: (1.0 - e) * SPAWN_SLIDE_PX,
                scale: SPAWN_SCALE_FROM + e * (1.0 - SPAWN_SCALE_FROM),
            }
        } else {
            let e = ease_in_cubic(t);
            AnimState {
                alpha: 1.0 - e,
                slide_y: e * SPAWN_SLIDE_PX,
                scale: 1.0 - e * 0.04,
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
        let anim = self.anim();
        // Audio reactivity is gated for the first few frames after
        // appearing, so the bars don't react to speech while the pill is
        // still flying in.
        let level_gated = if self.bars_grace_ms > 0.0 {
            0.0
        } else {
            self.level
        };
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

fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

fn ease_in_cubic(t: f32) -> f32 {
    t * t * t
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

/// Render one frame of the overlay into `pixmap` using tiny-skia. The pixmap
/// is the same size as the surface; coordinates are in pixels.
///
/// The animation transform (`anim`) controls a slide-up + scale + fade
/// applied uniformly to the pill — implemented as a tiny-skia `Transform`
/// so anti-aliasing handles the sub-pixel motion without us having to round.
fn draw_overlay(
    pixmap: &mut Pixmap,
    state: State,
    frame: u32,
    level: f32,
    anim: AnimState,
    theme: &Theme,
) {
    pixmap.fill(Color::TRANSPARENT);

    if anim.alpha <= 0.0 || state == State::Idle {
        return;
    }

    let width = pixmap.width() as f32;
    let height = pixmap.height() as f32;

    // Translate-then-scale around the pill center so the spawn animation
    // expands from the center, then translate again to apply the slide-up.
    let cx = width / 2.0;
    let cy = height / 2.0;
    let transform = Transform::from_translate(cx, cy)
        .pre_scale(anim.scale, anim.scale)
        .post_translate(-cx, -cy)
        .post_translate(0.0, anim.slide_y);

    // Pill background.
    let radius = height / 2.0;
    let pill_path = build_round_rect(0.0, 0.0, width, height, radius);
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color(theme_color(theme.bg, anim.alpha));
    if let Some(path) = &pill_path {
        pixmap.fill_path(path, &paint, FillRule::Winding, transform, None);
    }

    // Hairline ring — drawn as a 1 px stroked pill.
    let stroke = Stroke {
        width: 1.0,
        ..Default::default()
    };
    paint.set_color(theme_color(theme.ring, anim.alpha));
    if let Some(path) = &pill_path {
        pixmap.stroke_path(path, &paint, &stroke, transform, None);
    }

    match state {
        State::Recording => draw_bars(pixmap, theme, frame, level, anim, transform),
        State::Transcribing => draw_sweep(pixmap, theme, frame, anim, transform),
        State::Idle => {}
    }
}

/// Build a centered rounded-rect path. tiny-skia doesn't ship a rounded-rect
/// helper, so we approximate the corner arcs with cubic Bezier curves using
/// the standard 0.5523 control-point offset.
fn build_round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.0 {
        return PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?).into();
    }
    // Standard "kappa" cubic-bezier circle approximation: 4·(√2 − 1)/3.
    let k = 0.552_284_8_f32 * r;

    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
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

/// Gaussian taper across the bar row — center bars draw at ~100 % of their
/// dynamic height, edges fall off to ~37 %. `i` is the bar index, `count`
/// the total bar count.
fn taper_factor(i: u32, count: u32) -> f32 {
    if count <= 1 {
        return 1.0;
    }
    let center = (count as f32 - 1.0) / 2.0;
    let d = (i as f32 - center) / center; // -1..=1
    (-d * d).exp() // exp(-1) ≈ 0.367 at edges
}

/// Vertical padding inside the pill (top + bottom). Bars never reach the
/// pill edge.
const BAR_VPAD: f32 = 5.0;

/// Recording bars: react to audio level, gaussian taper across the row,
/// soft glow halo behind each bar at higher amplitudes.
fn draw_bars(
    pixmap: &mut Pixmap,
    theme: &Theme,
    frame: u32,
    level: f32,
    anim: AnimState,
    transform: Transform,
) {
    let width = pixmap.width() as f32;
    let height = pixmap.height() as f32;
    let cy = height / 2.0;
    let max_h = (height - BAR_VPAD * 2.0).max(BAR_BASELINE + 2.0);
    let bar_x_start = (width - BAR_BLOCK_W) / 2.0;

    for i in 0..BAR_COUNT {
        let taper = taper_factor(i, BAR_COUNT);
        // Per-bar phase keeps movement organic instead of marching in lockstep.
        let phase = ((frame as f32 / 5.0) + i as f32 * 0.7).sin().abs();
        let effective = (level * taper).clamp(0.0, 1.0);
        let dynamic = effective * (0.7 + 0.3 * phase);
        let h = (BAR_BASELINE + dynamic * (max_h - BAR_BASELINE)).max(BAR_BASELINE);
        let bx = bar_x_start + i as f32 * BAR_PITCH;
        let by = cy - h / 2.0;

        // Glow halo behind the bar — only visible above a small threshold.
        if effective > 0.02 {
            let glow_intensity = (effective * 0.9 + 0.1).clamp(0.0, 1.0);
            let glow_a = theme.glow[0] as f32 / 255.0 * glow_intensity * anim.alpha;
            let glow_color = Color::from_rgba(
                theme.glow[1] as f32 / 255.0,
                theme.glow[2] as f32 / 255.0,
                theme.glow[3] as f32 / 255.0,
                glow_a.clamp(0.0, 1.0),
            )
            .unwrap_or(Color::TRANSPARENT);
            let glow_w = BAR_W + 2.0;
            let glow_h = (h + 2.0).max(BAR_BASELINE + 2.0);
            if let Some(path) =
                build_round_rect(bx - 1.0, cy - glow_h / 2.0, glow_w, glow_h, glow_w / 2.0)
            {
                let mut paint = Paint {
                    anti_alias: true,
                    ..Default::default()
                };
                paint.set_color(glow_color);
                pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
            }
        }

        if let Some(path) = build_round_rect(bx, by, BAR_W, h, BAR_W / 2.0) {
            let mut paint = Paint {
                anti_alias: true,
                ..Default::default()
            };
            paint.set_color(theme_color(theme.rec_bar, anim.alpha));
            pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
        }
    }
}

/// Transcribing state: no audio level, just a center-out shimmer that
/// travels across the bar row to communicate "working on it" without
/// flat staticness.
fn draw_sweep(
    pixmap: &mut Pixmap,
    theme: &Theme,
    frame: u32,
    anim: AnimState,
    transform: Transform,
) {
    let width = pixmap.width() as f32;
    let height = pixmap.height() as f32;
    let cy = height / 2.0;
    let max_h = (height - BAR_VPAD * 2.0).max(BAR_BASELINE + 2.0);
    let bar_x_start = (width - BAR_BLOCK_W) / 2.0;

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
        let by = cy - h / 2.0;

        let bar_a = theme.trans_bar[0] as f32 / 255.0 * (0.3 + 0.7 * intensity) * anim.alpha;
        let bar_color = Color::from_rgba(
            theme.trans_bar[1] as f32 / 255.0,
            theme.trans_bar[2] as f32 / 255.0,
            theme.trans_bar[3] as f32 / 255.0,
            bar_a.clamp(0.0, 1.0),
        )
        .unwrap_or(Color::TRANSPARENT);

        if let Some(path) = build_round_rect(bx, by, BAR_W, h, BAR_W / 2.0) {
            let mut paint = Paint {
                anti_alias: true,
                ..Default::default()
            };
            paint.set_color(bar_color);
            pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 100;
    const H: u32 = 34;

    fn fresh_pixmap() -> Pixmap {
        Pixmap::new(W, H).unwrap()
    }

    fn shown() -> AnimState {
        AnimState {
            alpha: 1.0,
            slide_y: 0.0,
            scale: 1.0,
        }
    }
    fn hidden() -> AnimState {
        AnimState {
            alpha: 0.0,
            slide_y: 0.0,
            scale: 1.0,
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
        assert!((ease_out_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
        assert!((ease_in_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_in_cubic(1.0) - 1.0).abs() < 1e-6);
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
