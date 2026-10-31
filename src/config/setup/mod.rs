//! The side effects of `whisrs setup`, as plain functions the wizard calls:
//! the uinput check and its sudo fix, the user service, the compositor
//! keybinding, the GNOME extension, the model download, the microphone probe
//! and the API key test.
//!
//! None of them prints or asks: each returns what it found or did, and the
//! TUI (`src/cli/tui/screens/setup.rs`) says it. Whatever runs a program or
//! touches the network takes it from its caller, so tests never run sudo,
//! systemctl, a compositor's tools or a real download.

pub mod gnome;
pub mod key_test;
pub mod keybind;
pub mod mic;
pub mod model;
pub mod uinput;
pub mod unit;

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Runs a program with arguments: `Ok` when it exited 0, else what went
/// wrong (its last line on stderr, or why it did not start). Steps take one
/// so a test can stand in for systemctl and friends.
pub type Run<'a> = &'a dyn Fn(&str, &[&str]) -> Result<(), String>;

/// The [`Run`] that runs the program, its output captured so nothing lands
/// on the TUI's screen.
pub fn system(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
    Err(match (last, output.status.code()) {
        (Some(line), _) => format!("{program} {}: {}", args.join(" "), line.trim()),
        (None, Some(code)) => format!("{program} {} exited {code}", args.join(" ")),
        (None, None) => format!("{program} {} was killed", args.join(" ")),
    })
}

/// Find a file in the contrib/ directory relative to the executable or CWD.
pub fn find_contrib_file(name: &str) -> Option<PathBuf> {
    // Try relative to the executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Binary might be in target/release/ or target/debug/.
            for ancestor in exe_dir.ancestors() {
                let candidate = ancestor.join("contrib").join(name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    // Try relative to CWD.
    let cwd_candidate = PathBuf::from("contrib").join(name);
    if cwd_candidate.exists() {
        return Some(cwd_candidate);
    }
    None
}

/// Get the path to the `whisrsd` binary.
pub fn which_whisrsd() -> String {
    which_or_cargo("whisrsd")
}

/// Get the path to the `whisrs` binary.
pub fn which_whisrs() -> String {
    which_or_cargo("whisrs")
}

/// `name` as `which` finds it on PATH, else `~/.cargo/bin/<name>`, which is
/// not checked: [`unit::status`] never repoints at a path that is not there.
fn which_or_cargo(name: &str) -> String {
    if let Ok(output) = Command::new("which").arg(name).output() {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    home.join(".cargo/bin")
        .join(name)
        .to_string_lossy()
        .to_string()
}

/// Where `whisrs setup` puts whisper models.
pub fn default_model_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("whisrs/models")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_says_what_failed() {
        assert_eq!(system("true", &[]), Ok(()));
        let why = system(
            "sh",
            &["-c", "echo first >&2; echo 'the reason' >&2; exit 3"],
        )
        .unwrap_err();
        assert!(why.ends_with("the reason"), "{why}");
        let why = system("sh", &["-c", "exit 4"]).unwrap_err();
        assert!(why.ends_with("exited 4"), "{why}");
        let why = system("whisrs-no-such-program", &[]).unwrap_err();
        assert!(
            why.starts_with("cannot run whisrs-no-such-program"),
            "{why}"
        );
    }

    #[test]
    fn the_shipped_contrib_files_are_found() {
        assert!(find_contrib_file("whisrs.service").is_some());
        assert!(find_contrib_file("no-such-file").is_none());
    }
}
