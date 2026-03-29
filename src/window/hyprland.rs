//! Hyprland window tracking via `hyprctl` commands.

use std::process::Command;

use anyhow::Context;
use serde::Deserialize;
use tracing::debug;

use super::WindowTracker;

/// Window tracker for the Hyprland compositor.
///
/// Uses `hyprctl activewindow -j` to query and `hyprctl dispatch focuswindow`
/// to restore focus.
pub struct HyprlandTracker;

impl Default for HyprlandTracker {
    fn default() -> Self {
        Self
    }
}

impl HyprlandTracker {
    pub fn new() -> Self {
        Self
    }
}

/// Parsed JSON output from `hyprctl activewindow -j`.
#[derive(Debug, Deserialize)]
struct HyprctlActiveWindow {
    /// Window address (hex string like "0x5678abcd").
    address: String,
    /// Window class (e.g. "Alacritty", "firefox").
    #[serde(default)]
    class: String,
}

impl WindowTracker for HyprlandTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        let output = Command::new("hyprctl")
            .args(["activewindow", "-j"])
            .output()
            .context("failed to run hyprctl — is Hyprland running?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("hyprctl activewindow failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: HyprctlActiveWindow =
            serde_json::from_str(&stdout).context("failed to parse hyprctl activewindow JSON")?;

        debug!("hyprland focused window: {}", parsed.address);
        Ok(parsed.address)
    }

    fn get_focused_window_class(&self) -> Option<String> {
        let output = Command::new("hyprctl")
            .args(["activewindow", "-j"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: HyprctlActiveWindow = serde_json::from_str(&stdout).ok()?;

        if parsed.class.is_empty() {
            None
        } else {
            Some(parsed.class)
        }
    }

    fn focus_window(&self, id: &str) -> anyhow::Result<()> {
        // Hyprland expects: hyprctl dispatch focuswindow address:<ADDR>
        // The address from activewindow already includes "0x" prefix.
        let target = format!("address:{id}");
        debug!("focusing hyprland window: {target}");

        let output = Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &target])
            .output()
            .context("failed to run hyprctl dispatch focuswindow")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("hyprctl dispatch focuswindow failed: {stderr}");
        }

        Ok(())
    }
}
