//! Input injection: virtual keyboard and clipboard operations.

pub mod clipboard;
pub mod keymap;
pub mod uinput;

/// A single keypress with optional Shift and/or AltGr modifiers.
#[derive(Debug, Clone, Copy)]
pub struct KeyTap {
    pub keycode: u16,
    pub shift: bool,
    pub altgr: bool,
}

/// Information needed to produce a character at the cursor.
///
/// For most characters this is a single `KeyTap`. For characters that
/// XKB only exposes as a dead-key combination (e.g. `ã` = `dead_tilde + a`
/// on `us:intl`, or `'` = `dead_acute + space`), a `follow` tap is
/// recorded so the typer emits the dead-key keypress followed by the
/// base-letter (or space) keypress in sequence.
#[derive(Debug, Clone, Copy)]
pub struct KeyMapping {
    pub main: KeyTap,
    pub follow: Option<KeyTap>,
}

/// Trait for injecting keystrokes at the cursor.
pub trait KeyInjector: Send + Sync {
    /// Type text by injecting individual key events.
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;

    /// Send N backspace key events.
    fn backspace(&mut self, count: u32) -> anyhow::Result<()>;

    /// Paste text via clipboard (Ctrl+V). Used for characters
    /// that cannot be typed via the keyboard layout.
    fn paste_text(&mut self, text: &str) -> anyhow::Result<()>;
}

/// Trait for clipboard get/set operations.
pub trait ClipboardHandler: Send + Sync {
    /// Read the current clipboard text content.
    fn get_text(&self) -> anyhow::Result<String>;

    /// Set the clipboard to the given text.
    fn set_text(&self, text: &str) -> anyhow::Result<()>;

    /// Read the primary selection (highlighted text, no Ctrl+C needed).
    fn get_primary_selection(&self) -> anyhow::Result<String>;
}
