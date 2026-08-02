use anyhow::Context;
use tracing::{debug, warn};

use crate::context::DaemonContext;
use crate::injection::is_terminal_class;

/// Simulate a key combo (e.g. Ctrl+C, Ctrl+V) via a temporary uinput device.
fn simulate_key_combo(modifier: evdev::Key, key: evdev::Key) -> anyhow::Result<()> {
    use evdev::{AttributeSet, EventType, InputEvent, Key};
    use std::thread;
    use std::time::Duration;

    let mut keys = AttributeSet::<Key>::new();
    keys.insert(modifier);
    keys.insert(key);

    let mut device = evdev::uinput::VirtualDeviceBuilder::new()
        .context("failed to create VirtualDeviceBuilder")?
        .name("whisrs command")
        .with_keys(&keys)
        .context("failed to register key events")?
        .build()
        .context("failed to build uinput device")?;

    thread::sleep(Duration::from_millis(200));

    // Press modifier.
    device.emit(&[InputEvent::new(EventType::KEY, modifier.code(), 1)])?;
    thread::sleep(Duration::from_millis(2));
    // Press key.
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
    thread::sleep(Duration::from_millis(2));
    // Release key.
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;
    thread::sleep(Duration::from_millis(2));
    // Release modifier.
    device.emit(&[InputEvent::new(EventType::KEY, modifier.code(), 0)])?;
    thread::sleep(Duration::from_millis(2));

    Ok(())
}

/// Simulate a two-modifier + key combo (e.g. Ctrl+Shift+V) via a temporary uinput device.
fn simulate_key_combo_2mod(
    mod1: evdev::Key,
    mod2: evdev::Key,
    key: evdev::Key,
) -> anyhow::Result<()> {
    use evdev::{AttributeSet, EventType, InputEvent, Key};
    use std::thread;
    use std::time::Duration;

    let mut keys = AttributeSet::<Key>::new();
    keys.insert(mod1);
    keys.insert(mod2);
    keys.insert(key);

    let mut device = evdev::uinput::VirtualDeviceBuilder::new()
        .context("failed to create VirtualDeviceBuilder")?
        .name("whisrs command")
        .with_keys(&keys)
        .context("failed to register key events")?
        .build()
        .context("failed to build uinput device")?;

    thread::sleep(Duration::from_millis(200));

    device.emit(&[InputEvent::new(EventType::KEY, mod1.code(), 1)])?;
    thread::sleep(Duration::from_millis(2));
    device.emit(&[InputEvent::new(EventType::KEY, mod2.code(), 1)])?;
    thread::sleep(Duration::from_millis(2));
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
    thread::sleep(Duration::from_millis(2));
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;
    thread::sleep(Duration::from_millis(2));
    device.emit(&[InputEvent::new(EventType::KEY, mod2.code(), 0)])?;
    thread::sleep(Duration::from_millis(2));
    device.emit(&[InputEvent::new(EventType::KEY, mod1.code(), 0)])?;
    thread::sleep(Duration::from_millis(2));

    Ok(())
}

/// Simulate Ctrl+C (copy) via uinput.
fn simulate_copy() -> anyhow::Result<()> {
    simulate_key_combo(evdev::Key::KEY_LEFTCTRL, evdev::Key::KEY_C)
}

/// Simulate Ctrl+Shift+C (terminal copy) via uinput.
fn simulate_terminal_copy() -> anyhow::Result<()> {
    simulate_key_combo_2mod(
        evdev::Key::KEY_LEFTCTRL,
        evdev::Key::KEY_LEFTSHIFT,
        evdev::Key::KEY_C,
    )
}

/// Capture the currently selected text: primary selection first, else a
/// simulated copy (Ctrl+C, or Ctrl+Shift+C in terminals). Restores the
/// clipboard afterwards (this path never pastes). Returns `None` when nothing
/// usable is selected. Mirrors command mode's acquisition, minus the recording.
pub(crate) async fn acquire_selected_text(context: &DaemonContext) -> Option<String> {
    let clipboard = xkb_type::default_clipboard();
    let saved = clipboard.get_text().unwrap_or_default();

    let is_terminal = context
        .window_tracker
        .get_focused_window_class()
        .map(|c| is_terminal_class(&c))
        .unwrap_or(false);

    let mut text = clipboard.get_primary_selection().unwrap_or_default();

    if text.trim().is_empty() || text == saved {
        let copy_fn = if is_terminal {
            simulate_terminal_copy
        } else {
            simulate_copy
        };
        let _ = tokio::task::spawn_blocking(copy_fn).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        text = clipboard.get_text().unwrap_or_default();
    }

    // Restore the user's clipboard — the set path never pastes.
    let _ = clipboard.set_text(&saved);

    let text = text.trim().to_string();
    if text.is_empty() || text == saved.trim() {
        None
    } else {
        Some(text)
    }
}

/// Capture the currently-selected text for read-aloud and command mode.
///
/// The primary selection (highlighted text) is authoritative when present: it
/// needs no key simulation, so a non-empty primary selection is returned
/// directly. This also avoids the clipboard-equality heuristic below — which
/// only makes sense in the copy-fallback path — wrongly rejecting a selection
/// that happens to equal the current clipboard.
///
/// When the primary selection is empty, fall back to a simulated Ctrl+C
/// (Ctrl+Shift+C in terminals) and read the clipboard. A short settle delay
/// precedes the simulated copy: the triggering hotkey is typically a modifier
/// combo (e.g. Alt+Shift+A), and if the user is still physically holding those
/// modifiers when Ctrl+C fires, the app receives a garbled combo and the copy
/// silently fails. The delay lets a briefly-held hotkey clear first.
///
/// That fallback overwrites the user's clipboard, so it is restored as soon as
/// the copied text has been read — synchronously, because nothing downstream
/// needs the clipboard to keep holding the capture (command mode injects the
/// LLM result, read-aloud speaks the text). The restore is skipped, rather
/// than clobbering the clipboard, under the same two conditions as
/// [`inject_text`]: the original clipboard wasn't readable as text (an image
/// or a file list), or something else copied over our simulated Ctrl+C in the
/// meantime. It is also skipped when the copy captured nothing and left the
/// clipboard unchanged, so nothing is written when there was no selection to
/// copy. A failed restore is logged, never fatal.
///
/// Returns `Ok(text)` on success, or `Err(message)` describing why nothing was
/// captured (caller surfaces this as a `Response::Error` + notification).
pub(crate) async fn capture_selection(context: &DaemonContext) -> Result<String, String> {
    let clipboard = xkb_type::default_clipboard();

    // Trust a non-empty primary selection directly — no copy, no equality
    // check, and no clipboard write of any kind.
    let primary = clipboard.get_primary_selection().unwrap_or_default();
    if !primary.is_empty() {
        return Ok(primary);
    }

    // No primary selection: fall back to a simulated copy + clipboard read.
    // `None` means the clipboard held something that isn't text.
    let saved_clipboard = clipboard.get_text().ok();

    let is_terminal = context
        .window_tracker
        .get_focused_window_class()
        .map(|c| is_terminal_class(&c))
        .unwrap_or(false);

    // Let any briefly-held hotkey modifiers clear before synthesizing Ctrl+C.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let copy_fn = if is_terminal {
        simulate_terminal_copy
    } else {
        simulate_copy
    };

    match tokio::task::spawn_blocking(copy_fn).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("failed to copy selection: {e}")),
        Err(e) => return Err(format!("copy task panicked: {e}")),
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let copied = clipboard
        .get_text()
        .map_err(|e| format!("failed to read clipboard: {e}"))?;

    // The copy clobbered the user's clipboard with the selection; put the
    // original back now that we've read it (see doc comment above). Skipped
    // when the original wasn't text, when the clipboard no longer holds what
    // our Ctrl+C put there, and when the copy left the clipboard exactly as it
    // was (nothing was selected, so there is nothing to undo). Writing an
    // identical value back is not free: on Wayland it takes selection
    // ownership and re-offers plain text only, downgrading a clipboard that
    // held text/html and giving an empty one an empty-string owner.
    let saved_to_restore = saved_clipboard
        .as_deref()
        .filter(|saved| *saved != copied.as_str());
    if let Some(saved) = saved_to_restore {
        match clipboard.get_text() {
            Ok(current) if current == copied => {
                if let Err(e) = clipboard.set_text(saved) {
                    warn!("failed to restore clipboard after selection capture: {e}");
                }
            }
            Ok(_) => {
                debug!("clipboard changed during selection capture; skipping restore");
            }
            Err(e) => {
                warn!("failed to read clipboard before restore, skipping restore: {e}");
            }
        }
    }

    // With no primary selection, an unchanged clipboard means the copy captured
    // nothing (nothing selected, or the held hotkey garbled Ctrl+C).
    if copied.is_empty() || saved_clipboard.as_deref() == Some(copied.as_str()) {
        return Err("no text selected — select some text first".to_string());
    }

    Ok(copied)
}
