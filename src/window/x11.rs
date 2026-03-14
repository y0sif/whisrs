//! X11 window tracking via the `x11rb` crate.

use anyhow::Context;
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, InputFocus, Window};
use x11rb::rust_connection::RustConnection;

use super::WindowTracker;

/// Window tracker for X11 sessions.
///
/// Queries `_NET_ACTIVE_WINDOW` to get the focused window and uses
/// `set_input_focus` to restore it.
pub struct X11Tracker {
    conn: RustConnection,
    root: Window,
    net_active_window_atom: u32,
}

impl X11Tracker {
    /// Connect to the X11 display and look up needed atoms.
    pub fn new() -> anyhow::Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("failed to connect to X11 display")?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let atom_cookie = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?;
        let atom_reply = atom_cookie
            .reply()
            .context("failed to intern _NET_ACTIVE_WINDOW atom")?;

        Ok(Self {
            conn,
            root,
            net_active_window_atom: atom_reply.atom,
        })
    }
}

impl WindowTracker for X11Tracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        let cookie = self.conn.get_property(
            false,
            self.root,
            self.net_active_window_atom,
            AtomEnum::WINDOW,
            0,
            1,
        )?;

        let reply = cookie
            .reply()
            .context("failed to get _NET_ACTIVE_WINDOW property")?;

        if reply.value_len == 0 {
            anyhow::bail!("_NET_ACTIVE_WINDOW returned empty value");
        }

        // The property value is a 32-bit window ID.
        let window_id = u32::from_ne_bytes(
            reply.value[..4]
                .try_into()
                .context("unexpected _NET_ACTIVE_WINDOW value length")?,
        );

        if window_id == 0 {
            anyhow::bail!("no active window (root focused)");
        }

        debug!("X11 focused window: 0x{window_id:x}");
        Ok(window_id.to_string())
    }

    fn focus_window(&self, id: &str) -> anyhow::Result<()> {
        let window_id: u32 = id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid X11 window ID: {id}"))?;

        debug!("focusing X11 window: 0x{window_id:x}");

        self.conn
            .set_input_focus(InputFocus::PARENT, window_id, x11rb::CURRENT_TIME)?;
        self.conn
            .flush()
            .context("failed to flush X11 connection")?;

        Ok(())
    }
}
