//! Windows window tracking via Win32 API.
//!
//! Uses `GetForegroundWindow` to get the focused window and
//! `SetForegroundWindow` to restore focus.

use tracing::debug;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

use super::WindowTracker;

/// Window tracker for Windows using Win32 API.
pub struct Win32Tracker;

impl Win32Tracker {
    pub fn new() -> Self {
        Self
    }
}

impl WindowTracker for Win32Tracker {
    fn get_focused_window(&self) -> anyhow::Result<String> {
        let hwnd = unsafe { GetForegroundWindow() };

        if hwnd.0.is_null() {
            anyhow::bail!("no foreground window found");
        }

        let id = hwnd.0 as usize;
        debug!("Win32 focused window HWND: 0x{id:x}");
        Ok(id.to_string())
    }

    fn focus_window(&self, id: &str) -> anyhow::Result<()> {
        let raw: usize = id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Win32 HWND: {id}"))?;

        let hwnd = HWND(raw as *mut _);
        debug!("focusing Win32 window HWND: 0x{raw:x}");

        unsafe {
            SetForegroundWindow(hwnd);
        }

        Ok(())
    }
}
