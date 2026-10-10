//! Virtual keyboard typing via evdev/uinput with XKB layout-aware key mapping.
//!
//! This crate provides:
//! - [`Keyboard`] — a virtual uinput device that injects keystrokes.
//! - [`XkbKeymap`] — reverse char→keycode lookup from the active XKB layout.
//! - [`ClipboardBackend`] — trait + auto-detected clipboard implementations.
//!
//! # Example (auto-detect layout and clipboard)
//!
//! ```no_run
//! use xkb_type::{Keyboard, Key};
//! use std::time::Duration;
//!
//! let mut kb = Keyboard::new(Duration::from_millis(2))?;
//! kb.type_text("Hello — こんにちは — €100 — 😀")?;
//! kb.backspace(5)?;
//! kb.send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_C])?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # Example (explicit layout)
//!
//! ```no_run
//! use xkb_type::Keyboard;
//! use std::time::Duration;
//!
//! let mut kb = Keyboard::with_layout("de", None, Duration::from_millis(5))?;
//! kb.type_text("Schöne Grüße")?;
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod clipboard;
pub mod keyboard;
pub mod keymap;
#[cfg(feature = "wayland-vk")]
pub mod wayland_vk;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single keypress with optional Shift and/or AltGr modifiers.
#[derive(Debug, Clone, Copy)]
pub struct KeyTap {
    pub keycode: u16,
    pub shift: bool,
    pub altgr: bool,
}

/// Information needed to produce a character at the cursor.
///
/// For most characters this is a single [`KeyTap`]. For characters that
/// XKB only exposes as a dead-key combination (e.g. `ã` = `dead_tilde + a`
/// on `us:intl`, or `'` = `dead_acute + space`), a `follow` tap is
/// recorded so the typer emits the dead-key keypress followed by the
/// base-letter (or space) keypress in sequence.
#[derive(Debug, Clone, Copy)]
pub struct KeyMapping {
    pub main: KeyTap,
    pub follow: Option<KeyTap>,
}

// ---------------------------------------------------------------------------
// KeyInjector trait
// ---------------------------------------------------------------------------

/// High-level text-injection abstraction.
///
/// This unifies the available keystroke backends behind a single trait so a
/// caller can pick an implementation at runtime (e.g. the layout-independent
/// Wayland [`wayland_vk::WaylandVkKeyboard`] when the compositor supports
/// `zwp_virtual_keyboard_v1`, falling back to the uinput [`Keyboard`]
/// otherwise) without changing the typing code.
///
/// The semantics mirror [`Keyboard`]'s inherent methods.
pub trait KeyInjector: Send {
    /// Type `text` by injecting the corresponding keystrokes.
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;

    /// Emit Backspace `count` times.
    fn backspace(&mut self, count: usize) -> anyhow::Result<()>;

    /// Press all keys in `keys`, then release them in reverse order.
    fn send_combo(&mut self, keys: &[evdev::Key]) -> anyhow::Result<()>;

    /// Set the inter-event delay used between injected key events.
    fn set_key_delay(&mut self, delay: std::time::Duration);
}

impl KeyInjector for Keyboard {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        Keyboard::type_text(self, text)
    }

    fn backspace(&mut self, count: usize) -> anyhow::Result<()> {
        Keyboard::backspace(self, count)
    }

    fn send_combo(&mut self, keys: &[evdev::Key]) -> anyhow::Result<()> {
        Keyboard::send_combo(self, keys)
    }

    fn set_key_delay(&mut self, delay: std::time::Duration) {
        Keyboard::set_key_delay(self, delay)
    }
}

// ---------------------------------------------------------------------------
// ClipboardBackend trait
// ---------------------------------------------------------------------------

/// Trait for clipboard get/set operations.
pub trait ClipboardBackend: Send + Sync {
    /// Read the current clipboard text content.
    ///
    /// Returns `Ok("")` when the clipboard is empty (nothing owns it) and
    /// `Err` when it holds content that cannot be read as text, so a caller
    /// that saves and restores the clipboard never mistakes non-text content
    /// for empty and overwrites it with `""`.
    fn get_text(&self) -> anyhow::Result<String>;

    /// Set the clipboard to the given text.
    fn set_text(&self, text: &str) -> anyhow::Result<()>;

    /// Read the primary selection (highlighted text, no Ctrl+C needed).
    fn get_primary_selection(&self) -> anyhow::Result<String>;
}

// ---------------------------------------------------------------------------
// Re-exports
// ---------------------------------------------------------------------------

pub use clipboard::{default_clipboard, NoopClipboard, WaylandClipboard, X11Clipboard};
pub use keyboard::Keyboard;
pub use keymap::{KeyboardLayout, XkbKeymap};

// Re-export evdev's `Key` enum (and the full `evdev` module) so callers
// of [`Keyboard::send_combo`] don't need a direct `evdev` dependency.
pub use evdev;
pub use evdev::Key;

/// Convenience: build a [`Keyboard`] from the detected layout with a
/// sensible default key delay (5 ms).
///
/// Returns an error if the XKB keymap cannot be built (missing locale data)
/// or if `/dev/uinput` is not writable.
///
/// Equivalent to `Keyboard::new(Duration::from_millis(5))`.
pub fn keyboard_from_detected_layout() -> anyhow::Result<Keyboard> {
    Keyboard::new(std::time::Duration::from_millis(5))
}
