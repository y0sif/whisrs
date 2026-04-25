//! Wayland layer-shell overlay shown while recording or transcribing.

use std::sync::mpsc;
use std::time::Duration;

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
use tokio::sync::watch;
use tracing::{info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

use crate::State;

const WIDTH: u32 = 420;
const HEIGHT: u32 = 96;
const BOTTOM_MARGIN: i32 = 34;

/// Spawn the bottom recording overlay.
///
/// The Wayland event loop runs on a dedicated OS thread because it is a
/// blocking client loop. A small Tokio task forwards daemon state changes into
/// that thread.
pub async fn spawn_overlay(mut state_rx: watch::Receiver<State>, mut level_rx: watch::Receiver<f32>) {
    let gnome_state_rx = state_rx.clone();
    let gnome_level_rx = level_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_gnome_broadcaster(gnome_state_rx, gnome_level_rx).await {
            warn!("GNOME overlay D-Bus broadcaster unavailable: {e:#}");
        }
    });

    let (tx, rx) = mpsc::channel::<State>();
    let (level_tx, level_rx_thread) = mpsc::channel::<f32>();

    std::thread::Builder::new()
        .name("whisrs-overlay".to_string())
        .spawn(move || {
            if let Err(e) = run_overlay(rx, level_rx_thread) {
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
) -> anyhow::Result<()> {
    let conn = zbus::connection::Builder::session()?
        .serve_at("/org/whisrs/Overlay", GnomeOverlayBus)?
        .name("org.whisrs.Overlay")?
        .build()
        .await?;

    info!("GNOME overlay D-Bus broadcaster started");
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

struct GnomeOverlayBus;

#[zbus::interface(name = "org.whisrs.Overlay")]
impl GnomeOverlayBus {
    fn ping(&self) -> &'static str {
        "ok"
    }
}

fn run_overlay(state_rx: mpsc::Receiver<State>, level_rx: mpsc::Receiver<f32>) -> anyhow::Result<()> {
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
    layer.set_size(WIDTH, HEIGHT);

    // Make the transparent overlay non-interactive so it never blocks clicks.
    let input_region = compositor.wl_compositor().create_region(&qh, ());
    layer.set_input_region(Some(&input_region));
    input_region.destroy();

    layer.commit();

    let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm)?;
    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        state_rx,
        level_rx,
        exit: false,
        first_configure: true,
        width: WIDTH,
        height: HEIGHT,
        state: State::Idle,
        frame: 0,
        level: 0.0,
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
    layer: LayerSurface,
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    exit: bool,
    first_configure: bool,
    width: u32,
    height: u32,
    state: State,
    frame: u32,
    level: f32,
}

impl Overlay {
    fn apply_state_updates(&mut self) {
        while let Ok(state) = self.state_rx.try_recv() {
            self.state = state;
        }
        while let Ok(level) = self.level_rx.try_recv() {
            // Decay toward new value; take max to preserve peaks within a frame
            self.level = level.clamp(0.0, 1.0);
        }
        // Decay level each frame so silence brings bars down
        self.level = (self.level * 0.88).max(0.0);
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        self.apply_state_updates();

        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            warn!("failed to allocate overlay buffer");
            return;
        };

        draw_overlay(canvas, width, height, self.state, self.frame, self.level);
        self.frame = self.frame.wrapping_add(1);

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

        std::thread::sleep(Duration::from_millis(24));
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
        self.width = configure.new_size.0.max(WIDTH);
        self.height = configure.new_size.1.max(HEIGHT);

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

fn draw_overlay(canvas: &mut [u8], width: u32, height: u32, state: State, frame: u32, level: f32) {
    clear(canvas);

    if state == State::Idle {
        return;
    }

    let bg = [232, 18, 22, 28];
    let border = [105, 255, 255, 255];
    let accent = match state {
        State::Recording => [255, 239, 68, 68],
        State::Transcribing => [255, 245, 158, 11],
        State::Idle => [0, 0, 0, 0],
    };

    let x = 8;
    let y = 8;
    let w = width.saturating_sub(16);
    let h = height.saturating_sub(16);
    rounded_rect(canvas, width, height, x, y, w, h, 18, bg);
    rounded_stroke(canvas, width, height, x, y, w, h, 18, border);

    draw_status_dot(canvas, width, height, 48, height / 2, accent, frame);
    draw_wave(canvas, width, height, 92, height / 2, accent, frame, level);

    let label = match state {
        State::Recording => "RECORDING",
        State::Transcribing => "TRANSCRIBING",
        State::Idle => "",
    };
    draw_text(
        canvas,
        width,
        height,
        244,
        (height / 2).saturating_sub(10),
        label,
        [245, 245, 245, 245],
    );
}

fn clear(canvas: &mut [u8]) {
    canvas.fill(0);
}

#[allow(clippy::too_many_arguments)]
fn rounded_rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: [u8; 4],
) {
    for py in y..y + h {
        for px in x..x + w {
            if inside_rounded_rect(px, py, x, y, w, h, radius) {
                blend_pixel(canvas, width, height, px, py, color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rounded_stroke(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: [u8; 4],
) {
    for py in y..y + h {
        for px in x..x + w {
            if inside_rounded_rect(px, py, x, y, w, h, radius)
                && !inside_rounded_rect(
                    px,
                    py,
                    x + 1,
                    y + 1,
                    w - 2,
                    h - 2,
                    radius.saturating_sub(1),
                )
            {
                blend_pixel(canvas, width, height, px, py, color);
            }
        }
    }
}

fn inside_rounded_rect(px: u32, py: u32, x: u32, y: u32, w: u32, h: u32, radius: u32) -> bool {
    let right = x + w - 1;
    let bottom = y + h - 1;
    let cx = if px < x + radius {
        x + radius
    } else if px > right.saturating_sub(radius) {
        right.saturating_sub(radius)
    } else {
        px
    };
    let cy = if py < y + radius {
        y + radius
    } else if py > bottom.saturating_sub(radius) {
        bottom.saturating_sub(radius)
    } else {
        py
    };
    let dx = px as i32 - cx as i32;
    let dy = py as i32 - cy as i32;
    dx * dx + dy * dy <= (radius as i32) * (radius as i32)
}

fn draw_status_dot(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cx: u32,
    cy: u32,
    color: [u8; 4],
    frame: u32,
) {
    let pulse = ((frame / 4) % 18) as i32;
    circle(
        canvas,
        width,
        height,
        cx,
        cy,
        9 + pulse / 3,
        [34, color[1], color[2], color[3]],
    );
    circle(canvas, width, height, cx, cy, 7, color);
}

fn draw_wave(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    cy: u32,
    color: [u8; 4],
    frame: u32,
    level: f32,
) {
    for i in 0..10 {
        let phase = ((frame + i * 5) % 32) as f32 / 32.0;
        let animated = (phase * std::f32::consts::TAU).sin().abs();
        let variance = 0.65 + (i % 3) as f32 * 0.18;
        let effective = (level * variance).min(1.0);
        let bar_h = 6 + ((animated * 0.3 + effective * 0.7) * 26.0) as u32;
        let bx = x + i * 12;
        rounded_rect(
            canvas,
            width,
            height,
            bx,
            cy - bar_h / 2,
            6,
            bar_h,
            3,
            color,
        );
    }
}

fn circle(canvas: &mut [u8], width: u32, height: u32, cx: u32, cy: u32, r: i32, color: [u8; 4]) {
    for y in cy.saturating_sub(r as u32)..=(cy + r as u32).min(height.saturating_sub(1)) {
        for x in cx.saturating_sub(r as u32)..=(cx + r as u32).min(width.saturating_sub(1)) {
            let dx = x as i32 - cx as i32;
            let dy = y as i32 - cy as i32;
            if dx * dx + dy * dy <= r * r {
                blend_pixel(canvas, width, height, x, y, color);
            }
        }
    }
}

fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    text: &str,
    color: [u8; 4],
) {
    let mut cursor = x;
    for ch in text.chars() {
        if ch == ' ' {
            cursor += 8;
        } else {
            draw_char(canvas, width, height, cursor, y, ch, color);
            cursor += 13;
        }
    }
}

fn draw_char(canvas: &mut [u8], width: u32, height: u32, x: u32, y: u32, ch: char, color: [u8; 4]) {
    let glyph = glyph(ch);
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..5 {
            if bits & (1 << (4 - col)) != 0 {
                let px = x + col * 2;
                let py = y + row as u32 * 2;
                rect(canvas, width, height, px, py, 2, 2, color);
            }
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'G' => [
            0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        _ => [0; 7],
    }
}

#[allow(clippy::too_many_arguments)]
fn rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    for py in y..(y + h).min(height) {
        for px in x..(x + w).min(width) {
            blend_pixel(canvas, width, height, px, py, color);
        }
    }
}

fn blend_pixel(canvas: &mut [u8], width: u32, height: u32, x: u32, y: u32, color: [u8; 4]) {
    if x >= width || y >= height {
        return;
    }

    let index = ((y * width + x) * 4) as usize;
    canvas[index..index + 4].copy_from_slice(&u32::from_be_bytes(color).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_draw_is_transparent() {
        let mut canvas = vec![1; (WIDTH * HEIGHT * 4) as usize];
        draw_overlay(&mut canvas, WIDTH, HEIGHT, State::Idle, 0, 0.0);
        assert!(canvas.iter().all(|b| *b == 0));
    }

    #[test]
    fn active_draw_has_visible_pixels() {
        let mut canvas = vec![0; (WIDTH * HEIGHT * 4) as usize];
        draw_overlay(&mut canvas, WIDTH, HEIGHT, State::Recording, 0, 1.0);
        assert!(canvas.chunks_exact(4).any(|px| px[3] != 0));
    }
}
