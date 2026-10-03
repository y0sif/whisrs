//! `whisrsd --quiet`: log startup, then nothing.
//!
//! Runtime logs carry window classes, selected text and transcripts, so in
//! quiet mode the daemon stops writing anything once it is in use. The cut
//! happens on the first command (socket, hotkey or tray) rather than at the end
//! of `main`, so the tray, overlay and hotkey listener, which start in the
//! background, still get their startup lines out.
//!
//! The cut is made at the file descriptor, not the tracing filter: whisper.cpp,
//! ALSA and hook child processes write to stderr directly, and a filter would
//! not stop them.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SILENCED: Once = Once::new();

/// Turn quiet mode on. Output continues until the first [`enter_runtime`].
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Called before a command is handled. In quiet mode, the first call points
/// stdout and stderr at `/dev/null` for the rest of the process's life.
pub fn enter_runtime() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    SILENCED.call_once(|| {
        tracing::info!("quiet mode: startup complete, runtime logging disabled");
        let devnull = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("quiet mode: cannot open /dev/null, logging stays on: {e}");
                return;
            }
        };
        // SAFETY: dup2 onto the standard descriptors. Every writer goes
        // through fd 1/2 by number, so they simply land in /dev/null from here
        // on; `devnull`'s own descriptor is closed when it drops.
        unsafe {
            libc::dup2(devnull.as_raw_fd(), libc::STDOUT_FILENO);
            libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO);
        }
    });
}
