//! Niri window tracking via `niri msg` commands.

use std::process::Command;

use anyhow::Context;
use serde::Deserialize;
use tracing::debug;

use super::WindowTracker;

/// Window tracker for the Niri compositor.
///
/// Uses `niri msg --json focused-window` to query the current focus and
/// `niri msg action focus-window --id <ID>` to restore it. Niri's CLI is the
/// stable boundary here; talking to the IPC socket directly would save one
/// process spawn, but it would also duplicate request framing that the CLI
/// already owns.
pub struct NiriTracker;

impl Default for NiriTracker {
    fn default() -> Self {
        Self
    }
}

impl NiriTracker {
    pub fn new() -> Self {
        Self
    }
}

/// Parsed JSON output from `niri msg --json focused-window`.
#[derive(Debug, Deserialize)]
struct NiriFocusedWindow {
    /// Niri's stable window ID, used by targeted window actions.
    id: u64,
    /// Wayland application ID, equivalent to the class-like identifier used by
    /// terminal-aware command mode.
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NiriFocusedWindowResponse {
    /// Current `niri msg --json focused-window` output.
    FocusedWindow(NiriFocusedWindow),
    /// Raw IPC response shape documented for direct socket access.
    IpcEnvelope {
        #[serde(rename = "Ok")]
        ok: NiriIpcOk,
    },
}

#[derive(Debug, Deserialize)]
enum NiriIpcOk {
    FocusedWindow(NiriFocusedWindow),
}

impl NiriFocusedWindowResponse {
    fn into_window(self) -> NiriFocusedWindow {
        match self {
            Self::FocusedWindow(window) => window,
            Self::IpcEnvelope {
                ok: NiriIpcOk::FocusedWindow(window),
            } => window,
        }
    }
}

impl WindowTracker for NiriTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        let focused_window = query_focused_window()?;

        debug!("niri focused window id: {}", focused_window.id);
        Ok(focused_window.id.to_string())
    }

    fn get_focused_window_class(&self) -> Option<String> {
        let focused_window = match query_focused_window() {
            Ok(focused_window) => focused_window,
            Err(err) => {
                debug!("niri focused window class: query failed: {err}");
                return None;
            }
        };

        match focused_window.app_id.filter(|app_id| !app_id.is_empty()) {
            Some(app_id) => {
                debug!("niri focused window class: {app_id}");
                Some(app_id)
            }
            None => {
                debug!("niri focused window class: empty (no app_id reported)");
                None
            }
        }
    }

    fn focus_window(&self, id: &str) -> anyhow::Result<()> {
        let id: u64 = id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid niri window id: {id}"))?;

        debug!("focusing niri window id: {id}");

        let output = Command::new("niri")
            .args(["msg", "action", "focus-window", "--id", &id.to_string()])
            .output()
            .context("failed to run niri msg action focus-window")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("niri msg action focus-window failed: {stderr}");
        }

        Ok(())
    }
}

fn query_focused_window() -> anyhow::Result<NiriFocusedWindow> {
    let output = Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .output()
        .context("failed to run niri msg --json focused-window — is Niri running?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("niri msg --json focused-window failed: {stderr}");
    }

    parse_focused_window(&output.stdout)
}

fn parse_focused_window(stdout: &[u8]) -> anyhow::Result<NiriFocusedWindow> {
    let parsed: NiriFocusedWindowResponse =
        serde_json::from_slice(stdout).context("failed to parse niri focused-window JSON")?;

    Ok(parsed.into_window())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_focused_window_cli_json() {
        let window = parse_focused_window(
            br#"{"id":12,"title":"shell","app_id":"Alacritty","workspace_id":6,"is_focused":true}"#,
        )
        .unwrap();

        assert_eq!(window.id, 12);
        assert_eq!(window.app_id.as_deref(), Some("Alacritty"));
    }

    #[test]
    fn parses_focused_window_ipc_envelope() {
        let window = parse_focused_window(
            br#"{"Ok":{"FocusedWindow":{"id":12,"title":"shell","app_id":"Alacritty","workspace_id":6,"is_focused":true}}}"#,
        )
        .unwrap();

        assert_eq!(window.id, 12);
        assert_eq!(window.app_id.as_deref(), Some("Alacritty"));
    }
}
