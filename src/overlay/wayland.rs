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
        wlr_layer::{Anchor, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tracing::{debug, info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection as WaylandConnection, Dispatch, QueueHandle,
};

use crate::{OverlayConfig, OverlayHAlign, OverlayPosition, State};

use super::layer::{
    copy_pixmap_to_argb8888, create_layer, layer_action, LayerAction, LayerEvent, LayerStatus,
};
use super::palette::Palette;
use super::render::{OverlayRenderer, FRAME_MS};
use super::service::OverlayError;

pub(super) fn run_overlay(
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    config: OverlayConfig,
) -> Result<(), OverlayError> {
    let width = config.clamped_width();
    let height = config.clamped_height();
    let theme = Palette::from_config(&config);
    let position = config.position();

    let conn = WaylandConnection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let layer = create_layer(
        &compositor,
        &layer_shell,
        &qh,
        pill_anchor(position),
        (width, height),
        "whisrs",
    );

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

/// The pill's anchor: anchoring to one edge centers along it; adding a side
/// pins a corner.
fn pill_anchor(position: OverlayPosition) -> Anchor {
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
    vertical | horizontal
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
            pill_anchor(self.position),
            (self.renderer.width, self.renderer.height),
            "whisrs",
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

    #[test]
    fn pill_anchor_pins_an_edge_or_a_corner() {
        assert_eq!(pill_anchor(OverlayPosition::BottomCenter), Anchor::BOTTOM);
        assert_eq!(pill_anchor(OverlayPosition::TopCenter), Anchor::TOP);
        assert_eq!(
            pill_anchor(OverlayPosition::BottomRight),
            Anchor::BOTTOM | Anchor::RIGHT
        );
        assert_eq!(
            pill_anchor(OverlayPosition::TopLeft),
            Anchor::TOP | Anchor::LEFT
        );
    }
}
