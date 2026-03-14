//! Sway/i3 window tracking via the `swayipc` crate.

use swayipc::Connection;
use tracing::debug;

use super::WindowTracker;

/// Window tracker for Sway (and i3 with sway-compatible IPC).
///
/// Uses `swayipc` to query the window tree and focus windows by con_id.
pub struct SwayTracker;

impl Default for SwayTracker {
    fn default() -> Self {
        Self
    }
}

impl SwayTracker {
    pub fn new() -> Self {
        Self
    }
}

impl WindowTracker for SwayTracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        let mut conn = Connection::new().map_err(|e| anyhow::anyhow!("sway IPC connect: {e}"))?;
        let tree = conn
            .get_tree()
            .map_err(|e| anyhow::anyhow!("sway get_tree: {e}"))?;

        // Walk the tree to find the focused node.
        let focused = find_focused(&tree);
        match focused {
            Some(id) => {
                debug!("sway focused window con_id: {id}");
                Ok(id.to_string())
            }
            None => anyhow::bail!("no focused window found in sway tree"),
        }
    }

    fn focus_window(&self, id: &str) -> anyhow::Result<()> {
        let con_id: i64 = id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid sway con_id: {id}"))?;

        debug!("focusing sway window con_id: {con_id}");

        let mut conn = Connection::new().map_err(|e| anyhow::anyhow!("sway IPC connect: {e}"))?;
        let command = format!("[con_id={con_id}] focus");
        let results = conn
            .run_command(&command)
            .map_err(|e| anyhow::anyhow!("sway run_command: {e}"))?;

        for result in results {
            if let Err(e) = result {
                anyhow::bail!("sway focus command failed: {e}");
            }
        }

        Ok(())
    }
}

/// Recursively find the focused node in the sway tree, returning its `id`.
fn find_focused(node: &swayipc::Node) -> Option<i64> {
    if node.focused {
        return Some(node.id);
    }
    for child in &node.nodes {
        if let Some(id) = find_focused(child) {
            return Some(id);
        }
    }
    for child in &node.floating_nodes {
        if let Some(id) = find_focused(child) {
            return Some(id);
        }
    }
    None
}
