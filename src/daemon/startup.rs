use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use whisrs::config::profile::{self, Paths};
use whisrs::config::types::{unknown_config_keys, unknown_keys_warning};
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

/// Bind the IPC socket, readable and writable by its owner only.
///
/// Anyone who can connect can drive the daemon, and with `[[agents]]` that
/// runs a coding agent as the user, so the mode is set before the first
/// accept. `$XDG_RUNTIME_DIR` is 0700 already; the `/tmp` fallback is not.
/// Every client (the CLI, `whisrs agent-hook`) runs as the same user.
pub(crate) fn bind_socket(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    let listener = tokio::net::UnixListener::bind(path).context("failed to bind Unix socket")?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("failed to restrict {}", path.display()));
    }
    Ok(listener)
}

/// The config the daemon runs on, and what the load has to tell the user.
pub(crate) struct LoadedConfig {
    pub(crate) config: Config,
    /// What to notify, one line each: config.toml fell back to defaults,
    /// unknown or ignored keys, a profile that did not load.
    pub(crate) warning: Option<String>,
    /// The profile merged in: `None` is base.
    pub(crate) profile: Option<String>,
    /// Why the active profile did not load ([`profile::not_loaded`]).
    pub(crate) profile_error: Option<String>,
}

impl LoadedConfig {
    fn new(config: Config, warnings: Vec<String>) -> Self {
        Self {
            config,
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
            profile: None,
            profile_error: None,
        }
    }
}

/// Load configuration from config.toml and the active profile, falling back
/// to defaults. The warning is set when parsing fails and defaults are used,
/// or a profile did not load, so the caller can notify the user.
pub(crate) fn load_config() -> LoadedConfig {
    load_config_from(
        &Paths::current(),
        &whisrs::config::vocabulary::vocabulary_path(),
    )
}

/// The body of [`load_config`] against explicit paths, so the whole load order
/// is testable without mutating `XDG_CONFIG_HOME` (env mutation races with the
/// parallel test runner).
///
/// The profile merges into config.toml before the unknown-key check and
/// `validate_config`, and `vocabulary.txt` after it, so the keyterm limits
/// count the terms the backends actually receive.
fn load_config_from(paths: &Paths, vocabulary_path: &std::path::Path) -> LoadedConfig {
    let mut loaded = load_config_toml_at(paths);
    merge_vocabulary_file_at(&mut loaded.config, vocabulary_path);
    loaded
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

/// config.toml with the active profile merged in. Without one (no state
/// file, or `base`) it is config.toml alone, as before profiles. A profile
/// that does not load falls back to config.toml with a warning; a
/// config.toml that does not load falls back to defaults, as ever.
fn load_config_toml_at(paths: &Paths) -> LoadedConfig {
    let config_path = paths.config.as_path();
    let mut warnings = Vec::new();
    let active = match profile::read_active(&paths.state) {
        Ok(active) => active,
        Err(why) => {
            let msg = format!("the active profile did not load, using base: {why}");
            warn!("{msg}");
            warnings.push(msg);
            None
        }
    };
    let failed = |msg: String, mut warnings: Vec<String>| {
        error!("{msg}");
        warnings.insert(0, msg);
        let mut loaded = LoadedConfig::new(default_config(), warnings);
        if let Some(name) = &active {
            loaded.profile_error = Some(profile::not_loaded(name, "config.toml did not load"));
        }
        loaded
    };

    let contents = if config_path.exists() {
        match std::fs::read_to_string(config_path) {
            Ok(contents) => contents,
            Err(e) => {
                let msg = format!(
                    "Failed to read config at {}: {e} — using defaults",
                    config_path.display()
                );
                return failed(msg, warnings);
            }
        }
    } else {
        info!(
            "no config file found at {}; using defaults",
            config_path.display()
        );
        if active.is_none() {
            return LoadedConfig::new(default_config(), warnings);
        }
        String::new()
    };
    let config = match toml::from_str::<Config>(&contents) {
        Ok(config) => config,
        Err(e) => {
            let msg = format!(
                "Failed to parse config at {}: {e} — using defaults",
                config_path.display()
            );
            return failed(msg, warnings);
        }
    };

    let Some(name) = active else {
        info!("loaded config from {}", config_path.display());
        let unknown = unknown_config_keys(&contents);
        if let Some(msg) = unknown_keys_warning(config_path, &unknown) {
            warn!("{msg}");
            warnings.push(msg);
        }
        return LoadedConfig::new(config, warnings);
    };

    let eff = match profile::effective_at(paths, &name) {
        Ok(eff) => eff,
        // config.toml loaded a moment ago: it changed since.
        Err(why) => {
            let msg = profile::not_loaded(&name, &why);
            warn!("{msg}");
            warnings.push(msg.clone());
            let mut loaded = LoadedConfig::new(config, warnings);
            loaded.profile_error = Some(msg);
            return loaded;
        }
    };
    if let Some(msg) = unknown_keys_warning(config_path, &eff.unknown_base) {
        warn!("{msg}");
        warnings.push(msg);
    }
    for msg in &eff.warnings {
        warn!("{msg}");
    }
    warnings.extend(eff.warnings);
    match eff.error {
        None => {
            info!(
                "loaded config from {} with profile {name} ({})",
                config_path.display(),
                profile::label(&name)
            );
            let mut loaded = LoadedConfig::new(eff.config, warnings);
            loaded.profile = Some(name);
            loaded
        }
        Some(why) => {
            let msg = profile::not_loaded(&name, &why);
            warn!("{msg}");
            warnings.push(msg.clone());
            let mut loaded = LoadedConfig::new(eff.config, warnings);
            loaded.profile_error = Some(msg);
            loaded
        }
    }
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
        agents: Vec::new(),
        agent_overlay: None,
        replacements: Default::default(),
        recording: None,
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
    "XAUTHORITY",
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

/// Log what `[audio] device` resolves to. Diagnostic only: it can run before
/// the sound server is up, so it must not start the PulseAudio backoff or
/// mark an unknown name as warned for the first recording.
pub(crate) fn check_audio_devices(device: &str) {
    match whisrs::audio::device::resolve_input_for_diagnostics(device) {
        Ok(resolved) => {
            info!(
                "audio input: [audio] device = \"{device}\" resolved to {} ({})",
                resolved.device, resolved.source
            );
        }
        Err(e) => {
            warn!("{e}");
            let names: Vec<String> = whisrs::audio::block_off_runtime(|| {
                whisrs::audio::device::list_input_devices_for(device)
            })
            .into_iter()
            .map(|d| d.id)
            .collect();
            if names.is_empty() {
                warn!("no audio input devices available at all");
            } else {
                warn!("available audio input devices: {}", names.join(", "));
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

    /// [`load_config_from`] on a config.toml in a temp dir, with no profile
    /// (the state file would be beside it, and is not there).
    fn load_pair(
        config_path: &std::path::Path,
        vocab_path: &std::path::Path,
    ) -> (Config, Option<String>) {
        let paths = Paths::under(config_path.parent().expect("in a temp dir"));
        let loaded = load_config_from(&paths, vocab_path);
        assert_eq!(loaded.profile, None);
        (loaded.config, loaded.warning)
    }

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
        let (config, _) = load_pair(&config_path, &vocab_path);
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

        let (config, _) = load_pair(&config_path, &vocab_path);
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

        let (config, warning) = load_pair(&config_path, &vocab_path);

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

        let (config, warning) = load_pair(&config_path, &vocab_path);
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
    /// `a_deleted_term_cannot_resurrect_from_config_toml` in `src/config/vocabulary.rs`.
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

        let (config, _) = load_pair(&config_path, &vocab_path);
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

    /// A config.toml, a vocabulary.txt and profiles in a temp dir, `active`
    /// in the state file.
    fn with_profiles(
        config_toml: Option<&str>,
        vocabulary_txt: Option<&str>,
        profiles: &[(&str, &str)],
        active: Option<&str>,
    ) -> (tempfile::TempDir, LoadedConfig) {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::under(dir.path());
        if let Some(contents) = config_toml {
            std::fs::write(&paths.config, contents).expect("write config.toml");
        }
        let vocab_path = dir.path().join("vocabulary.txt");
        if let Some(contents) = vocabulary_txt {
            std::fs::write(&vocab_path, contents).expect("write vocabulary.txt");
        }
        std::fs::create_dir_all(&paths.profiles).expect("profiles dir");
        for (name, contents) in profiles {
            std::fs::write(paths.profile(name), contents).expect("write a profile");
        }
        if let Some(active) = active {
            std::fs::create_dir_all(paths.state.parent().unwrap()).unwrap();
            std::fs::write(&paths.state, format!("{active}\n")).expect("write the state file");
        }
        let loaded = load_config_from(&paths, &vocab_path);
        (dir, loaded)
    }

    const SIDECAR_BASE: &str = "[general]\nbackend = \"asr-sidecar\"\n\
                                vocabulary = [\"Base\"]\n[asr-sidecar]\nmodel = \"m\"\n";

    /// The profile is merged before the unknown-key check and validation:
    /// its unknown key is named with its file, and the config validates
    /// only because the profile names a backend that exists. `vocabulary.txt`
    /// merges after it, so the profile's list comes first.
    #[test]
    fn the_profile_merges_before_the_checks_and_vocabulary_after() {
        let base = "[general]\nbackend = \"nope\"\nvocabulary = [\"Base\"]\n\
                    [asr-sidecar]\nmodel = \"m\"\n";
        assert!(
            toml::from_str::<Config>(base).unwrap().validate().is_err(),
            "premise: the base alone does not validate"
        );
        let (_dir, loaded) = with_profiles(
            Some(base),
            Some("File\n"),
            &[(
                "offline",
                "[general]\nbackend = \"asr-sidecar\"\nvocabulary = [\"Profile\"]\n\
                 silense_timeout_ms = 1\n[asr-sidecar]\nurl = \"http://127.0.0.1:1\"\n",
            )],
            Some("offline"),
        );
        assert_eq!(loaded.profile.as_deref(), Some("offline"));
        assert_eq!(loaded.profile_error, None);
        let config = &loaded.config;
        assert_eq!(
            config.asr_sidecar.as_ref().unwrap().url,
            "http://127.0.0.1:1"
        );
        assert_eq!(config.asr_sidecar.as_ref().unwrap().model, "m");
        assert!(config.validate().is_ok());
        assert_eq!(config.general.vocabulary, ["Profile", "File"]);
        assert_eq!(
            loaded.warning.as_deref(),
            Some(
                "Unknown keys in config at profiles/offline.toml ignored: \
                 general.silense_timeout_ms"
            )
        );
    }

    /// A missing or broken profile runs on base, says why, and is
    /// remembered for `info`. Its dropped keys are told too.
    #[test]
    fn a_profile_that_does_not_load_runs_on_base() {
        for (profile, why) in [
            (None, "profiles/offline.toml does not exist"),
            (
                Some("[general\n"),
                "profiles/offline.toml is not valid TOML (line 1, column 9)",
            ),
            (
                Some("[groq]\napi_key = \"gsk_p\"\n[general]\nbackend = \"nope\"\n"),
                "the config with profiles/offline.toml is not valid: ",
            ),
        ] {
            let profiles: Vec<(&str, &str)> = profile.map(|p| ("offline", p)).into_iter().collect();
            let (_dir, loaded) =
                with_profiles(Some(SIDECAR_BASE), None, &profiles, Some("offline"));
            assert_eq!(loaded.profile, None, "{profile:?}");
            let error = loaded.profile_error.expect("the profile failed");
            let line = format!("profile offline did not load, using base: {why}");
            assert!(error.starts_with(&line), "{profile:?}: {error}");
            let warning = loaded.warning.expect("the user is told");
            assert!(warning.contains(&error), "{warning}");
            assert!(!warning.contains("gsk_p"), "{warning}");
            assert_eq!(loaded.config.general.backend, "asr-sidecar", "base");
            assert_eq!(loaded.config.general.vocabulary, ["Base"]);
        }

        let (_dir, loaded) = with_profiles(
            Some(SIDECAR_BASE),
            None,
            &[("offline", "[hotkeys]\ntoggle = \"Super+P\"\n")],
            Some("offline"),
        );
        assert_eq!(
            loaded.profile.as_deref(),
            Some("offline"),
            "applied all the same"
        );
        let warning = loaded.warning.unwrap();
        assert!(
            warning.starts_with("profiles/offline.toml: hotkeys is ignored"),
            "{warning}"
        );
    }

    /// No state file, or `base` in it, is config.toml alone. A state file
    /// naming no valid profile is told and runs on base.
    #[test]
    fn base_and_a_bad_state_file_run_on_config_toml() {
        for active in [None, Some("base")] {
            let (_dir, loaded) = with_profiles(
                Some(SIDECAR_BASE),
                None,
                &[("offline", "[general]\nlanguage = \"de\"\n")],
                active,
            );
            assert_eq!((loaded.profile, loaded.profile_error), (None, None));
            assert_eq!(loaded.warning, None);
            assert_eq!(loaded.config.general.language, "en");
        }
        let (_dir, loaded) = with_profiles(Some(SIDECAR_BASE), None, &[], Some("Not Valid"));
        assert_eq!(loaded.profile, None);
        let warning = loaded.warning.unwrap();
        assert!(
            warning.starts_with("the active profile did not load, using base: "),
            "{warning}"
        );
    }

    /// With no config.toml a profile goes over the defaults; with a broken
    /// one the defaults win, as without profiles, and the profile is not
    /// applied.
    #[test]
    fn a_profile_over_no_config_toml_and_over_a_broken_one() {
        let (_dir, loaded) = with_profiles(
            None,
            None,
            &[("offline", "[general]\nbackend = \"local-whisper\"\n")],
            Some("offline"),
        );
        assert_eq!(loaded.profile.as_deref(), Some("offline"));
        assert_eq!(loaded.config.general.backend, "local-whisper");

        let (_dir, loaded) = with_profiles(
            Some("this is [ not toml"),
            None,
            &[("offline", "[general]\nbackend = \"local-whisper\"\n")],
            Some("offline"),
        );
        assert_eq!(loaded.profile, None);
        assert_eq!(loaded.config.general.backend, "groq", "the defaults");
        assert_eq!(
            loaded.profile_error.as_deref(),
            Some("profile offline did not load, using base: config.toml did not load")
        );
        assert!(loaded.warning.unwrap().contains("using defaults"));
    }

    /// The socket is 0600 from the first accept, and still takes connections.
    #[tokio::test]
    async fn bind_socket_is_owner_only_and_accepts() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("whisrs.sock");
        let listener = bind_socket(&path).expect("bind");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        let (client, accepted) =
            tokio::join!(tokio::net::UnixStream::connect(&path), listener.accept());
        client.expect("connect");
        accepted.expect("accept");
    }
}
