//! Window tracking: detect focused window and restore focus.
//!
//! Auto-detects the compositor at runtime and provides the appropriate backend.

pub mod dbus;
pub mod hyprland;
pub mod sway;
pub mod x11;

use tracing::{info, warn};

/// Trait for tracking and restoring window focus.
pub trait WindowTracker: Send + Sync {
    /// Get the identifier of the currently focused window.
    fn get_focused_window(&self) -> anyhow::Result<String>;

    /// Focus the window with the given identifier.
    fn focus_window(&self, id: &str) -> anyhow::Result<()>;
}

/// A no-op tracker that always succeeds without doing anything.
///
/// Used as a graceful fallback when compositor detection fails.
pub struct NoopTracker;

impl WindowTracker for NoopTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        Ok("noop".to_string())
    }

    fn focus_window(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Auto-detect the compositor and return the appropriate `WindowTracker`.
///
/// Detection order:
/// 1. `$HYPRLAND_INSTANCE_SIGNATURE` → Hyprland
/// 2. `$SWAYSOCK` → Sway
/// 3. `$XDG_SESSION_TYPE == x11` → X11
/// 4. Fallback → NoopTracker
pub fn detect_tracker() -> Box<dyn WindowTracker> {
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        info!("detected Hyprland compositor for window tracking");
        return Box::new(hyprland::HyprlandTracker::new());
    }

    if std::env::var("SWAYSOCK").is_ok() {
        info!("detected Sway compositor for window tracking");
        return Box::new(sway::SwayTracker::new());
    }

    if std::env::var("XDG_SESSION_TYPE")
        .map(|v| v == "x11")
        .unwrap_or(false)
    {
        info!("detected X11 session for window tracking");
        match x11::X11Tracker::new() {
            Ok(tracker) => return Box::new(tracker),
            Err(e) => {
                warn!("failed to initialize X11 tracker: {e}; falling back to noop");
            }
        }
    }

    // Check for GNOME/KDE on Wayland (stub/placeholder).
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
        let desktop_lower = desktop.to_lowercase();
        if desktop_lower.contains("gnome") || desktop_lower.contains("kde") {
            info!("detected {desktop} desktop — window tracking is limited; using D-Bus stub");
            return Box::new(dbus::DbusTracker::new(&desktop));
        }
    }

    warn!("could not detect compositor — window tracking disabled (using noop)");
    Box::new(NoopTracker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_tracker_always_succeeds() {
        let tracker = NoopTracker;
        let id = tracker.get_focused_window().unwrap();
        assert_eq!(id, "noop");
        tracker.focus_window("anything").unwrap();
    }

    #[test]
    fn detect_tracker_returns_something() {
        // In a test environment, we may not have any compositor running,
        // but detect_tracker should never panic — it should return NoopTracker.
        let tracker = detect_tracker();
        // Just verify it doesn't panic on use.
        let _ = tracker.get_focused_window();
    }
}
