//! Init-system abstraction for controlling the whisrs user service.
//!
//! whisrs is a per-session daemon, so it is managed by whatever runs the
//! user's *session* services. That was originally assumed to be systemd, which
//! left users on OpenRC distros (Gentoo, Artix, Alpine, Devuan) being told to
//! run `systemctl` commands that do not exist on their machine.
//!
//! Everything init-specific lives here: detection, the control commands, and
//! the command strings shown in help text. Callers ask the detected
//! [`ServiceManager`] what to do rather than hardcoding a command.

use std::path::{Path, PathBuf};
use std::process::Command;

/// systemd unit name, as installed under `~/.config/systemd/user`.
pub const SYSTEMD_UNIT: &str = "whisrs.service";

/// OpenRC service name, as installed under `~/.config/rc/init.d`.
pub const OPENRC_SERVICE: &str = "whisrs";

/// Which init system manages the user's session services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    /// systemd user manager, driven by `systemctl --user`.
    Systemd,
    /// OpenRC in user mode, driven by `rc-service --user` (OpenRC >= 0.55).
    OpenRc,
    /// Neither detected — the daemon has to be started by hand.
    None,
}

/// Outcome of an attempt to restart the daemon through the service manager.
///
/// The CLI and `whisrs config` both nudge the daemon after writing a new
/// `config.toml`. They share this detection logic but format output
/// differently (e.g. ANSI colors only when stdout is a TTY), so this is a
/// structured outcome rather than something printed directly.
#[derive(Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The restart command succeeded.
    Restarted,
    /// No whisrs service is installed — the caller should show fallback hints.
    NoService,
    /// A service manager is present but the restart command failed.
    Failed,
}

impl ServiceManager {
    /// Detect the init system managing this session.
    ///
    /// Both systemd and OpenRC advertise themselves with a runtime directory,
    /// which is more reliable than looking for the client binaries on `$PATH`:
    /// `systemctl` is often installed on non-systemd machines as a dependency
    /// of some other package, and would otherwise produce a false positive.
    pub fn detect() -> Self {
        Self::detect_in(Path::new("/run"))
    }

    /// [`Self::detect`] against an arbitrary `/run`, so tests can fake it.
    fn detect_in(run_dir: &Path) -> Self {
        // sd_booted(3): this directory exists if and only if the system was
        // booted with systemd.
        if run_dir.join("systemd/system").is_dir() {
            Self::Systemd
        } else if run_dir.join("openrc").is_dir() {
            Self::OpenRc
        } else {
            Self::None
        }
    }

    /// Human-readable name, for log lines and prompts.
    pub fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::OpenRc => "OpenRC",
            Self::None => "no service manager",
        }
    }

    /// Whether a whisrs service is installed for this manager.
    pub fn service_installed(self) -> bool {
        match self {
            // `is-enabled` exits 0 for enabled/static/linked units and
            // non-zero when the unit isn't loaded. Fall back to
            // `list-unit-files` for distros where `is-enabled` exits non-zero
            // on static or linked units.
            Self::Systemd => {
                if run_ok("systemctl", &["--user", "is-enabled", SYSTEMD_UNIT]) {
                    return true;
                }
                match capture_ok("systemctl", &["--user", "list-unit-files", SYSTEMD_UNIT]) {
                    Some(out) => out.contains(SYSTEMD_UNIT),
                    None => false,
                }
            }
            // `rc-service --user -e <svc>` exits 0 when the init script
            // exists, 1 when it does not.
            Self::OpenRc => run_ok("rc-service", &["--user", "-e", OPENRC_SERVICE]),
            Self::None => false,
        }
    }

    /// Whether the daemon is currently running under this manager.
    ///
    /// Only distinguishes active from not — callers use this to decide
    /// whether a restart is meaningful, not to report failure detail.
    pub fn is_active(self) -> bool {
        match self {
            Self::Systemd => {
                matches!(
                    capture("systemctl", &["--user", "is-active", SYSTEMD_UNIT]).as_deref(),
                    Some(out) if out.trim() == "active"
                )
            }
            // `status` exits 0 when started and 3 when stopped.
            Self::OpenRc => run_ok("rc-service", &["--user", OPENRC_SERVICE, "status"]),
            Self::None => false,
        }
    }

    /// Restart the daemon, if a service is installed for this manager.
    ///
    /// Returns [`RestartOutcome::NoService`] without running anything when no
    /// service is installed, so callers can print manual instructions instead.
    pub fn restart(self) -> RestartOutcome {
        if !self.service_installed() {
            return RestartOutcome::NoService;
        }
        let ok = match self {
            Self::Systemd => run_ok("systemctl", &["--user", "restart", SYSTEMD_UNIT]),
            Self::OpenRc => run_ok("rc-service", &["--user", OPENRC_SERVICE, "restart"]),
            Self::None => return RestartOutcome::NoService,
        };
        if ok {
            RestartOutcome::Restarted
        } else {
            RestartOutcome::Failed
        }
    }

    /// [`Self::restart`] from inside the service it restarts
    /// ([`restart_self`]): the restart is queued, not waited for. Waiting
    /// races this process's own stop, which ends the waiting command too
    /// (`systemctl` exits 143) and reads as a failure. systemd queues the
    /// job (`--no-block`); OpenRC has no such flag, so `rc-service` is
    /// started in a process group of its own and not waited for.
    pub fn restart_queued(self) -> RestartOutcome {
        if !self.service_installed() {
            return RestartOutcome::NoService;
        }
        let Some((program, args)) = self.queued_restart() else {
            return RestartOutcome::NoService;
        };
        let ok = match self {
            Self::OpenRc => spawn_detached(program, args),
            _ => run_ok(program, args),
        };
        if ok {
            RestartOutcome::Restarted
        } else {
            RestartOutcome::Failed
        }
    }

    /// The command [`Self::restart_queued`] runs.
    fn queued_restart(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Systemd => Some((
                "systemctl",
                &["--user", "restart", "--no-block", SYSTEMD_UNIT],
            )),
            Self::OpenRc => Some(("rc-service", &["--user", OPENRC_SERVICE, "restart"])),
            Self::None => None,
        }
    }

    /// Command that enables the service to start at login, and starts it now.
    ///
    /// `None` when no service manager was detected, in which case callers
    /// should fall back to [`Self::manual_start_hint`].
    pub fn enable_hint(self) -> Option<&'static str> {
        match self {
            Self::Systemd => Some("systemctl --user enable --now whisrs.service"),
            Self::OpenRc => {
                Some("rc-update --user add whisrs default && rc-service --user whisrs start")
            }
            Self::None => None,
        }
    }

    /// Command that restarts the running daemon.
    pub fn restart_hint(self) -> Option<&'static str> {
        match self {
            Self::Systemd => Some("systemctl --user restart whisrs.service"),
            Self::OpenRc => Some("rc-service --user whisrs restart"),
            Self::None => None,
        }
    }

    /// Command that reports service status.
    pub fn status_hint(self) -> Option<&'static str> {
        match self {
            Self::Systemd => Some("systemctl --user status whisrs.service"),
            Self::OpenRc => Some("rc-service --user whisrs status"),
            Self::None => None,
        }
    }

    /// Fallback for when there is no service manager to delegate to.
    pub fn manual_start_hint(self) -> &'static str {
        "whisrsd &"
    }

    /// Directory the service definition is installed into.
    pub fn unit_dir(self) -> Option<PathBuf> {
        let config = dirs::config_dir()?;
        match self {
            Self::Systemd => Some(config.join("systemd/user")),
            Self::OpenRc => Some(config.join("rc/init.d")),
            Self::None => None,
        }
    }
}

/// Restart this whisrsd from inside it: a profile switch, the tray's
/// "Restart Daemon". Returns at once; the work runs on a thread of its own.
///
/// Under its service (systemd: its cgroup is `whisrs.service`; OpenRC:
/// `RC_SVCNAME=whisrs`) the service manager restarts it. Otherwise, or with
/// no service installed, it re-execs itself (`/proc/self/exe`, the same
/// arguments): with a unit installed but the daemon started by hand,
/// [`ServiceManager::restart`] would start a second daemon that exits on the
/// socket this one holds, and `Restart=on-failure` would retry it for good.
///
/// The service's restart is queued ([`ServiceManager::restart_queued`]); its
/// stop sends SIGTERM, whose handler shuts the daemon down. A re-exec runs
/// `prepare` first instead, the same shutdown, and starts the binary the
/// daemon's path names now, so an upgrade takes effect.
///
/// When nothing restarted, `on_failure` gets why. Before `prepare` the daemon
/// keeps running and keeps its socket; after it (a failed exec) the daemon
/// is shut down, so the process exits once `on_failure` has run.
pub fn restart_self(
    socket: PathBuf,
    prepare: impl FnOnce() + Send + 'static,
    on_failure: impl FnOnce(String) + Send + 'static,
) {
    std::thread::spawn(move || {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok();
        let rc_svcname = std::env::var("RC_SVCNAME").ok();
        if let Some(manager) = running_service(cgroup.as_deref(), rc_svcname.as_deref()) {
            match manager.restart_queued() {
                // Queued: the manager stops this process next.
                RestartOutcome::Restarted => return,
                RestartOutcome::Failed => {
                    let (program, args) = manager.queued_restart().unwrap_or_default();
                    on_failure(format!("`{program} {}` failed", args.join(" ")));
                    return;
                }
                RestartOutcome::NoService => {}
            }
        }
        let failed = reexec(&socket, prepare);
        let prepared = failed.prepared;
        on_failure(failed.why);
        if prepared {
            // Shut down already: end as a stop does, once the notification
            // `on_failure` sent has had a moment to go out.
            std::thread::sleep(std::time::Duration::from_secs(1));
            let _ = std::fs::remove_file(&socket);
            std::process::exit(1);
        }
    });
}

/// The service this process runs as, if it runs as one: its cgroup (the
/// text of `/proc/self/cgroup`) ends in `/whisrs.service`, or OpenRC set
/// `RC_SVCNAME` to `whisrs`.
fn running_service(cgroup: Option<&str>, rc_svcname: Option<&str>) -> Option<ServiceManager> {
    let unit = format!("/{SYSTEMD_UNIT}");
    if cgroup.is_some_and(|text| text.lines().any(|line| line.trim_end().ends_with(&unit))) {
        Some(ServiceManager::Systemd)
    } else if rc_svcname == Some(OPENRC_SERVICE) {
        Some(ServiceManager::OpenRc)
    } else {
        None
    }
}

/// Where the socket waits while this daemon re-execs: put back if the exec
/// fails, so the daemon stays reachable. The new daemon removes it.
pub fn socket_aside(socket: &Path) -> PathBuf {
    let mut aside = socket.as_os_str().to_owned();
    aside.push(".restart");
    PathBuf::from(aside)
}

/// A re-exec that did not happen.
struct ExecFailed {
    why: String,
    /// `prepare` ran: the daemon is shut down.
    prepared: bool,
}

/// Replace this process with a fresh start of itself ([`exec_target`]), the
/// same arguments. `prepare` runs once the binary is found, just before.
/// Returns only when the exec did not happen.
fn reexec(socket: &Path, prepare: impl FnOnce()) -> ExecFailed {
    use std::os::unix::process::CommandExt;

    let link = std::fs::read_link("/proc/self/exe").ok();
    let target = exec_target(link.as_deref(), Path::is_file);
    if let Err(e) = std::fs::metadata(&target) {
        return ExecFailed {
            why: format!("could not restart whisrsd: {}: {e}", target.display()),
            prepared: false,
        };
    }
    prepare();
    let aside = socket_aside(socket);
    let moved = std::fs::rename(socket, &aside).is_ok();
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| "whisrsd".into());
    let error = Command::new(&target).arg0(arg0).args(args).exec();
    if moved {
        let _ = std::fs::rename(&aside, socket);
    }
    ExecFailed {
        why: format!("could not restart whisrsd ({error}), so it stopped: start it again"),
        prepared: true,
    }
}

/// What a re-exec runs: the path `/proc/self/exe` links to, a trailing
/// ` (deleted)` cut off, when a file is there (an upgrade put the new binary
/// in place of this one), else `/proc/self/exe`, this process's own binary
/// even when deleted.
fn exec_target(link: Option<&Path>, exists: impl Fn(&Path) -> bool) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    const THIS: &str = "/proc/self/exe";
    let Some(link) = link else {
        return PathBuf::from(THIS);
    };
    let bytes = link.as_os_str().as_bytes();
    let path = Path::new(std::ffi::OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ));
    if exists(path) {
        path.to_path_buf()
    } else {
        PathBuf::from(THIS)
    }
}

/// Start a command in a process group of its own, and leave it: it is
/// reaped on a thread, its outcome unread. Whether it started.
fn spawn_detached(program: &str, args: &[&str]) -> bool {
    use std::os::unix::process::CommandExt;

    let child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match child {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Run a command, reporting whether it exited successfully.
///
/// A missing binary is indistinguishable from a failed run here, which is what
/// callers want: both mean "this service manager cannot do the thing".
fn run_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout, or `None` if it could not run.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// [`capture`], but `None` unless the command also exited successfully.
///
/// Used where stdout alone is not evidence: `systemctl list-unit-files` still
/// prints a header when it finds nothing, so a substring match on failed output
/// could report a unit that does not exist.
fn capture_ok(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_systemd_from_runtime_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("systemd/system")).unwrap();
        assert_eq!(
            ServiceManager::detect_in(tmp.path()),
            ServiceManager::Systemd
        );
    }

    #[test]
    fn detects_openrc_from_runtime_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("openrc")).unwrap();
        assert_eq!(
            ServiceManager::detect_in(tmp.path()),
            ServiceManager::OpenRc
        );
    }

    #[test]
    fn detects_none_when_no_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(ServiceManager::detect_in(tmp.path()), ServiceManager::None);
    }

    #[test]
    fn prefers_systemd_when_both_markers_exist() {
        // Some systemd machines ship OpenRC's runtime dir via a compat
        // package; systemd is the one actually running the session.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("systemd/system")).unwrap();
        std::fs::create_dir_all(tmp.path().join("openrc")).unwrap();
        assert_eq!(
            ServiceManager::detect_in(tmp.path()),
            ServiceManager::Systemd
        );
    }

    #[test]
    fn no_manager_reports_nothing_installed_and_no_hints() {
        let none = ServiceManager::None;
        assert!(!none.service_installed());
        assert!(!none.is_active());
        assert_eq!(none.restart(), RestartOutcome::NoService);
        assert!(none.enable_hint().is_none());
        assert!(none.restart_hint().is_none());
        assert!(none.status_hint().is_none());
    }

    /// Only the whisrs unit's own cgroup, or OpenRC's service name for it,
    /// counts as running under the service: a daemon started by hand on a
    /// machine with the unit installed re-execs itself instead.
    #[test]
    fn running_service_reads_the_cgroup_and_rc_svcname() {
        let unit = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/whisrs.service\n";
        assert_eq!(
            running_service(Some(unit), None),
            Some(ServiceManager::Systemd)
        );
        let v1 =
            "12:pids:/user.slice\n1:name=systemd:/user.slice/user@1000.service/whisrs.service\n";
        assert_eq!(
            running_service(Some(v1), None),
            Some(ServiceManager::Systemd)
        );
        for by_hand in [
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foot.scope\n",
            "0::/user.slice/user-1000.slice/session-2.scope\n",
            "0::/user.slice/user@1000.service/app.slice/my-whisrs.service.d\n",
            "",
        ] {
            assert_eq!(running_service(Some(by_hand), None), None, "{by_hand}");
        }
        assert_eq!(running_service(None, None), None);
        assert_eq!(
            running_service(None, Some("whisrs")),
            Some(ServiceManager::OpenRc)
        );
        assert_eq!(running_service(None, Some("pipewire")), None);
    }

    /// A restart from inside the service is queued, never waited for: a
    /// waiting `systemctl` dies with the unit's stop and reads as a failure.
    /// `whisrs restart` keeps waiting.
    #[test]
    fn a_restart_from_inside_is_queued() {
        assert_eq!(
            ServiceManager::Systemd.queued_restart(),
            Some((
                "systemctl",
                &["--user", "restart", "--no-block", "whisrs.service"][..]
            ))
        );
        assert_eq!(
            ServiceManager::OpenRc.queued_restart(),
            Some(("rc-service", &["--user", "whisrs", "restart"][..]))
        );
        assert_eq!(ServiceManager::None.queued_restart(), None);
        assert_eq!(
            ServiceManager::None.restart_queued(),
            RestartOutcome::NoService
        );
        assert!(ServiceManager::Systemd
            .restart_hint()
            .is_some_and(|hint| !hint.contains("--no-block")));
    }

    /// A detached command starts and is not waited for.
    #[test]
    fn a_detached_command_is_not_waited_for() {
        let started = std::time::Instant::now();
        assert!(spawn_detached("sleep", &["5"]));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!spawn_detached("/nonexistent/whisrs-no-such-program", &[]));
    }

    /// A re-exec runs the binary at the daemon's path when one is there (an
    /// upgrade replaced the deleted one), else this process's own.
    #[test]
    fn a_reexec_runs_the_binary_at_the_path() {
        let installed = |p: &Path| p == Path::new("/usr/bin/whisrsd");
        assert_eq!(
            exec_target(Some(Path::new("/usr/bin/whisrsd (deleted)")), installed),
            Path::new("/usr/bin/whisrsd")
        );
        assert_eq!(
            exec_target(Some(Path::new("/usr/bin/whisrsd")), installed),
            Path::new("/usr/bin/whisrsd")
        );
        let gone = |_: &Path| false;
        assert_eq!(
            exec_target(Some(Path::new("/usr/bin/whisrsd (deleted)")), gone),
            Path::new("/proc/self/exe")
        );
        assert_eq!(exec_target(None, installed), Path::new("/proc/self/exe"));
    }

    #[test]
    fn the_socket_waits_beside_itself() {
        assert_eq!(
            socket_aside(Path::new("/run/user/1000/whisrs.sock")),
            Path::new("/run/user/1000/whisrs.sock.restart")
        );
    }

    #[test]
    fn hints_name_the_right_tool_for_each_manager() {
        for (mgr, tool) in [
            (ServiceManager::Systemd, "systemctl"),
            (ServiceManager::OpenRc, "rc-service"),
        ] {
            assert!(mgr.restart_hint().unwrap().contains(tool));
            assert!(mgr.status_hint().unwrap().contains(tool));
        }
        // Enabling under OpenRC goes through rc-update, not rc-service.
        assert!(ServiceManager::OpenRc
            .enable_hint()
            .unwrap()
            .contains("rc-update"));
        assert!(ServiceManager::Systemd
            .enable_hint()
            .unwrap()
            .contains("systemctl"));
    }
}
