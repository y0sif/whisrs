//! Clipboard operations — cross-platform via arboard, with Wayland override on Linux.

use anyhow::Context;
use tracing::{debug, warn};

use super::ClipboardHandler;

/// Detect whether we're running under Wayland (Linux only).
#[cfg(target_os = "linux")]
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
}

/// Concrete clipboard implementation that auto-detects the best backend.
#[derive(Debug)]
pub enum ClipboardOps {
    /// Wayland: uses wl-copy/wl-paste (Linux only).
    #[cfg(target_os = "linux")]
    Wayland,
    /// Cross-platform: uses arboard (X11 on Linux, native on macOS/Windows).
    Native,
}

impl ClipboardOps {
    /// Create a new clipboard handler, auto-detecting the best backend.
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        if is_wayland() {
            debug!("detected Wayland display server for clipboard");
            return Self::Wayland;
        }

        debug!("using native clipboard backend");
        Self::Native
    }
}

impl ClipboardHandler for ClipboardOps {
    fn get_text(&self) -> anyhow::Result<String> {
        match self {
            #[cfg(target_os = "linux")]
            ClipboardOps::Wayland => wayland_get_text(),
            ClipboardOps::Native => native_get_text(),
        }
    }

    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            ClipboardOps::Wayland => wayland_set_text(text),
            ClipboardOps::Native => native_set_text(text),
        }
    }

    fn get_primary_selection(&self) -> anyhow::Result<String> {
        match self {
            #[cfg(target_os = "linux")]
            ClipboardOps::Wayland => wayland_get_primary(),
            // Primary selection is a Linux/X11 concept; on macOS/Windows return empty.
            #[cfg(target_os = "linux")]
            ClipboardOps::Native => x11_get_primary(),
            #[cfg(not(target_os = "linux"))]
            ClipboardOps::Native => Ok(String::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland: shell out to wl-copy / wl-paste (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn wayland_get_text() -> anyhow::Result<String> {
    use std::process::Command;

    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .context("failed to run wl-paste — is wl-clipboard installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // wl-paste exits non-zero when clipboard is empty; treat as empty.
        if stderr.contains("no suitable type") || stderr.contains("nothing is copied") {
            return Ok(String::new());
        }
        anyhow::bail!("wl-paste failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn wayland_get_primary() -> anyhow::Result<String> {
    use std::process::Command;

    let output = Command::new("wl-paste")
        .args(["--no-newline", "--primary"])
        .output()
        .context("failed to run wl-paste --primary — is wl-clipboard installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no suitable type") || stderr.contains("nothing is copied") {
            return Ok(String::new());
        }
        anyhow::bail!("wl-paste --primary failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn wayland_set_text(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::Command;

    let mut child = Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run wl-copy — is wl-clipboard installed?")?;

    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(text.as_bytes())
            .context("failed to write to wl-copy stdin")?;
    }

    let status = child.wait().context("failed to wait for wl-copy")?;
    if !status.success() {
        warn!("wl-copy exited with status {status}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native: arboard crate (cross-platform)
// ---------------------------------------------------------------------------

fn native_get_text() -> anyhow::Result<String> {
    let mut clipboard = arboard::Clipboard::new().context("failed to open clipboard")?;
    clipboard.get_text().context("failed to get clipboard text")
}

fn native_set_text(text: &str) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("failed to open clipboard")?;
    clipboard
        .set_text(text)
        .context("failed to set clipboard text")
}

// ---------------------------------------------------------------------------
// X11 primary selection via arboard (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn x11_get_primary() -> anyhow::Result<String> {
    use arboard::GetExtLinux;
    let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
    clipboard
        .get()
        .clipboard(arboard::LinuxClipboardKind::Primary)
        .text()
        .context("failed to get text from X11 primary selection")
}
