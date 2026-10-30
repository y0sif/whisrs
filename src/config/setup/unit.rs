//! The user service: whether one is installed and runs the whisrsd on PATH,
//! writing it, repointing a stale one (issue #146) and enabling it.
//!
//! systemd units are repointed in place, so the user's own edits survive;
//! OpenRC init scripts are written and enabled, never repointed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Run;
use crate::service::{ServiceManager, OPENRC_SERVICE, SYSTEMD_UNIT};

/// What is at the service file's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitFile {
    Missing,
    /// It runs the whisrsd installed now (or is an OpenRC script, which is
    /// not repointed).
    Current,
    /// It runs `binary`, another whisrsd than `new`, the one on PATH now.
    Stale {
        binary: String,
        new: String,
    },
    /// It is there, but setup cannot tell what it runs (unreadable, no
    /// `ExecStart=`, a quoted binary, a specifier other than `%h`), so it
    /// is left alone.
    Foreign,
}

/// Where `manager`'s whisrs service file goes.
pub fn path(manager: ServiceManager) -> Option<PathBuf> {
    let dir = manager.unit_dir()?;
    match manager {
        ServiceManager::Systemd => Some(dir.join(SYSTEMD_UNIT)),
        ServiceManager::OpenRc => Some(dir.join(OPENRC_SERVICE)),
        ServiceManager::None => None,
    }
}

/// The service file of `manager` as it is now, against the whisrsd on PATH.
pub fn status(manager: ServiceManager) -> UnitFile {
    let Some(dest) = path(manager) else {
        return UnitFile::Missing;
    };
    status_of(
        manager,
        &dest,
        &super::which_whisrsd(),
        dirs::home_dir().as_deref(),
        &systemd_search_path(),
    )
}

/// [`status`] of the file at `dest`, against `whisrsd`; `home` expands `%h`
/// and `search_path` is systemd's own, for a bare `ExecStart=whisrsd`.
pub fn status_of(
    manager: ServiceManager,
    dest: &Path,
    whisrsd: &str,
    home: Option<&Path>,
    search_path: &[PathBuf],
) -> UnitFile {
    if !dest.exists() {
        return UnitFile::Missing;
    }
    if manager != ServiceManager::Systemd {
        return UnitFile::Current;
    }
    let Ok(contents) = fs::read_to_string(dest) else {
        return UnitFile::Foreign;
    };
    let Some(binary) = exec_start_binary(&contents) else {
        return UnitFile::Foreign;
    };
    let quoted = binary.starts_with(['"', '\'']) || binary.contains('\\');
    let unknown_specifier = binary.contains('%')
        && home.is_none_or(|home| expand_unit_specifiers(binary, home).is_none());
    if quoted || unknown_specifier {
        return UnitFile::Foreign;
    }
    if unit_needs_repoint(binary, Path::new(whisrsd), home, search_path) {
        UnitFile::Stale {
            binary: binary.to_string(),
            new: whisrsd.to_string(),
        }
    } else {
        UnitFile::Current
    }
}

/// Write `manager`'s service file to `dest`, running `whisrsd`: the file in
/// `contrib/` (`contrib` finds it) with its `ExecStart=` made absolute, or
/// an inline copy where `contrib/` is not on disk (`cargo install`). An
/// OpenRC script is made executable, and its conf.d tunables are installed
/// in `confd_dir` when missing. What it did besides, to say.
pub fn write(
    manager: ServiceManager,
    dest: &Path,
    whisrsd: &str,
    contrib: impl Fn(&str) -> Option<PathBuf>,
    confd_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let contrib_name = match manager {
        ServiceManager::Systemd => "whisrs.service",
        ServiceManager::OpenRc => "openrc/whisrs.initd",
        ServiceManager::None => anyhow::bail!("no service manager to write a service for"),
    };
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    match contrib(contrib_name) {
        // Keep the packaged unit, but write an absolute ExecStart. systemd
        // resolves a bare `ExecStart=whisrsd` against a compiled-in search
        // path that never contains ~/.cargo/bin, so the shipped line only
        // works when the binary landed in /usr/bin or /usr/local/bin.
        Some(src) if manager == ServiceManager::Systemd => {
            let contents = fs::read_to_string(&src).context("failed to read service file")?;
            fs::write(dest, rewrite_exec_start(&contents, whisrsd))
                .context("failed to write service file")?;
        }
        Some(src) => {
            fs::copy(&src, dest).context("failed to copy service file")?;
        }
        None => {
            let content = match manager {
                ServiceManager::Systemd => systemd_unit_contents(whisrsd),
                _ => openrc_initd_contents(whisrsd),
            };
            fs::write(dest, content).context("failed to write service file")?;
        }
    }
    let mut notes = Vec::new();
    // OpenRC init scripts are executed directly, so they must be executable:
    // a copied or freshly written file is not, by default.
    if manager == ServiceManager::OpenRc {
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
            .context("failed to make init script executable")?;
        if let Some(dir) = confd_dir {
            notes.extend(install_openrc_confd(dir, &contrib));
        }
    }
    Ok(notes)
}

/// Install the OpenRC conf.d tunables file in `dir`, next to the init
/// script; what to say about it.
///
/// The init script defaults every value it reads, so a missing conf.d is not a
/// failure, but README and docs/troubleshooting.md both point users at
/// `~/.config/rc/conf.d/whisrs`, so it should exist for them to edit. Never
/// overwrites: the file holds user settings and may hold API keys.
fn install_openrc_confd(dir: &Path, contrib: &impl Fn(&str) -> Option<PathBuf>) -> Option<String> {
    let dest = dir.join(OPENRC_SERVICE);
    if dest.exists() {
        return None;
    }
    // Inline fallback (`cargo install`, no contrib/ on disk) ships no
    // conf.d; the init script's defaults cover it.
    let src = contrib("openrc/whisrs.confd")?;
    if let Err(e) = fs::create_dir_all(dir) {
        return Some(format!("could not create {}: {e}", dir.display()));
    }
    Some(match fs::copy(&src, &dest) {
        Ok(_) => {
            // May hold API keys, so keep it owner-only rather than the 0644
            // the README suggests for a bare tunables file.
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o600));
            format!("installed tunables to {}", dest.display())
        }
        Err(e) => format!("could not install conf.d tunables: {e}"),
    })
}

/// Repoint the systemd unit at `dest` to run `whisrsd`, in place, and
/// reload systemd. The daemon keeps running the old binary until it
/// restarts, which is the caller's: a restart would stop a dictation.
pub fn repoint(dest: &Path, whisrsd: &str, run: Run) -> Result<()> {
    let contents =
        fs::read_to_string(dest).with_context(|| format!("failed to read {}", dest.display()))?;
    fs::write(dest, rewrite_exec_start(&contents, whisrsd))
        .context("failed to write service file")?;
    // Restarting without a successful reload would start the old ExecStart
    // again, and report success while doing it.
    run("systemctl", &["--user", "daemon-reload"]).map_err(|why| {
        anyhow::anyhow!(
            "the service file was updated, but systemctl --user daemon-reload failed, so the \
             service still runs the old binary ({why})"
        )
    })
}

/// Enable the service to start at login, and start it now.
pub fn enable(manager: ServiceManager, run: Run) -> Result<()> {
    let result = match manager {
        ServiceManager::Systemd => {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
            run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])
        }
        ServiceManager::OpenRc => run("rc-update", &["--user", "add", OPENRC_SERVICE, "default"])
            .and_then(|()| run("rc-service", &["--user", OPENRC_SERVICE, "start"])),
        ServiceManager::None => Err("no service manager".to_string()),
    };
    result.map_err(|why| {
        let hint = manager.enable_hint().unwrap_or("whisrsd &");
        anyhow::anyhow!("{why}; you can do it by hand: {hint}")
    })
}

/// Repoint a systemd unit's `ExecStart=` at `whisrsd_path`, leaving the rest
/// of the file alone.
///
/// Only the binary token is replaced. Everything after it is kept, so a unit
/// that grows a flag does not silently lose it, and systemd's execution
/// prefixes (`@-:+!`, which sit *before* the binary) are kept too. Both
/// details are load-bearing in opposite directions: replacing the whole line
/// drops arguments, and replacing from the `=` swallows the prefix and turns
/// `@whisrsd argv0` into a stray argument. `contrib/whisrs.service` exercises
/// neither shape today, which is exactly why they are pinned by tests.
///
/// `ExecStartPre=` and `ExecStartPost=` are deliberately untouched: they are
/// not the daemon, and rewriting them to the daemon path would be nonsense.
fn rewrite_exec_start(contents: &str, whisrsd_path: &str) -> String {
    let mut out = String::with_capacity(contents.len() + whisrsd_path.len());
    for line in contents.lines() {
        match parse_exec_start(line) {
            Some((prefix, _, args)) => {
                out.push_str("ExecStart=");
                out.push_str(prefix);
                out.push_str(whisrsd_path);
                // Everything after the binary is the caller's, keep it verbatim.
                if let Some(args) = args {
                    out.push(' ');
                    out.push_str(args);
                }
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Split an `ExecStart=` line into (execution prefixes, binary, arguments).
///
/// Shared by [`rewrite_exec_start`] and [`exec_start_binary`] so the reader
/// and the writer cannot disagree about which token is the binary. `None` for
/// any other line, including `ExecStartPre=`/`ExecStartPost=`: their name is
/// not followed by `=`, so the prefix strip already rejects them.
fn parse_exec_start(line: &str) -> Option<(&str, &str, Option<&str>)> {
    let command = line.strip_prefix("ExecStart=")?;
    // systemd strips whitespace around the value, so `ExecStart= x` names the
    // binary `x`. Splitting before trimming would re-emit that name as an
    // argument to the path we just resolved.
    let command = command.trim_start();
    let prefixes: &[char] = &['@', '-', ':', '+', '!'];
    let rest = command.trim_start_matches(prefixes);
    let prefix = &command[..command.len() - rest.len()];
    Some(match rest.split_once(char::is_whitespace) {
        Some((binary, args)) => (prefix, binary, Some(args)),
        None => (prefix, rest, None),
    })
}

/// The binary a systemd unit's first `ExecStart=` runs, without prefixes or
/// arguments. `None` when the unit has no `ExecStart=` or it names nothing.
fn exec_start_binary(contents: &str) -> Option<&str> {
    let (_, binary, _) = contents.lines().find_map(parse_exec_start)?;
    (!binary.is_empty()).then_some(binary)
}

/// Whether a unit running `unit_binary` should be repointed at `resolved`.
///
/// Never repoints at a path that does not exist: `which_whisrsd` falls back
/// to `~/.cargo/bin/whisrsd` without checking it, and swapping a working
/// unit for a missing binary would break it. Paths are compared after
/// canonicalizing, because a plain string compare flags `/bin/whisrsd`
/// against `/usr/bin/whisrsd` on distros where `/bin` is a symlink.
///
/// Never repoints at a relative path either: `which` prints `./whisrsd` for a
/// relative PATH entry, and a relative ExecStart breaks the unit.
///
/// `home` expands `%h` in the unit token; a token with any other specifier is
/// left alone, since it cannot be resolved reliably here. A bare name is
/// looked up in `search_path`, systemd's own binary search path (see
/// [`systemd_search_path`]), not the user's PATH.
fn unit_needs_repoint(
    unit_binary: &str,
    resolved: &Path,
    home: Option<&Path>,
    search_path: &[PathBuf],
) -> bool {
    if !resolved.is_absolute() {
        return false;
    }
    // Quoting and escapes can put spaces inside the binary, which the
    // whitespace split in `parse_exec_start` cannot see, and a rewrite would
    // leave an unbalanced quote. Leave such a unit alone.
    if unit_binary.starts_with(['"', '\'']) || unit_binary.contains('\\') {
        return false;
    }
    let Ok(resolved) = fs::canonicalize(resolved) else {
        return false;
    };
    let unit_binary = if unit_binary.contains('%') {
        let Some(home) = home else {
            return false;
        };
        match expand_unit_specifiers(unit_binary, home) {
            Some(expanded) => PathBuf::from(expanded),
            None => return false,
        }
    } else {
        PathBuf::from(unit_binary)
    };
    let current = if unit_binary.is_absolute() {
        unit_binary
    } else if unit_binary.components().count() == 1 {
        // A bare name: systemd takes the first search dir that has it. Found
        // nowhere means the unit cannot start as written.
        match search_path
            .iter()
            .map(|dir| dir.join(&unit_binary))
            .find(|candidate| candidate.exists())
        {
            Some(found) => found,
            None => return true,
        }
    } else {
        // A relative path with a directory is not valid in ExecStart.
        return true;
    };
    // A unit binary that no longer exists (uninstalled package) is stale.
    match fs::canonicalize(current) {
        Ok(current) => current != resolved,
        Err(_) => true,
    }
}

/// Expand the systemd specifiers setup can resolve in an `ExecStart=` token:
/// `%h` (the user's home) and `%%` (a literal `%`).
///
/// `None` when any other specifier is present, including a trailing lone `%`,
/// so the caller can leave a unit it does not fully understand alone.
fn expand_unit_specifiers(token: &str, home: &Path) -> Option<String> {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(home.to_str()?),
            Some('%') => out.push('%'),
            _ => return None,
        }
    }
    Some(out)
}

/// The directories systemd searches for a bare `ExecStart=` name.
///
/// Asks `systemd-path search-binaries-default`, falling back to the usual
/// compiled-in default when that fails. This is deliberately not the user's
/// PATH: the service manager never sees it.
fn systemd_search_path() -> Vec<PathBuf> {
    const FALLBACK: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin";
    let output = std::process::Command::new("systemd-path")
        .arg("search-binaries-default")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    output
        .as_deref()
        .unwrap_or(FALLBACK)
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Inline systemd unit, used when `contrib/whisrs.service` isn't on disk.
fn systemd_unit_contents(whisrsd_path: &str) -> String {
    format!(
        "[Unit]\n\
         Description=whisrs dictation daemon\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={whisrsd_path}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         PassEnvironment=HYPRLAND_INSTANCE_SIGNATURE NIRI_SOCKET SWAYSOCK WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Inline OpenRC init script, used when `contrib/openrc/` isn't on disk.
///
/// This is a reduced version of `contrib/openrc/whisrs.initd`: it keeps the
/// supervision and session-environment recovery, which the daemon cannot work
/// without, but drops the conf.d tunables. Users who want those should install
/// the full script from contrib.
fn openrc_initd_contents(whisrsd_path: &str) -> String {
    format!(
        "#!/sbin/openrc-run\n\
         # whisrs dictation daemon: OpenRC user service\n\
         \n\
         description=\"whisrs dictation daemon\"\n\
         command=\"{whisrsd_path}\"\n\
         supervisor=\"supervise-daemon\"\n\
         respawn_delay=3\n\
         respawn_max=5\n\
         respawn_period=60\n\
         \n\
         : \"${{whisrsd_log_dir:=${{XDG_STATE_HOME:-$HOME/.local/state}}/whisrs}}\"\n\
         output_log=\"${{whisrsd_log_dir}}/whisrsd.log\"\n\
         error_log=\"${{whisrsd_log_dir}}/whisrsd.log\"\n\
         \n\
         # conf.d variables are sourced, not exported; re-export the ones\n\
         # whisrsd reads from its own environment, or they are silently lost.\n\
         for _var in RUST_LOG \\\n\
         \tWHISRS_DEEPGRAM_API_KEY WHISRS_GROQ_API_KEY WHISRS_OPENAI_API_KEY \\\n\
         \tWHISRS_ASR_SIDECAR_API_KEY \\\n\
         \tXKB_DEFAULT_LAYOUT XKB_DEFAULT_VARIANT\n\
         do\n\
         \teval \"[ -n \\\"\\${{$_var}}\\\" ]\" && export \"$_var\"\n\
         done\n\
         \n\
         start_pre() {{\n\
         \tcheckpath -d -m 0700 \"$whisrsd_log_dir\" || return 1\n\
         \n\
         \t# OpenRC scrubs the environment, so recover the session vars the\n\
         \t# daemon needs. The compositor's environ holds what it inherited;\n\
         \t# what it created after exec is derived from the runtime dir.\n\
         \tfor _p in $(pgrep -u \"$(id -u)\" -x 'Hyprland|sway|niri|gnome-shell|kwin_wayland|labwc|river' 2>/dev/null); do\n\
         \t\tfor _v in DBUS_SESSION_BUS_ADDRESS XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XAUTHORITY; do\n\
         \t\t\teval \"[ -n \\\"\\${{$_v}}\\\" ]\" && continue\n\
         \t\t\t_val=$(tr '\\0' '\\n' < \"/proc/$_p/environ\" 2>/dev/null | sed -n \"s/^$_v=//p\" | head -1)\n\
         \t\t\t[ -n \"$_val\" ] && export \"$_v=$_val\"\n\
         \t\tdone\n\
         \t\tbreak\n\
         \tdone\n\
         \t[ -n \"$WAYLAND_DISPLAY\" ] || for _s in \"$XDG_RUNTIME_DIR\"/wayland-[0-9]*; do\n\
         \t\t[ -S \"$_s\" ] && export WAYLAND_DISPLAY=\"${{_s##*/}}\" && break\n\
         \tdone\n\
         \t[ -n \"$HYPRLAND_INSTANCE_SIGNATURE\" ] || for _d in $(ls -1td \"$XDG_RUNTIME_DIR\"/hypr/*/ 2>/dev/null); do\n\
         \t\texport HYPRLAND_INSTANCE_SIGNATURE=\"$(basename \"$_d\")\"; break\n\
         \tdone\n\
         \t[ -n \"$DISPLAY\" ] || for _p in $(pgrep -u \"$(id -u)\" -x Xwayland 2>/dev/null); do\n\
         \t\t# Compositors that pass -displayfd have no :N in argv; exporting\n\
         \t\t# an empty DISPLAY is worse than leaving it unset.\n\
         \t\t_val=$(tr '\\0' '\\n' < \"/proc/$_p/cmdline\" 2>/dev/null | sed -n '/^:[0-9][0-9]*$/p' | head -1)\n\
         \t\t[ -n \"$_val\" ] && export DISPLAY=\"$_val\"\n\
         \t\tbreak\n\
         \tdone\n\
         \t[ -n \"$SWAYSOCK\" ] || for _s in \"$XDG_RUNTIME_DIR\"/sway-ipc.*.sock; do\n\
         \t\t[ -S \"$_s\" ] && export SWAYSOCK=\"$_s\" && break\n\
         \tdone\n\
         \t[ -n \"$NIRI_SOCKET\" ] || for _s in \"$XDG_RUNTIME_DIR\"/niri.*.sock; do\n\
         \t\t[ -S \"$_s\" ] && export NIRI_SOCKET=\"$_s\" && break\n\
         \tdone\n\
         \tif [ -z \"$DBUS_SESSION_BUS_ADDRESS\" ] && [ -S \"$XDG_RUNTIME_DIR/bus\" ]; then\n\
         \t\texport DBUS_SESSION_BUS_ADDRESS=\"unix:path=$XDG_RUNTIME_DIR/bus\"\n\
         \tfi\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::setup::find_contrib_file;

    /// Slice out the `for _var in ... done` conf.d re-export loop.
    ///
    /// Searching the whole script for a variable name is also satisfied by a
    /// name that only appears in the comment above the loop, which is exactly
    /// the drift being guarded against, so the assertions below run against
    /// the loop alone.
    fn confd_reexport_loop(script: &str) -> &str {
        const END: &str = "\ndone\n";
        let start = script
            .find("for _var in ")
            .expect("OpenRC script must re-export conf.d variables");
        let from_loop = &script[start..];
        let end = from_loop
            .find(END)
            .expect("conf.d re-export loop must be terminated by `done`")
            + END.len();
        &from_loop[..end]
    }

    /// `whisrs setup` rewrites the packaged unit's `ExecStart=` instead of
    /// copying it, because systemd resolves a bare `whisrsd` against a
    /// compiled-in search path that never contains `~/.cargo/bin`. Verified
    /// directly: a user unit with `ExecStart=whisrsd` fails to run while the
    /// same unit with an absolute path starts, on the same binary.
    #[test]
    fn exec_start_rewrite_repoints_the_binary() {
        let out = rewrite_exec_start("[Service]\nExecStart=whisrsd\n", "/opt/bin/whisrsd");
        assert_eq!(out, "[Service]\nExecStart=/opt/bin/whisrsd\n");
    }

    /// The obvious spelling of the rewrite replaces the whole line, which
    /// throws away every argument. `contrib/whisrs.service` carries none
    /// today, so nothing would have caught it: the first flag added to the
    /// unit would just stop reaching the daemon, on the install path least
    /// likely to be re-tested.
    #[test]
    fn exec_start_rewrite_keeps_arguments() {
        let out = rewrite_exec_start("ExecStart=whisrsd --foo bar\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=/opt/bin/whisrsd --foo bar\n");

        // Not `split_once(' ')`: systemd tokenises on any whitespace run, and a
        // space-only split drops a tab-separated flag entirely.
        let out = rewrite_exec_start("ExecStart=whisrsd\t--foo\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=/opt/bin/whisrsd --foo\n");
    }

    /// systemd strips whitespace around a directive value, so `ExecStart= x`
    /// names the binary `x`. Splitting on whitespace before trimming re-emits
    /// that name as an argument to the path just resolved, producing
    /// `ExecStart=/abs/whisrsd whisrsd`, which clap rejects with exit 2 while
    /// `Restart=on-failure` burns its retries. The whole-line replace this
    /// function replaced was accidentally right here, so the argument fix has
    /// to not regress it.
    #[test]
    fn exec_start_rewrite_trims_before_splitting() {
        let out = rewrite_exec_start("ExecStart= whisrsd\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=/opt/bin/whisrsd\n");

        let out = rewrite_exec_start("ExecStart=  whisrsd --foo\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=/opt/bin/whisrsd --foo\n");
    }

    /// systemd's execution prefixes sit before the binary, so a rewrite that
    /// starts at the `=` swallows them. `@` in particular takes the next token
    /// as argv[0], so dropping it silently promotes that token to a real
    /// argument.
    #[test]
    fn exec_start_rewrite_keeps_execution_prefixes() {
        let out = rewrite_exec_start("ExecStart=-whisrsd\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=-/opt/bin/whisrsd\n");

        let out = rewrite_exec_start("ExecStart=@whisrsd argv0 --foo\n", "/opt/bin/whisrsd");
        assert_eq!(out, "ExecStart=@/opt/bin/whisrsd argv0 --foo\n");
    }

    /// `ExecStartPre=`/`ExecStartPost=`/`ExecStop=` are not the daemon, so
    /// repointing them at the daemon path would be nonsense. `starts_with`
    /// on the bare directive name would match the first two.
    #[test]
    fn exec_start_rewrite_leaves_sibling_directives_alone() {
        let unit = "ExecStartPre=/bin/true\nExecStartPost=/bin/true\nExecStop=/bin/kill\n";
        assert_eq!(rewrite_exec_start(unit, "/opt/bin/whisrsd"), unit);
    }

    /// The rewrite is the only edit: comments, blank lines, section order and
    /// every other directive survive byte-for-byte. Run against the file users
    /// actually get, so a change to the packaged unit is covered too.
    #[test]
    fn exec_start_rewrite_touches_nothing_else_in_the_shipped_unit() {
        // Not an early return: `Cargo.toml` sets no `include`/`exclude`, so
        // `contrib/` ships with the crate and is on disk wherever this test
        // runs. A `return` here would let the whole assertion go vacuous the
        // day that stops being true.
        let src = find_contrib_file("whisrs.service").expect("contrib/whisrs.service is on disk");
        let original = std::fs::read_to_string(&src).expect("contrib unit is readable");
        let rewritten = rewrite_exec_start(&original, "/opt/bin/whisrsd");

        let changed: Vec<_> = original
            .lines()
            .zip(rewritten.lines())
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed,
            vec![("ExecStart=whisrsd", "ExecStart=/opt/bin/whisrsd")],
            "the rewrite edited a line other than ExecStart"
        );
        assert_eq!(
            original.lines().count(),
            rewritten.lines().count(),
            "the rewrite added or dropped a line"
        );
    }

    /// The reader behind the issue #146 repoint check must pick the same
    /// token the rewrite replaces, or setup would offer a repoint it then
    /// applies to a different part of the line.
    #[test]
    fn exec_start_binary_reads_plain_and_argument_forms() {
        assert_eq!(
            exec_start_binary("[Service]\nExecStart=/usr/bin/whisrsd\n"),
            Some("/usr/bin/whisrsd")
        );
        assert_eq!(
            exec_start_binary("ExecStart=/usr/bin/whisrsd --foo bar\n"),
            Some("/usr/bin/whisrsd")
        );
        assert_eq!(
            exec_start_binary("ExecStart=/usr/bin/whisrsd\t--foo\n"),
            Some("/usr/bin/whisrsd")
        );
    }

    #[test]
    fn exec_start_binary_strips_prefixes_and_leading_whitespace() {
        for line in [
            "ExecStart=@/usr/bin/whisrsd argv0",
            "ExecStart=-/usr/bin/whisrsd",
            "ExecStart=:/usr/bin/whisrsd",
            "ExecStart=+/usr/bin/whisrsd",
            "ExecStart=!/usr/bin/whisrsd",
            "ExecStart=-@/usr/bin/whisrsd argv0",
            "ExecStart=  /usr/bin/whisrsd --foo",
        ] {
            assert_eq!(exec_start_binary(line), Some("/usr/bin/whisrsd"), "{line}");
        }
    }

    #[test]
    fn exec_start_binary_is_none_without_an_exec_start() {
        assert_eq!(exec_start_binary("[Service]\nRestart=on-failure\n"), None);
        assert_eq!(
            exec_start_binary("ExecStartPre=/bin/true\nExecStartPost=/bin/true\n"),
            None
        );
        assert_eq!(exec_start_binary("ExecStart=\n"), None);
        assert_eq!(exec_start_binary("ExecStart=  \n"), None);
    }

    #[test]
    fn exec_start_binary_reads_the_shipped_unit() {
        let src = find_contrib_file("whisrs.service").expect("contrib/whisrs.service is on disk");
        let unit = std::fs::read_to_string(&src).expect("contrib unit is readable");
        assert_eq!(exec_start_binary(&unit), Some("whisrsd"));
    }

    /// Two real files plus a symlink to one of them, in a scratch dir.
    fn repoint_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a-whisrsd");
        let b = dir.path().join("b-whisrsd");
        let link = dir.path().join("link-whisrsd");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        (dir, a, b, link)
    }

    /// `unit_needs_repoint` with no home and an empty search path, so nothing
    /// on the host can leak into the result.
    fn needs_repoint(unit_binary: &str, resolved: &Path) -> bool {
        unit_needs_repoint(unit_binary, resolved, None, &[])
    }

    #[test]
    fn unit_needs_repoint_compares_resolved_files() {
        let (_dir, a, b, link) = repoint_fixture();
        let a_str = a.to_str().unwrap();
        assert!(!needs_repoint(a_str, &a), "same path");
        assert!(needs_repoint(a_str, &b), "different existing file");
        // `/bin` -> `/usr/bin` on Arch: a string compare would flag this.
        assert!(
            !needs_repoint(link.to_str().unwrap(), &a),
            "symlink to the same file"
        );
    }

    /// `which_whisrsd` falls back to `~/.cargo/bin/whisrsd` without checking
    /// it exists, so a missing resolved path must never trigger a repoint.
    #[test]
    fn unit_needs_repoint_never_points_at_a_missing_binary() {
        let (dir, a, _b, _link) = repoint_fixture();
        let missing = dir.path().join("missing-whisrsd");
        assert!(!needs_repoint(a.to_str().unwrap(), &missing));
        assert!(!needs_repoint("whisrsd", &missing));
        assert!(!unit_needs_repoint(
            "whisrsd",
            &missing,
            Some(dir.path()),
            &[dir.path().to_path_buf()]
        ));
    }

    #[test]
    fn unit_needs_repoint_flags_bare_and_deleted_unit_binaries() {
        let (dir, a, _b, _link) = repoint_fixture();
        assert!(needs_repoint("whisrsd", &a), "bare name found nowhere");
        let gone = dir.path().join("uninstalled-whisrsd");
        assert!(
            needs_repoint(gone.to_str().unwrap(), &a),
            "unit binary that no longer exists"
        );
    }

    /// The maintainer's live unit is `ExecStart=%h/.cargo/bin/whisrsd` and
    /// `which whisrsd` is that same file. Treating `%h/...` as a bare name
    /// prompted to repoint the unit at the binary it already runs.
    #[test]
    fn unit_needs_repoint_skips_percent_h_unit_already_on_the_cargo_binary() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let whisrsd = bin.join("whisrsd");
        std::fs::write(&whisrsd, "").unwrap();
        assert!(!unit_needs_repoint(
            "%h/.cargo/bin/whisrsd",
            &whisrsd,
            Some(home.path()),
            &[]
        ));
        // Without a home, `%h` cannot be resolved, so leave the unit alone.
        assert!(!unit_needs_repoint(
            "%h/.cargo/bin/whisrsd",
            &whisrsd,
            None,
            &[]
        ));
    }

    #[test]
    fn unit_needs_repoint_flags_percent_h_unit_on_another_binary() {
        let (dir, _a, b, _link) = repoint_fixture();
        std::fs::write(dir.path().join("whisrsd"), "").unwrap();
        assert!(unit_needs_repoint("%h/whisrsd", &b, Some(dir.path()), &[]));
    }

    #[test]
    fn unit_needs_repoint_leaves_unsupported_specifiers_alone() {
        let (dir, _a, b, _link) = repoint_fixture();
        assert!(!unit_needs_repoint("%U/whisrsd", &b, Some(dir.path()), &[]));
    }

    /// systemd resolves a bare `ExecStart=whisrsd` against its own search
    /// path, so a bare name that lands on the resolved binary is fine.
    #[test]
    fn unit_needs_repoint_resolves_bare_name_against_search_path() {
        let search = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let found = search.path().join("whisrsd");
        std::fs::write(&found, "").unwrap();
        let dirs = [empty.path().to_path_buf(), search.path().to_path_buf()];
        assert!(
            !unit_needs_repoint("whisrsd", &found, None, &dirs),
            "bare name found in a search dir, same file"
        );

        let (_other, a, _b, _link) = repoint_fixture();
        assert!(
            unit_needs_repoint("whisrsd", &a, None, &dirs),
            "bare name found in a search dir, different file"
        );
        assert!(
            unit_needs_repoint("whisrsd", &found, None, &[empty.path().to_path_buf()]),
            "bare name found nowhere"
        );
    }

    /// The first search dir that has the name wins, as in systemd.
    #[test]
    fn unit_needs_repoint_takes_the_first_search_dir_hit() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("whisrsd"), "").unwrap();
        let later = second.path().join("whisrsd");
        std::fs::write(&later, "").unwrap();
        let dirs = [first.path().to_path_buf(), second.path().to_path_buf()];
        assert!(unit_needs_repoint("whisrsd", &later, None, &dirs));
    }

    /// A quoted binary can hold spaces the whitespace split cannot see, so
    /// rewriting it would leave an unbalanced quote.
    #[test]
    fn unit_needs_repoint_leaves_quoted_and_escaped_binaries_alone() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = dir.path().join("whisrsd");
        fs::write(&resolved, "").unwrap();
        assert!(!needs_repoint("\"/opt/my", &resolved));
        assert!(!needs_repoint("'/opt/my", &resolved));
        assert!(!needs_repoint("/opt/my\\x20apps/whisrsd", &resolved));
    }

    /// `which` prints `./whisrsd` for a relative PATH entry; writing that into
    /// ExecStart would break the unit.
    #[test]
    fn unit_needs_repoint_never_points_at_a_relative_path() {
        let (_dir, a, _b, _link) = repoint_fixture();
        assert!(!needs_repoint(a.to_str().unwrap(), Path::new("./whisrsd")));
        assert!(!needs_repoint("whisrsd", Path::new("whisrsd")));
    }

    #[test]
    fn expand_unit_specifiers_handles_home_and_literal_percent() {
        let home = tempfile::tempdir().unwrap();
        let expected = format!("{}/.cargo/bin/whisrsd", home.path().display());
        assert_eq!(
            expand_unit_specifiers("%h/.cargo/bin/whisrsd", home.path()),
            Some(expected)
        );
        assert_eq!(
            expand_unit_specifiers("/opt/100%%/whisrsd", home.path()),
            Some("/opt/100%/whisrsd".to_string())
        );
        assert_eq!(
            expand_unit_specifiers("/usr/bin/whisrsd", home.path()),
            Some("/usr/bin/whisrsd".to_string())
        );
    }

    #[test]
    fn expand_unit_specifiers_rejects_unsupported_specifiers() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(expand_unit_specifiers("%U/x", home.path()), None);
        assert_eq!(
            expand_unit_specifiers("/usr/bin/whisrsd%", home.path()),
            None
        );
    }

    /// The inline OpenRC fallback is used on `cargo install`, where `contrib/`
    /// is not on disk. It is a hand-maintained copy of
    /// `contrib/openrc/whisrs.initd`, so it silently drifts: an earlier
    /// revision omitted the conf.d re-export loop, which made
    /// `XKB_DEFAULT_LAYOUT` in conf.d a no-op with no diagnostic.
    ///
    /// `contrib/openrc/whisrs.initd` is the copy users actually install, so it
    /// is held to the same list: asserting the two loops are byte-equal catches
    /// drift in either direction and covers the shipped file's own var list.
    #[test]
    fn openrc_scripts_reexport_confd_vars() {
        let script = openrc_initd_contents("/usr/bin/whisrsd");
        let inline_loop = confd_reexport_loop(&script);
        for var in [
            "RUST_LOG",
            "WHISRS_DEEPGRAM_API_KEY",
            "WHISRS_GROQ_API_KEY",
            "WHISRS_OPENAI_API_KEY",
            "WHISRS_ASR_SIDECAR_API_KEY",
            "XKB_DEFAULT_LAYOUT",
            "XKB_DEFAULT_VARIANT",
        ] {
            assert!(
                inline_loop.contains(var),
                "inline OpenRC fallback must re-export {var} from inside the \
                 `for _var in ... done` loop; conf.d values are sourced, not \
                 exported, so omitting it silently drops the setting. Naming it \
                 in the comment above the loop does not count."
            );
        }

        // `cargo install` builds have no contrib/ on disk; skip rather than fail.
        let Some(shipped_path) = find_contrib_file("openrc/whisrs.initd") else {
            return;
        };
        let shipped = fs::read_to_string(&shipped_path).unwrap_or_else(|e| {
            panic!("failed to read {}: {e}", shipped_path.display());
        });
        assert_eq!(
            confd_reexport_loop(&shipped),
            inline_loop,
            "contrib/openrc/whisrs.initd is the script users install, and its \
             conf.d re-export loop must stay identical to the inline fallback's"
        );
    }

    /// Exporting an empty `DISPLAY` is worse than leaving it unset: it makes
    /// x11rb and clipboard calls fail confusingly instead of being skipped.
    /// Compositors that spawn Xwayland with `-displayfd` have no `:N` in argv.
    #[test]
    fn inline_openrc_fallback_guards_empty_display() {
        let script = openrc_initd_contents("/usr/bin/whisrsd");
        assert!(
            !script.contains("export DISPLAY=\"$(tr"),
            "DISPLAY must be captured first and exported only when non-empty"
        );
        assert!(script.contains("[ -n \"$_val\" ] && export DISPLAY=\"$_val\""));
    }

    /// `openrc-run` parses the whole script at start, so a syntax error means
    /// the service never starts. Nothing else here executes it.
    #[test]
    fn inline_openrc_fallback_is_valid_shell() {
        let script = openrc_initd_contents("/usr/bin/whisrsd");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs");
        // Drop the openrc-run shebang: `sh -n` only checks syntax, and
        // /sbin/openrc-run does not exist on non-OpenRC machines.
        fs::write(&path, script.replacen("#!/sbin/openrc-run", "#!/bin/sh", 1)).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("failed to run sh -n");
        assert!(
            out.status.success(),
            "inline OpenRC fallback is not valid shell: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// What a recorder was asked, one line per run.
    type Seen = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    /// A runner that records what it was asked and fails the runs whose
    /// line holds one of `fail`.
    fn recorder(
        fail: &'static [&'static str],
    ) -> (Seen, impl Fn(&str, &[&str]) -> Result<(), String>) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = std::rc::Rc::clone(&seen);
        let run = move |program: &str, args: &[&str]| {
            let line = format!("{program} {}", args.join(" "));
            log.borrow_mut().push(line.clone());
            if fail.iter().any(|f| line.contains(f)) {
                Err(format!("{line}: failed"))
            } else {
                Ok(())
            }
        };
        (seen, run)
    }

    #[test]
    fn status_tells_missing_current_stale_and_foreign_apart() {
        let (dir, a, b, _link) = repoint_fixture();
        let unit = dir.path().join("whisrs.service");
        let whisrsd = a.to_str().unwrap();
        let status = |contents: Option<&str>| {
            match contents {
                Some(text) => fs::write(&unit, text).unwrap(),
                None => {
                    let _ = fs::remove_file(&unit);
                }
            }
            status_of(ServiceManager::Systemd, &unit, whisrsd, None, &[])
        };
        assert_eq!(status(None), UnitFile::Missing);
        assert_eq!(
            status(Some(&format!("ExecStart={whisrsd}\n"))),
            UnitFile::Current
        );
        assert_eq!(
            status(Some(&format!("ExecStart={} --foo\n", b.display()))),
            UnitFile::Stale {
                binary: b.to_string_lossy().into_owned(),
                new: whisrsd.to_string(),
            }
        );
        assert_eq!(status(Some("[Service]\n")), UnitFile::Foreign);
        assert_eq!(
            status(Some("ExecStart=\"/opt/my apps/whisrsd\"\n")),
            UnitFile::Foreign
        );
        assert_eq!(status(Some("ExecStart=%U/whisrsd\n")), UnitFile::Foreign);
        // An OpenRC script is never repointed.
        fs::write(&unit, "command=/elsewhere\n").unwrap();
        assert_eq!(
            status_of(ServiceManager::OpenRc, &unit, whisrsd, None, &[]),
            UnitFile::Current
        );
    }

    /// The packaged unit is written with an absolute ExecStart; without
    /// contrib/ the inline unit is.
    #[test]
    fn write_puts_the_resolved_binary_in_the_unit() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("systemd/user/whisrs.service");
        let notes = write(
            ServiceManager::Systemd,
            &dest,
            "/opt/bin/whisrsd",
            crate::config::setup::find_contrib_file,
            None,
        )
        .unwrap();
        assert!(notes.is_empty());
        let unit = fs::read_to_string(&dest).unwrap();
        assert_eq!(exec_start_binary(&unit), Some("/opt/bin/whisrsd"));

        let inline = dir.path().join("inline.service");
        write(
            ServiceManager::Systemd,
            &inline,
            "/opt/bin/whisrsd",
            |_| None,
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&inline).unwrap(),
            systemd_unit_contents("/opt/bin/whisrsd")
        );
        assert!(write(ServiceManager::None, &inline, "/x", |_| None, None).is_err());
    }

    /// An OpenRC script is executable, and its conf.d is installed once,
    /// private, never over one that is there.
    #[test]
    fn write_installs_an_executable_openrc_script_and_its_confd() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("rc/init.d/whisrs");
        let confd = dir.path().join("rc/conf.d");
        let notes = write(
            ServiceManager::OpenRc,
            &dest,
            "/opt/bin/whisrsd",
            crate::config::setup::find_contrib_file,
            Some(&confd),
        )
        .unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("installed tunables"), "{notes:?}");
        let tunables = confd.join(OPENRC_SERVICE);
        assert_eq!(
            fs::metadata(&tunables).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&tunables, "WHISRS_GROQ_API_KEY=mine\n").unwrap();
        let again = write(
            ServiceManager::OpenRc,
            &dest,
            "/opt/bin/whisrsd",
            crate::config::setup::find_contrib_file,
            Some(&confd),
        )
        .unwrap();
        assert!(again.is_empty());
        assert_eq!(
            fs::read_to_string(&tunables).unwrap(),
            "WHISRS_GROQ_API_KEY=mine\n"
        );
    }

    /// A repoint rewrites the binary only and reloads systemd; a failed
    /// reload is an error saying the old binary still runs.
    #[test]
    fn repoint_rewrites_in_place_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("whisrs.service");
        fs::write(
            &dest,
            "# mine\n[Service]\nExecStart=-/usr/bin/whisrsd --x\n",
        )
        .unwrap();
        let (seen, run) = recorder(&[]);
        repoint(&dest, "/home/ada/.cargo/bin/whisrsd", &run).unwrap();
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "# mine\n[Service]\nExecStart=-/home/ada/.cargo/bin/whisrsd --x\n"
        );
        assert_eq!(*seen.borrow(), ["systemctl --user daemon-reload"]);

        let (_, failing) = recorder(&["daemon-reload"]);
        let why = repoint(&dest, "/x/whisrsd", &failing)
            .unwrap_err()
            .to_string();
        assert!(why.contains("still runs the old binary"), "{why}");
    }

    #[test]
    fn enable_runs_the_managers_commands() {
        let (seen, run) = recorder(&[]);
        enable(ServiceManager::Systemd, &run).unwrap();
        assert_eq!(
            *seen.borrow(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now whisrs.service"
            ]
        );
        let (seen, run) = recorder(&[]);
        enable(ServiceManager::OpenRc, &run).unwrap();
        assert_eq!(
            *seen.borrow(),
            [
                "rc-update --user add whisrs default",
                "rc-service --user whisrs start"
            ]
        );
        // A failure says how to do it by hand.
        let (_, run) = recorder(&["enable"]);
        let why = enable(ServiceManager::Systemd, &run)
            .unwrap_err()
            .to_string();
        assert!(
            why.contains("systemctl --user enable --now whisrs.service"),
            "{why}"
        );
        let (seen, run) = recorder(&["rc-update"]);
        assert!(enable(ServiceManager::OpenRc, &run).is_err());
        assert_eq!(seen.borrow().len(), 1, "no start after a failed add");
    }
}
