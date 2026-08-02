use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

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
    let config_path = whisrs::config_path();
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!("loaded config from {}", config_path.display());
                    return (config, None);
                }
                Err(e) => {
                    let msg = format!(
                        "Failed to parse config at {}: {e} — using defaults",
                        config_path.display()
                    );
                    error!("{msg}");
                    return (
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
                            llm_commands: Vec::new(),
                            overlay: None,
                        },
                        Some(msg),
                    );
                }
            },
            Err(e) => {
                let msg = format!(
                    "Failed to read config at {}: {e} — using defaults",
                    config_path.display()
                );
                error!("{msg}");
                return (
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
                        llm_commands: Vec::new(),
                        overlay: None,
                    },
                    Some(msg),
                );
            }
        }
    } else {
        info!(
            "no config file found at {}; using defaults",
            config_path.display()
        );
    }
    (
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
            llm_commands: Vec::new(),
            overlay: None,
        },
        None,
    )
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
pub(crate) async fn import_compositor_env() {
    // Already have a display server — nothing to do.
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        debug!("compositor environment already available");
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
