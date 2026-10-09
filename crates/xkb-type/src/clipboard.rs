//! Clipboard implementations — Wayland (wl-copy/wl-paste), X11 (arboard), and noop.

use crate::ClipboardBackend;
use anyhow::Context;
use std::process::Command;

// ---------------------------------------------------------------------------
// Wayland: shell out to wl-paste / wl-copy
// ---------------------------------------------------------------------------

/// MIME types treated as "this clipboard offer is text" — the standard
/// `text/plain` variants plus the legacy X11-selection type names wl-paste
/// also reports under Xwayland interop.
const TEXT_MIME_TYPES: &[&str] = &[
    "text/plain",
    "text/plain;charset=utf-8",
    "TEXT",
    "STRING",
    "UTF8_STRING",
];

/// Run `wl-paste` with the given extra args (e.g. `--primary`) and return its
/// text, distinguishing a genuinely empty clipboard from one holding
/// non-text content (image, files, ...).
///
/// Does **not** rely on `wl-paste`'s own "inferred type" content negotiation
/// (a bare `wl-paste --no-newline` with no explicit `--type`): that path was
/// observed, live, to sometimes return a non-text selection's raw bytes as
/// if they were text — even immediately after copying an image, with
/// `wl-paste --list-types` correctly reporting only `image/png` on offer.
/// The negotiation wl-paste performs when no type is given is apparently not
/// reliable across all wl-clipboard/compositor/clipboard-manager
/// combinations (reproduced under KDE Plasma 6 / KWin with Klipper active).
///
/// Instead, this always queries `--list-types` first (a plain listing, no
/// negotiation involved) and only issues a real read with an explicit
/// `--type text/plain` if a text MIME type is actually listed. If the list is
/// empty, the clipboard genuinely has no selection — a legitimate empty
/// string. If it's non-empty but contains no text type, the selection holds
/// non-text content and this errors, so a caller restoring a saved clipboard
/// value (see `whisrs`'s paste-injection path) doesn't mistake "can't read
/// this" for "empty" and overwrite it with `""`.
fn run_wl_paste(extra_args: &[&str], command_desc: &str) -> anyhow::Result<String> {
    let list_output = Command::new("wl-paste")
        .arg("--list-types")
        .args(extra_args)
        .output()
        .context("failed to run wl-paste --list-types — is wl-clipboard installed?")?;

    if !list_output.status.success() {
        let stderr = String::from_utf8_lossy(&list_output.stderr);
        if is_empty_clipboard_message(&stderr) {
            return Ok(String::new());
        }
        anyhow::bail!("{command_desc} --list-types failed: {stderr}");
    }

    let types = String::from_utf8_lossy(&list_output.stdout);
    let offered: Vec<&str> = types.lines().map(str::trim).collect();

    if offered.is_empty() {
        // No selection owner at all — genuinely empty clipboard.
        return Ok(String::new());
    }

    if !offers_text(&offered) {
        anyhow::bail!(
            "{command_desc}: clipboard holds non-text content (offered types: {offered:?})"
        );
    }

    let output = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text/plain"])
        .args(extra_args)
        .output()
        .context("failed to run wl-paste — is wl-clipboard installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{command_desc} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether any of `offered` (as reported by `wl-paste --list-types`) is a
/// text MIME type.
fn offers_text(offered: &[&str]) -> bool {
    offered.iter().any(|t| TEXT_MIME_TYPES.contains(t))
}

/// Whether a `wl-paste` stderr message means "the clipboard has no selection
/// at all" (a legitimate empty string). Kept as a defensive fallback in case
/// `--list-types` itself ever errors instead of returning an empty listing.
fn is_empty_clipboard_message(stderr: &str) -> bool {
    stderr.to_lowercase().contains("nothing is copied")
}

/// Clipboard backend that shells out to `wl-paste` (get) and `wl-copy` (set).
pub struct WaylandClipboard;

impl ClipboardBackend for WaylandClipboard {
    fn get_text(&self) -> anyhow::Result<String> {
        run_wl_paste(&[], "wl-paste")
    }

    fn set_text(&self, text: &str) -> anyhow::Result<()> {
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
            #[cfg(feature = "logging")]
            log::warn!("wl-copy exited with status {status}");
        }

        Ok(())
    }

    fn get_primary_selection(&self) -> anyhow::Result<String> {
        run_wl_paste(&["--primary"], "wl-paste --primary")
    }
}

// ---------------------------------------------------------------------------
// X11: arboard crate (behind "arboard" feature)
// ---------------------------------------------------------------------------

/// The one process-wide `arboard` handle behind every [`X11Clipboard`] call.
///
/// X11 has no clipboard store: the app that copied must keep serving the
/// selection. arboard treats dropping its last handle as the app exiting, so
/// it asks the clipboard manager to take the data (waiting at most 100 ms)
/// and destroys its selection window. The old per-call handle therefore gave
/// the selection away milliseconds after each copy, and the Ctrl+V that
/// followed raced the clipboard manager or compositor, often pasting nothing
/// (#193). This handle is opened on first use and never dropped.
///
/// A failed open is returned, not cached, so a later call retries. The guard
/// must not be held across another clipboard call: the lock is not reentrant.
#[cfg(feature = "arboard")]
fn x11_clipboard() -> anyhow::Result<std::sync::MutexGuard<'static, arboard::Clipboard>> {
    use std::sync::{Mutex, OnceLock, PoisonError};

    static CLIPBOARD: OnceLock<Mutex<arboard::Clipboard>> = OnceLock::new();

    if CLIPBOARD.get().is_none() {
        let clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
        // A lost race drops this extra handle, which is harmless: the
        // winner's handle still exists, so arboard keeps the selection.
        let _ = CLIPBOARD.set(Mutex::new(clipboard));
    }
    let clipboard = CLIPBOARD.get().expect("X11 clipboard was just initialized");
    // A panic in another clipboard call leaves the handle itself usable.
    Ok(clipboard.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Whether nothing owns the X11 CLIPBOARD selection, i.e. the clipboard is
/// genuinely empty: nothing copied since login, or the app that copied exited
/// and no clipboard manager took over.
///
/// arboard returns the same `ContentNotAvailable` error when nothing owns the
/// selection, when the owner offers no text target, and when the owner times
/// out. Only "no owner" means empty: a caller restoring a saved clipboard
/// reads `Err` as "non-text, do not overwrite it", so `whisrs` typed every
/// paste-mode dictation on an empty X11 clipboard (#79). Asking the X server
/// directly gives [`X11Clipboard`] the same contract as `run_wl_paste` on
/// Wayland. Uses its own short-lived connection, not the arboard handle, and
/// only runs after a read failed.
#[cfg(feature = "arboard")]
fn x11_clipboard_unowned() -> anyhow::Result<bool> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, _screen) = x11rb::connect(None).context("failed to connect to X server")?;
    let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let owner = conn.get_selection_owner(clipboard)?.reply()?.owner;
    Ok(owner == x11rb::NONE)
}

/// Map an arboard text read to the [`ClipboardBackend::get_text`] contract.
///
/// `ContentNotAvailable` becomes `Ok("")` only when `is_unowned` (in
/// production [`x11_clipboard_unowned`]) confirms nothing owns CLIPBOARD. A
/// negative or failed check keeps the error, so an uncertain clipboard is
/// still protected. The probe runs only for `ContentNotAvailable`.
#[cfg(feature = "arboard")]
fn text_or_empty_if_unowned(
    result: Result<String, arboard::Error>,
    is_unowned: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<String> {
    match result {
        Err(arboard::Error::ContentNotAvailable) if matches!(is_unowned(), Ok(true)) => {
            Ok(String::new())
        }
        result => result.context("failed to get text from X11 clipboard"),
    }
}

/// Clipboard backend that uses the `arboard` crate (X11).
/// All instances share one handle that lives as long as the process.
#[cfg(feature = "arboard")]
pub struct X11Clipboard;

#[cfg(feature = "arboard")]
impl ClipboardBackend for X11Clipboard {
    fn get_text(&self) -> anyhow::Result<String> {
        // Bound first so the handle's guard drops before the owner check.
        let result = x11_clipboard()?.get_text();
        text_or_empty_if_unowned(result, x11_clipboard_unowned)
    }

    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        x11_clipboard()?
            .set_text(text)
            .context("failed to set text on X11 clipboard")
    }

    fn get_primary_selection(&self) -> anyhow::Result<String> {
        use arboard::GetExtLinux;
        x11_clipboard()?
            .get()
            .clipboard(arboard::LinuxClipboardKind::Primary)
            .text()
            .context("failed to get text from X11 primary selection")
    }
}

// When arboard is not available, X11Clipboard is not available — callers on
// X11 without the feature will get NoopClipboard from default_clipboard().
#[cfg(not(feature = "arboard"))]
pub struct X11Clipboard;

#[cfg(not(feature = "arboard"))]
impl ClipboardBackend for X11Clipboard {
    fn get_text(&self) -> anyhow::Result<String> {
        anyhow::bail!("X11Clipboard requires the 'arboard' feature");
    }
    fn set_text(&self, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("X11Clipboard requires the 'arboard' feature");
    }
    fn get_primary_selection(&self) -> anyhow::Result<String> {
        anyhow::bail!("X11Clipboard requires the 'arboard' feature");
    }
}

// ---------------------------------------------------------------------------
// Noop clipboard
// ---------------------------------------------------------------------------

/// Clipboard backend that never succeeds or fails — all operations are no-ops.
pub struct NoopClipboard;

impl ClipboardBackend for NoopClipboard {
    fn get_text(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn set_text(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_primary_selection(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------
// Auto-detection
// ---------------------------------------------------------------------------

/// Return the appropriate clipboard backend for the current display server.
///
/// Checks `WAYLAND_DISPLAY` to decide: Wayland if set, X11 otherwise.
pub fn default_clipboard() -> Box<dyn ClipboardBackend> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        Box::new(WaylandClipboard)
    } else {
        Box::new(X11Clipboard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact message wl-clipboard emits for a truly empty clipboard (no
    /// selection owner at all), verified against wl-clipboard 2.2.1's
    /// embedded strings.
    #[test]
    fn recognizes_truly_empty_clipboard() {
        assert!(is_empty_clipboard_message("Nothing is copied\n"));
        // Case-insensitive: wording/casing has varied across wl-clipboard
        // versions.
        assert!(is_empty_clipboard_message("nothing is copied\n"));
    }

    /// A selection that exists but isn't text must NOT be treated as empty —
    /// doing so is exactly the bug this fixes: a caller restoring a saved
    /// clipboard value after a paste would silently overwrite an image or
    /// file selection with `""`.
    #[test]
    fn does_not_treat_non_text_content_as_empty() {
        assert!(!is_empty_clipboard_message(
            "Clipboard content is not available as inferred output type \"text/plain\"\n\
             Use \"wl-paste --list-types\" to view available types."
        ));
        assert!(!is_empty_clipboard_message(
            "Clipboard content is not available as requested type \"text/plain\"\n\
             Use \"wl-paste --list-types\" to view available types."
        ));
    }

    #[test]
    fn does_not_match_stale_no_suitable_type_phrasing() {
        // The old pattern this used to check for; confirm it alone (without
        // "nothing is copied") is correctly treated as an error, not empty.
        assert!(!is_empty_clipboard_message(
            "no suitable type of content copied"
        ));
    }

    /// `offers_text` is what `get_text`/`get_primary_selection` now gate on
    /// (via `wl-paste --list-types`) instead of trusting wl-paste's own
    /// "inferred type" negotiation — that negotiation was observed, live, to
    /// sometimes return an image selection's raw bytes as if they were text,
    /// even when `--list-types` correctly listed only `image/png`. Listing
    /// types first and only reading with an explicit `--type text/plain`
    /// when a text type is actually present avoids depending on that
    /// negotiation at all.
    #[test]
    fn offers_text_true_for_plain_text_types() {
        assert!(offers_text(&["text/plain"]));
        assert!(offers_text(&["text/plain;charset=utf-8"]));
        assert!(offers_text(&["TEXT"]));
        assert!(offers_text(&["STRING"]));
        assert!(offers_text(&["UTF8_STRING"]));
        assert!(offers_text(&["image/png", "text/plain"]));
    }

    #[test]
    fn offers_text_false_for_image_only() {
        assert!(!offers_text(&["image/png"]));
        assert!(!offers_text(&[
            "image/png",
            "application/x-qt-image",
            "image/bmp"
        ]));
    }

    #[test]
    fn offers_text_false_for_empty_list() {
        assert!(!offers_text(&[]));
    }

    /// `text_or_empty_if_unowned`: only a confirmed unowned CLIPBOARD may
    /// turn `ContentNotAvailable` into `""` (#79).
    #[cfg(feature = "arboard")]
    mod x11 {
        use super::super::text_or_empty_if_unowned;
        use std::cell::Cell;

        /// Run the helper with a probe that records whether it was called.
        fn run(
            result: Result<String, arboard::Error>,
            probe: anyhow::Result<bool>,
        ) -> (anyhow::Result<String>, bool) {
            let called = Cell::new(false);
            let out = text_or_empty_if_unowned(result, || {
                called.set(true);
                probe
            });
            (out, called.get())
        }

        /// Unwrap the error and check it carries the X11 context.
        fn x11_error(out: anyhow::Result<String>) -> anyhow::Error {
            let err = out.expect_err("must stay an error");
            assert!(format!("{err:#}").contains("failed to get text from X11 clipboard"));
            err
        }

        fn is_content_not_available(err: &anyhow::Error) -> bool {
            matches!(
                err.downcast_ref::<arboard::Error>(),
                Some(arboard::Error::ContentNotAvailable)
            )
        }

        #[test]
        fn text_is_returned_without_owner_check() {
            let (out, called) = run(Ok("hello".into()), Ok(true));
            assert_eq!(out.unwrap(), "hello");
            assert!(!called);
        }

        #[test]
        fn content_not_available_with_no_owner_is_empty() {
            let (out, called) = run(Err(arboard::Error::ContentNotAvailable), Ok(true));
            assert_eq!(out.unwrap(), "");
            assert!(called);
        }

        /// An owner without a text target (image, files) must not read as
        /// empty, or a restore would overwrite it with `""`.
        #[test]
        fn content_not_available_with_owner_stays_error() {
            let (out, called) = run(Err(arboard::Error::ContentNotAvailable), Ok(false));
            assert!(is_content_not_available(&x11_error(out)));
            assert!(called);
        }

        /// A failed owner check is uncertain, so it fails safe.
        #[test]
        fn content_not_available_with_failed_owner_check_stays_error() {
            let (out, called) = run(
                Err(arboard::Error::ContentNotAvailable),
                Err(anyhow::anyhow!("no X server")),
            );
            assert!(is_content_not_available(&x11_error(out)));
            assert!(called);
        }

        #[test]
        fn other_error_stays_error_without_owner_check() {
            let (out, called) = run(Err(arboard::Error::ClipboardOccupied), Ok(true));
            let err = x11_error(out);
            assert!(matches!(
                err.downcast_ref::<arboard::Error>(),
                Some(arboard::Error::ClipboardOccupied)
            ));
            assert!(!called);
        }
    }
}
