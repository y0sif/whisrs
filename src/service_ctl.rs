//! Daemon control: restarting the whisrs daemon through systemd.

/// Outcome of an attempt to restart the daemon via systemd.
///
/// The CLI and `whisrs config` both need to nudge the daemon after writing a
/// new `config.toml`. They want the same systemd detection logic but different
/// output formatting (e.g. ANSI colors only when stdout is a TTY), so this
/// helper returns a structured outcome instead of printing directly.
#[derive(Debug)]
pub enum RestartOutcome {
    /// `systemctl --user restart whisrs.service` succeeded.
    Restarted,
    /// No `whisrs.service` user unit is loaded — caller should show fallback hints.
    NoSystemdUnit,
    /// systemd is installed but the restart command failed (non-zero exit).
    Failed,
}

/// Restart the whisrs daemon via systemd if a user unit is loaded.
///
/// Returns [`RestartOutcome::NoSystemdUnit`] without running anything when the
/// unit isn't present; callers can fall back to printing manual instructions.
pub fn restart_daemon_via_systemd() -> RestartOutcome {
    if !has_systemd_unit() {
        return RestartOutcome::NoSystemdUnit;
    }
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "whisrs.service"])
        .status();
    match status {
        Ok(s) if s.success() => RestartOutcome::Restarted,
        _ => RestartOutcome::Failed,
    }
}

/// Returns `true` when `whisrs.service` is loaded as a user unit.
fn has_systemd_unit() -> bool {
    // `is-enabled` exits 0 for enabled/static/linked units and non-zero when
    // the unit isn't loaded. Falls back to `list-unit-files` for distros where
    // `is-enabled` may exit non-zero on static/linked units.
    let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "whisrs.service"])
        .output()
    else {
        return false;
    };
    if output.status.success() {
        return true;
    }
    let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "list-unit-files", "whisrs.service"])
        .output()
    else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).contains("whisrs.service")
}
