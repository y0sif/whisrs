//! macOS window tracking via Accessibility API.
//!
//! Requires Accessibility permissions (System Settings > Privacy & Security > Accessibility).
//! Currently a stub — returns noop-like behavior with a warning.

use tracing::warn;

use super::WindowTracker;

/// Window tracker for macOS using the Accessibility API.
///
/// TODO: Implement using CGWindowListCopyWindowInfo / AXUIElement APIs.
pub struct MacosTracker;

impl MacosTracker {
    pub fn new() -> Self {
        Self
    }
}

impl WindowTracker for MacosTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        warn!("macOS window tracking not yet fully implemented — text will be typed at current cursor");
        Ok("macos-stub".to_string())
    }

    fn focus_window(&self, _id: &str) -> anyhow::Result<()> {
        warn!("macOS window focus restoration not yet implemented — skipping");
        Ok(())
    }
}
