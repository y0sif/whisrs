//! Wayland layer-shell overlay backend.

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
use tiny_skia::{Pixmap, PremultipliedColorU8};
use tracing::{debug, info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection as WaylandConnection, Dispatch, QueueHandle,
};

use crate::{OverlayConfig, OverlayHAlign, OverlayPosition, State};

use super::render::{OverlayRenderer, Theme, EDGE_MARGIN, FRAME_MS};
use super::service::OverlayError;

pub(super) fn run_overlay(
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    config: OverlayConfig,
) -> Result<(), OverlayError> {
    let width = config.clamped_width();
    let height = config.clamped_height();
    let theme = Theme::from_config(&config);
    let position = config.position();

    let conn = WaylandConnection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let layer = create_layer(&compositor, &layer_shell, &qh, position, width, height);

    let output_state = OutputState::new(&globals, &qh);
    let startup_outputs = output_state.outputs().collect();
    let pool = SlotPool::new((width * height * 4) as usize, &shm)?;
    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state,
        startup_outputs,
        compositor,
        layer_shell,
        shm,
        pool,
        layer,
        position,
        renderer: OverlayRenderer::new(
            state_rx,
            level_rx,
            width,
            height,
            theme,
            position.is_top(),
        )?,
        first_configure: true,
        layer_closed: false,
        outputs_changed: false,
    };

    info!("recording overlay started");
    loop {
        overlay.renderer.apply_state_updates();
        if overlay.renderer.disconnected {
            break;
        }
        // Take the flag every pass so it never goes stale.
        if overlay.renderer.take_session_started() {
            overlay.on_layer_event(&qh, LayerEvent::SessionStart, None);
        }
        event_queue.blocking_dispatch(&mut overlay)?;
    }

    Ok(())
}

/// Create the overlay's layer surface. Output `None` lets the compositor
/// place it, normally on the focused output. Layer-shell cannot move a
/// surface to another output, so following focus means a new surface.
fn create_layer(
    compositor: &CompositorState,
    layer_shell: &LayerShell,
    qh: &QueueHandle<Overlay>,
    position: OverlayPosition,
    width: u32,
    height: u32,
) -> LayerSurface {
    let surface = compositor.create_surface(qh);
    let layer = layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some("whisrs"), None);
    // Anchoring to one edge centers along it; adding a side pins a corner.
    let vertical = if position.is_top() {
        Anchor::TOP
    } else {
        Anchor::BOTTOM
    };
    let horizontal = match position.h_align() {
        OverlayHAlign::Left => Anchor::LEFT,
        OverlayHAlign::Center => Anchor::empty(),
        OverlayHAlign::Right => Anchor::RIGHT,
    };
    layer.set_anchor(vertical | horizontal);
    // Margins on unanchored edges are ignored, so set all four.
    layer.set_margin(EDGE_MARGIN, EDGE_MARGIN, EDGE_MARGIN, EDGE_MARGIN);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);

    // Make the transparent overlay non-interactive so it never blocks clicks.
    let input_region = compositor.wl_compositor().create_region(qh, ());
    layer.set_input_region(Some(&input_region));
    input_region.destroy();

    layer.commit();
    layer
}

/// Where the current layer surface stands, as far as recovery cares.
#[derive(Clone, Copy, Debug)]
struct LayerStatus {
    /// The compositor closed it.
    closed: bool,
    /// Its first configure arrived.
    configured: bool,
    /// An output appeared or went away since it was created.
    outputs_changed: bool,
}

/// An event that may call for a new layer surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerEvent {
    /// The compositor closed the current layer.
    Closed,
    /// An output appeared.
    OutputAdded,
    /// An output is going away.
    OutputRemoved,
    /// A recording session started.
    SessionStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerAction {
    /// Replace the current layer with a fresh one.
    Recreate,
    /// Keep the current layer: it is fine, or recovery waits for an output
    /// change.
    Keep,
}

/// Decide whether `event` should replace the current layer.
/// `live_outputs` leaves out an output that is going away.
fn layer_action(status: LayerStatus, event: LayerEvent, live_outputs: usize) -> LayerAction {
    // Nothing can hold a new surface. The next output to appear retries.
    if live_outputs == 0 {
        return LayerAction::Keep;
    }
    let recreate = match event {
        // A close before the first configure means the compositor placed the
        // surface on an output it is tearing down. Retrying at once could
        // land there again, so wait for an output to come or go, unless one
        // already did since this surface was created.
        LayerEvent::Closed => status.configured || status.outputs_changed,
        // Output changes are finite, so retrying on them cannot loop. Retry a
        // layer that was never configured as well as a closed one: Hyprland
        // sends neither configure nor closed to a surface it created while
        // no monitor was left.
        LayerEvent::OutputAdded => status.closed || !status.configured,
        // Only a closed layer. Sway sends `closed` and the output's removal
        // in one batch, so a layer not configured yet is likely the
        // replacement `closed` just made. If that one is closed before its
        // first configure, `outputs_changed` lets the `Closed` arm retry.
        LayerEvent::OutputRemoved => status.closed,
        // With several outputs, a new surface follows focus. With one there
        // is nothing to follow, and a fresh map would replay the compositor's
        // layer open animation on every session. A dead layer is no reason
        // either: it draws no frames, so nothing wakes the loop when a
        // session starts. It waits for the next output change.
        LayerEvent::SessionStart => live_outputs > 1,
    };
    if recreate {
        LayerAction::Recreate
    } else {
        LayerAction::Keep
    }
}

struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    /// Outputs bound at startup that sctk has not announced yet. sctk calls
    /// `new_output` for them too, but they are no change to the output set:
    /// counting them would recreate a startup layer still waiting on its
    /// first configure.
    startup_outputs: Vec<wl_output::WlOutput>,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    position: OverlayPosition,
    renderer: OverlayRenderer,
    first_configure: bool,
    /// The compositor closed the current layer and no replacement exists yet.
    layer_closed: bool,
    /// An output appeared or went away since the current layer was created.
    outputs_changed: bool,
}

impl Overlay {
    /// Swap in a fresh layer surface, at the start of a session on a
    /// multi-output setup so the pill opens on the output focused now
    /// (issue #198), or after the
    /// compositor closed the old one. Dropping the old `LayerSurface`
    /// destroys its role object and its `wl_surface`.
    fn recreate_layer(&mut self, qh: &QueueHandle<Self>) {
        self.layer = create_layer(
            &self.compositor,
            &self.layer_shell,
            qh,
            self.position,
            self.renderer.width,
            self.renderer.height,
        );
        // No buffer may be attached before the new surface's first
        // configure. That configure draws and restarts the frame callbacks.
        self.first_configure = true;
        self.layer_closed = false;
        self.outputs_changed = false;
    }

    /// Outputs still advertised, minus `leaving`: sctk calls
    /// `output_destroyed` before it drops that output from `outputs()`.
    fn live_outputs(&self, leaving: Option<&wl_output::WlOutput>) -> usize {
        self.output_state
            .outputs()
            .filter(|output| Some(output) != leaving)
            .count()
    }

    fn layer_status(&self) -> LayerStatus {
        LayerStatus {
            closed: self.layer_closed,
            configured: !self.first_configure,
            outputs_changed: self.outputs_changed,
        }
    }

    /// Replace the current layer if [`layer_action`] says `event` calls for
    /// it, and return the decision.
    fn on_layer_event(
        &mut self,
        qh: &QueueHandle<Self>,
        event: LayerEvent,
        leaving: Option<&wl_output::WlOutput>,
    ) -> LayerAction {
        let action = layer_action(self.layer_status(), event, self.live_outputs(leaving));
        if action == LayerAction::Recreate {
            // A healthy layer replaced at session start to follow focus is
            // routine, so it goes unlogged.
            if self.layer_closed {
                info!("recreating the overlay surface the compositor closed");
            } else if self.first_configure {
                info!("recreating the unconfigured overlay surface");
            }
            self.recreate_layer(qh);
        }
        action
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        // A closed layer takes no more buffers. The chain resumes on the
        // replacement's first configure.
        if self.layer_closed {
            return;
        }
        let width = self.renderer.width;
        let height = self.renderer.height;
        let stride = width as i32 * 4;

        self.renderer.draw_frame();

        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            warn!("failed to allocate overlay buffer");
            return;
        };
        copy_pixmap_to_argb8888(&self.renderer.pixmap, canvas);

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

        std::thread::sleep(Duration::from_millis(FRAME_MS));
    }
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
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // A callback left over from a replaced surface must not draw: the
        // current surface may not be configured yet.
        if surface != self.layer.wl_surface() {
            return;
        }
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let name = self.output_state.info(output).and_then(|info| info.name);
        debug!(
            "overlay surface on output {}",
            name.as_deref().unwrap_or("unknown")
        );
    }

    fn surface_leave(
        &mut self,
        _conn: &WaylandConnection,
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
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(i) = self.startup_outputs.iter().position(|o| *o == output) {
            self.startup_outputs.swap_remove(i);
            return;
        }
        self.outputs_changed = true;
        self.on_layer_event(qh, LayerEvent::OutputAdded, None);
    }

    fn update_output(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.outputs_changed = true;
        self.on_layer_event(qh, LayerEvent::OutputRemoved, Some(&output));
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &WaylandConnection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // Ignore a close for a surface already replaced.
        if *layer != self.layer {
            return;
        }
        // The compositor closes the surface when its output goes away, such
        // as on undock. Exiting here would end the overlay for good.
        self.layer_closed = true;
        if self.on_layer_event(qh, LayerEvent::Closed, None) == LayerAction::Keep {
            info!("overlay surface closed by the compositor, waiting for an output change");
        }
    }

    fn configure(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Ignore a configure for a surface already replaced.
        if *layer != self.layer {
            return;
        }
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
        _conn: &WaylandConnection,
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

#[cfg(test)]
mod tests {
    use super::*;
    use LayerAction::{Keep, Recreate};
    use LayerEvent::{Closed, OutputAdded, OutputRemoved, SessionStart};

    /// Configured and showing.
    const OPEN: LayerStatus = LayerStatus {
        closed: false,
        configured: true,
        outputs_changed: false,
    };
    /// Configured, then closed by the compositor.
    const CLOSED: LayerStatus = LayerStatus {
        closed: true,
        ..OPEN
    };
    /// Closed before its first configure.
    const CLOSED_UNCONFIGURED: LayerStatus = LayerStatus {
        configured: false,
        ..CLOSED
    };
    /// Neither configured nor closed.
    const UNCONFIGURED: LayerStatus = LayerStatus {
        configured: false,
        ..OPEN
    };

    /// The output handlers mark the change before they decide.
    fn changed(status: LayerStatus) -> LayerStatus {
        LayerStatus {
            outputs_changed: true,
            ..status
        }
    }

    #[test]
    fn closed_after_configure_recreates_while_an_output_remains() {
        assert_eq!(layer_action(CLOSED, Closed, 1), Recreate);
    }

    #[test]
    fn closed_with_no_outputs_waits() {
        assert_eq!(layer_action(CLOSED, Closed, 0), Keep);
    }

    #[test]
    fn closed_before_configure_waits_for_an_output_change() {
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, Closed, 1), Keep);
    }

    #[test]
    fn closed_before_configure_after_an_output_change_recreates() {
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), Closed, 1),
            Recreate
        );
        assert_eq!(layer_action(changed(CLOSED_UNCONFIGURED), Closed, 0), Keep);
    }

    #[test]
    fn output_added_revives_a_closed_layer() {
        assert_eq!(layer_action(changed(CLOSED), OutputAdded, 1), Recreate);
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn output_added_retries_a_layer_never_configured() {
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn output_removed_with_outputs_left_retries_a_closed_layer() {
        assert_eq!(layer_action(changed(CLOSED), OutputRemoved, 1), Recreate);
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), OutputRemoved, 1),
            Recreate
        );
    }

    #[test]
    fn output_removed_keeps_a_fresh_unconfigured_replacement() {
        for live in [1, 2] {
            assert_eq!(
                layer_action(changed(UNCONFIGURED), OutputRemoved, live),
                Keep
            );
        }
    }

    #[test]
    fn output_removed_leaving_no_outputs_waits() {
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED, OPEN] {
            assert_eq!(layer_action(changed(status), OutputRemoved, 0), Keep);
        }
    }

    #[test]
    fn output_change_leaves_an_open_layer_alone() {
        for event in [OutputAdded, OutputRemoved] {
            for live in [1, 2] {
                assert_eq!(layer_action(changed(OPEN), event, live), Keep);
            }
        }
    }

    #[test]
    fn hyprland_unplug_of_the_only_monitor_recovers_on_replug() {
        // `closed` lands before `global_remove`, so the leaving monitor still
        // counts and a replacement goes up at once.
        assert_eq!(layer_action(CLOSED, Closed, 1), Recreate);
        // Hyprland never configures or closes that replacement. Then the
        // monitor's global goes away.
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 0), Keep);
        // Replugging retries it.
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn sway_unplug_with_a_monitor_left_recreates_once() {
        // `closed` and `global_remove` arrive in one batch. At `closed` the
        // leaving output still counts, and a replacement goes up.
        assert_eq!(layer_action(CLOSED, Closed, 2), Recreate);
        // The removal then finds that replacement not configured yet.
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 1), Keep);
    }

    #[test]
    fn replacement_closed_before_configure_after_an_output_removal_recreates() {
        assert_eq!(layer_action(CLOSED, Closed, 2), Recreate);
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 1), Keep);
        // The removal came after the replacement was made, so its early close
        // is not the one it was waiting for.
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), Closed, 1),
            Recreate
        );
    }

    #[test]
    fn session_start_keeps_a_healthy_layer_on_one_output() {
        assert_eq!(layer_action(OPEN, SessionStart, 1), Keep);
    }

    #[test]
    fn session_start_with_several_outputs_follows_focus() {
        for live in [2, 3] {
            assert_eq!(layer_action(OPEN, SessionStart, live), Recreate);
        }
    }

    #[test]
    fn session_start_leaves_a_dead_layer_on_one_output_alone() {
        // A dead layer draws no frames, so in practice no session start
        // reaches it. It waits for an output change instead.
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED] {
            assert_eq!(layer_action(status, SessionStart, 1), Keep);
        }
    }

    #[test]
    fn session_start_after_a_replug_keeps_the_fresh_layer() {
        // A session that started while no monitor was left is still flagged
        // when a monitor comes back. The replug already made a fresh layer,
        // so the flag must not replace it again.
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
        assert_eq!(layer_action(UNCONFIGURED, SessionStart, 1), Keep);
    }

    #[test]
    fn session_start_with_no_outputs_waits() {
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED, OPEN] {
            assert_eq!(layer_action(status, SessionStart, 0), Keep);
        }
    }

    #[test]
    fn removal_before_close_then_early_close_recovers_on_the_next_output_change() {
        // `global_remove` lands first and leaves the open layer alone.
        assert_eq!(layer_action(changed(OPEN), OutputRemoved, 1), Keep);
        // Then the old layer is closed, and a replacement goes up.
        assert_eq!(layer_action(changed(CLOSED), Closed, 1), Recreate);
        // The compositor closes the replacement before its first configure.
        // The output change it would wait for already happened, so it stays
        // down until the next one. A known gap: no compositor is known to
        // order events this way.
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, Closed, 1), Keep);
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, SessionStart, 1), Keep);
        for event in [OutputAdded, OutputRemoved] {
            assert_eq!(
                layer_action(changed(CLOSED_UNCONFIGURED), event, 1),
                Recreate
            );
        }
    }
}
