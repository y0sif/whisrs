//! Global hotkey listener.
//!
//! On Linux: passively monitors keyboard input devices via evdev for configured
//! key combos and sends commands to the daemon when they match.
//!
//! On other platforms: not yet implemented (no-op).

#[cfg(target_os = "linux")]
mod parse;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::start_hotkey_listener;
#[cfg(target_os = "linux")]
pub use parse::{parse_hotkey, HotkeyBinding};

/// No-op hotkey listener for platforms without implementation yet.
#[cfg(not(target_os = "linux"))]
pub async fn start_hotkey_listener(
    _config: &crate::HotkeyConfig,
    _cmd_tx: tokio::sync::mpsc::Sender<crate::Command>,
) {
    tracing::warn!("global hotkeys are not yet supported on this platform");
}
