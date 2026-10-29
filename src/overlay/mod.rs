//! On-screen overlays: the recording pill, and the agent overlay (a row
//! per active agent in a screen corner of its own).
//!
//! Both are optional and have native Wayland layer-shell and X11 backends,
//! each on its own thread and display connection. The agent overlay's
//! drawing ([`AgentScene`]) is headless and lives beside the pill's. The
//! pill's theme colors ([`palette`]) are in every build, for the TUI.

use tokio::sync::watch;

use crate::agent::display::AgentOverlayEntry;
use crate::{AgentOverlayConfig, OverlayConfig, State};

#[cfg(feature = "overlay")]
mod agent_face;
#[cfg(feature = "overlay")]
mod agent_host;
#[cfg(feature = "overlay")]
mod agent_orb;
#[cfg(feature = "overlay")]
mod agent_render;
#[cfg(feature = "overlay")]
mod agent_wayland;
#[cfg(feature = "overlay")]
mod agent_x11;
#[cfg(feature = "overlay")]
mod gnome;
#[cfg(feature = "overlay")]
mod layer;
pub mod palette;
#[cfg(feature = "overlay")]
mod render;
#[cfg(feature = "overlay")]
mod service;
#[cfg(feature = "overlay")]
mod text;
#[cfg(feature = "overlay")]
mod wayland;
#[cfg(feature = "overlay")]
mod x11;

#[cfg(feature = "overlay")]
pub use agent_render::{canvas_size, AgentScene, Pace, CANVAS_HEIGHT, CANVAS_WIDTH, ENTRANCE};
#[cfg(feature = "overlay")]
pub use service::spawn_overlays;

/// The overlays to show, and what each one reads.
pub struct OverlayFeeds {
    /// The recording pill.
    pub pill: Option<PillFeed>,
    /// The agent overlay.
    pub agents: Option<AgentsFeed>,
}

/// What the pill reads: the daemon state without agent mode, and the
/// voice level.
pub struct PillFeed {
    pub state_rx: watch::Receiver<State>,
    pub level_rx: watch::Receiver<f32>,
    pub config: OverlayConfig,
}

/// What the agent overlay reads: the board's rows, and the voice level
/// (the same channel as the pill's).
pub struct AgentsFeed {
    pub entries_rx: watch::Receiver<Vec<AgentOverlayEntry>>,
    pub level_rx: watch::Receiver<f32>,
    pub config: AgentOverlayConfig,
}

/// Whether the agent overlay is on screen: true once its host's window
/// exists, false while it starts, when none could be made (GNOME Wayland,
/// no display, a compositor without layer-shell) and after its host ended.
#[derive(Debug, Clone)]
pub struct AgentOverlayShown(watch::Receiver<bool>);

impl AgentOverlayShown {
    pub fn get(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait for the next change. `false` once nothing can change it any
    /// more (it then stays as [`Self::get`] says).
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

#[cfg(not(feature = "overlay"))]
pub fn spawn_overlays(feeds: OverlayFeeds) -> AgentOverlayShown {
    if feeds.pill.is_some() || feeds.agents.is_some() {
        tracing::warn!("overlay feature is disabled at compile time");
    }
    AgentOverlayShown(watch::channel(false).1)
}
