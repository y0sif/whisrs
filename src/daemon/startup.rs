use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use whisrs::config::types::unknown_config_keys;
use whisrs::service::ServiceManager;
use whisrs::Config;

/// Try to connect to an existing socket.
async fn socket_is_alive(path: &std::path::Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// Remove a stale socket file if no daemon is listening on it.
pub(crate) async fn cleanup_stale_socket(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        if socket_is_alive(path).await {
            anyhow::bail!("another whisrsd instance is already running");
        }
        warn!("removing stale socket at {}", path.display());
        std::fs::remove_file(path).context("failed to remove stale socket")?;
    }
    Ok(())
}

/// Load configuration from config.toml, falling back to defaults.
/// Returns (Config, Option<warning_message>) — the warning is set when config
/// parsing fails and defaults are used, so the caller can notify the user.
pub(crate) fn load_config() -> (Config, Option<String>) {
    load_config_from(
        &whisrs::config_path(),
        &whisrs::config::vocabulary::vocabulary_path(),
    )
}

/// The body of [`load_config`] against explicit paths, so the whole load order
/// is testable without mutating `XDG_CONFIG_HOME` (env mutation races with the
/// parallel test runner).
///
/// The merge must run before `validate_config`, so the keyterm limits count the
/// terms the backends actually receive.
fn load_config_from(
    config_path: &std::path::Path,
    vocabulary_path: &std::path::Path,
) -> (Config, Option<String>) {
    let (mut config, warning) = load_config_toml_at(config_path);
    merge_vocabulary_file_at(&mut config, vocabulary_path);
    (config, warning)
}

/// Merge `vocabulary.txt` into `[general] vocabulary`, config.toml's terms
/// first. A missing file is the opt-out; an unreadable one is warned about and
/// ignored, because the daemon still has to start.
fn merge_vocabulary_file_at(config: &mut Config, path: &std::path::Path) {
    use whisrs::config::vocabulary::{load_vocabulary_file, merge_vocabulary};

    match load_vocabulary_file(path) {
        Ok(Some(terms)) if !terms.is_empty() => {
            let file_count = terms.len();
            let merged = merge_vocabulary(std::mem::take(&mut config.general.vocabulary), terms);
            info!(
                "vocabulary: {file_count} term(s) from {}, {} effective after merging \
                 with config.toml",
                path.display(),
                merged.len()
            );
            config.general.vocabulary = merged;
        }
        Ok(_) => {}
        Err(e) => warn!(
            "failed to read vocabulary file at {}: {e} — ignoring it",
            path.display()
        ),
    }
}

fn load_config_toml_at(config_path: &std::path::Path) -> (Config, Option<String>) {
    if config_path.exists() {
        match std::fs::read_to_string(config_path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!("loaded config from {}", config_path.display());
                    let unknown = unknown_config_keys(&contents);
                    if unknown.is_empty() {
                        return (config, None);
                    }
                    let msg = format!(
                        "Unknown keys in config at {} ignored: {}",
                        config_path.display(),
                        unknown.join(", ")
                    );
                    warn!("{msg}");
                    return (config, Some(msg));
                }
                Err(e) => {
                    let msg = format!(
                        "Failed to parse config at {}: {e} — using defaults",
                        config_path.display()
                    );
                    error!("{msg}");
                    return (default_config(), Some(msg));
                }
            },
            Err(e) => {
                let msg = format!(
                    "Failed to read config at {}: {e} — using defaults",
                    config_path.display()
                );
                error!("{msg}");
                return (default_config(), Some(msg));
            }
        }
    } else {
        info!(
            "no config file found at {}; using defaults",
            config_path.display()
        );
    }
    (default_config(), None)
}

/// The built-in default configuration, used by every `load_config` fallback
/// (missing file, unreadable file, parse error). `Config` doesn't implement
/// `Default`, so the field-by-field construction lives here, once.
fn default_config() -> Config {
    Config {
        general: Default::default(),
        audio: Default::default(),
        input: Default::default(),
        deepgram: None,
        groq: None,
        openai: None,
        local_whisper: None,
        local_vosk: None,
        local_parakeet: None,
        asr_sidecar: None,
        openai_compatible_realtime: None,
        llm: None,
        tts: None,
        hotkeys: None,
        hooks: None,
        llm_commands: Vec::new(),
        overlay: None,
    }
}

/// Maximum number of attempts to detect compositor environment.
const COMPOSITOR_ENV_MAX_RETRIES: u32 = 10;

/// Initial retry delay for compositor env detection (doubles each attempt, capped at 10 s).
const COMPOSITOR_ENV_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Compositor environment variables to import from systemd.
const COMPOSITOR_ENV_VARS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "XDG_CURRENT_DESKTOP",
];

/// Wait for compositor environment variables to become available.
///
/// When the daemon starts via systemd on boot, it may launch before the
/// compositor sets session environment variables (WAYLAND_DISPLAY, etc.).
/// Without these, clipboard operations (wl-paste) and window tracking fail.
///
/// Polls `systemctl --user show-environment` with exponential backoff until
/// a display server variable is found, then imports all compositor-related
/// vars into the process environment.
///
/// This recovery path only exists under systemd, which is the only init system
/// here that keeps a queryable user-environment store. Under OpenRC the init
/// script recovers the session environment before exec instead — see
/// `contrib/openrc/whisrs.initd`.
pub(crate) async fn import_compositor_env() {
    // Already have a display server — nothing to do.
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        debug!("compositor environment already available");
        return;
    }

    // Without systemd there is nothing to poll: retrying would just burn ~55s
    // of backoff running a command that does not exist on this machine.
    if ServiceManager::detect() != ServiceManager::Systemd {
        warn!(
            "no display server in environment and no systemd user environment to import from \
             — clipboard and window tracking will not work. If you start whisrsd from a \
             service manager, it must pass the compositor environment through."
        );
        return;
    }

    info!("compositor env vars not set — polling systemd user environment");

    let mut delay = COMPOSITOR_ENV_INITIAL_DELAY;

    for attempt in 1..=COMPOSITOR_ENV_MAX_RETRIES {
        if let Some(imported) = try_import_from_systemd() {
            info!("imported compositor environment from systemd (attempt {attempt}): {imported}");
            return;
        }

        if attempt == COMPOSITOR_ENV_MAX_RETRIES {
            warn!(
                "compositor environment not available after {COMPOSITOR_ENV_MAX_RETRIES} attempts \
                 — clipboard and window tracking may not work"
            );
            return;
        }

        info!(
            "compositor env not available (attempt {attempt}/{COMPOSITOR_ENV_MAX_RETRIES}) \
             — retrying in {delay:?}"
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}

/// Try to read compositor env vars from systemd's user environment.
///
/// Returns a summary string of imported vars on success, or None if no
/// display server variable was found.
fn try_import_from_systemd() -> Option<String> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut imported = Vec::new();

    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if COMPOSITOR_ENV_VARS.contains(&key) && std::env::var(key).is_err() {
                std::env::set_var(key, value);
                imported.push(key.to_string());
            }
        }
    }

    // Only succeed if we found a display server.
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        Some(imported.join(", "))
    } else {
        None
    }
}

pub(crate) fn check_uinput_access() {
    use std::fs::OpenOptions;
    match OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => info!("uinput access: ok"),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                warn!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER\n\
                          # Then log out and log back in\n\
                     Or install the udev rule:\n\
                          sudo install -m644 contrib/99-whisrs.rules /etc/udev/rules.d/\n\
                          # On NixOS/Guix, point the rule at your setfacl:\n\
                          command -v setfacl >/dev/null && sudo sed -i \\\n\
                              \"s|/usr/bin/setfacl|$(command -v setfacl)|g\" \\\n\
                              /etc/udev/rules.d/99-whisrs.rules\n\
                          sudo udevadm control --reload-rules\n\
                          sudo udevadm trigger"
                );
            } else {
                warn!("Cannot open /dev/uinput: {e}");
            }
        }
    }
}

pub(crate) fn check_audio_devices() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(device) => {
            let name = device.name().unwrap_or_else(|_| "unknown".into());
            info!("default audio input device: {name}");
        }
        None => {
            warn!("no default audio input device found");
            if let Ok(devices) = host.input_devices() {
                let names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
                if names.is_empty() {
                    warn!("no audio input devices available at all");
                } else {
                    warn!("available audio input devices: {}", names.join(", "));
                }
            }
        }
    }
}

/// Check if the D-Bus session bus is reachable. Required for MPRIS media
/// pause. Warns once at startup if unavailable.
#[cfg(feature = "hooks")]
pub(crate) async fn check_session_bus() {
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        zbus::Connection::session(),
    )
    .await
    {
        Ok(Ok(_)) => info!("D-Bus session bus: available"),
        Ok(Err(e)) => {
            warn!(
                "D-Bus session bus unavailable: {e}\n\
                 MPRIS media pause will not work.\n\
                 Install dbus-broker or dbus-daemon and ensure \
                 DBUS_SESSION_BUS_ADDRESS is set."
            );
        }
        Err(_) => {
            warn!(
                "D-Bus session bus connection timed out (5 s)\n\
                 MPRIS media pause will not work.\n\
                 Ensure dbus-broker or dbus-daemon is running and \
                 DBUS_SESSION_BUS_ADDRESS is set."
            );
        }
    }
}

pub(crate) fn validate_config(config: &Config) {
    match config.validate() {
        Ok(warnings) => {
            for w in &warnings {
                warn!("config: {}", w);
            }
        }
        Err(e) => error!("config: {e}"),
    }
    if !config.has_any_backend_configured() {
        warn!("No transcription backend configured. Run 'whisrs setup' to get started.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config.toml + vocabulary.txt pair in a fresh temp dir.
    ///
    /// Both paths are passed explicitly, so nothing here touches
    /// `XDG_CONFIG_HOME` — mutating the environment races with the parallel
    /// test runner.
    fn write_pair(config_toml: &str, vocabulary_txt: Option<&str>) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config_toml).expect("write config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        if let Some(contents) = vocabulary_txt {
            std::fs::write(&vocab_path, contents).expect("write vocabulary.txt");
        }
        let (config, _) = load_config_from(&config_path, &vocab_path);
        (dir, config)
    }

    /// The whole point of the feature: terms that live only in vocabulary.txt
    /// end up in the config the daemon runs on. Deleting the merge call from
    /// `load_config_from` fails here.
    #[test]
    fn vocabulary_file_terms_reach_the_loaded_config() {
        let (_dir, config) = write_pair(
            "[general]\nbackend = \"groq\"\n",
            Some("# keyterms\nNixOS\n\nClaude Code\n"),
        );
        assert_eq!(config.general.vocabulary, vec!["NixOS", "Claude Code"]);
    }

    /// config.toml's terms come first and a term in both stores is kept once.
    /// Ordering is not cosmetic: Deepgram charges keyterms against a byte and
    /// word budget in list order, so a reshuffle changes which terms survive.
    #[test]
    fn config_toml_terms_come_first_and_duplicates_are_dropped() {
        let (_dir, config) = write_pair(
            "[general]\nbackend = \"groq\"\nvocabulary = [\"whisrs\", \"GNOME\"]\n",
            Some("Deepgram\nwhisrs\nNixOS\n"),
        );
        assert_eq!(
            config.general.vocabulary,
            vec!["whisrs", "GNOME", "Deepgram", "NixOS"],
            "config.toml first, then the file's new terms, each term once"
        );
    }

    /// The feature is opt-in by creating the file, so no file means the
    /// config.toml list is passed through untouched.
    #[test]
    fn a_missing_vocabulary_file_is_a_no_op() {
        let (_dir, config) = write_pair(
            "[general]\nbackend = \"groq\"\nvocabulary = [\"whisrs\"]\n",
            None,
        );
        assert_eq!(config.general.vocabulary, vec!["whisrs"]);
    }

    /// An unreadable path is warned about and ignored, never fatal: the daemon
    /// still has to start. A directory is the portable stand-in for a file the
    /// process cannot read (chmod is a no-op for root in CI).
    #[test]
    fn an_unreadable_vocabulary_file_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vocab_path = dir.path().join("vocabulary.txt");
        std::fs::create_dir(&vocab_path).expect("make vocabulary.txt a directory");

        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.vocabulary = vec!["whisrs".to_string()];
        merge_vocabulary_file_at(&mut config, &vocab_path);

        assert_eq!(config.general.vocabulary, vec!["whisrs"]);
    }

    /// The end of the "user added a term" flow: `whisrs config` wrote the whole
    /// merged list to vocabulary.txt and an empty `vocabulary` to config.toml.
    /// The daemon must then load exactly the list the user saw in the editor.
    /// This drives the real writer, not a hand-rolled file.
    #[test]
    fn a_migrated_vocabulary_round_trips_back_through_the_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[general]\nbackend = \"groq\"\nvocabulary = []\n",
        )
        .expect("write config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        let migrated = vec![
            "whisrs".to_string(),
            "Hyprland".to_string(),
            "NixOS".to_string(),
        ];
        whisrs::config::vocabulary::write_vocabulary_file(&vocab_path, &migrated)
            .expect("write vocabulary.txt");

        let (config, _) = load_config_from(&config_path, &vocab_path);
        assert_eq!(config.general.vocabulary, migrated);
    }

    /// An unparseable config.toml still gets the vocabulary merge.
    ///
    /// `load_config_toml_at` falls back to `default_config()` on a parse or
    /// read error, and that fallback runs *before* the merge, so a broken
    /// config.toml does not also cost the user their vocabulary file — which is
    /// the point, since the file exists for setups where config.toml is
    /// generated and the user cannot fix it in place. Moving the merge above
    /// the fallback, or into the `Ok` arm only, breaks this.
    #[test]
    fn a_broken_config_toml_still_gets_the_vocabulary_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is [ not toml").expect("write config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        std::fs::write(&vocab_path, "NixOS\nClaude Code\n").expect("write vocabulary.txt");

        let (config, warning) = load_config_from(&config_path, &vocab_path);

        assert_eq!(config.general.vocabulary, vec!["NixOS", "Claude Code"]);
        let warning = warning.expect("a parse failure must be reported to the user");
        assert!(
            warning.contains("using defaults"),
            "the warning must say defaults were substituted: {warning}"
        );

        // The other fallback arm: a config.toml that exists but cannot be read
        // (a directory is the portable stand-in — chmod is a no-op for root).
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::create_dir(&config_path).expect("make config.toml a directory");
        let vocab_path = dir.path().join("vocabulary.txt");
        std::fs::write(&vocab_path, "NixOS\n").expect("write vocabulary.txt");

        let (config, warning) = load_config_from(&config_path, &vocab_path);
        assert_eq!(config.general.vocabulary, vec!["NixOS"]);
        assert!(warning.is_some(), "an unreadable config must be reported");
    }

    /// An empty vocabulary.txt plus an empty `[general] vocabulary` yields no
    /// terms: the file existing is not itself a term, and the merge invents
    /// nothing.
    ///
    /// This deliberately does *not* prove that a deleted term cannot come back.
    /// That property belongs to the editor's save path, which blanks
    /// `[general] vocabulary` in the same save that writes the file — see
    /// `a_deleted_term_cannot_resurrect_from_config_toml` in `src/config/edit.rs`.
    /// If config.toml still held the term, the merge below would happily
    /// resurrect it, which is exactly why the save blanks it.
    #[test]
    fn an_emptied_vocabulary_file_leaves_the_daemon_with_no_terms() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[general]\nbackend = \"groq\"\nvocabulary = []\n",
        )
        .expect("write config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        whisrs::config::vocabulary::write_vocabulary_file(&vocab_path, &[])
            .expect("write an empty vocabulary.txt");

        let (config, _) = load_config_from(&config_path, &vocab_path);
        assert!(
            config.general.vocabulary.is_empty(),
            "deleted terms came back: {:?}",
            config.general.vocabulary
        );
    }

    /// The contract that fixes the load order: the merge runs *before*
    /// `validate_config`, so the Deepgram keyterm warning counts the terms the
    /// backend actually receives.
    ///
    /// `[general] vocabulary` is empty in config.toml, so without the merge
    /// `deepgram_keyterm_warnings` sees zero usable terms and returns nothing
    /// at all. The warning existing, and naming the file's term count, is only
    /// possible if the file reached `validate`.
    #[test]
    fn vocabulary_file_terms_are_counted_by_config_validate() {
        // Well past every keyterm cap, so the "N of M reach Deepgram" warning
        // is the one that fires.
        let terms: Vec<String> = (0..1000).map(|i| format!("term{i:04}")).collect();
        let (_dir, config) = write_pair(
            "[general]\n\
             backend = \"deepgram\"\n\
             \n\
             [deepgram]\n\
             api_key = \"test-key\"\n\
             model = \"nova-3\"\n",
            Some(&format!("{}\n", terms.join("\n"))),
        );
        assert_eq!(
            config.general.vocabulary.len(),
            terms.len(),
            "premise: every term came from vocabulary.txt, none from config.toml"
        );

        let warnings = config.validate().expect("the config is valid");
        let warning = warnings
            .iter()
            .find(|w| w.message.starts_with("[general] vocabulary: "))
            .unwrap_or_else(|| {
                panic!("the file's terms never reached Config::validate: {warnings:?}")
            });
        assert!(
            warning
                .message
                .contains(&format!("of {} usable term(s) reach", terms.len())),
            "the warning must count the file-sourced terms: {}",
            warning.message
        );
    }
}
