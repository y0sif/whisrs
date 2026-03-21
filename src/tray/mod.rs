//! System tray indicator.
//!
//! On Linux: uses StatusNotifierItem (KDE/freedesktop SNI protocol) via ksni.
//! On other platforms: not yet implemented (no-op).

#[cfg(all(feature = "tray", target_os = "linux"))]
mod service;

#[cfg(all(feature = "tray", target_os = "linux"))]
pub use service::spawn_tray;

#[cfg(not(all(feature = "tray", target_os = "linux")))]
pub async fn spawn_tray(_state_rx: tokio::sync::watch::Receiver<crate::State>) {
    // Tray not available — either feature disabled or unsupported platform.
}
