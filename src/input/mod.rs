//! Input injection: virtual keyboard and clipboard operations.

pub mod clipboard;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub mod enigo_backend;
#[cfg(target_os = "linux")]
pub mod keymap;
#[cfg(target_os = "linux")]
pub mod uinput;

/// A modifier key that may need to be held during key injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Shift,
}

/// Information needed to produce a character via a physical keypress.
#[derive(Debug, Clone, Copy)]
pub struct KeyMapping {
    /// The platform-specific keycode (evdev keycode on Linux).
    pub keycode: u16,
    /// Whether Shift must be held.
    pub shift: bool,
}

/// Trait for injecting keystrokes at the cursor.
pub trait KeyInjector: Send + Sync {
    /// Type text by injecting individual key events.
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;

    /// Send N backspace key events.
    fn backspace(&mut self, count: u32) -> anyhow::Result<()>;

    /// Paste text via clipboard (Ctrl+V / Cmd+V). Used for characters
    /// that cannot be typed via the keyboard layout.
    fn paste_text(&mut self, text: &str) -> anyhow::Result<()>;
}

/// Trait for clipboard get/set operations.
pub trait ClipboardHandler: Send + Sync {
    /// Read the current clipboard text content.
    fn get_text(&self) -> anyhow::Result<String>;

    /// Set the clipboard to the given text.
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}
