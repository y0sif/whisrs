//! Bottom-screen recording overlay.
//!
//! The overlay is optional and currently implemented for wlroots layer-shell
//! compositors such as Hyprland and Sway.

#[cfg(feature = "overlay")]
mod service;

#[cfg(feature = "overlay")]
pub use service::spawn_overlay;

#[cfg(not(feature = "overlay"))]
pub async fn spawn_overlay(
    _state_rx: tokio::sync::watch::Receiver<crate::State>,
    _level_rx: tokio::sync::watch::Receiver<f32>,
    _config: crate::OverlayConfig,
) {
    tracing::warn!("overlay feature is disabled at compile time");
}
