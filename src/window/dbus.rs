//! D-Bus window tracking stub for GNOME and KDE.
//!
//! GNOME requires the `window-calls` extension, and KDE requires KWin scripting.
//! Both are limited and not yet fully implemented. This module provides a stub
//! that returns clear error messages.

use tracing::warn;

use super::WindowTracker;

/// Stub window tracker for GNOME/KDE desktops via D-Bus.
pub struct DbusTracker {
    desktop: String,
}

impl DbusTracker {
    pub fn new(desktop: &str) -> Self {
        Self {
            desktop: desktop.to_string(),
        }
    }
}

impl WindowTracker for DbusTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        warn!(
            "{} window tracking not yet supported — text will be typed at current cursor",
            self.desktop
        );
        // Return a placeholder so the flow doesn't break.
        Ok("dbus-stub".to_string())
    }

    fn focus_window(&self, _id: &str) -> anyhow::Result<()> {
        warn!(
            "{} window focus restoration not yet supported — skipping",
            self.desktop
        );
        // Don't fail — graceful degradation.
        Ok(())
    }
}
