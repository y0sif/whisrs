//! Access to `/dev/uinput`, which the uinput typing backend writes keys to,
//! and the sudo commands that grant it.
//!
//! The commands are only returned: the TUI runs them in the plain terminal,
//! where sudo can ask for a password. None of them restarts systemd-udevd.

use std::io::ErrorKind;
use std::path::Path;

/// What opening `/dev/uinput` for writing found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uinput {
    Ok,
    /// Permission denied: [`fix_commands`] grants it.
    Denied,
    /// Anything else (no such device, the module not loaded), and why.
    Other(String),
}

/// Whether `/dev/uinput` can be opened for writing.
pub fn check() -> Uinput {
    check_at(Path::new("/dev/uinput"))
}

/// [`check`] on `path`.
pub fn check_at(path: &Path) -> Uinput {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => Uinput::Ok,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Uinput::Denied,
        Err(e) => Uinput::Other(format!("cannot open {}: {e}", path.display())),
    }
}

/// The udev rule written when `contrib/99-whisrs.rules` is not on disk
/// (`cargo install`): its two rules, without the comments.
pub const UDEV_RULE: &str = "KERNEL==\"uinput\", SUBSYSTEM==\"misc\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\nKERNEL==\"uinput\", SUBSYSTEM==\"misc\", TEST==\"/usr/bin/setfacl\", RUN+=\"/usr/bin/setfacl -m g:input:rw /dev/$name\"";

/// The commands that grant `user` access to `/dev/uinput`, each a program
/// and its arguments: install the udev rule (`rule` is the shipped file, or
/// `None` to write [`UDEV_RULE`]), reload the rules and apply them, then add
/// `user` to the input group, which takes effect at the next login.
pub fn fix_commands(rule: Option<&Path>, user: &str) -> Vec<Vec<String>> {
    let words = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let install = match rule {
        Some(src) => vec![
            "sudo".to_string(),
            "cp".to_string(),
            src.to_string_lossy().into_owned(),
            "/etc/udev/rules.d/".to_string(),
        ],
        None => vec![
            "sudo".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            format!("echo '{UDEV_RULE}' > /etc/udev/rules.d/99-whisrs.rules"),
        ],
    };
    vec![
        install,
        words(&["sudo", "udevadm", "control", "--reload-rules"]),
        words(&["sudo", "udevadm", "trigger"]),
        words(&["sudo", "usermod", "-aG", "input", user]),
    ]
}

/// The same fix, for someone who runs it by hand.
pub const MANUAL_FIX: &[&str] = &[
    "sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/",
    "sudo udevadm control --reload-rules",
    "sudo udevadm trigger",
    "sudo usermod -aG input $USER",
];

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_writable_file_is_ok_and_a_read_only_one_denied() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::write(&open, "").unwrap();
        assert_eq!(check_at(&open), Uinput::Ok);
        let shut = dir.path().join("shut");
        std::fs::write(&shut, "").unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o400)).unwrap();
        // Root opens anything: only check where the mode bits count.
        if std::fs::OpenOptions::new().write(true).open(&shut).is_err() {
            assert_eq!(check_at(&shut), Uinput::Denied);
        }
        match check_at(&dir.path().join("missing")) {
            Uinput::Other(why) => assert!(why.contains("missing"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// The shipped rule is copied; without it the inline one is written.
    /// Nothing restarts systemd-udevd.
    #[test]
    fn the_fix_installs_the_rule_and_joins_the_group() {
        let shipped = fix_commands(Some(Path::new("/src/contrib/99-whisrs.rules")), "ada");
        assert_eq!(
            shipped[0],
            [
                "sudo",
                "cp",
                "/src/contrib/99-whisrs.rules",
                "/etc/udev/rules.d/"
            ]
        );
        assert_eq!(shipped[3], ["sudo", "usermod", "-aG", "input", "ada"]);
        let inline = fix_commands(None, "ada");
        assert!(inline[0][3].contains("static_node=uinput"));
        assert!(inline[0][3].ends_with("> /etc/udev/rules.d/99-whisrs.rules"));
        for command in shipped.iter().chain(&inline) {
            let line = command.join(" ");
            assert!(!line.contains("systemd-udevd"), "{line}");
            assert!(!line.contains("restart"), "{line}");
        }
    }

    /// The inline rule is the shipped one, comments off.
    #[test]
    fn the_inline_rule_matches_the_shipped_file() {
        let src = crate::config::setup::find_contrib_file("99-whisrs.rules")
            .expect("contrib/99-whisrs.rules is on disk");
        let shipped = std::fs::read_to_string(src).unwrap();
        let rules: Vec<&str> = shipped
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .collect();
        assert_eq!(rules.join("\n"), UDEV_RULE);
    }
}
