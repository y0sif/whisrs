//! Clipboard operations — Wayland (wl-copy/wl-paste) and X11 (arboard).

use std::process::Command;

use anyhow::Context;
use tracing::{debug, warn};

use super::ClipboardHandler;

/// Detect whether we're running under Wayland.
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
}

/// Concrete clipboard implementation that auto-detects Wayland vs X11.
#[derive(Debug)]
pub enum ClipboardOps {
    Wayland,
    X11,
}

impl ClipboardOps {
    /// Create a new clipboard handler, auto-detecting the display server.
    pub fn detect() -> Self {
        if is_wayland() {
            debug!("detected Wayland display server for clipboard");
            Self::Wayland
        } else {
            debug!("detected X11 display server for clipboard");
            Self::X11
        }
    }
}

impl ClipboardHandler for ClipboardOps {
    fn get_text(&self) -> anyhow::Result<String> {
        match self {
            ClipboardOps::Wayland => wayland_get_text(),
            ClipboardOps::X11 => x11_get_text(),
        }
    }

    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        match self {
            ClipboardOps::Wayland => wayland_set_text(text),
            ClipboardOps::X11 => x11_set_text(text),
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland: shell out to wl-copy / wl-paste
// ---------------------------------------------------------------------------

fn wayland_get_text() -> anyhow::Result<String> {
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

fn wayland_set_text(text: &str) -> anyhow::Result<()> {
    use std::io::Write;

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
// X11: arboard crate
// ---------------------------------------------------------------------------

fn x11_get_text() -> anyhow::Result<String> {
    let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
    clipboard
        .get_text()
        .context("failed to get text from X11 clipboard")
}

fn x11_set_text(text: &str) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
    clipboard
        .set_text(text)
        .context("failed to set text on X11 clipboard")
}
