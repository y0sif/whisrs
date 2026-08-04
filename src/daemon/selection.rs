use std::time::Duration;

use anyhow::Context;
use tracing::{debug, warn};
use xkb_type::ClipboardBackend;

use crate::context::DaemonContext;
use crate::injection::is_terminal_class;

/// Delay before the simulated copy so briefly-held hotkey modifiers clear
/// (see [`capture_selection`]).
const COPY_SETTLE_DELAY: Duration = Duration::from_millis(250);

/// Delay after the simulated copy before reading the clipboard back, giving
/// the focused app time to service the copy.
const COPY_READ_DELAY: Duration = Duration::from_millis(100);

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

/// Capture the currently selected text for the `set_hotkey` reprogram path.
///
/// Thin wrapper over [`capture_selection`]: same acquisition order (primary
/// selection first, simulated-copy fallback) and — crucially — the same
/// clipboard-restore policy. An earlier standalone implementation restored
/// the clipboard unconditionally, outside the branch that decides whether a
/// copy ever fired, so a clipboard holding non-text content (`get_text()`
/// errors on an image or file list) was replaced with an empty string even
/// when the primary selection was populated and no Ctrl+C ran (#80).
///
/// Returns the trimmed selection, or `None` when nothing usable is selected
/// (the caller surfaces that as a "select the instruction text first"
/// notification).
pub(crate) async fn acquire_selected_text(context: &DaemonContext) -> Option<String> {
    match capture_selection(context).await {
        Ok(text) => instruction_from_capture(&text),
        Err(reason) => {
            debug!("set-instruction selection capture failed: {reason}");
            None
        }
    }
}

/// Post-process a captured selection into a usable instruction: trimmed,
/// `None` when only whitespace (or nothing) was captured.
fn instruction_from_capture(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Capture the currently-selected text for read-aloud, command mode and the
/// set_hotkey path (via [`acquire_selected_text`]).
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
/// copy. A failed restore is logged, never fatal. When the original clipboard
/// wasn't text, the fallback copy overwrites content that cannot be restored;
/// a warning is logged before the copy fires (#80).
///
/// Returns `Ok(text)` on success, or `Err(message)` describing why nothing was
/// captured (caller surfaces this as a `Response::Error` + notification).
pub(crate) async fn capture_selection(context: &DaemonContext) -> Result<String, String> {
    let clipboard = xkb_type::default_clipboard();

    // The terminal check is resolved lazily, inside the copy closure, so the
    // primary-selection fast path never queries the window tracker.
    let tracker = context.window_tracker.clone();
    let copy = move || {
        let is_terminal = tracker
            .get_focused_window_class()
            .map(|c| is_terminal_class(&c))
            .unwrap_or(false);
        if is_terminal {
            simulate_terminal_copy()
        } else {
            simulate_copy()
        }
    };

    capture_selection_impl(clipboard.as_ref(), copy, COPY_SETTLE_DELAY, COPY_READ_DELAY).await
}

/// Testable core of [`capture_selection`]: the clipboard, the copy
/// simulation, and the settle delays are injected so unit tests can pin the
/// acquisition and clipboard-restore policy without uinput or a real
/// clipboard. See [`capture_selection`] for the behavioral contract.
async fn capture_selection_impl<F>(
    clipboard: &dyn ClipboardBackend,
    copy: F,
    settle_delay: Duration,
    read_delay: Duration,
) -> Result<String, String>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    // Trust a non-empty primary selection directly — no copy, no equality
    // check, and no clipboard write of any kind.
    let primary = clipboard.get_primary_selection().unwrap_or_default();
    if !primary.is_empty() {
        return Ok(primary);
    }

    // No primary selection: fall back to a simulated copy + clipboard read.
    // `None` means the clipboard held something that isn't text.
    let saved_clipboard = clipboard.get_text().ok();
    if saved_clipboard.is_none() {
        // Post-#77 a non-text clipboard is a reliable `Err`, so this is
        // detectable before the copy — but the copy is still the only way to
        // capture the selection, so the overwrite is accepted (#80).
        warn!(
            "clipboard holds non-text content (image, files, ...); \
             the simulated copy will overwrite it and it cannot be restored"
        );
    }

    // Let any briefly-held hotkey modifiers clear before synthesizing Ctrl+C.
    tokio::time::sleep(settle_delay).await;

    match tokio::task::spawn_blocking(copy).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("failed to copy selection: {e}")),
        Err(e) => return Err(format!("copy task panicked: {e}")),
    }

    tokio::time::sleep(read_delay).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Scripted clipboard double.
    ///
    /// `texts` is the sequence of `get_text` results — `Some(text)` for a
    /// text clipboard, `None` for non-text content (an image or a file list:
    /// the real backends return `Err` for those post-#77). Each `get_text`
    /// call consumes the front entry; the last entry is sticky. `writes`
    /// records every `set_text`, so tests can assert the restore policy
    /// byte-for-byte — including that nothing is ever written.
    struct ScriptedClipboard {
        primary: String,
        texts: Mutex<Vec<Option<String>>>,
        writes: Mutex<Vec<String>>,
    }

    impl ScriptedClipboard {
        fn new(primary: &str, texts: &[Option<&str>]) -> Self {
            Self {
                primary: primary.to_string(),
                texts: Mutex::new(texts.iter().map(|t| t.map(String::from)).collect()),
                writes: Mutex::new(Vec::new()),
            }
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl ClipboardBackend for ScriptedClipboard {
        fn get_text(&self) -> anyhow::Result<String> {
            let mut texts = self.texts.lock().unwrap();
            assert!(!texts.is_empty(), "get_text called with no scripted result");
            let head = if texts.len() > 1 {
                texts.remove(0)
            } else {
                texts[0].clone()
            };
            head.ok_or_else(|| anyhow::anyhow!("clipboard holds non-text content"))
        }

        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            self.writes.lock().unwrap().push(text.to_string());
            Ok(())
        }

        fn get_primary_selection(&self) -> anyhow::Result<String> {
            Ok(self.primary.clone())
        }
    }

    /// A copy simulation that only records whether it fired.
    fn tracking_copy(fired: Arc<AtomicBool>) -> impl FnOnce() -> anyhow::Result<()> + Send {
        move || {
            fired.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn primary_selection_never_touches_the_clipboard() {
        // The #80 repro: an image on the clipboard (non-text => scripted
        // `None`), text in the primary selection. No copy fires and nothing
        // is ever written back — the old set-path restore wrote "" here,
        // destroying the image.
        let clipboard = ScriptedClipboard::new("selected text", &[None]);
        let fired = Arc::new(AtomicBool::new(false));

        let result = capture_selection_impl(
            &clipboard,
            tracking_copy(Arc::clone(&fired)),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok("selected text".to_string()));
        assert!(!fired.load(Ordering::SeqCst), "no copy should fire");
        assert!(
            clipboard.writes().is_empty(),
            "clipboard must stay untouched"
        );
    }

    #[tokio::test]
    async fn copy_fallback_restores_a_text_clipboard() {
        // saved = "original", post-copy read = "copied text", still there at
        // restore time => the original text is written back, once.
        let clipboard = ScriptedClipboard::new(
            "",
            &[Some("original"), Some("copied text"), Some("copied text")],
        );
        let fired = Arc::new(AtomicBool::new(false));

        let result = capture_selection_impl(
            &clipboard,
            tracking_copy(Arc::clone(&fired)),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok("copied text".to_string()));
        assert!(fired.load(Ordering::SeqCst), "copy fallback should fire");
        assert_eq!(clipboard.writes(), vec!["original".to_string()]);
    }

    #[tokio::test]
    async fn copy_fallback_never_restores_over_a_non_text_clipboard() {
        // The original clipboard is an image (get_text errs). The copy
        // overwrites it — inherent to copying — but no "" restore follows:
        // an unreadable original means there is nothing we could faithfully
        // put back.
        let clipboard = ScriptedClipboard::new("", &[None, Some("copied text")]);
        let fired = Arc::new(AtomicBool::new(false));

        let result = capture_selection_impl(
            &clipboard,
            tracking_copy(Arc::clone(&fired)),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok("copied text".to_string()));
        assert!(fired.load(Ordering::SeqCst), "copy fallback should fire");
        assert!(clipboard.writes().is_empty(), "must not restore \"\"");
    }

    #[tokio::test]
    async fn unchanged_clipboard_means_no_selection_and_no_restore() {
        // The copy captured nothing: the clipboard still holds the original,
        // so there is nothing to undo and no selection to return.
        let clipboard = ScriptedClipboard::new("", &[Some("original"), Some("original")]);
        let fired = Arc::new(AtomicBool::new(false));

        let result = capture_selection_impl(
            &clipboard,
            tracking_copy(Arc::clone(&fired)),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(
            result,
            Err("no text selected — select some text first".to_string())
        );
        assert!(clipboard.writes().is_empty());
    }

    #[tokio::test]
    async fn clipboard_replaced_after_copy_skips_restore() {
        // Something else copied over our simulated Ctrl+C between the
        // post-copy read and the restore: leave the intruder alone.
        let clipboard = ScriptedClipboard::new(
            "",
            &[Some("original"), Some("copied text"), Some("intruder")],
        );
        let fired = Arc::new(AtomicBool::new(false));

        let result = capture_selection_impl(
            &clipboard,
            tracking_copy(Arc::clone(&fired)),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok("copied text".to_string()));
        assert!(clipboard.writes().is_empty());
    }

    #[tokio::test]
    async fn copy_failure_surfaces_an_error_without_writing() {
        let clipboard = ScriptedClipboard::new("", &[Some("original")]);

        let result = capture_selection_impl(
            &clipboard,
            || anyhow::bail!("uinput unavailable"),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;

        assert_eq!(
            result,
            Err("failed to copy selection: uinput unavailable".to_string())
        );
        assert!(clipboard.writes().is_empty());
    }

    #[test]
    fn instruction_from_capture_trims_and_rejects_whitespace() {
        assert_eq!(
            instruction_from_capture("  fix grammar\n"),
            Some("fix grammar".to_string())
        );
        assert_eq!(instruction_from_capture("   \n\t"), None);
        assert_eq!(instruction_from_capture(""), None);
    }
}
