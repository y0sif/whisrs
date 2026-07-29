//! Clipboard implementations — Wayland (wl-copy/wl-paste), X11 (arboard), and noop.

use crate::ClipboardBackend;
use anyhow::Context;
use std::process::Command;

// ---------------------------------------------------------------------------
// Wayland: shell out to wl-paste / wl-copy
// ---------------------------------------------------------------------------

/// Run `wl-paste` with the given extra args (e.g. `--primary`) and return its
/// text, distinguishing a genuinely empty clipboard from one holding
/// non-text content (image, files, ...).
///
/// wl-clipboard reports these as two different messages: an empty clipboard
/// (no selection owner at all) says "Nothing is copied"; a selection that
/// exists but doesn't offer a text MIME type says "Clipboard content is not
/// available as \[inferred output|requested\] type ...". Only the former is a
/// legitimate empty string — the latter must surface as an error, or a
/// caller restoring a saved clipboard value (see `whisrs`'s paste-injection
/// path) would overwrite non-text content with `""`.
///
/// Matched case-insensitively since wl-clipboard's exact capitalization has
/// varied across versions; verified against wl-clipboard 2.2.1's actual
/// wording (embedded strings), which no longer matches the older "no
/// suitable type" phrasing this used to check for.
fn run_wl_paste(extra_args: &[&str], command_desc: &str) -> anyhow::Result<String> {
    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .args(extra_args)
        .output()
        .context("failed to run wl-paste — is wl-clipboard installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_empty_clipboard_message(&stderr) {
            return Ok(String::new());
        }
        anyhow::bail!("{command_desc} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a `wl-paste` stderr message means "the clipboard has no selection
/// at all" (a legitimate empty string), as opposed to "a selection exists but
/// isn't text" (which must propagate as an error — see [`run_wl_paste`]).
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
}
