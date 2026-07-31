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

/// Clipboard backend that uses the `arboard` crate (X11).
#[cfg(feature = "arboard")]
pub struct X11Clipboard;

#[cfg(feature = "arboard")]
impl ClipboardBackend for X11Clipboard {
    fn get_text(&self) -> anyhow::Result<String> {
        let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
        clipboard
            .get_text()
            .context("failed to get text from X11 clipboard")
    }

    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
        clipboard
            .set_text(text)
            .context("failed to set text on X11 clipboard")
    }

    fn get_primary_selection(&self) -> anyhow::Result<String> {
        use arboard::GetExtLinux;
        let mut clipboard = arboard::Clipboard::new().context("failed to open X11 clipboard")?;
        clipboard
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
}
