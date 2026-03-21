//! Cross-platform keyboard injection via the `enigo` crate.
//!
//! Used on Windows (SendInput) and macOS (CGEvent).

use std::thread;
use std::time::Duration;

use anyhow::Context;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use tracing::{debug, warn};

use super::clipboard::ClipboardOps;
use super::{ClipboardHandler, KeyInjector};

/// Delay between key events to prevent dropped characters.
const KEY_DELAY: Duration = Duration::from_millis(2);

/// Keyboard that injects keystrokes via enigo (SendInput on Windows, CGEvent on macOS).
pub struct EnigoKeyboard {
    enigo: Enigo,
    clipboard: ClipboardOps,
}

impl EnigoKeyboard {
    /// Create a new enigo-based keyboard.
    ///
    /// On macOS, the process must have Accessibility permissions.
    pub fn new(clipboard: ClipboardOps) -> anyhow::Result<Self> {
        let enigo =
            Enigo::new(&Settings::default()).map_err(|e| anyhow::anyhow!("enigo init: {e}"))?;

        debug!("enigo keyboard created");

        Ok(Self { enigo, clipboard })
    }

    /// Inject the platform-specific paste shortcut (Ctrl+V on Windows, Cmd+V on macOS).
    fn inject_paste(&mut self) -> anyhow::Result<()> {
        let modifier = paste_modifier();

        self.enigo
            .key(modifier, Direction::Press)
            .map_err(|e| anyhow::anyhow!("key press: {e}"))?;
        thread::sleep(KEY_DELAY);

        self.enigo
            .key(Key::Unicode('v'), Direction::Click)
            .map_err(|e| anyhow::anyhow!("key click: {e}"))?;
        thread::sleep(KEY_DELAY);

        self.enigo
            .key(modifier, Direction::Release)
            .map_err(|e| anyhow::anyhow!("key release: {e}"))?;
        thread::sleep(KEY_DELAY);

        Ok(())
    }
}

/// Return the platform-appropriate paste modifier key.
fn paste_modifier() -> Key {
    #[cfg(target_os = "macos")]
    {
        Key::Meta
    }
    #[cfg(not(target_os = "macos"))]
    {
        Key::Control
    }
}

/// Return the platform-appropriate copy modifier key.
fn copy_modifier() -> Key {
    paste_modifier()
}

impl KeyInjector for EnigoKeyboard {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.enigo
            .text(text)
            .map_err(|e| anyhow::anyhow!("enigo text: {e}"))?;
        Ok(())
    }

    fn backspace(&mut self, count: u32) -> anyhow::Result<()> {
        for _ in 0..count {
            self.enigo
                .key(Key::Backspace, Direction::Click)
                .map_err(|e| anyhow::anyhow!("backspace: {e}"))?;
            thread::sleep(KEY_DELAY);
        }
        Ok(())
    }

    fn paste_text(&mut self, text: &str) -> anyhow::Result<()> {
        // Save current clipboard.
        let saved = self.clipboard.get_text().ok();

        // Set new clipboard content.
        self.clipboard
            .set_text(text)
            .context("failed to set clipboard for paste")?;

        // Small delay to ensure clipboard is ready.
        thread::sleep(Duration::from_millis(10));

        // Inject paste shortcut.
        self.inject_paste()?;

        // Small delay before restoring clipboard.
        thread::sleep(Duration::from_millis(50));

        // Restore previous clipboard content.
        if let Some(previous) = saved {
            if let Err(e) = self.clipboard.set_text(&previous) {
                warn!("failed to restore clipboard: {e}");
            }
        }

        Ok(())
    }
}

/// Simulate Ctrl+C (Windows) or Cmd+C (macOS) to copy the current selection.
pub fn simulate_copy() -> anyhow::Result<()> {
    let mut enigo =
        Enigo::new(&Settings::default()).map_err(|e| anyhow::anyhow!("enigo init: {e}"))?;

    let modifier = copy_modifier();

    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| anyhow::anyhow!("key press: {e}"))?;
    thread::sleep(KEY_DELAY);
    enigo
        .key(Key::Unicode('c'), Direction::Click)
        .map_err(|e| anyhow::anyhow!("key click: {e}"))?;
    thread::sleep(KEY_DELAY);
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| anyhow::anyhow!("key release: {e}"))?;

    Ok(())
}

/// Simulate Ctrl+V (Windows) or Cmd+V (macOS) to paste from clipboard.
pub fn simulate_paste() -> anyhow::Result<()> {
    let mut enigo =
        Enigo::new(&Settings::default()).map_err(|e| anyhow::anyhow!("enigo init: {e}"))?;

    let modifier = paste_modifier();

    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| anyhow::anyhow!("key press: {e}"))?;
    thread::sleep(KEY_DELAY);
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| anyhow::anyhow!("key click: {e}"))?;
    thread::sleep(KEY_DELAY);
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| anyhow::anyhow!("key release: {e}"))?;

    Ok(())
}
