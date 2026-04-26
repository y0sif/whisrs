//! Virtual keyboard via evdev/uinput.
//!
//! Creates a virtual keyboard device via `/dev/uinput` and injects key events
//! using the reverse XKB keymap to produce the correct characters regardless
//! of the user's keyboard layout.

use std::thread;
use std::time::Duration;

use anyhow::Context;
use evdev::{AttributeSet, EventType, InputEvent, Key};
use tracing::{debug, warn};

use super::clipboard::ClipboardOps;
use super::keymap::XkbKeymap;
use super::{ClipboardHandler, KeyInjector};

/// Delay after creating the virtual device to let the kernel register it.
const DEVICE_SETTLE_DELAY: Duration = Duration::from_millis(200);

/// Virtual keyboard that injects keystrokes via uinput.
pub struct UinputKeyboard {
    device: evdev::uinput::VirtualDevice,
    keymap: XkbKeymap,
    clipboard: ClipboardOps,
    key_delay: Duration,
}

impl UinputKeyboard {
    /// Create a new virtual keyboard device.
    ///
    /// Requires write access to `/dev/uinput` (user must be in the `input`
    /// group or have the appropriate udev rule installed).
    ///
    /// `key_delay` is the inter-event delay. Raise it for TUIs that drop
    /// characters in raw mode (e.g. Node/Ink-based apps like Claude Code).
    pub fn new(
        keymap: XkbKeymap,
        clipboard: ClipboardOps,
        key_delay: Duration,
    ) -> anyhow::Result<Self> {
        // Register all key codes we might need.
        let mut keys = AttributeSet::<Key>::new();
        for code in 1..=247 {
            keys.insert(Key::new(code));
        }

        let device = evdev::uinput::VirtualDeviceBuilder::new()
            .context("failed to create VirtualDeviceBuilder")?
            .name("whisrs virtual keyboard")
            .with_keys(&keys)
            .context("failed to register key events")?
            .build()
            .context("failed to build uinput virtual device — check /dev/uinput permissions")?;

        // Give the kernel time to register the new device.
        thread::sleep(DEVICE_SETTLE_DELAY);

        debug!("uinput virtual keyboard created");

        Ok(Self {
            device,
            keymap,
            clipboard,
            key_delay,
        })
    }

    /// Press and release a single key, optionally with Shift held.
    fn tap_key(&mut self, keycode: u16, shift: bool) -> anyhow::Result<()> {
        let key = Key::new(keycode);

        if shift {
            // Press Shift
            self.device.emit(&[InputEvent::new(
                EventType::KEY,
                Key::KEY_LEFTSHIFT.code(),
                1,
            )])?;
            thread::sleep(self.key_delay);
        }

        // Press key
        self.device
            .emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
        thread::sleep(self.key_delay);

        // Release key
        self.device
            .emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;
        thread::sleep(self.key_delay);

        if shift {
            // Release Shift
            self.device.emit(&[InputEvent::new(
                EventType::KEY,
                Key::KEY_LEFTSHIFT.code(),
                0,
            )])?;
            thread::sleep(self.key_delay);
        }

        Ok(())
    }

    /// Release all modifier keys to prevent interference with injected text.
    fn release_all_modifiers(&mut self) -> anyhow::Result<()> {
        let modifiers = [
            Key::KEY_LEFTSHIFT,
            Key::KEY_RIGHTSHIFT,
            Key::KEY_LEFTCTRL,
            Key::KEY_RIGHTCTRL,
            Key::KEY_LEFTALT,
            Key::KEY_RIGHTALT,
            Key::KEY_LEFTMETA,
            Key::KEY_RIGHTMETA,
        ];

        for modifier in &modifiers {
            self.device
                .emit(&[InputEvent::new(EventType::KEY, modifier.code(), 0)])?;
        }
        thread::sleep(self.key_delay);

        Ok(())
    }

    /// Inject Ctrl+V to paste from clipboard.
    fn inject_ctrl_v(&mut self) -> anyhow::Result<()> {
        // Press Ctrl
        self.device
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_LEFTCTRL.code(), 1)])?;
        thread::sleep(self.key_delay);

        // Press V
        self.device
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_V.code(), 1)])?;
        thread::sleep(self.key_delay);

        // Release V
        self.device
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_V.code(), 0)])?;
        thread::sleep(self.key_delay);

        // Release Ctrl
        self.device
            .emit(&[InputEvent::new(EventType::KEY, Key::KEY_LEFTCTRL.code(), 0)])?;
        thread::sleep(self.key_delay);

        Ok(())
    }
}

impl KeyInjector for UinputKeyboard {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.release_all_modifiers()?;

        // Collect characters that can't be typed via keyboard into a buffer,
        // then paste them as a batch when we hit a typeable character or end.
        let mut paste_buf = String::new();

        for ch in text.chars() {
            if let Some(&mapping) = self.keymap.lookup(ch) {
                // Flush any pending paste buffer first.
                if !paste_buf.is_empty() {
                    self.paste_text(&paste_buf)?;
                    paste_buf.clear();
                }
                self.tap_key(mapping.keycode, mapping.shift)?;
            } else {
                // Character not in keymap — accumulate for clipboard paste.
                paste_buf.push(ch);
            }
        }

        // Flush remaining paste buffer.
        if !paste_buf.is_empty() {
            self.paste_text(&paste_buf)?;
        }

        Ok(())
    }

    fn backspace(&mut self, count: u32) -> anyhow::Result<()> {
        self.release_all_modifiers()?;

        for _ in 0..count {
            self.tap_key(Key::KEY_BACKSPACE.code(), false)?;
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

        // Inject Ctrl+V.
        self.release_all_modifiers()?;
        self.inject_ctrl_v()?;

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
