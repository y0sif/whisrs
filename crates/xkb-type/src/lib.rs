//! Virtual keyboard typing via evdev/uinput or X11 XTEST with XKB layout-aware
//! key mapping.
//!
//! This crate provides:
//! - [`Keyboard`] — a virtual uinput device that injects keystrokes (or,
//!   with the `xtest` feature, an X11 XTEST connection).
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
//!
//! # Example (pause or stop between keys)
//!
//! [`KeyInjector::type_text`] asks a gate before every key, with the byte
//! offset of the next character. The gate may block (to wait for something)
//! and can stop the typing; the result says where it stopped.
//!
//! ```no_run
//! use std::ops::ControlFlow;
//! use std::time::Duration;
//! use xkb_type::{KeyInjector, Keyboard, Typed};
//!
//! let mut kb: Box<dyn KeyInjector> = Box::new(Keyboard::new(Duration::from_millis(2))?);
//! let text = "hello world";
//! // Stop before the space.
//! let mut gate = |at: usize| {
//!     if at < 5 {
//!         ControlFlow::Continue(())
//!     } else {
//!         ControlFlow::Break(())
//!     }
//! };
//! match kb.type_text(text, &mut gate)? {
//!     Typed::All => println!("typed everything"),
//!     Typed::StoppedAt(at) => println!("not typed: {:?}", &text[at..]),
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod clipboard;
pub mod keyboard;
pub mod keymap;
mod layout_control;
#[cfg(feature = "wayland-vk")]
pub mod wayland_vk;
#[cfg(feature = "xtest")]
mod xtest;

use std::ops::ControlFlow;

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

/// How far a [`KeyInjector::type_text`] call got.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// Every character of the text was sent.
    All,
    /// The gate stopped the typing before the character at this byte offset
    /// of the text: `text[..offset]` was sent, `text[offset..]` was not.
    /// Always a char boundary of the text.
    StoppedAt(usize),
}

/// Send `steps` in order, asking `gate` with each step's byte offset first.
/// Stops before the first step the gate breaks on and returns
/// [`Typed::StoppedAt`] its offset; an error from `send` ends the run at once.
/// Shared by the backends so every one gates the same way.
pub(crate) fn run_gated<S>(
    steps: impl IntoIterator<Item = (usize, S)>,
    gate: &mut dyn FnMut(usize) -> ControlFlow<()>,
    mut send: impl FnMut(S) -> anyhow::Result<()>,
) -> anyhow::Result<Typed> {
    for (at, step) in steps {
        if gate(at).is_break() {
            return Ok(Typed::StoppedAt(at));
        }
        send(step)?;
    }
    Ok(Typed::All)
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
/// The semantics mirror [`Keyboard`]'s inherent methods, except that
/// [`type_text`](Self::type_text) takes a gate.
pub trait KeyInjector: Send {
    /// Type `text` by injecting the corresponding keystrokes, asking `gate`
    /// before each one.
    ///
    /// `gate` gets the byte offset in `text` of the next character to send.
    /// It is asked before every key tap, and once before each clipboard
    /// paste of a run of characters the keymap lacks (with the offset of the
    /// run's first character). A character typed as two taps (a dead key and
    /// its base letter) is asked about once and never split. A character the
    /// backend cannot type at all is skipped without asking.
    ///
    /// [`ControlFlow::Continue`] sends the character. [`ControlFlow::Break`]
    /// stops before it: what was already sent is flushed, and the result is
    /// [`Typed::StoppedAt`] that offset. The gate may block, for example
    /// until the user lets go of a modifier key. Pass
    /// `&mut |_| ControlFlow::Continue(())` to type everything.
    fn type_text(
        &mut self,
        text: &str,
        gate: &mut dyn FnMut(usize) -> ControlFlow<()>,
    ) -> anyhow::Result<Typed>;

    /// Emit Backspace `count` times.
    fn backspace(&mut self, count: usize) -> anyhow::Result<()>;

    /// Press all keys in `keys`, then release them in reverse order.
    fn send_combo(&mut self, keys: &[evdev::Key]) -> anyhow::Result<()>;

    /// Set the inter-event delay used between injected key events.
    fn set_key_delay(&mut self, delay: std::time::Duration);
}

impl KeyInjector for Keyboard {
    fn type_text(
        &mut self,
        text: &str,
        gate: &mut dyn FnMut(usize) -> ControlFlow<()>,
    ) -> anyhow::Result<Typed> {
        Keyboard::type_text_gated(self, text, gate)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `steps` (offsets) through [`run_gated`] with a gate that breaks
    /// at `break_at` (an offset), returning the result, the offsets the gate
    /// saw and the steps sent.
    fn run(steps: &[usize], break_at: Option<usize>) -> (Typed, Vec<usize>, Vec<usize>) {
        let mut asked = Vec::new();
        let mut sent = Vec::new();
        let mut gate = |at: usize| {
            asked.push(at);
            if Some(at) == break_at {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let typed = run_gated(steps.iter().map(|&at| (at, at)), &mut gate, |step| {
            sent.push(step);
            Ok(())
        })
        .unwrap();
        (typed, asked, sent)
    }

    #[test]
    fn run_gated_asks_before_every_step_and_sends_them_all() {
        let (typed, asked, sent) = run(&[0, 1, 3, 7], None);
        assert_eq!(typed, Typed::All);
        assert_eq!(asked, vec![0, 1, 3, 7]);
        assert_eq!(sent, vec![0, 1, 3, 7]);
    }

    #[test]
    fn run_gated_stops_before_the_step_the_gate_breaks_on() {
        let (typed, asked, sent) = run(&[0, 1, 3, 7], Some(3));
        assert_eq!(typed, Typed::StoppedAt(3));
        assert_eq!(asked, vec![0, 1, 3], "nothing is asked after the break");
        assert_eq!(sent, vec![0, 1], "the step it broke on is not sent");

        let (typed, _, sent) = run(&[0, 1], Some(0));
        assert_eq!(typed, Typed::StoppedAt(0));
        assert!(sent.is_empty());
    }

    #[test]
    fn run_gated_with_no_steps_is_all_without_asking() {
        let (typed, asked, sent) = run(&[], Some(0));
        assert_eq!(typed, Typed::All);
        assert!(asked.is_empty() && sent.is_empty());
    }

    #[test]
    fn run_gated_ends_at_a_send_error() {
        let mut asked = 0;
        let mut gate = |_: usize| {
            asked += 1;
            ControlFlow::Continue(())
        };
        let result = run_gated([(0, ()), (1, ()), (2, ())], &mut gate, |()| {
            anyhow::bail!("device gone")
        });
        assert!(result.is_err());
        assert_eq!(asked, 1, "no step is asked after a failed send");
    }
}
