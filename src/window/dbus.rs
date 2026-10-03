//! D-Bus window tracking stub for GNOME and KDE.
//!
//! Neither desktop exposes focus tracking to an unprivileged daemon out of the
//! box. On GNOME, `org.gnome.Shell.Introspect` returns exactly the right
//! fields but is allow listed to the GTK and portal backends, and Mutter
//! implements neither foreign-toplevel protocol; a Shell extension is the only
//! route. On KDE it would take the `org_kde_plasma_window_management` Wayland
//! protocol, which whisrs does not speak. See #72 and #127 for the protocol
//! surveys behind those two sentences.
//!
//! So this backend cannot answer, and the only honest thing left is to say so
//! clearly once instead of failing quietly on every dictation.

use tracing::{debug, warn};

use super::WindowTracker;

/// Stub window tracker for GNOME/KDE desktops via D-Bus.
pub struct DbusTracker {
    desktop: String,
}

impl DbusTracker {
    /// Warns once, at construction, rather than once per call.
    ///
    /// The daemon builds exactly one tracker ([`super::detect_tracker`]) and
    /// every method below runs on each dictation, so the per-call warnings
    /// this replaces were noise repeating a limitation the user can do nothing
    /// about. One startup line naming both consequences is more useful than a
    /// hundred lines naming neither.
    pub fn new(desktop: &str) -> Self {
        warn!(
            "{desktop} exposes no unprivileged focused-window query, so window tracking is off: \
             focus is not restored after recording (text goes wherever focus ends up), and \
             terminal detection never fires, which leaves the terminal-aware selection copy \
             and [input] terminal_classes inactive; the line-break guards treat every window \
             as a possible terminal unless [input] unknown_window_is_terminal = false"
        );
        Self {
            desktop: desktop.to_string(),
        }
    }
}

impl WindowTracker for DbusTracker {
    /// A placeholder id, so the caller's save-then-restore flow still runs.
    ///
    /// `debug!` rather than `warn!`: [`DbusTracker::new`] already warned once,
    /// and this runs on every dictation.
    fn get_focused_window(&self) -> anyhow::Result<String> {
        debug!(
            "{} window tracking not supported; returning a stub id",
            self.desktop
        );
        Ok("dbus-stub".to_string())
    }

    /// A no-op, deliberately `Ok`: focus restoration failing is not a reason to
    /// abandon a transcript the user already spoke.
    fn focus_window(&self, _id: &str) -> anyhow::Result<()> {
        debug!("{} focus restoration not supported; skipping", self.desktop);
        Ok(())
    }

    /// Always `None`, which is what keeps terminal detection off here.
    ///
    /// Required rather than defaulted since #71: a backend that cannot answer
    /// has to say so explicitly, because the defaulted `None` this replaced
    /// disabled terminal detection on four platforms without a compile error.
    fn get_focused_window_class(&self) -> Option<String> {
        debug!(
            "{} focused window class not supported; terminal detection stays off",
            self.desktop
        );
        None
    }
}
