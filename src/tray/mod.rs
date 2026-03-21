//! System tray indicator via StatusNotifierItem (KDE/freedesktop SNI protocol).
//!
//! Shows the daemon state (idle/recording/transcribing) as a tray icon.
//! Works with any SNI-compatible tray host: waybar, swaybar, KDE Plasma,
//! GNOME (with AppIndicator extension), etc.

#[cfg(feature = "tray")]
mod service;

#[cfg(feature = "tray")]
pub use service::spawn_tray;

#[cfg(not(feature = "tray"))]
pub async fn spawn_tray(_state_rx: tokio::sync::watch::Receiver<crate::State>) {
    // Tray feature not enabled — no-op.
}
