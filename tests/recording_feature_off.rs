//! `whisrs recording` in a build without the `recording` feature: the error
//! on stderr and exit code 1, before any daemon is asked. Empty in a build
//! with the feature.
#![cfg(not(feature = "recording"))]

use std::process::Command;

const EXPECTED: &str = "error: this whisrs was built without the recording feature, so \
    `whisrs recording` is not available. Use a release build, or rebuild with `cargo install \
    whisrs --features recording`.";

#[test]
fn recording_exits_1_and_says_how_to_get_it() {
    // A clean environment where every folder whisrs could use is its own empty
    // temp folder: a regression that sends the command fails here ("whisrsd is
    // not running") instead of reaching a daemon on this machine, and one that
    // reads or writes config, history or recordings never touches the real
    // user's files.
    let root = tempfile::tempdir().expect("temp dir");
    let dirs: Vec<(&str, std::path::PathBuf)> = [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ]
    .into_iter()
    .map(|(var, name)| {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).expect("create temp folder");
        (var, dir)
    })
    .collect();
    for args in [
        &["recording"][..],
        &[
            "recording",
            "start",
            "-t",
            "Weekly sync",
            "--with",
            "Sarah, Omar",
        ],
        &["recording", "toggle"],
        &["recording", "list", "--json"],
        &["recording", "show", "last"],
        &["recording", "summarize", "last"],
        &["recording", "delete", "2026-10-27_1402_standup", "--yes"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_whisrs"))
            .args(args)
            .env_clear()
            .envs(dirs.iter().map(|(var, dir)| (*var, dir.as_os_str())))
            .output()
            .expect("run whisrs");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        assert_eq!(stderr.trim_end(), EXPECTED, "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: nothing on stdout");
    }
}
