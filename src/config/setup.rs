//! Interactive onboarding flow for `whisrs setup`.
//!
//! Guides the user through selecting a backend, entering an API key,
//! choosing a language, testing the microphone, writing `config.toml`,
//! setting up uinput permissions, installing the user service,
//! and configuring keybindings.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Password, Select};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::config::types::{unknown_config_keys, unknown_keys_warning, PreservedKeys};
use crate::llm::LlmConfig;
use crate::service::{ServiceManager, OPENRC_SERVICE, SYSTEMD_UNIT};
use crate::{
    AsrSidecarConfig, AudioConfig, Config, DeepgramConfig, GeneralConfig, GroqConfig,
    InjectorBackend, InputConfig, LocalWhisperConfig, OpenAiCompatibleRealtimeConfig, OpenAiConfig,
    RestartOutcome,
};

// ANSI color codes.
pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const YELLOW: &str = "\x1b[33m";
pub(crate) const RED: &str = "\x1b[31m";
pub(crate) const BOLD: &str = "\x1b[1m";
pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const RESET: &str = "\x1b[0m";

/// Backend choices presented to the user.
pub(crate) const BACKEND_CHOICES: &[&str] = &[
    "Groq               (free, fast, cloud)",
    "Deepgram Streaming (free credits, true streaming, cloud)",
    "Deepgram REST      (free credits, simple, cloud)",
    "OpenAI Realtime    (best streaming, cloud)",
    "OpenAI REST        (simple, cloud)",
    "OpenAI-compatible Realtime (external WebSocket, Lemonade-style)",
    "Local              (offline, no API key needed)",
    "ASR sidecar        (local HTTP sidecar, model-agnostic)",
];

/// Map selection index to backend string used in config.
pub(crate) const BACKEND_VALUES: &[&str] = &[
    "groq",
    "deepgram-streaming",
    "deepgram",
    "openai-realtime",
    "openai",
    "openai-compatible-realtime",
    "local",
    "asr-sidecar",
];

#[derive(Default)]
pub(crate) struct BackendConfigSelection {
    pub deepgram: Option<DeepgramConfig>,
    pub groq: Option<GroqConfig>,
    pub openai: Option<OpenAiConfig>,
    pub local_whisper: Option<LocalWhisperConfig>,
    pub asr_sidecar: Option<AsrSidecarConfig>,
    pub openai_compatible_realtime: Option<OpenAiCompatibleRealtimeConfig>,
}

/// Whisper model choices (name, file size, description).
pub(crate) const WHISPER_MODEL_CHOICES: &[&str] = &[
    "tiny.en    (75 MB,  decent accuracy, very fast)",
    "base.en    (142 MB, good accuracy, real-time)  <- recommended",
    "small.en   (466 MB, very good accuracy, slower)",
];
pub(crate) const WHISPER_MODEL_NAMES: &[&str] = &["tiny.en", "base.en", "small.en"];

/// What the interactive flows found at the config path.
///
/// Before issue #134 this was an `Option` and collapsed four outcomes into
/// `None`: no file, an unreadable file, invalid TOML, and valid TOML that does
/// not deserialize into [`Config`]. Both callers read `None` as "no config
/// file" and went on to write one from defaults, which deleted every section
/// the defaults leave `None` — API keys included — with no backup. The three
/// cases that differ in what the caller must do are separate variants now.
// `Loaded` makes the enum `Config`-sized (~1.2 KB). It is returned by value
// exactly once per `whisrs setup` / `whisrs config` invocation and never
// collected, so boxing would buy an allocation and cost the callers a deref.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ExistingConfig {
    /// No file at the path: first run, nothing to preserve.
    Missing,
    /// A file is there and cannot be used. `message` is what to show the user,
    /// underlying error verbatim — the `toml` crate's error names the line,
    /// column and offending value, which is the single most useful thing the
    /// user gets out of this. `kind` decides what the caller can offer to do
    /// about it.
    Unusable { kind: UnusableKind, message: String },
    /// A usable file, together with the keys in it the schema does not know.
    Loaded {
        config: Config,
        unknown: Vec<String>,
    },
}

/// Why an [`ExistingConfig::Unusable`] file is unusable — the distinction that
/// decides whether writing a fresh config over it can work at all.
///
/// Fusing the two was the second half of issue #134: `run_setup` warned that the
/// old file would be "backed up when the new config is written", asked every
/// wizard question, and only then hit `write_config_to`'s own
/// `fs::read_to_string`, which failed exactly as the load had. Nothing written,
/// no `.bak`, every answer discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnusableKind {
    /// The bytes could not be read at all: mode bits, a root-owned file left by
    /// a `sudo whisrs setup`, a directory at the path, non-UTF-8 content in it.
    /// Rewriting cannot help, because
    /// the write reads the file first and fails the same way. Only a hand fix
    /// gets out of this, so neither flow may offer `whisrs setup`.
    Unreadable,
    /// Read fine — valid TOML or not — but does not deserialize into [`Config`].
    /// Regenerating from scratch works, and [`backup_and_regenerate`] preserves
    /// the original as `.bak`.
    Undeserializable,
}

/// Try to load an existing config from disk, together with the keys in it the
/// schema does not know.
///
/// The daemon warns about those keys via `tracing`, which prints nothing in the
/// CLI flows, so both interactive entry points get the list here and say so
/// themselves (issue #116).
pub(crate) fn load_existing_config() -> ExistingConfig {
    load_existing_config_from(&crate::config_path())
}

/// Implementation of [`load_existing_config`] against an explicit path
/// (testable).
fn load_existing_config_from(path: &Path) -> ExistingConfig {
    // `NotFound` is the missing file, rather than a preceding `path.exists()`:
    // the pre-check left a window in which a file deleted between the two calls
    // came back as `Unusable("...No such file or directory")`, i.e. `whisrs
    // config` telling the user to hand-fix a file that is not there. One `open`
    // makes the function total and removes the TOCTOU.
    //
    // Wording is shared with the daemon's loader (`daemon::startup::load_config`
    // says "Failed to read/parse config at {path}: {e} — using defaults"), so
    // the two name the same broken file the same way. Lowercase and without the
    // trailing fallback: neither CLI flow silently falls back to defaults, and
    // `src/cli/main.rs` prints this after "config failed: " / "setup failed: ".
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ExistingConfig::Missing,
        Err(e) => {
            return ExistingConfig::Unusable {
                kind: UnusableKind::Unreadable,
                message: format!("cannot read config at {}: {e}", path.display()),
            };
        }
    };
    match toml::from_str::<Config>(&contents) {
        Ok(config) => ExistingConfig::Loaded {
            config,
            unknown: unknown_config_keys(&contents),
        },
        // `toml::de::Error` renders with a trailing newline. Trimmed here rather
        // than at each consumer: every one of them continues the sentence, so
        // the newline would leave a blank line mid-message.
        Err(e) => ExistingConfig::Unusable {
            kind: UnusableKind::Undeserializable,
            message: format!("cannot parse config at {}: {e}", path.display())
                .trim_end()
                .to_string(),
        },
    }
}

/// Where [`write_config_to`] parks the previous contents of `config_path` when
/// it has to regenerate the file from scratch.
fn config_backup_path(config_path: &Path) -> PathBuf {
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    config_path.with_file_name(format!("{file_name}.bak"))
}

/// The full text `whisrs config` fails with when the file on disk is
/// [`ExistingConfig::Unusable`]: the message, then what to do about it, which
/// depends on whether rewriting the file could work at all.
///
/// Returned as the error rather than printed, because `src/cli/main.rs` already
/// renders it once as `config failed: {e:#}` and exits non-zero; printing it
/// here as well would show the user the same serde error twice.
pub(crate) fn unusable_config_refusal(
    kind: UnusableKind,
    message: &str,
    config_path: &Path,
) -> String {
    match kind {
        UnusableKind::Unreadable => unreadable_config_refusal(message, config_path),
        UnusableKind::Undeserializable => format!(
            "{message}\n\nNothing was changed. Fix {} by hand, or run `whisrs setup` to start from \
             a fresh config (it backs the current file up to {} first).",
            config_path.display(),
            config_backup_path(config_path).display()
        ),
    }
}

/// The refusal both flows share for [`UnusableKind::Unreadable`], because both
/// can only refuse: the write reads the file first, so `whisrs setup` fails on
/// exactly the read that just failed. Offering it would send the user around a
/// loop that cannot terminate, which is why this text names the hand fix only.
fn unreadable_config_refusal(message: &str, config_path: &Path) -> String {
    let path = config_path.display();
    format!(
        "{message}\n\nNothing was changed, and rewriting the file cannot help: `whisrs setup` \
         writes the config by reading it first, so it fails on the same read. Fix it by hand. The \
         usual cause is ownership or permissions: a config written by `sudo whisrs setup` belongs \
         to root. Check that {path} is a regular file, owned by you and readable at mode 0600:\
         \n\n    sudo chown $USER {path}\n    chmod 600 {path}"
    )
}

/// The warning `whisrs setup` prints for an [`UnusableKind::Undeserializable`]
/// file. Setup's job is to produce a config, so it says what it found and
/// carries on into the wizard; the write at the end keeps the old file as `.bak`.
fn undeserializable_config_setup_warning(message: &str, config_path: &Path) -> String {
    format!(
        "{message}\nThe existing config cannot be used, so setup starts fresh; the current file is \
         backed up to {} when the new config is written.",
        config_backup_path(config_path).display()
    )
}

/// Print the unknown-key warning for a config just loaded from disk, if there
/// is one. Called at load time by both interactive flows, so the user sees the
/// typo before deciding what to do about it.
pub(crate) fn print_unknown_keys_warning(unknown: &[String]) {
    if let Some(w) = unknown_keys_warning(&crate::config_path(), unknown) {
        println!("  {YELLOW}warning:{RESET} {w}");
    }
}

/// Mask an API key for display, showing only the last 4 characters.
pub(crate) fn mask_api_key(key: &str) -> String {
    if key.len() <= 4 {
        "****".to_string()
    } else {
        format!("****{}", &key[key.len() - 4..])
    }
}

/// Run the full interactive setup flow.
///
/// This function does NOT require the daemon to be running.
pub fn run_setup() -> Result<()> {
    println!("\n{BOLD}whisrs setup{RESET} — interactive onboarding\n");

    // Check for existing config.
    match load_existing_config() {
        ExistingConfig::Loaded {
            config: existing_cfg,
            unknown,
        } => {
            println!(
                "  {GREEN}Found existing config{RESET} (backend: {BOLD}{}{RESET})",
                existing_cfg.general.backend
            );
            // Before the prompt, not after: "Use existing" returns immediately
            // below, so a warning printed later would never be shown at all.
            print_unknown_keys_warning(&unknown);
            println!();
            let choice = Select::new()
                .with_prompt("What would you like to do?")
                .items(&["Use existing", "Start fresh"])
                .default(0)
                .interact()
                .context("failed to read setup mode")?;
            if choice == 0 {
                println!("\n  {GREEN}Keeping existing config.{RESET}");
                print_done();
                return Ok(());
            }
        }
        // A file we cannot read at all is a refusal, and it has to come before
        // the first question (issue #134). Warning and continuing here promised a
        // `.bak` that cannot be written, ran the whole wizard — backend, key,
        // language, service install — and then died in `write_config_to`'s own
        // `fs::read_to_string`, discarding every answer. Nothing downstream can
        // recover from an unreadable file, so stop while the user has typed
        // nothing.
        ExistingConfig::Unusable {
            kind: UnusableKind::Unreadable,
            message,
        } => {
            anyhow::bail!(unreadable_config_refusal(&message, &crate::config_path()));
        }
        // A file we did read but cannot deserialize used to land here silently
        // (the other half of #134): setup said nothing and ran the new-install
        // wizard, and the write at the end of it ate the sections the wizard
        // leaves unset. Say what is wrong, then carry on — producing a config is
        // the whole point of setup, so refusing would be wrong, and
        // `write_config_to` keeps the broken file as `.bak`.
        ExistingConfig::Unusable {
            kind: UnusableKind::Undeserializable,
            message,
        } => {
            println!(
                "  {YELLOW}warning:{RESET} {}",
                undeserializable_config_setup_warning(&message, &crate::config_path())
            );
            println!();
        }
        ExistingConfig::Missing => {}
    }

    // 1. Select backend.
    let backend = select_backend(None)?;

    // 2. Configure backend (API key or model download).
    let backend_config = configure_backend(&backend, None)?;

    // 3. Language.
    let language = select_language(None)?;

    // 4. Test microphone.
    test_microphone();

    // 5. Extra options.
    let (remove_filler_words, audio_feedback) = configure_extras()?;

    // 5b. Recording overlay.
    let (overlay, overlay_config) = configure_overlay();

    // 5c. Keyboard-injection backend.
    let injector_backend = select_injector_backend(None)?;

    // 6. Command mode LLM (optional).
    let llm_config = configure_llm()?;

    // 7. Build and write config.
    let config = Config {
        general: GeneralConfig {
            backend,
            language,
            silence_timeout_ms: 2000,
            notify: true,
            remove_filler_words,
            filler_words: Vec::new(),
            audio_feedback,
            audio_feedback_volume: 0.5,
            vocabulary: Vec::new(),
            prompt: None,
            tray: true,
            overlay,
            // Onboarding stays minimal: LLM post-processing of dictation is
            // opt-in and edited by hand (see docs/configuration.md).
            ..GeneralConfig::default()
        },
        audio: AudioConfig {
            device: "default".to_string(),
        },
        input: InputConfig {
            backend: injector_backend,
            ..InputConfig::default()
        },
        deepgram: backend_config.deepgram,
        groq: backend_config.groq,
        openai: backend_config.openai,
        local_whisper: backend_config.local_whisper,
        local_vosk: None,
        local_parakeet: None,
        asr_sidecar: backend_config.asr_sidecar,
        openai_compatible_realtime: backend_config.openai_compatible_realtime,
        llm: llm_config,
        tts: None,
        hotkeys: None,
        hooks: None,
        overlay: if overlay { overlay_config } else { None },
        llm_commands: Vec::new(),
    };

    let config_path = write_config(&config)?;
    println!(
        "\n{GREEN}Config written to {}{RESET}",
        config_path.display()
    );

    // 7. Check and optionally fix uinput permissions.
    setup_uinput_permissions();

    // 8. Offer to install and enable the user service.
    setup_user_service();

    // 9. Offer to add keybinding.
    setup_keybinding();

    // 10. Print summary.
    print_done();

    Ok(())
}

/// Prompt the user to select a transcription backend.
pub(crate) fn select_backend(existing: Option<&Config>) -> Result<String> {
    // Determine the default index based on existing config.
    let default_idx = existing
        .map(|cfg| {
            let b = cfg.general.backend.as_str();
            match b {
                "groq" => 0,
                "deepgram-streaming" => 1,
                "deepgram" => 2,
                "openai-realtime" => 3,
                "openai" => 4,
                "openai-compatible-realtime" => 5,
                _ if b.starts_with("local") => 6,
                "asr-sidecar" | "asr" | "vibevoice" => 7,
                _ => 0,
            }
        })
        .unwrap_or(0);

    let selection = Select::new()
        .with_prompt("Select a transcription backend")
        .items(BACKEND_CHOICES)
        .default(default_idx)
        .interact()
        .context("failed to read backend selection")?;

    let mut backend = BACKEND_VALUES[selection].to_string();

    // If "local" selected, show engine sub-menu.
    if backend == "local" {
        backend = select_local_engine()?;
    }

    println!("  {DIM}Selected: {backend}{RESET}");
    Ok(backend)
}

/// Prompt the user to select the keyboard-injection backend.
///
/// `auto` is recommended: it uses the Wayland virtual keyboard when the
/// compositor supports `zwp_virtual_keyboard_v1` and otherwise falls back to
/// uinput. The Wayland backend types layout-independently, which fixes
/// garbled bilingual / code-switching dictation on Wayland (issue #44).
pub(crate) fn select_injector_backend(existing: Option<&Config>) -> Result<InjectorBackend> {
    const BACKENDS: &[InjectorBackend] = &[
        InjectorBackend::Auto,
        InjectorBackend::Uinput,
        InjectorBackend::WaylandVk,
    ];
    let default_idx = existing
        .map(|cfg| match cfg.input.backend {
            InjectorBackend::Auto => 0,
            InjectorBackend::Uinput => 1,
            InjectorBackend::WaylandVk => 2,
        })
        .unwrap_or(0);

    println!();
    let selection = Select::new()
        .with_prompt("Select a keyboard-injection backend")
        .items(&[
            "Auto        (recommended — Wayland virtual keyboard, falls back to uinput)",
            "uinput      (evdev/uinput; layout-dependent on Wayland)",
            "wayland-vk  (force zwp_virtual_keyboard_v1 — fixes bilingual typing on Wayland)",
        ])
        .default(default_idx)
        .interact()
        .context("failed to read injection backend selection")?;

    Ok(BACKENDS[selection])
}

/// Sub-menu for choosing a local transcription engine.
fn select_local_engine() -> Result<String> {
    println!();
    let selection = Select::new()
        .with_prompt("Select a local engine")
        .items(&[
            "whisper.cpp     (recommended — best accuracy, CPU/GPU)",
            "Vosk            (coming soon — true streaming, tiny model)",
            "Parakeet        (coming soon — NVIDIA, ultra-fast)",
        ])
        .default(0)
        .interact()
        .context("failed to read engine selection")?;

    match selection {
        0 => Ok("local-whisper".to_string()),
        1 => {
            println!(
                "  {YELLOW}Vosk support is coming in a future release. Selecting whisper.cpp instead.{RESET}"
            );
            Ok("local-whisper".to_string())
        }
        _ => {
            println!(
                "  {YELLOW}Parakeet support is coming in a future release. Selecting whisper.cpp instead.{RESET}"
            );
            Ok("local-whisper".to_string())
        }
    }
}

/// Configure the selected backend (API key or model path).
pub(crate) fn configure_backend(
    backend: &str,
    existing: Option<&Config>,
) -> Result<BackendConfigSelection> {
    match backend {
        "deepgram" | "deepgram-streaming" => {
            let existing_key = existing
                .and_then(|c| c.deepgram.as_ref())
                .map(|d| &d.api_key);
            let api_key = prompt_api_key_with_existing(
                "Deepgram API key",
                "Get one free ($200 credit) at https://console.deepgram.com/signup",
                existing_key,
                "WHISRS_DEEPGRAM_API_KEY",
            )?;
            let model = existing
                .and_then(|c| c.deepgram.as_ref())
                .map(|d| d.model.clone())
                .unwrap_or_else(crate::config::types::default_deepgram_model);
            Ok(BackendConfigSelection {
                deepgram: Some(DeepgramConfig { api_key, model }),
                ..BackendConfigSelection::default()
            })
        }
        "groq" => {
            let existing_key = existing.and_then(|c| c.groq.as_ref()).map(|g| &g.api_key);
            let api_key = prompt_api_key_with_existing(
                "Groq API key",
                "Get one free at https://console.groq.com/keys",
                existing_key,
                "WHISRS_GROQ_API_KEY",
            )?;
            let model = existing
                .and_then(|c| c.groq.as_ref())
                .map(|g| g.model.clone())
                .unwrap_or_else(|| "whisper-large-v3-turbo".to_string());
            Ok(BackendConfigSelection {
                groq: Some(GroqConfig { api_key, model }),
                ..BackendConfigSelection::default()
            })
        }
        "openai-realtime" | "openai" => {
            let existing_key = existing.and_then(|c| c.openai.as_ref()).map(|o| &o.api_key);
            let api_key = prompt_api_key_with_existing(
                "OpenAI API key",
                "Get one at https://platform.openai.com/api-keys",
                existing_key,
                "WHISRS_OPENAI_API_KEY",
            )?;
            let model = if backend == "openai-realtime" {
                "gpt-realtime-whisper".to_string()
            } else {
                let selection = Select::new()
                    .with_prompt("Select OpenAI model")
                    .items(&[
                        "gpt-4o-mini-transcribe (recommended)",
                        "gpt-4o-transcribe",
                        "whisper-1",
                    ])
                    .default(0)
                    .interact()
                    .context("failed to read model selection")?;
                match selection {
                    0 => "gpt-4o-mini-transcribe",
                    1 => "gpt-4o-transcribe",
                    _ => "whisper-1",
                }
                .to_string()
            };
            Ok(BackendConfigSelection {
                openai: Some(OpenAiConfig { api_key, model }),
                ..BackendConfigSelection::default()
            })
        }
        "openai-compatible-realtime" => {
            let existing_realtime = existing.and_then(|c| c.openai_compatible_realtime.as_ref());
            let url: String = Input::new()
                .with_prompt("Realtime WebSocket URL")
                .default(
                    existing_realtime
                        .map(|v| v.url.clone())
                        .unwrap_or_else(|| "ws://localhost:12345/realtime".to_string()),
                )
                .interact_text()
                .context("failed to read realtime WebSocket URL")?;

            let model: String = Input::new()
                .with_prompt("Realtime model")
                .default(
                    existing_realtime
                        .map(|v| v.model.clone())
                        .unwrap_or_else(|| "Whisper-Tiny".to_string()),
                )
                .interact_text()
                .context("failed to read realtime model")?;

            let profile_items = ["lemonade (recommended)"];
            let profile_default = existing_realtime
                .map(|v| usize::from(v.profile.trim() != "lemonade"))
                .unwrap_or(0);
            let profile_selection = Select::new()
                .with_prompt("Compatibility profile")
                .items(&profile_items)
                .default(profile_default.min(profile_items.len() - 1))
                .interact()
                .context("failed to read realtime profile")?;
            let profile = match profile_selection {
                0 => "lemonade".to_string(),
                _ => unreachable!(),
            };

            let turn_detection_items = [
                "server-vad    (recommended — type completed phrases while you keep speaking)",
                "manual-commit (flush only when recording stops)",
            ];
            let turn_detection_default = existing_realtime
                .map(|v| usize::from(v.turn_detection.trim() == "manual-commit"))
                .unwrap_or(0);
            let turn_detection = match Select::new()
                .with_prompt("Turn detection")
                .items(&turn_detection_items)
                .default(turn_detection_default)
                .interact()
                .context("failed to read realtime turn detection")?
            {
                0 => "server-vad".to_string(),
                1 => "manual-commit".to_string(),
                _ => unreachable!(),
            };

            let api_key = prompt_optional_api_key_with_existing(
                "Optional bearer token",
                "Leave blank for servers that do not require auth (Lemonade commonly does not).",
                existing_realtime.and_then(|v| v.api_key.as_ref()),
            )?;

            Ok(BackendConfigSelection {
                openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                    url,
                    model,
                    profile,
                    turn_detection,
                    api_key,
                }),
                ..BackendConfigSelection::default()
            })
        }
        "local-whisper" => {
            // Select model size.
            println!();
            let model_idx = Select::new()
                .with_prompt("Select a whisper model")
                .items(WHISPER_MODEL_CHOICES)
                .default(1) // base.en is recommended
                .interact()
                .context("failed to read model selection")?;

            let model_name = WHISPER_MODEL_NAMES[model_idx];

            let model_dir = default_model_dir();
            let dest = model_dir.join(format!("ggml-{model_name}.bin"));

            if dest.exists() {
                println!("  {GREEN}Model already exists at {}{RESET}", dest.display());
            } else {
                // Offer to download.
                let should_download = Select::new()
                    .with_prompt("Download model now?")
                    .items(&["Yes, download now", "No, I'll download it manually"])
                    .default(0)
                    .interact()
                    .context("failed to read download choice")?;

                if should_download == 0 {
                    download_whisper_model(model_name, &model_dir)?;
                } else {
                    println!("  {DIM}Download the model manually from:{RESET}");
                    println!(
                        "  {DIM}https://huggingface.co/ggerganov/whisper.cpp/tree/main{RESET}"
                    );
                    println!("  {DIM}Place it at: {}{RESET}", dest.display());
                }
            }

            let model_path = dest.to_string_lossy().to_string();
            Ok(BackendConfigSelection {
                local_whisper: Some(LocalWhisperConfig::new(model_path)),
                ..BackendConfigSelection::default()
            })
        }
        "asr-sidecar" | "asr" | "vibevoice" => {
            let existing_sidecar = existing.and_then(|c| c.asr_sidecar.as_ref());
            let url: String = Input::new()
                .with_prompt("ASR sidecar URL")
                .default(
                    existing_sidecar
                        .map(|v| v.url.clone())
                        .unwrap_or_else(|| "http://127.0.0.1:8765/transcribe".to_string()),
                )
                .interact_text()
                .context("failed to read ASR sidecar URL")?;

            let model: String = Input::new()
                .with_prompt("ASR sidecar model")
                .default(
                    existing_sidecar
                        .map(|v| v.model.clone())
                        .unwrap_or_else(|| "microsoft/VibeVoice-ASR-HF".to_string()),
                )
                .interact_text()
                .context("failed to read ASR sidecar model")?;

            let api_key = prompt_optional_api_key_with_existing(
                "Optional bearer token",
                "Leave blank for local sidecars that do not require auth.",
                existing_sidecar.and_then(|v| v.api_key.as_ref()),
            )?;

            Ok(BackendConfigSelection {
                asr_sidecar: Some(AsrSidecarConfig {
                    url,
                    model,
                    api_key,
                }),
                ..BackendConfigSelection::default()
            })
        }
        _ => Ok(BackendConfigSelection::default()),
    }
}

/// Return the default directory for storing whisper models.
fn default_model_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("whisrs/models")
}

/// Download a whisper.cpp GGML model from HuggingFace.
fn download_whisper_model(model_name: &str, model_dir: &std::path::Path) -> Result<()> {
    use std::io::{Read, Write};

    let url =
        format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model_name}.bin");
    let dest = model_dir.join(format!("ggml-{model_name}.bin"));

    fs::create_dir_all(model_dir)
        .with_context(|| format!("failed to create model directory {}", model_dir.display()))?;

    println!("\n  Downloading ggml-{model_name}.bin from HuggingFace...");

    // Run download in a separate thread to avoid conflict with tokio runtime.
    let dest_clone = dest.clone();
    let url_clone = url.clone();
    std::thread::spawn(move || -> Result<()> {
        let response = reqwest::blocking::Client::builder()
            .user_agent("whisrs")
            .build()
            .context("failed to build HTTP client")?
            .get(&url_clone)
            .send()
            .context("failed to connect to HuggingFace — check your internet connection")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "download failed: HTTP {} from {url_clone}",
                response.status()
            );
        }

        let total_size = response.content_length().unwrap_or(0);

        let pb = indicatif::ProgressBar::new(total_size);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )
            .unwrap()
            .progress_chars("=> "),
        );

        let mut file = fs::File::create(&dest_clone)
            .with_context(|| format!("failed to create {}", dest_clone.display()))?;

        let mut reader = std::io::BufReader::new(response);
        let mut buf = [0u8; 8192];

        loop {
            let n = reader.read(&mut buf).context("download interrupted")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .context("failed to write model file")?;
            pb.inc(n as u64);
        }

        pb.finish_and_clear();
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("download thread panicked"))??;

    println!("  {GREEN}Model saved to {}{RESET}", dest.display());
    println!("  {DIM}No API key needed — everything runs on your machine.{RESET}");

    Ok(())
}

/// Prompt for an API key, offering to keep the existing one if present.
pub(crate) fn prompt_api_key_with_existing(
    prompt: &str,
    hint: &str,
    existing_key: Option<&String>,
    env_var: &str,
) -> Result<String> {
    if let Some(key) = existing_key {
        if !key.is_empty() {
            println!(
                "  Existing API key found ({BOLD}{}{RESET})",
                mask_api_key(key)
            );
            let keep = Confirm::new()
                .with_prompt("Keep existing key?")
                .default(true)
                .interact()
                .unwrap_or(true);
            if keep {
                return Ok(key.clone());
            }
        }
    }
    println!("  {DIM}{hint}{RESET}");
    let key = Password::new()
        .with_prompt(prompt)
        .interact()
        .context("failed to read API key")?;
    if key.is_empty() {
        let env_set = std::env::var(env_var).is_ok_and(|v| !v.is_empty());
        println!("  {}", empty_key_notice(env_var, env_set));
    }
    Ok(key)
}

/// What to say when the user leaves the API key blank. `env_set` follows the
/// daemon's rule for these backends: the env var counts if it is non-empty,
/// untrimmed. The shell having it does not mean the daemon does, so that
/// branch says so rather than warning about a var that is already set.
fn empty_key_notice(env_var: &str, env_set: bool) -> String {
    if env_set {
        format!(
            "{DIM}No key saved; {env_var} is set in this shell. \
             The daemon must also see it in its own environment.{RESET}"
        )
    } else {
        format!(
            "{YELLOW}Warning: empty API key — set it later in config.toml or via {env_var}{RESET}"
        )
    }
}

pub(crate) fn prompt_optional_api_key_with_existing(
    prompt: &str,
    hint: &str,
    existing_key: Option<&String>,
) -> Result<Option<String>> {
    if let Some(key) = existing_key {
        if !key.is_empty() {
            println!(
                "  Existing bearer token found ({BOLD}{}{RESET})",
                mask_api_key(key)
            );
            let keep = Confirm::new()
                .with_prompt("Keep existing token?")
                .default(true)
                .interact()
                .unwrap_or(true);
            if keep {
                return Ok(Some(key.clone()));
            }
        }
    }
    println!("  {DIM}{hint}{RESET}");
    let key = Password::new()
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()
        .context("failed to read optional bearer token")?;
    Ok((!key.trim().is_empty()).then_some(key))
}

/// Common languages with their ISO 639-1 codes.
pub(crate) const LANGUAGE_CHOICES: &[(&str, &str)] = &[
    ("en", "English"),
    ("auto", "Auto-detect"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("pt", "Portuguese"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
    ("ko", "Korean"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("ru", "Russian"),
    ("pl", "Polish"),
    ("tr", "Turkish"),
    ("sv", "Swedish"),
    ("uk", "Ukrainian"),
];

/// Ask the user for their preferred language.
pub(crate) fn select_language(existing: Option<&Config>) -> Result<String> {
    let default_lang = existing
        .map(|c| c.general.language.clone())
        .unwrap_or_else(|| "en".to_string());

    // Build display items.
    let mut items: Vec<String> = LANGUAGE_CHOICES
        .iter()
        .map(|(code, name)| format!("{name:<15} ({code})"))
        .collect();
    items.push("Other (enter ISO 639-1 code)".to_string());

    // Find default index.
    let default_idx = LANGUAGE_CHOICES
        .iter()
        .position(|(code, _)| *code == default_lang)
        .unwrap_or(0);

    let selection = Select::new()
        .with_prompt("Select language")
        .items(&items)
        .default(default_idx)
        .interact()
        .context("failed to read language selection")?;

    if selection < LANGUAGE_CHOICES.len() {
        let (code, name) = LANGUAGE_CHOICES[selection];
        println!("  {DIM}Selected: {name} ({code}){RESET}");
        Ok(code.to_string())
    } else {
        // "Other" selected — prompt for manual code.
        let code: String = Input::new()
            .with_prompt("Language code (ISO 639-1, e.g. \"fi\", \"cs\", \"vi\")")
            .default(default_lang)
            .interact_text()
            .context("failed to read language code")?;
        Ok(code)
    }
}

/// Attempt to open the default audio input device and report success/failure.
fn test_microphone() {
    use cpal::traits::DeviceTrait;

    println!("\n{BOLD}Testing microphone...{RESET}");

    match crate::audio::device::resolve_input("default") {
        Ok(resolved) => {
            let device = resolved.device;
            println!("  {GREEN}Microphone OK:{RESET} {device}");

            // Try to get a supported config to verify the device actually works.
            match device.default_input_config() {
                Ok(config) => {
                    println!(
                        "  {DIM}Format: {} Hz, {} channel(s){RESET}",
                        config.sample_rate(),
                        config.channels()
                    );
                }
                Err(e) => {
                    println!("  {YELLOW}Warning: could not query device config: {e}{RESET}");
                }
            }
        }
        Err(_) => {
            println!("  {RED}No default audio input device found.{RESET}");

            let devices = crate::audio::device::list_input_devices();
            if devices.is_empty() {
                println!("  No input devices detected. Check that your microphone is connected");
                println!("  and that PipeWire/PulseAudio is running.");
            } else {
                println!("  Available input devices:");
                for d in &devices {
                    println!("    - {} ({})", d.id, d.description);
                }
                println!(
                    "  {DIM}Set the device in config.toml under [audio] device = \"...\"{RESET}"
                );
            }
        }
    }
}

/// Write the config to `~/.config/whisrs/config.toml` with `chmod 0600`.
///
/// Format-preserving (issue #82): when a config file already exists on disk it
/// is parsed with `toml_edit` and updated in place, so the user's comments,
/// section order, and formatting of unchanged values all survive. The daemon
/// calls this on every `set_hotkey` press, which must not shred a hand-tuned
/// file. The write itself is atomic (0600 temp file in the same directory +
/// rename), so a crash or full disk mid-write can never leave a truncated
/// config and the API keys inside are never world-readable, even transiently.
pub fn write_config(config: &Config) -> Result<PathBuf> {
    let config_path = crate::config_path();
    write_config_to(config, &config_path)?;
    Ok(config_path)
}

/// Implementation of [`write_config`] against an explicit path (testable).
///
/// `pub(crate)` so the `whisrs config` save path can be driven end to end in a
/// temp dir — the composition of "write vocabulary.txt, then write config.toml
/// with the right list" is where the destructive bugs live, not in either half.
pub(crate) fn write_config_to(config: &Config, config_path: &Path) -> Result<()> {
    let config_dir = config_path
        .parent()
        .expect("config path should have a parent directory");

    // Create the config directory if it doesn't exist.
    fs::create_dir_all(config_dir)
        .with_context(|| format!("failed to create config directory {}", config_dir.display()))?;

    // Serialize the struct, then parse that back into a TOML document. This
    // "fresh" document is the source of truth for *which* keys exist and what
    // their values are; the on-disk document is the source of truth for
    // comments, ordering, and formatting.
    let fresh_str = toml::to_string_pretty(config).context("failed to serialize config to TOML")?;
    let mut output = match fs::read_to_string(config_path) {
        Ok(existing_str) => match existing_str.parse::<DocumentMut>() {
            // Derived from the bytes on disk, on every write: a set built from
            // `fresh_str`, or cached across writes, would describe keys that are
            // not there and break byte-stability.
            //
            // "Every write" includes the daemon's LLM-command `set` path, not
            // just the interactive flows. Measured cost of the scan: +175 us on a
            // clean synthetic config, +2.2 ms on a real one, 148 ms on a
            // pathological 200-unknown-section file. The confirmation pass only
            // runs for keys the cheap reserialize diff already flagged, so a clean
            // config pays for the diff and nothing else — fine in practice, no
            // optimization wanted.
            //
            // This one call also decides *whether* to merge. The set is what the
            // merge's stale-key pass consults, so asking it directly is the only
            // condition that cannot drift from what the merge will do: an empty
            // set means "understood, nothing unknown", an error means "these bytes
            // cannot be described". Re-deriving the answer here from a separate
            // `toml::from_str::<Config>` agreed only by coincidence — it missed
            // the reserialization failure, which would have merged against an
            // empty set and deleted the unknown sections all over again (#134).
            Ok(mut existing) => match PreservedKeys::from_config_str(&existing_str) {
                Ok(preserved) => {
                    let fresh: DocumentMut = fresh_str
                        .parse()
                        .context("failed to reparse serialized config")?;
                    merge_table(existing.as_table_mut(), fresh.as_table(), &preserved);
                    existing.to_string()
                }
                // A file with no preserve set cannot be merged into: the
                // stale-key pass would delete every section the struct does not
                // carry — API keys included — and the `.bak` backstop below never
                // fired, because it was reached only for a file that is not TOML
                // at all. That was issue #134. Same treatment here: back the file
                // up byte for byte and regenerate.
                //
                // Not a merge with a smarter preserve set on purpose: keeping an
                // aliased section (`[local]`, `[asr]`, `[vibevoice]`) beside the
                // canonical one the writer emits makes the result fail to load
                // with `duplicate field`, which is total config loss (see
                // [`is_preserved`]).
                Err(reason) => {
                    backup_and_regenerate(config_path, &existing_str, &reason.to_string())?;
                    fresh_str
                }
            },
            // Unparseable on-disk file: there is no layout to preserve, so
            // fall back to regenerating it from the struct (pre-#82
            // behavior). This branch skips the merge entirely, so unknown
            // keys are lost here by design — the file is kept verbatim as
            // `.bak`, which is the recovery path for them.
            Err(e) => {
                backup_and_regenerate(
                    config_path,
                    &existing_str,
                    &format!("is not valid TOML ({e})"),
                )?;
                fresh_str
            }
        },
        // First-time setup: nothing on disk yet, plain serialization is fine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fresh_str,
        Err(e) => {
            return Err(e).with_context(|| {
                format!("failed to read existing config {}", config_path.display())
            });
        }
    };

    // Appended as a trailing comment: `merge_table` only touches keys, so
    // unrecognized trailing text survives the round-trip.  Only shown when the
    // user hasn't configured hooks yet, and only if the hint isn't already
    // present (idempotent: writing twice must produce the same bytes).
    const HOOKS_HINT: &str = "\n# [hooks]\n\
             # media_auto_pause = true   # pause playing MPRIS media while dictating, resume it after\n\
             # on_record_start = \"\"     # shell command on recording start\n\
             # on_record_stop = \"\"      # shell command on recording stop\n";
    if config.hooks.is_none() && !output.contains(HOOKS_HINT) {
        output.push_str(HOOKS_HINT);
    }

    atomic_write(config_path, &output)
        .with_context(|| format!("failed to write config to {}", config_path.display()))
}

/// Park the current contents of `config_path` in a private `.bak` beside it and
/// tell the user the file is about to be regenerated from the struct.
///
/// The shared body of the two [`write_config_to`] branches that cannot merge
/// into the file on disk — `toml_edit` could not parse it, or it has no
/// [`PreservedKeys`] set describing it. `reason` completes the sentence "existing
/// config at `<path>` …" and is the only thing that differs between them.
///
/// The broken file is the only copy of the user's hand-edits, so it is saved
/// first, at 0600 because it may hold API keys, and an error here aborts the
/// write rather than overwriting the only copy.
fn backup_and_regenerate(config_path: &Path, existing_str: &str, reason: &str) -> Result<()> {
    let backup_path = config_backup_path(config_path);
    write_private_file(&backup_path, existing_str).with_context(|| {
        format!(
            "existing config {} {reason}, and backing it up to {} failed; refusing to overwrite \
             the only copy",
            config_path.display(),
            backup_path.display()
        )
    })?;
    // `tracing` alone is invisible in the CLI flows (`whisrs setup` / `whisrs
    // config` install no subscriber that prints warnings), so tell the user on
    // stderr as well.
    tracing::warn!(
        "existing config at {} {reason}; rewriting it from scratch (backup saved to {})",
        config_path.display(),
        backup_path.display()
    );
    eprintln!(
        "warning: existing config at {} {reason}; rewriting it from scratch (backup saved to {})",
        config_path.display(),
        backup_path.display()
    );
    Ok(())
}

/// Sync `existing` (the user's on-disk TOML, decor intact) to hold exactly the
/// keys and values of `fresh` (the reserialized struct), plus whatever
/// `preserved` marks as confirmed-unknown. Matching keys keep their comments
/// and formatting, recursing into sub-tables; keys missing from `fresh` are
/// removed unless preserved; new keys are appended.
///
/// `preserved` is the node for *this* table. A file the preserve set cannot
/// describe — not TOML, not a `Config`, or a `Config` that does not reserialize
/// — used to arrive here as an *empty* set, which made the merge delete every
/// section the struct does not carry: issue #134. That cannot happen now.
/// [`write_config_to`] branches on `PreservedKeys::from_config_str` itself and
/// diverts such a file to [`backup_and_regenerate`] before the merge is reached,
/// so an empty set here always means "described, nothing unknown".
///
/// Two deliberate gaps remain:
///
/// * `llm_commands = [{ name = "x", bogus = 1 }]` written as an array of
///   *inline* tables takes `merge_item`'s catch-all and is replaced wholesale,
///   losing `bogus`. Pre-existing; the `[[llm_commands]]` spelling is handled.
/// * A section that is *entirely* unknown but whose name is a serde alias for
///   one the writer emits (`[asr]`, `[vibevoice]`, `[local]`) is warned about
///   and then deleted anyway. Keeping it would put both spellings in the file
///   and make it fail to load with `duplicate field`, so the alias loses. See
///   [`is_preserved`].
fn merge_table(existing: &mut Table, fresh: &Table, preserved: &PreservedKeys) {
    // Drop keys the struct no longer carries. `Config` has no catch-all field,
    // so this used to delete keys it never knew about as well — including the
    // very typo the load-time warning had just told the user to fix (issue
    // #116). Confirmed-unknown keys are exempt now; everything else still goes.
    let stale: Vec<String> = existing
        .iter()
        .filter(|(key, item)| !fresh.contains_key(key) && !is_preserved(preserved, key, item))
        .map(|(key, _)| key.to_string())
        .collect();
    for key in stale {
        existing.remove(&key);
    }

    for (key, fresh_item) in fresh.iter() {
        match existing.get_mut(key) {
            Some(existing_item) => merge_item(existing_item, fresh_item, preserved.table(key)),
            None => {
                // A header-less parent (e.g. `[overlay.colors]` without an
                // explicit `[overlay]`) must gain its header once it holds a
                // plain value.
                if fresh_item.is_value() && existing.is_implicit() {
                    existing.set_implicit(false);
                }
                existing.insert(key, fresh_item.clone());
            }
        }
    }
}

/// Whether `key` names something the writer must keep even though `fresh` no
/// longer carries it: a confirmed-unknown key, or a subtree made entirely of
/// them.
///
/// This is the *only* place a stale key is judged, and it is reached only for
/// keys `fresh` does not carry — which makes it the one place that can safely
/// ask `PreservedKeys::table_prunable`, i.e. "would deleting this whole table
/// change how the file parses?". Two rules, both learned the hard way:
///
/// * "Every leaf of the on-disk subtree is confirmed-unknown" is the rule, and
///   "prefixes a confirmed-unknown path" is *not* good enough. `[local]` is a
///   serde alias for `[local-whisper]` (likewise `[asr]`/`[vibevoice]` for
///   `[asr-sidecar]`), so keeping a `[local]` that still holds real settings
///   while the merge also writes the canonical `[local-whisper]` makes the file
///   fail to deserialize with `duplicate field` — and the daemon then silently
///   falls back to defaults, throwing away the user's entire config. Losing one
///   stray key next to a real one is by far the lesser evil.
/// * Even an *all*-unknown table can be an alias in disguise (`[asr] bogus = 1`
///   prunes to an empty table that still means `[asr-sidecar]`), so a whole
///   table is kept only when removing it provably changes nothing. Asking that
///   question anywhere but here is what deleted typo'd keys out of `[hooks]`
///   and `[hotkeys]`: those names *are* in `fresh`, so this function is never
///   called for them and the merge simply recurses into them.
fn is_preserved(preserved: &PreservedKeys, key: &str, item: &Item) -> bool {
    if preserved.is_empty() {
        return false;
    }
    match item {
        Item::None => false,
        Item::Value(value) => value_is_preserved(preserved, key, value),
        Item::Table(table) => {
            let child = preserved.table(key);
            // `table_prunable()` is what carries the safety property here: a
            // leafless on-disk table contributes no `PreservedKeys` node, so
            // an empty `[asr]` is already refused by that flag before the
            // emptiness check is consulted. `!table.is_empty()` is a cheap
            // early-out and defence in depth, not the guard — replacing it
            // with `true` leaves the suite green. Kept anyway: nothing should
            // be *kept whole* when there is nothing in it to keep.
            child.table_prunable()
                && !table.is_empty()
                && table
                    .iter()
                    .all(|(key, item)| subtree_is_preserved(child, key, item))
        }
        // Only an array of tables under a wholly unknown key can be stale; the
        // schema's own (`[[llm_commands]]`) is always present in `fresh`.
        Item::ArrayOfTables(_) => preserved.contains_leaf(key),
    }
}

/// [`is_preserved`] for a key *inside* a subtree the caller is already keeping
/// whole. Same rules, plus one that only makes sense there: a table with no
/// leaves at all is preserved vacuously.
///
/// `t = {}`, `a = { b = {} }` and a bare `[bogus.emptysub]` header name nothing,
/// so "every leaf of this subtree is confirmed-unknown" is trivially true of
/// them — but they contribute no leaf, so `PreservedKeys` has no node for them
/// and the flag-based rules read them as *not* preserved. One stray `{}`
/// anywhere inside `[bogus]` then vetoed the whole section, and the writer
/// deleted it: the comment, the real keys, and — because the rewritten file no
/// longer held the keys — the warning that would have mentioned them. Issue
/// #116's exact symptom, produced by the fix for issue #116.
///
/// Deliberately *not* applied at the top level, where [`is_preserved`] is
/// reached from the stale filter. An empty `[asr]` beside any other unknown key
/// would then be kept next to the canonical `[asr-sidecar]` the writer emits,
/// and serde rejects that file with `duplicate field` — total config loss. Up
/// there a leafless table must keep failing; the `table_prunable` check is the
/// only thing standing between an aliased section and a config the daemon
/// cannot load.
fn subtree_is_preserved(preserved: &PreservedKeys, key: &str, item: &Item) -> bool {
    match item {
        Item::Value(value) => subtree_value_is_preserved(preserved, key, value),
        Item::Table(table) if table_has_no_leaves(table) => true,
        _ => is_preserved(preserved, key, item),
    }
}

/// The value half of [`is_preserved`]: inline tables recurse, everything else
/// is a leaf. A whole inline table takes the same prunability check as a header
/// table — `asr = { bogus = 1 }` is `[asr] bogus = 1` in another spelling.
fn value_is_preserved(preserved: &PreservedKeys, key: &str, value: &Value) -> bool {
    match value {
        Value::InlineTable(inline) => {
            let child = preserved.table(key);
            // Same division of labour as in [`is_preserved`]: the flag is the
            // guard against `asr = {}` surviving beside `[asr-sidecar]`,
            // `!inline.is_empty()` only the cheap early-out beside it.
            child.table_prunable()
                && !inline.is_empty()
                && inline
                    .iter()
                    .all(|(key, value)| subtree_value_is_preserved(child, key, value))
        }
        _ => preserved.contains_leaf(key),
    }
}

/// The value half of [`subtree_is_preserved`].
fn subtree_value_is_preserved(preserved: &PreservedKeys, key: &str, value: &Value) -> bool {
    match value {
        Value::InlineTable(inline) if inline_has_no_leaves(inline) => true,
        _ => value_is_preserved(preserved, key, value),
    }
}

/// Whether `table` holds no leaf keys at all — only (possibly nested) empty
/// tables. Mirrors `types::has_no_leaves`, over the `toml_edit` types.
fn table_has_no_leaves(table: &Table) -> bool {
    table.iter().all(|(_, item)| match item {
        Item::Table(inner) => table_has_no_leaves(inner),
        Item::Value(Value::InlineTable(inner)) => inline_has_no_leaves(inner),
        _ => false,
    })
}

/// [`table_has_no_leaves`] for an inline table, whose entries are all values.
fn inline_has_no_leaves(inline: &InlineTable) -> bool {
    inline.iter().all(|(_, value)| match value {
        Value::InlineTable(inner) => inline_has_no_leaves(inner),
        _ => false,
    })
}

/// Merge one item of a table, dispatching on its structure. `preserved` is the
/// node for this item's own subtree.
fn merge_item(existing: &mut Item, fresh: &Item, preserved: &PreservedKeys) {
    match (existing, fresh) {
        (Item::Table(existing), Item::Table(fresh)) => merge_table(existing, fresh, preserved),
        (Item::ArrayOfTables(existing), Item::ArrayOfTables(fresh)) => {
            merge_array_of_tables(existing, fresh, preserved)
        }
        // The user wrote a section as an inline table (`colors = { ... }`);
        // the serializer always produces a header table. Keep their spelling —
        // at every depth. Bailing out to the catch-all when the fresh table was
        // not flat is what ate `overlay = { colors = { bogos = 1 } }`: the root
        // inline spelling of the one section with a sub-section, replaced
        // wholesale by the fresh item, unknown keys and all.
        (Item::Value(Value::InlineTable(existing)), Item::Table(fresh)) => {
            merge_inline_table(existing, fresh, preserved)
        }
        (Item::Value(existing), Item::Value(fresh)) => merge_value(existing, fresh),
        // Structural change (e.g. `llm_commands = []` becoming a populated
        // `[[llm_commands]]` array, or vice versa): take the fresh item as-is.
        (existing, fresh) => *existing = fresh.clone(),
    }
}

/// Replace a scalar/array value only when it actually changed, carrying the
/// old decor (surrounding whitespace + trailing inline comment) over to the
/// new value. Untouched values keep their exact user spelling (quoting style,
/// number format, array layout).
fn merge_value(existing: &mut Value, fresh: &Value) {
    if values_equal(existing, fresh) {
        return;
    }
    let decor = existing.decor().clone();
    let mut new_value = fresh.clone();
    *new_value.decor_mut() = decor;
    *existing = new_value;
}

/// Structural equality of two TOML values, ignoring formatting/decor.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, av)| b.get(key).is_some_and(|bv| values_equal(av, bv)))
        }
        _ => false,
    }
}

/// Sync an array-of-tables (e.g. `[[llm_commands]]`). Fresh entries are
/// matched to their on-disk counterpart by `name` first (so deleting or
/// reordering entries keeps each survivor's comments), falling back to
/// position for tables without a usable `name`. Fresh entries with no match
/// are appended; on-disk entries with no match are dropped — whole, unknown
/// keys included, because the user deleted them.
fn merge_array_of_tables(
    existing: &mut ArrayOfTables,
    fresh: &ArrayOfTables,
    preserved: &PreservedKeys,
) {
    let mut consumed = vec![false; existing.len()];
    let mut merged: Vec<Table> = Vec::with_capacity(fresh.len());

    for (fresh_idx, fresh_table) in fresh.iter().enumerate() {
        let by_name = entry_name(fresh_table).and_then(|name| {
            (0..existing.len()).find(|&i| {
                !consumed[i] && existing.get(i).is_some_and(|t| entry_name(t) == Some(name))
            })
        });
        let matched = by_name
            .or_else(|| (fresh_idx < existing.len() && !consumed[fresh_idx]).then_some(fresh_idx));
        match matched.and_then(|i| existing.get(i).cloned().map(|t| (i, t))) {
            Some((i, mut table)) => {
                consumed[i] = true;
                // `i`, never `fresh_idx`: `Seg::Index` is the position in the
                // on-disk document, and deleting an earlier entry shifts every
                // later one. Indexing with the fresh position would apply a
                // deleted entry's unknown keys to its successor and delete the
                // successor's own.
                merge_table(&mut table, fresh_table, preserved.element(i));
                merged.push(table);
            }
            None => merged.push(fresh_table.clone()),
        }
    }

    existing.clear();
    for table in merged {
        existing.push(table);
    }
}

/// The `name` key of an array-of-tables entry, if it is a string.
fn entry_name(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

/// Sync a user-written inline table against the header table the serializer
/// produced for the same section.
///
/// Carries its own copy of `merge_table`'s stale filter, so it needs its own
/// copy of the preserve exemption too: `colors = { theme = "x", bogos = 1 }`
/// reaches this path and nothing else.
///
/// A sub-section of `fresh` (`[overlay.colors]` under a root-level
/// `overlay = { ... }`) has to be re-spelled inline to go inside an inline
/// table. Where the user already wrote one, recurse into it so its unknown keys
/// take the same exemption; otherwise `Item::into_value` does the conversion
/// (`Table` -> `InlineTable`, `ArrayOfTables` -> array of inline tables) and
/// normalizes the decor, so the header-table blank lines do not leak into a
/// value position.
fn merge_inline_table(existing: &mut InlineTable, fresh: &Table, preserved: &PreservedKeys) {
    let stale: Vec<String> = existing
        .iter()
        .filter(|(key, value)| {
            !fresh.contains_key(key) && !value_is_preserved(preserved, key, value)
        })
        .map(|(key, _)| key.to_string())
        .collect();
    for key in stale {
        existing.remove(&key);
    }
    for (key, fresh_item) in fresh.iter() {
        if let (Some(Value::InlineTable(nested)), Item::Table(fresh_table)) =
            (existing.get_mut(key), fresh_item)
        {
            merge_inline_table(nested, fresh_table, preserved.table(key));
            continue;
        }
        // `Item::None` is the only shape with no value form, and a serialized
        // `Config` never produces one.
        let Ok(fresh_value) = fresh_item.clone().into_value() else {
            continue;
        };
        match existing.get_mut(key) {
            Some(existing_value) => merge_value(existing_value, &fresh_value),
            None => {
                existing.insert(key, fresh_value);
            }
        }
    }
}

/// Write `contents` to `path` atomically: create a 0600 temp file in the same
/// directory, fsync it, then rename it over `path`. Interrupted writes can
/// only ever leave the temp file behind, never a truncated config, and the
/// mode is set at creation so the file is private for its entire lifetime.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .expect("config path should have a parent directory");
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    // Per-process temp name so a concurrent `whisrs config` save and a daemon
    // persist do not scribble on the same temp file.
    let tmp_path = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let result = write_private_file(&tmp_path, contents).and_then(|()| fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Create (or truncate) `path` with mode 0600 and write `contents`, fsyncing
/// before returning.
///
/// `pub(crate)` because every file whisrs writes next to `config.toml` gets the
/// same treatment — `vocabulary.txt` included, since the feature moves user
/// data out of the 0600 config into a sibling file.
pub(crate) fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // `mode(0o600)` only applies at creation; enforce it again in case a
    // stale temp file from an interrupted run survived with laxer permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Check if /dev/uinput is accessible. If not, offer to fix it automatically.
fn setup_uinput_permissions() {
    use std::fs::OpenOptions;

    println!("\n{BOLD}Checking uinput permissions...{RESET}");

    match OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => {
            println!("  {GREEN}uinput access: OK{RESET}");
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::PermissionDenied {
                println!("  {YELLOW}Cannot open /dev/uinput: {e}{RESET}");
                return;
            }

            println!("  {RED}Cannot open /dev/uinput — permission denied.{RESET}");
            println!();

            // Locate the udev rule file (check common locations).
            let udev_rule_src = find_contrib_file("99-whisrs.rules");

            let choice = Select::new()
                .with_prompt("Fix uinput permissions?")
                .items(&[
                    "Yes — install udev rule + add me to input group (requires sudo)",
                    "No — I'll do it myself later",
                ])
                .default(0)
                .interact();

            match choice {
                Ok(0) => {
                    // Install udev rule.
                    if let Some(src) = &udev_rule_src {
                        let status = std::process::Command::new("sudo")
                            .args(["cp", &src.to_string_lossy(), "/etc/udev/rules.d/"])
                            .status();
                        match status {
                            Ok(s) if s.success() => {
                                println!("  {GREEN}Installed udev rule{RESET}");
                                // Reload rules.
                                let _ = std::process::Command::new("sudo")
                                    .args(["udevadm", "control", "--reload-rules"])
                                    .status();
                                let _ = std::process::Command::new("sudo")
                                    .args(["udevadm", "trigger"])
                                    .status();
                            }
                            _ => {
                                println!("  {YELLOW}Failed to install udev rule{RESET}");
                            }
                        }
                    } else {
                        // Write the rule inline if contrib file not found.
                        let rule = "KERNEL==\"uinput\", SUBSYSTEM==\"misc\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"\nKERNEL==\"uinput\", SUBSYSTEM==\"misc\", TEST==\"/usr/bin/setfacl\", RUN+=\"/usr/bin/setfacl -m g:input:rw /dev/$name\"";
                        let status = std::process::Command::new("sudo")
                            .args([
                                "bash",
                                "-c",
                                &format!("echo '{}' > /etc/udev/rules.d/99-whisrs.rules", rule),
                            ])
                            .status();
                        match status {
                            Ok(s) if s.success() => {
                                println!("  {GREEN}Installed udev rule{RESET}");
                                let _ = std::process::Command::new("sudo")
                                    .args(["udevadm", "control", "--reload-rules"])
                                    .status();
                                let _ = std::process::Command::new("sudo")
                                    .args(["udevadm", "trigger"])
                                    .status();
                            }
                            _ => println!("  {YELLOW}Failed to install udev rule{RESET}"),
                        }
                    }

                    // Add user to input group.
                    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
                    let status = std::process::Command::new("sudo")
                        .args(["usermod", "-aG", "input", &user])
                        .status();
                    match status {
                        Ok(s) if s.success() => {
                            println!("  {GREEN}Added {user} to input group{RESET}");
                            println!("  {YELLOW}You need to log out and back in for group changes to take effect.{RESET}");
                        }
                        _ => {
                            println!("  {YELLOW}Failed to add user to input group{RESET}");
                        }
                    }
                }
                _ => {
                    println!();
                    println!("  Fix manually with one of:");
                    println!();
                    println!("  1. Add yourself to the input group:");
                    println!("     sudo usermod -aG input $USER");
                    println!("     # Then log out and log back in");
                    println!();
                    println!("  2. Install the udev rule (included in contrib/):");
                    println!("     sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/");
                    println!("     sudo udevadm control --reload-rules");
                    println!("     sudo udevadm trigger");
                }
            }
        }
    }
}

/// Offer to install and enable the user service for the detected init system.
fn setup_user_service() {
    let manager = ServiceManager::detect();

    if manager == ServiceManager::None {
        println!("\n{BOLD}Auto-start service...{RESET}");
        println!("  {YELLOW}Neither systemd nor OpenRC detected.{RESET}");
        println!("  {DIM}Start the daemon manually, or wire it into your session:{RESET}");
        println!("    whisrsd &");
        return;
    }

    println!("\n{BOLD}{} service...{RESET}", manager.name());

    let Some(unit_dir) = manager.unit_dir() else {
        println!("  {RED}Could not determine a config directory to install into{RESET}");
        return;
    };
    let dest = match manager {
        ServiceManager::Systemd => unit_dir.join(SYSTEMD_UNIT),
        ServiceManager::OpenRc => unit_dir.join(OPENRC_SERVICE),
        ServiceManager::None => unreachable!("handled above"),
    };

    if dest.exists() {
        println!(
            "  {GREEN}Service already installed at {}{RESET}",
            dest.display()
        );
        if manager.service_installed() && manager.is_active() {
            // Running is not the same as running the right binary: after a
            // switch from a package to `cargo install`, the unit still points
            // at the old whisrsd (issue #146).
            if manager == ServiceManager::Systemd && offer_repoint(manager, &dest) {
                return;
            }
            println!("  {GREEN}Service is already enabled and running{RESET}");
            return;
        }
    }

    let choice = Select::new()
        .with_prompt("Enable whisrs daemon to start automatically?")
        .items(&[
            format!("Yes — install and enable {} service", manager.name()).as_str(),
            "No — I'll start it manually",
        ])
        .default(0)
        .interact();

    match choice {
        Ok(0) => {
            if let Err(e) = fs::create_dir_all(&unit_dir) {
                println!("  {RED}Failed to create {}: {e}{RESET}", unit_dir.display());
                return;
            }
            if !write_service_file(manager, &dest) {
                return;
            }
            println!("  {GREEN}Installed service to {}{RESET}", dest.display());
            enable_service(manager);
        }
        _ => {
            println!("  {DIM}You can start the daemon manually: whisrsd &{RESET}");
            println!("  {DIM}Or enable the service later:{RESET}");
            // Enabling only works once the service file is installed, so name
            // the install step too — `systemctl enable` on its own fails with
            // "Unit does not exist".
            match manager {
                ServiceManager::Systemd => {
                    // Not a plain `cp`: the packaged unit ships
                    // `ExecStart=whisrsd`, and systemd resolves a bare name
                    // against a compiled-in search path that never contains
                    // ~/.cargo/bin. Copying it verbatim is the same broken
                    // unit `write_service_file` exists to avoid, so name the
                    // resolved path here too.
                    println!("    cp contrib/whisrs.service ~/.config/systemd/user/");
                    println!(
                        "    sed -i 's|^ExecStart=.*|ExecStart={}|' \
                         ~/.config/systemd/user/whisrs.service",
                        which_whisrsd()
                    );
                }
                ServiceManager::OpenRc => {
                    println!(
                        "    install -Dm755 contrib/openrc/whisrs.initd ~/.config/rc/init.d/whisrs"
                    );
                    println!(
                        "    install -Dm644 contrib/openrc/whisrs.confd ~/.config/rc/conf.d/whisrs"
                    );
                }
                ServiceManager::None => {}
            }
            if let Some(enable) = manager.enable_hint() {
                println!("    {enable}");
            }
        }
    }
}

/// Offer to repoint an installed systemd unit whose `ExecStart=` runs a
/// different whisrsd than the one installed now.
///
/// Returns false when there is nothing to offer (unit unreadable, no
/// `ExecStart=`, or already pointing at the resolved binary), so the caller
/// falls through to its usual "already running" message. Rewrites the unit in
/// place rather than re-copying `contrib/`, so the user's own edits survive.
fn offer_repoint(manager: ServiceManager, dest: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(dest) else {
        return false;
    };
    let Some(current) = exec_start_binary(&contents) else {
        return false;
    };
    let resolved = which_whisrsd();
    let home = dirs::home_dir();
    if !unit_needs_repoint(
        current,
        Path::new(&resolved),
        home.as_deref(),
        &systemd_search_path(),
    ) {
        return false;
    }

    println!("  {YELLOW}The service runs a different whisrsd than the one installed now:{RESET}");
    println!("    service:   {current}");
    println!("    installed: {resolved}");

    let choice = Select::new()
        .with_prompt(format!("Repoint the service at {resolved}?"))
        .items(&["Yes", "No"])
        .default(0)
        .interact();

    if !matches!(choice, Ok(0)) {
        // Not a `sed` one-liner: replacing the whole line drops arguments and
        // execution prefixes, which is what `rewrite_exec_start` avoids.
        println!(
            "  {DIM}You can repoint it later: edit the ExecStart= line in {} \
             to point at {resolved} (or re-run `whisrs setup`), then run:{RESET}",
            dest.display()
        );
        println!("    systemctl --user daemon-reload");
        if let Some(restart) = manager.restart_hint() {
            println!("    {restart}");
        }
        return true;
    }

    if let Err(e) = fs::write(dest, rewrite_exec_start(&contents, &resolved)) {
        println!("  {RED}Failed to write service file: {e}{RESET}");
        return true;
    }
    println!("  {GREEN}Repointed service at {resolved}{RESET}");

    // Restarting without a successful reload would start the old ExecStart
    // again, and report success while doing it.
    let reloaded = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .is_ok_and(|s| s.success());
    if !reloaded {
        println!(
            "  {RED}The service file was updated, but `systemctl --user daemon-reload` \
             failed, so the service still runs the old binary. Run:{RESET}"
        );
        println!("    systemctl --user daemon-reload");
        if let Some(restart) = manager.restart_hint() {
            println!("    {restart}");
        }
        return true;
    }
    match manager.restart() {
        RestartOutcome::Restarted => {
            println!("  {GREEN}Service restarted{RESET}");
        }
        RestartOutcome::Failed | RestartOutcome::NoService => {
            println!("  {YELLOW}Failed to restart the service, you can do it manually:{RESET}");
            if let Some(restart) = manager.restart_hint() {
                println!("    {restart}");
            }
        }
    }
    true
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

/// Write the service definition for `manager` to `dest`.
///
/// Prefers the file shipped in `contrib/`, falling back to an inline copy for
/// installs where `contrib/` isn't on disk (e.g. `cargo install`). Returns
/// false when it printed an error and the caller should stop.
fn write_service_file(manager: ServiceManager, dest: &Path) -> bool {
    let contrib_name = match manager {
        ServiceManager::Systemd => "whisrs.service",
        ServiceManager::OpenRc => "openrc/whisrs.initd",
        ServiceManager::None => return false,
    };

    if let Some(src) = find_contrib_file(contrib_name) {
        if manager == ServiceManager::Systemd {
            // Keep the packaged unit, but write an absolute ExecStart. systemd
            // resolves a bare `ExecStart=whisrsd` against a compiled-in search
            // path that never contains ~/.cargo/bin, so the shipped line only
            // works when the binary landed in /usr/bin or /usr/local/bin.
            let whisrsd_path = which_whisrsd();
            let content = match fs::read_to_string(&src) {
                Ok(contents) => rewrite_exec_start(&contents, &whisrsd_path),
                Err(e) => {
                    println!("  {RED}Failed to read service file: {e}{RESET}");
                    return false;
                }
            };
            if let Err(e) = fs::write(dest, content) {
                println!("  {RED}Failed to write service file: {e}{RESET}");
                return false;
            }
        } else if let Err(e) = fs::copy(&src, dest) {
            println!("  {RED}Failed to copy service file: {e}{RESET}");
            return false;
        }
    } else {
        let whisrsd_path = which_whisrsd();
        let content = match manager {
            ServiceManager::Systemd => systemd_unit_contents(&whisrsd_path),
            ServiceManager::OpenRc => openrc_initd_contents(&whisrsd_path),
            ServiceManager::None => return false,
        };
        if let Err(e) = fs::write(dest, &content) {
            println!("  {RED}Failed to write service file: {e}{RESET}");
            return false;
        }
    }

    // OpenRC init scripts are executed directly, so they must be executable —
    // a copied or freshly written file is not, by default.
    if manager == ServiceManager::OpenRc {
        if let Err(e) = fs::set_permissions(dest, fs::Permissions::from_mode(0o755)) {
            println!("  {RED}Failed to make init script executable: {e}{RESET}");
            return false;
        }
        install_openrc_confd();
    }
    true
}

/// Install the OpenRC conf.d tunables file next to the init script.
///
/// The init script defaults every value it reads, so a missing conf.d is not a
/// failure — but README and docs/troubleshooting.md both point users at
/// `~/.config/rc/conf.d/whisrs`, so it should exist for them to edit. Never
/// overwrites: the file holds user settings and may hold API keys.
fn install_openrc_confd() {
    let Some(dir) = dirs::config_dir().map(|c| c.join("rc/conf.d")) else {
        return;
    };
    let dest = dir.join(OPENRC_SERVICE);
    if dest.exists() {
        return;
    }
    let Some(src) = find_contrib_file("openrc/whisrs.confd") else {
        // Inline fallback (`cargo install`, no contrib/ on disk) ships no
        // conf.d; the init script's defaults cover it.
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        println!("  {YELLOW}Could not create {}: {e}{RESET}", dir.display());
        return;
    }
    match fs::copy(&src, &dest) {
        Ok(_) => {
            // May hold API keys, so keep it owner-only rather than the 0644
            // the README suggests for a bare tunables file.
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o600));
            println!("  {GREEN}Installed tunables to {}{RESET}", dest.display());
        }
        Err(e) => {
            println!("  {YELLOW}Could not install conf.d tunables: {e}{RESET}");
        }
    }
}

/// Enable and start the service, reporting a manual fallback on failure.
fn enable_service(manager: ServiceManager) {
    let ok = match manager {
        ServiceManager::Systemd => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", SYSTEMD_UNIT])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        ServiceManager::OpenRc => {
            let added = std::process::Command::new("rc-update")
                .args(["--user", "add", OPENRC_SERVICE, "default"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let started = std::process::Command::new("rc-service")
                .args(["--user", OPENRC_SERVICE, "start"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            added && started
        }
        ServiceManager::None => false,
    };

    if ok {
        println!("  {GREEN}Service enabled and started{RESET}");
    } else {
        println!("  {YELLOW}Failed to enable service — you can do it manually:{RESET}");
        if let Some(enable) = manager.enable_hint() {
            println!("    {enable}");
        }
    }
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
/// This is a reduced version of `contrib/openrc/whisrs.initd` — it keeps the
/// supervision and session-environment recovery, which the daemon cannot work
/// without, but drops the conf.d tunables. Users who want those should install
/// the full script from contrib.
fn openrc_initd_contents(whisrsd_path: &str) -> String {
    format!(
        "#!/sbin/openrc-run\n\
         # whisrs dictation daemon — OpenRC user service\n\
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
         # conf.d variables are sourced, not exported — re-export the ones\n\
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

/// Detect the compositor and offer to add a keybinding for `whisrs toggle`.
fn setup_keybinding() {
    println!("\n{BOLD}Keybinding...{RESET}");

    let compositor = detect_compositor();

    match compositor.as_deref() {
        Some("hyprland") => setup_hyprland_keybinding(),
        Some("sway") => setup_sway_keybinding(),
        Some(name) => {
            println!("  Detected compositor: {name}");
            println!(
                "  {DIM}Add a keybinding for {BOLD}whisrs toggle{RESET}{DIM} in your WM/DE config.{RESET}"
            );
        }
        None => {
            println!(
                "  {DIM}Could not detect compositor. Add a keybinding for {BOLD}whisrs toggle{RESET}{DIM} in your WM/DE config.{RESET}"
            );
        }
    }
}

/// Detect which compositor/WM is running.
fn detect_compositor() -> Option<String> {
    // Check HYPRLAND_INSTANCE_SIGNATURE first (most specific).
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        return Some("hyprland".to_string());
    }
    // Check SWAYSOCK.
    if std::env::var("SWAYSOCK").is_ok() {
        return Some("sway".to_string());
    }
    // Fallback: XDG_CURRENT_DESKTOP.
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
        let lower = desktop.to_lowercase();
        if lower.contains("hyprland") {
            return Some("hyprland".to_string());
        }
        if lower.contains("sway") {
            return Some("sway".to_string());
        }
        if lower.contains("gnome") {
            return Some("gnome".to_string());
        }
        if lower.contains("kde") || lower.contains("plasma") {
            return Some("kde".to_string());
        }
        if lower.contains("i3") {
            return Some("i3".to_string());
        }
        return Some(lower);
    }
    None
}

/// Offer to add a Hyprland keybinding.
fn setup_hyprland_keybinding() {
    println!("  Detected: {GREEN}Hyprland{RESET}");

    let hypr_conf = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("hypr/hyprland.conf");

    if !hypr_conf.exists() {
        println!(
            "  {YELLOW}Hyprland config not found at {}{RESET}",
            hypr_conf.display()
        );
        println!("  {DIM}Add this to your config manually:{RESET}");
        println!("    bind = $mainMod, W, exec, whisrs toggle");
        return;
    }

    // Check if binding already exists.
    if let Ok(contents) = fs::read_to_string(&hypr_conf) {
        if contents.contains("whisrs toggle") {
            println!("  {GREEN}Keybinding already configured in hyprland.conf{RESET}");
            return;
        }
    }

    let whisrs_path = which_whisrs();

    let choice = Select::new()
        .with_prompt("Add keybinding (Super+W) for whisrs toggle?")
        .items(&["Yes — append to hyprland.conf", "No — I'll add it myself"])
        .default(0)
        .interact();

    match choice {
        Ok(0) => {
            let binding = format!(
                "\n# whisrs — voice-to-text dictation\nbind = $mainMod, W, exec, {whisrs_path} toggle\n"
            );
            match fs::OpenOptions::new().append(true).open(&hypr_conf) {
                Ok(mut file) => {
                    use std::io::Write;
                    if let Err(e) = file.write_all(binding.as_bytes()) {
                        println!("  {RED}Failed to write to hyprland.conf: {e}{RESET}");
                    } else {
                        println!("  {GREEN}Added binding: Super+W → whisrs toggle{RESET}");
                        println!("  {DIM}Reload Hyprland config or log out/in to activate.{RESET}");
                    }
                }
                Err(e) => {
                    println!("  {RED}Failed to open hyprland.conf: {e}{RESET}");
                }
            }
        }
        _ => {
            println!("  {DIM}Add this to your hyprland.conf:{RESET}");
            println!("    bind = $mainMod, W, exec, {whisrs_path} toggle");
        }
    }
}

/// Offer to add a Sway keybinding.
fn setup_sway_keybinding() {
    println!("  Detected: {GREEN}Sway{RESET}");

    let sway_conf = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("sway/config");

    if !sway_conf.exists() {
        println!(
            "  {YELLOW}Sway config not found at {}{RESET}",
            sway_conf.display()
        );
        println!("  {DIM}Add this to your config manually:{RESET}");
        println!("    bindsym $mod+w exec whisrs toggle");
        return;
    }

    // Check if binding already exists.
    if let Ok(contents) = fs::read_to_string(&sway_conf) {
        if contents.contains("whisrs toggle") {
            println!("  {GREEN}Keybinding already configured in sway config{RESET}");
            return;
        }
    }

    let whisrs_path = which_whisrs();

    let choice = Select::new()
        .with_prompt("Add keybinding (Mod+W) for whisrs toggle?")
        .items(&["Yes — append to sway config", "No — I'll add it myself"])
        .default(0)
        .interact();

    match choice {
        Ok(0) => {
            let binding = format!(
                "\n# whisrs — voice-to-text dictation\nbindsym $mod+w exec {whisrs_path} toggle\n"
            );
            match fs::OpenOptions::new().append(true).open(&sway_conf) {
                Ok(mut file) => {
                    use std::io::Write;
                    if let Err(e) = file.write_all(binding.as_bytes()) {
                        println!("  {RED}Failed to write to sway config: {e}{RESET}");
                    } else {
                        println!("  {GREEN}Added binding: Mod+W → whisrs toggle{RESET}");
                        println!("  {DIM}Reload Sway config to activate.{RESET}");
                    }
                }
                Err(e) => {
                    println!("  {RED}Failed to open sway config: {e}{RESET}");
                }
            }
        }
        _ => {
            println!("  {DIM}Add this to your sway config:{RESET}");
            println!("    bindsym $mod+w exec {whisrs_path} toggle");
        }
    }
}

/// Find a file in the contrib/ directory relative to the executable or CWD.
fn find_contrib_file(name: &str) -> Option<PathBuf> {
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
fn which_whisrsd() -> String {
    // Check if it's in PATH.
    if let Ok(output) = std::process::Command::new("which").arg("whisrsd").output() {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    // Fallback to ~/.cargo/bin/whisrsd.
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    home.join(".cargo/bin/whisrsd")
        .to_string_lossy()
        .to_string()
}

/// Get the path to the `whisrs` binary.
fn which_whisrs() -> String {
    if let Ok(output) = std::process::Command::new("which").arg("whisrs").output() {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    home.join(".cargo/bin/whisrs").to_string_lossy().to_string()
}

/// Ask the user about extra features (filler removal, audio feedback).
fn configure_extras() -> Result<(bool, bool)> {
    println!("\n{BOLD}Extra features...{RESET}");

    let remove_fillers = Confirm::new()
        .with_prompt("Enable filler word removal? (strips \"um\", \"uh\", \"you know\", etc.)")
        .default(true)
        .interact()
        .unwrap_or(true);

    let audio_feedback = Confirm::new()
        .with_prompt("Enable audio feedback? (subtle tones on record start/stop)")
        .default(true)
        .interact()
        .unwrap_or(true);

    if remove_fillers {
        println!("  {GREEN}Filler removal enabled{RESET}");
    }
    if audio_feedback {
        println!("  {GREEN}Audio feedback enabled{RESET}");
    }

    Ok((remove_fillers, audio_feedback))
}

/// Ask the user whether to enable the recording overlay, and on GNOME
/// offer to install the bundled Shell extension that renders it.
fn configure_overlay() -> (bool, Option<crate::OverlayConfig>) {
    println!("\n{BOLD}Recording overlay (optional)...{RESET}");
    println!("  {DIM}A small on-screen audio meter while recording.{RESET}");

    let enable = Confirm::new()
        .with_prompt("Enable the recording overlay?")
        .default(false)
        .interact()
        .unwrap_or(false);

    if !enable {
        return (false, None);
    }

    let theme = pick_overlay_theme();

    if detect_compositor().as_deref() == Some("gnome") {
        offer_install_gnome_extension();
    }

    println!("  {GREEN}Overlay enabled (theme: {theme}){RESET}");
    let cfg = crate::OverlayConfig {
        theme,
        ..crate::OverlayConfig::default()
    };
    (true, Some(cfg))
}

/// Theme picker for the overlay. Always returns a named theme — "custom" is
/// left for power users to set in config.toml.
pub(crate) fn pick_overlay_theme() -> String {
    println!();
    let selection = Select::new()
        .with_prompt("Pick an overlay theme")
        .items(&[
            "Carbon  — monochrome, terminal-clean (recommended)",
            "Ember   — warm amber \"tally light\"",
            "Cyan    — electric blue, audio-equipment vibe",
        ])
        .default(0)
        .interact()
        .unwrap_or(0);

    match selection {
        1 => "ember".to_string(),
        2 => "cyan".to_string(),
        _ => "carbon".to_string(),
    }
}

/// On GNOME, offer to copy the bundled Shell extension into the user's
/// extensions directory and enable it. Falls back to printing manual
/// instructions if anything fails (e.g. running from a `cargo install` build
/// without the contrib/ tree).
fn offer_install_gnome_extension() {
    const UUID: &str = "whisrs-overlay@eresende.github";
    let ext_src = find_contrib_file(&format!("gnome-shell-extension/{UUID}"));

    let ext_target_root = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("gnome-shell/extensions");
    let ext_target = ext_target_root.join(UUID);

    println!();
    println!("  {DIM}GNOME does not support wlroots layer-shell. The bundled GNOME{RESET}");
    println!("  {DIM}Shell extension renders the overlay inside the shell instead.{RESET}");

    if ext_target.exists() {
        println!(
            "  {GREEN}Extension already installed at {}{RESET}",
            ext_target.display()
        );
        return;
    }

    let choice = Select::new()
        .with_prompt("Install the GNOME Shell extension now?")
        .items(&["Yes — copy and enable", "No — I'll install it manually"])
        .default(0)
        .interact();

    if !matches!(choice, Ok(0)) {
        println!("  {DIM}Install manually with:{RESET}");
        println!(
            "    cp -r contrib/gnome-shell-extension/{UUID} ~/.local/share/gnome-shell/extensions/"
        );
        println!("    gnome-extensions enable {UUID}");
        return;
    }

    let Some(src) = ext_src else {
        println!(
            "  {YELLOW}Extension source not found in contrib/ — install whisrs from a clone of{RESET}"
        );
        println!(
            "  {YELLOW}https://github.com/y0sif/whisrs and re-run setup, or copy the extension{RESET}"
        );
        println!("  {YELLOW}directory manually as shown above.{RESET}");
        return;
    };

    if let Err(e) = fs::create_dir_all(&ext_target_root) {
        println!(
            "  {RED}Failed to create {}: {e}{RESET}",
            ext_target_root.display()
        );
        return;
    }

    let status = std::process::Command::new("cp")
        .arg("-r")
        .arg(&src)
        .arg(&ext_target_root)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!(
                "  {GREEN}Installed extension to {}{RESET}",
                ext_target.display()
            );
        }
        _ => {
            println!("  {RED}Failed to copy extension files{RESET}");
            return;
        }
    }

    let status = std::process::Command::new("gnome-extensions")
        .args(["enable", UUID])
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("  {GREEN}Enabled GNOME Shell extension{RESET}");
            println!("  {YELLOW}Log out and back in if it doesn't appear immediately.{RESET}");
        }
        _ => {
            println!("  {YELLOW}Could not enable automatically. Run:{RESET}");
            println!("    gnome-extensions enable {UUID}");
        }
    }
}

/// LLM provider choices for command mode.
pub(crate) const LLM_PROVIDER_CHOICES: &[&str] = &[
    "OpenAI         (recommended)",
    "Groq           (fast, free tier)",
    "OpenRouter     (many models, free options)",
    "Google Gemini  (generous free tier)",
    "Skip           (configure later in config.toml)",
];

/// LLM provider API URLs.
pub(crate) const LLM_PROVIDER_URLS: &[&str] = &[
    "https://api.openai.com/v1/chat/completions",
    "https://api.groq.com/openai/v1/chat/completions",
    "https://openrouter.ai/api/v1/chat/completions",
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
];

/// Model choices per provider: (model_id, display_label).
const OPENAI_MODELS: &[(&str, &str)] = &[
    (
        "gpt-4o-mini",
        "gpt-4o-mini             (cheap, great quality) <- recommended",
    ),
    (
        "gpt-5-mini",
        "gpt-5-mini              (newest, smarter, costs more)",
    ),
    (
        "gpt-5.4-nano",
        "gpt-5.4-nano            (cheapest, fastest, newest)",
    ),
    (
        "gpt-5.4-mini",
        "gpt-5.4-mini            (newest mini, best quality)",
    ),
    ("gpt-4o", "gpt-4o                  (powerful, costs more)"),
];

const GROQ_MODELS: &[(&str, &str)] = &[
    (
        "qwen-qwq-32b",
        "qwen-qwq-32b           (fast, good quality) <- recommended",
    ),
    (
        "deepseek-r1-distill-llama-70b",
        "deepseek-r1-distill-70b (strong reasoning)",
    ),
    (
        "llama-3.3-70b-versatile",
        "llama-3.3-70b           (versatile, general purpose)",
    ),
    (
        "deepseek-r1-distill-qwen-32b",
        "deepseek-r1-distill-32b (fast reasoning)",
    ),
    ("qwen3-32b", "qwen3-32b               (good all-rounder)"),
];

const OPENROUTER_MODELS: &[(&str, &str)] = &[
    (
        "qwen/qwen3-32b:free",
        "qwen3-32b               (free) <- recommended",
    ),
    (
        "deepseek/deepseek-r1-0528:free",
        "deepseek-r1             (free, strong reasoning)",
    ),
    (
        "google/gemini-2.5-flash-preview:free",
        "gemini-2.5-flash        (free, fast)",
    ),
    (
        "openai/gpt-4o-mini",
        "gpt-4o-mini             (paid, reliable)",
    ),
    (
        "anthropic/claude-haiku-4-5",
        "claude-haiku-4.5        (paid, fast)",
    ),
];

const GEMINI_MODELS: &[(&str, &str)] = &[
    (
        "gemini-2.5-flash",
        "gemini-2.5-flash        (fast, cheap) <- recommended",
    ),
    (
        "gemini-3.1-flash-lite-preview",
        "gemini-3.1-flash-lite   (newest, cheapest)",
    ),
    (
        "gemini-2.5-pro",
        "gemini-2.5-pro          (best quality, costs more)",
    ),
    (
        "gemini-3.1-pro-preview",
        "gemini-3.1-pro          (newest pro, preview)",
    ),
];

/// Configure the LLM for command mode (optional).
pub(crate) fn configure_llm() -> Result<Option<LlmConfig>> {
    println!("\n{BOLD}Command mode (optional)...{RESET}");
    println!("  {DIM}Select text + hotkey + speak instruction → LLM rewrites it in place{RESET}");
    println!();

    let selection = Select::new()
        .with_prompt("Select an LLM provider for command mode")
        .items(LLM_PROVIDER_CHOICES)
        .default(LLM_PROVIDER_CHOICES.len() - 1) // default to "Skip"
        .interact()
        .context("failed to read LLM provider selection")?;

    // "Skip" is the last option.
    if selection >= LLM_PROVIDER_URLS.len() {
        println!("  {DIM}Skipped — you can add [llm] to config.toml later{RESET}");
        return Ok(None);
    }

    let api_url = LLM_PROVIDER_URLS[selection];
    let provider_name = LLM_PROVIDER_CHOICES[selection]
        .split_whitespace()
        .next()
        .unwrap_or("LLM");

    // Model selection.
    let model = select_llm_model(selection)?;

    // API key.
    let hint = match selection {
        0 => "Get one at https://platform.openai.com/api-keys",
        1 => "Get one free at https://console.groq.com/keys",
        2 => "Get one at https://openrouter.ai/settings/keys",
        3 => "Get one at https://aistudio.google.com/apikey",
        _ => "",
    };
    println!("  {DIM}{hint}{RESET}");
    let api_key = Password::new()
        .with_prompt(format!("{provider_name} API key"))
        .interact()
        .context("failed to read LLM API key")?;

    if api_key.is_empty() {
        println!("  {YELLOW}Warning: empty API key — command mode won't work until you set it in config.toml{RESET}");
    }

    println!("  {GREEN}Command mode configured: {provider_name} / {model}{RESET}");

    Ok(Some(LlmConfig {
        api_key,
        model,
        api_url: api_url.to_string(),
    }))
}

/// Show model selection menu for a given provider, with an "Other" option.
pub(crate) fn select_llm_model(provider_idx: usize) -> Result<String> {
    let models: &[(&str, &str)] = match provider_idx {
        0 => OPENAI_MODELS,
        1 => GROQ_MODELS,
        2 => OPENROUTER_MODELS,
        3 => GEMINI_MODELS,
        _ => return Ok("gpt-4o-mini".to_string()),
    };

    let mut items: Vec<String> = models.iter().map(|(_, label)| label.to_string()).collect();
    items.push("Other (enter model name manually)".to_string());

    let selection = Select::new()
        .with_prompt("Select a model")
        .items(&items)
        .default(0)
        .interact()
        .context("failed to read model selection")?;

    if selection < models.len() {
        Ok(models[selection].0.to_string())
    } else {
        let default = models[0].0;
        let model: String = Input::new()
            .with_prompt("Model name")
            .default(default.to_string())
            .interact_text()
            .context("failed to read model name")?;
        Ok(model)
    }
}

/// Print the final success message.
fn print_done() {
    // OpenRC has no journal — the init script logs to a file instead.
    let logs = match ServiceManager::detect() {
        ServiceManager::OpenRc => {
            "${XDG_STATE_HOME:-~/.local/state}/whisrs/whisrsd.log".to_string()
        }
        _ => "journalctl --user -u whisrs -f".to_string(),
    };

    println!("\n{GREEN}{BOLD}You're all set!{RESET}");
    println!();
    println!("  {DIM}Config:    ~/.config/whisrs/config.toml{RESET}");
    println!("  {DIM}Logs:      {logs}{RESET}");
    println!("  {DIM}Re-run:    whisrs setup (to change backend or settings){RESET}");
    println!();
    println!("  You can adjust all settings (filler words, audio feedback, silence");
    println!(
        "  timeout, etc.) by editing the config file or re-running {BOLD}whisrs setup{RESET}."
    );
    println!();
}

/// `source` with its whole-line `//` comments dropped, for the tests that assert
/// on the control flow of a function too interactive to call.
///
/// Those tests search a slice of source text for spellings like `return` and
/// `bail!`. The arms they slice carry long explanations, so without this a
/// comment that merely *mentions* returning would fail a test checking that the
/// code does not return. Whole lines only: a trailing comment after code is not
/// worth the quote-tracking, and none of the checked arms has one.
#[cfg(test)]
pub(in crate::config) fn source_without_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice out the `for _var in ... done` conf.d re-export loop.
    ///
    /// Searching the whole script for a variable name is also satisfied by a
    /// name that only appears in the comment above the loop — which is exactly
    /// the drift being guarded against — so the assertions below run against
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

    /// A config the way a user actually writes it: header comments, inline
    /// comments, custom section order ([audio] before [general]), and
    /// commented [[llm_commands]] entries pasted from the docs.
    const COMMENTED_CONFIG: &str = r#"# whisrs config - hand-tuned, do not regenerate
# see docs/configuration.md before touching anything below

[audio]
device = "default" # usb mic drops out, stick to default

[general]
backend = "groq" # fastest cloud option
language = "en"

[hotkeys]
toggle = "Super+D"

# translate the selection into german
[[llm_commands]]
name = "german"
hotkey = "Super+Shift+G"
set_hotkey = "Super+Shift+S"
instruction = "Translate to German."

# tidy up prose without changing meaning
[[llm_commands]]
name = "polish"
hotkey = "Super+Shift+P"
instruction = "Polish the text."
"#;

    /// The issue #134 repro: valid TOML, so `toml_edit` parses it happily, but
    /// `silence_timeout_ms` is quoted and so it does not deserialize into
    /// `Config`. The API key is what the user stood to lose.
    const CONFIG_THAT_DOES_NOT_DESERIALIZE: &str = r#"[general]
backend = "groq"
silence_timeout_ms = "2000"

[groq]
api_key = "sk-my-real-secret-key"
model = "whisper-large-v3-turbo"
"#;

    fn parse_config(toml_str: &str) -> Config {
        toml::from_str(toml_str).expect("fixture should deserialize")
    }

    fn write_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).expect("write fixture");
        path
    }

    #[test]
    fn value_change_keeps_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // The #82 scenario: a set_hotkey press persists a new instruction.
        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands[0].instruction = "Translate to French.".to_string();
        // Plus a changed scalar that carries an inline comment.
        config.general.backend = "openai".to_string();
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        assert!(out.contains("# see docs/configuration.md before touching anything below"));
        assert!(out.contains("# usb mic drops out, stick to default"));
        assert!(out.contains("# translate the selection into german"));
        assert!(out.contains("# tidy up prose without changing meaning"));
        // The changed value keeps its trailing comment.
        assert!(out.contains(r#"backend = "openai" # fastest cloud option"#));
        assert!(out.contains("Translate to French."));
        assert!(!out.contains("Translate to German."));
        // The user's section order survives ([audio] written above [general]).
        assert!(out.find("[audio]").unwrap() < out.find("[general]").unwrap());
        // And the result still round-trips into the struct.
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.general.backend, "openai");
        assert_eq!(reparsed.llm_commands[0].instruction, "Translate to French.");
    }

    #[test]
    fn added_keys_appear() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        let mut config = parse_config(COMMENTED_CONFIG);
        config.general.prompt = Some("Vocabulary: whisrs, Hyprland".to_string());
        config.hotkeys.as_mut().unwrap().speak = Some("Super+R".to_string());
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        // New key in an existing section.
        assert!(out.contains(r#"prompt = "Vocabulary: whisrs, Hyprland""#));
        assert!(out.contains(r#"speak = "Super+R""#));
        // A whole section the file never had (serialized from defaults).
        assert!(out.contains("[input]"));
        // Comments still intact.
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed.general.prompt.as_deref(),
            Some("Vocabulary: whisrs, Hyprland")
        );
        assert_eq!(reparsed.hotkeys.unwrap().speak.as_deref(), Some("Super+R"));
    }

    #[test]
    fn removed_llm_command_disappears_and_survivor_keeps_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // Remove the *first* entry, so the survivor only keeps its comment if
        // entries are matched by name rather than by position.
        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands.retain(|c| c.name != "german");
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("Translate to German."));
        assert!(!out.contains(r#"name = "german""#));
        // The removed entry's comment goes with it.
        assert!(!out.contains("# translate the selection into german"));
        // The survivor keeps its own comment and content.
        assert!(out.contains("# tidy up prose without changing meaning"));
        assert!(out.contains(r#"name = "polish""#));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm_commands.len(), 1);
        assert_eq!(reparsed.llm_commands[0].name, "polish");
    }

    #[test]
    fn clearing_llm_commands_removes_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands.clear();
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("[[llm_commands]]"));
        // Comments elsewhere survive the structural change.
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.llm_commands.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_create_and_rewrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = parse_config(COMMENTED_CONFIG);

        // Fresh create (no file on disk yet).
        write_config_to(&config, &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Rewrite over a file that drifted to laxer permissions.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_config_to(&config, &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn first_time_write_creates_parseable_file() {
        let dir = tempfile::tempdir().unwrap();
        // Parent directory does not exist yet: exercised create_dir_all.
        let path = dir.path().join("whisrs").join("config.toml");

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");
        assert_eq!(reparsed.llm_commands.len(), 2);
    }

    #[test]
    fn unparseable_existing_file_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is [ not toml").unwrap();

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");

        // The broken file was the only copy of the user's hand-edits: it must
        // survive, byte for byte, as a private backup next to the config.
        let backup = dir.path().join("config.toml.bak");
        assert_eq!(fs::read(&backup).unwrap(), b"this is [ not toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "backup may hold API keys, must be 0600");
        }
    }

    /// Issue #134: valid TOML that does not deserialize used to reach the merge
    /// with an empty preserve set, which deleted every section the struct does
    /// not carry, and the `.bak` backstop never fired because it only covered
    /// files that are not TOML at all.
    #[test]
    fn existing_file_that_does_not_deserialize_is_backed_up_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        // What both CLI flows do next: write a config built without any
        // knowledge of the file on disk (the wizard's, or `default_config()`).
        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        // The rewritten file loads — it is a config again, not a second broken one.
        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");

        // And the file it replaced survives byte for byte, 0600 because it may
        // hold API keys (this fixture does).
        let backup = dir.path().join("config.toml.bak");
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes()
        );
        #[cfg(unix)]
        {
            let mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "backup may hold API keys, must be 0600");
        }
    }

    /// The user-visible stake of issue #134: the `[groq] api_key` in a file the
    /// flow could not read still leaves the live config (the struct being
    /// written has no `[groq]` at all), but it is recoverable from the backup
    /// instead of gone without a trace.
    #[test]
    fn api_key_of_an_undeserializable_config_survives_in_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        write_config_to(&parse_config(COMMENTED_CONFIG), &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(
            !out.contains("sk-my-real-secret-key"),
            "the struct being written has no [groq]; nothing should invent one\n{out}"
        );
        let backup = fs::read_to_string(dir.path().join("config.toml.bak")).unwrap();
        assert!(backup.contains("[groq]"), "{backup}");
        assert!(
            backup.contains(r#"api_key = "sk-my-real-secret-key""#),
            "{backup}"
        );
    }

    /// The second write after a divert must take the merge path, because the
    /// file it now finds is one it just wrote.
    ///
    /// Safe today only because `fresh_str` round-trips and `HOOKS_HINT` is
    /// comments: if any `Config` field ever stops round-tripping, every
    /// subsequent write diverts again and overwrites the `.bak` with the
    /// regenerated file, destroying the user's only copy of the original. The
    /// backup has to survive writes that follow the one that made it.
    #[test]
    fn a_second_write_after_a_divert_keeps_the_first_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup = dir.path().join("config.toml.bak");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();
        let after_divert = fs::read(&path).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes()
        );

        // Same struct again — a `whisrs config` save, or a `set_hotkey` press.
        write_config_to(&config, &path).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes(),
            "the second write re-backed-up, clobbering the only copy of the user's original"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            after_divert,
            "the second write did not take the merge path: the regenerated file is not stable"
        );
    }

    /// A serde alias beside the canonical section: `[local]` is an alias for
    /// `[local-whisper]`, so a file carrying both fails to deserialize with
    /// `duplicate field` — a different failure class from the type error every
    /// other non-deserializing fixture here uses, and the one that makes the
    /// "merge with a smarter preserve set" design lose the whole config.
    /// Preserving `[local]` alongside the `[local-whisper]` the writer emits
    /// would produce a file the daemon cannot load at all. Divert instead.
    const CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE: &str = r#"[general]
backend = "local-whisper"

[groq]
api_key = "sk-my-real-secret-key"

# Hand-written before the section was renamed, and kept afterwards.
[local]
model_path = "/home/u/.local/share/whisrs/models/ggml-base.en.bin"

[local-whisper]
model_path = "/home/u/.local/share/whisrs/models/ggml-small.en.bin"
"#;

    #[test]
    fn an_alias_section_beside_the_canonical_one_is_backed_up_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE).unwrap();
        // The premise: this is a `duplicate field` failure, not a type error.
        let err = toml::from_str::<Config>(CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE)
            .expect_err("fixture must not deserialize")
            .to_string();
        assert!(err.contains("duplicate field `local-whisper`"), "{err}");

        write_config_to(&parse_config(COMMENTED_CONFIG), &path).unwrap();

        // The rewritten file loads. A merge that kept `[local]` would not: both
        // spellings in one file is the `duplicate field` above, i.e. the daemon
        // discarding the user's entire config.
        let out = fs::read_to_string(&path).unwrap();
        let reparsed: Config =
            toml::from_str(&out).unwrap_or_else(|e| panic!("rewrite does not load: {e}\n{out}"));
        assert_eq!(reparsed.general.backend, "groq");
        assert!(!out.contains("[local]"), "{out}");

        // And the original survives byte for byte, which is where the user's
        // `api_key` and both `model_path`s are recoverable from.
        assert_eq!(
            fs::read(dir.path().join("config.toml.bak")).unwrap(),
            CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE.as_bytes()
        );
    }

    /// The write half of issue #134's read failure: `write_config_to` reads the
    /// file before rewriting it, so a file it cannot read stops the write. It
    /// must stop *without* side effects — no truncated config, and no `.bak`
    /// holding whatever it managed to read (`run_setup` refuses before the wizard
    /// for exactly this reason, so this path should now only be reachable by a
    /// permission change mid-session).
    #[test]
    #[cfg(unix)]
    fn write_config_to_an_unreadable_file_errors_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits, as in `unreadable_config_is_unusable_not_missing`.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        let err = write_config_to(&parse_config(COMMENTED_CONFIG), &path)
            .expect_err("an unreadable config must not be silently overwritten");
        let rendered = format!("{err:#}");
        assert!(rendered.contains(&path.display().to_string()), "{rendered}");

        // No backup: nothing was read, so there is nothing to have saved, and a
        // partial one would look like a recovery point that is not.
        assert!(
            !dir.path().join("config.toml.bak").exists(),
            "a failed read must not leave a .bak"
        );
        // And the file itself is exactly as it was.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED_CONFIG);
    }

    /// The `?` in `backup_and_regenerate` is the whole safety argument for the
    /// divert: the broken file is the only copy of the user's hand-edits, so a
    /// backup that cannot be written must abort the write rather than let
    /// `atomic_write` replace the original anyway. Pinned here because that
    /// property is now shared by two branches instead of one.
    #[test]
    fn a_divert_whose_backup_cannot_be_written_leaves_the_original_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();
        // A directory where the `.bak` goes: `write_private_file` cannot open it.
        fs::create_dir(dir.path().join("config.toml.bak")).unwrap();

        let err = write_config_to(&parse_config(COMMENTED_CONFIG), &path)
            .expect_err("a failed backup must abort the write");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("refusing to overwrite the only copy"),
            "{rendered}"
        );

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE,
            "the only copy of the user's config must survive a failed backup"
        );
    }

    #[test]
    fn rewriting_identical_config_is_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // First write merges the struct into the commented fixture.
        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();
        let first = fs::read(&path).unwrap();

        // Writing the identical Config again (e.g. a set_hotkey press that
        // changes nothing) must not perturb a single byte.
        write_config_to(&config, &path).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(
            first, second,
            "second write of an identical Config must be byte-identical"
        );
    }

    /// A config carrying keys the schema does not know, in every shape the
    /// merge has to handle: a typo'd key with a trailing comment, a stray key
    /// beside a real one, a whole unknown section with a nested sub-section,
    /// and a stray key inside the *second* `[[llm_commands]]` entry.
    const CONFIG_WITH_UNKNOWN_KEYS: &str = r#"[general]
backend = "groq"

[input]
past = true # typo for `paste` - do not eat my comment

[deepgram]
api_key = "k"
bogus = 2

[bogus]
foo = 1

[bogus.nested]
bar = 2

[[llm_commands]]
name = "german"
hotkey = "Super+Shift+G"
instruction = "Translate to German."

[[llm_commands]]
name = "polish"
hotkey = "Super+Shift+P"
instruction = "Polish the text."
stray = "keep me"
"#;

    fn write_unknown_key_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_WITH_UNKNOWN_KEYS).expect("write fixture");
        path
    }

    /// Write `contents` to a fresh temp config and round-trip it through
    /// `write_config_to`, returning the rewritten file.
    fn rewrite(dir: &tempfile::TempDir, contents: &str) -> String {
        let path = dir.path().join("config.toml");
        fs::write(&path, contents).expect("write fixture");
        write_config_to(&parse_config(contents), &path).unwrap();
        fs::read_to_string(&path).unwrap()
    }

    /// [`rewrite`] plus the two invariants that outrank whatever the caller is
    /// actually asserting: the rewritten file still deserializes into `Config`
    /// (a file that stops loading is total config loss, strictly worse than the
    /// key-eating bug), and writing it again does not perturb a byte (a
    /// preserved key that moves or re-renders churns the file on every save).
    fn rewrite_checked(dir: &tempfile::TempDir, contents: &str) -> String {
        let out = rewrite(dir, contents);
        toml::from_str::<Config>(&out)
            .unwrap_or_else(|e| panic!("rewrite no longer deserializes: {e}\n{out}"));
        let path = dir.path().join("config.toml");
        write_config_to(&parse_config(&out), &path).unwrap();
        assert_eq!(
            out,
            fs::read_to_string(&path).unwrap(),
            "the second write is not byte-identical"
        );
        out
    }

    #[test]
    fn unknown_keys_and_their_comments_survive_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        write_config_to(&parse_config(CONFIG_WITH_UNKNOWN_KEYS), &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        // The typo the load-time warning points at, comment included.
        assert!(
            out.contains("past = true # typo for `paste` - do not eat my comment"),
            "{out}"
        );
        // A stray key beside a real one in a known section.
        assert!(out.contains("bogus = 2"), "{out}");
        // A whole unknown section, nested sub-section included.
        assert!(out.contains("[bogus]"), "{out}");
        assert!(out.contains("foo = 1"), "{out}");
        assert!(out.contains("[bogus.nested]"), "{out}");
        assert!(out.contains("bar = 2"), "{out}");
        // And one inside an array-of-tables entry.
        assert!(out.contains(r#"stray = "keep me""#), "{out}");

        // The known keys still merged normally and the file still loads.
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.deepgram.unwrap().api_key, "k");
        assert!(!reparsed.input.paste);
        // Still reported, so the warning keeps naming them.
        assert!(unknown_config_keys(&out).contains(&"input.past".to_string()));
    }

    #[test]
    fn rewriting_a_config_with_unknown_keys_is_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        let config = parse_config(CONFIG_WITH_UNKNOWN_KEYS);
        write_config_to(&config, &path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        write_config_to(&config, &path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(
            first, second,
            "a preserved key must not move, duplicate, or re-render on the next write"
        );
    }

    #[test]
    fn a_hotkey_alias_is_recanonicalized_while_the_typo_beside_it_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[hotkeys]\nread = \"Super+Shift+R\"\nbogus = \"Super+X\"\n",
        );

        // `read` is a serde alias for `speak`, so it is *known*, never
        // preserved, and replaced by the canonical spelling exactly once —
        // keeping both would be a `duplicate field` error on the next load.
        assert!(!out.contains("read ="), "alias key survived: {out}");
        assert_eq!(out.matches("speak = ").count(), 1, "{out}");
        assert!(out.contains(r#"bogus = "Super+X""#), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed.hotkeys.unwrap().speak.as_deref(),
            Some("Super+Shift+R")
        );
    }

    #[test]
    fn an_unknown_key_survives_the_deletion_of_an_earlier_llm_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        // `stray` lives in on-disk entry 1; deleting entry 0 makes `polish`
        // *fresh* entry 0. Descending with the fresh index would delete
        // `stray` and try to apply the removed entry's keys to `polish`.
        let mut config = parse_config(CONFIG_WITH_UNKNOWN_KEYS);
        config.llm_commands.retain(|c| c.name != "german");
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains(r#"name = "german""#), "{out}");
        assert!(out.contains(r#"name = "polish""#), "{out}");
        assert!(out.contains(r#"stray = "keep me""#), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm_commands.len(), 1);
    }

    #[test]
    fn an_unknown_key_in_an_inline_table_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\ntheme = \"custom\"\n\
             colors = { background = \"#000000\", bogos = 1 }\n",
        );

        // `merge_inline_table` carries its own stale filter, so it needs its
        // own copy of the exemption: the user's inline spelling survives, and
        // so does the stray key inside it.
        assert!(out.contains("colors = {"), "{out}");
        assert!(out.contains("bogos = 1"), "{out}");
        assert!(out.contains(r##"background = "#000000""##), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_mixed_alias_section_is_still_dropped_whole() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"local\"\n\n[local]\nmodel_path = \"/m.bin\"\nbogus = 1\n",
        );

        // `model_path` is a real setting reached through the `local` ->
        // `local-whisper` alias, so the subtree is mixed and goes wholesale.
        // Keeping `[local]` beside the canonical section would make the file
        // fail to deserialize with `duplicate field`, i.e. the daemon would
        // discard the user's entire config.
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[local]"), "{out}");
        assert!(out.contains("[local-whisper]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.local_whisper.unwrap().model_path, "/m.bin");
    }

    #[test]
    fn a_wholly_unknown_alias_section_is_dropped_rather_than_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(&dir, "[general]\nbackend = \"groq\"\n\n[asr]\nbogus = 1\n");

        // Every leaf of `[asr]` is confirmed-unknown, but `asr` is a serde
        // alias for the `asr-sidecar` section the writer emits. The file
        // still loading is the whole point of the check.
        assert!(!out.contains("bogus = 1"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());
    }

    /// The class of config every other fixture here misses: a section the
    /// schema *knows*, whose fields are all optional, holding nothing but
    /// typos. `[hotkeys] speek = "Super+R"` is the likeliest real-world shape
    /// of issue #116 and the one the first cut of the fix still ate — the
    /// section parses to `Some(default)`, deleting it would parse to `None`,
    /// and the whole-table guard read that as "not ignorable" and pruned the
    /// node, taking the keys and their comments with it. Worse, the rewritten
    /// file then reported *nothing* unknown, so even the warning stopped.
    #[test]
    fn a_typo_that_is_a_sections_only_key_survives_with_its_comments() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n\
             # pause music while I dictate\n\
             [hooks]\nmedia_autopause = true      # typo for media_auto_pause\n\n\
             # my hotkeys\n[hotkeys]\nbogus = \"Super+X\"\nspeek = \"Super+R\" # meant `speak`\n\n\
             [input]\npast = true\n\n\
             [llm]\nbogus = 1\n\n[tts]\nbogus = 2\n",
        );

        assert!(
            out.contains("media_autopause = true      # typo for media_auto_pause"),
            "{out}"
        );
        assert!(out.contains("# pause music while I dictate"), "{out}");
        assert!(
            out.contains(r#"speek = "Super+R" # meant `speak`"#),
            "{out}"
        );
        assert!(out.contains("# my hotkeys"), "{out}");
        assert!(out.contains(r#"bogus = "Super+X""#), "{out}");
        assert!(out.contains("past = true"), "{out}");
        assert!(out.contains("bogus = 1"), "{out}");
        assert!(out.contains("bogus = 2"), "{out}");

        // The file must still load, and the warning must keep naming the keys:
        // losing them silently also lost the only prompt to fix them.
        let _: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            unknown_config_keys(&out),
            vec![
                "hooks.media_autopause",
                "hotkeys.bogus",
                "hotkeys.speek",
                "input.past",
                "llm.bogus",
                "tts.bogus"
            ],
            "{out}"
        );
    }

    #[test]
    fn a_typo_in_a_nested_all_optional_section_survives() {
        let dir = tempfile::tempdir().unwrap();

        // As a standard table.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay.colors]\nbogos = 1\n",
        );
        assert!(out.contains("bogos = 1"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();

        // And as an inline table whose *every* key is unknown (`theme` is a
        // field of `[overlay]`, not of `colors`), which is what makes the
        // whole-table question apply one level down.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\ncolors = { theme = \"dark\", bogos = 1 }\n",
        );
        assert!(out.contains(r#"theme = "dark""#), "{out}");
        assert!(out.contains("bogos = 1"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_wholly_unknown_array_of_tables_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[[bogus]]\nfoo = 1\n\n[[bogus]]\nfoo = 2\n",
        );

        // The array takes `is_preserved`'s `ArrayOfTables` arm, where the
        // confirmed-unknown *leaf* `bogus` carries the whole thing.
        assert_eq!(out.matches("[[bogus]]").count(), 2, "{out}");
        assert!(out.contains("foo = 1") && out.contains("foo = 2"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_wholly_unknown_alias_section_is_dropped_in_every_spelling() {
        let dir = tempfile::tempdir().unwrap();

        // `[vibevoice]` aliases `[asr-sidecar]` exactly as `[asr]` does.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[vibevoice]\nbogus = 1\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[vibevoice]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // Same section written as a root inline table, which reaches the
        // exemption through `value_is_preserved` instead of `is_preserved`.
        let out = rewrite(
            &dir,
            "asr = { bogus = 1 }\n\n[general]\nbackend = \"groq\"\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("asr = {"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // `[local]` aliases `[local-whisper]`, and since #137 gave
        // `model_path` a serde default it can be wholly unknown as well.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[local]\nbogus = 1\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[local]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.local_whisper.is_some());

        // And its inline spelling, through `value_is_preserved`.
        let out = rewrite(
            &dir,
            "local = { bogus = 1 }\n\n[general]\nbackend = \"groq\"\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("local = {"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.local_whisper.is_some());
    }

    /// The whole preserve rule as one table, driven through the real writer.
    ///
    /// The invariant that outranks every row: the rewritten file still
    /// deserializes into `Config`. A file that stops loading is total config
    /// loss — strictly worse than the key-eating bug this all exists to fix —
    /// so it is asserted for every row, including the ones that drop keys.
    #[test]
    fn the_preserve_matrix_holds_and_every_rewrite_still_loads() {
        struct Row {
            what: &'static str,
            contents: &'static str,
            /// Substrings the rewritten file must still contain.
            kept: &'static [&'static str],
            /// Substrings it must not contain.
            gone: &'static [&'static str],
            /// Substrings it must contain exactly once (no alias duplication).
            once: &'static [&'static str],
        }

        const GENERAL: &str = "[general]\nbackend = \"groq\"\n\n";
        let rows = [
            Row {
                what: "a typo that is a known section's only key",
                contents: "[hotkeys]\nspeek = \"Super+R\" # meant `speak`\n",
                kept: &["speek = \"Super+R\" # meant `speak`"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo in [hooks]",
                contents: "[hooks]\nmedia_autopause = true\n",
                kept: &["media_autopause = true"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "an all-unknown inline sub-table",
                contents: "[overlay]\ncolors = { theme = \"dark\", bogos = 1 }\n",
                kept: &["theme = \"dark\"", "bogos = 1"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo in a nested standard table",
                contents: "[overlay.colors]\nbogos = 1\n",
                kept: &["bogos = 1"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "typos in [llm] and [tts]",
                contents: "[llm]\nbogus = 1\n\n[tts]\nbogus = 2\n",
                kept: &["bogus = 1", "bogus = 2"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a wholly unknown alias section",
                contents: "[asr]\nbogus = 1\n",
                kept: &["[asr-sidecar]"],
                gone: &["bogus = 1", "[asr]\n"],
                once: &["[asr-sidecar]"],
            },
            Row {
                what: "the other alias for the same section",
                contents: "[vibevoice]\nbogus = 1\n",
                kept: &["[asr-sidecar]"],
                gone: &["bogus = 1", "[vibevoice]"],
                once: &["[asr-sidecar]"],
            },
            Row {
                what: "a mixed alias section",
                contents: "[local]\nmodel_path = \"/m.bin\"\nbogus = 1\n",
                kept: &["[local-whisper]", "/m.bin"],
                gone: &["bogus = 1", "[local]\n"],
                once: &["model_path"],
            },
            Row {
                what: "an unknown sub-table inside a known section",
                contents: "[hooks]\nmedia_auto_pause = true\n\n[hooks.bogus]\nx = 1\n",
                kept: &["[hooks.bogus]", "x = 1", "media_auto_pause = true"],
                gone: &[],
                once: &["[hooks.bogus]"],
            },
            Row {
                what: "a wholly unknown section with a nested one",
                contents: "[bogus]\nfoo = 1\n\n[bogus.nested]\nbar = 2\n",
                kept: &["[bogus]", "foo = 1", "[bogus.nested]", "bar = 2"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo beside real keys",
                contents: "[input]\npast = true\n",
                kept: &["past = true"],
                gone: &[],
                once: &["paste = "],
            },
            Row {
                what: "a hotkey alias",
                contents: "[hotkeys]\nread = \"Super+R\"\n",
                kept: &["speak = \"Super+R\""],
                gone: &["read = "],
                once: &["speak = "],
            },
        ];

        let dir = tempfile::tempdir().unwrap();
        for row in rows {
            let out = rewrite(&dir, &format!("{GENERAL}{}", row.contents));
            for needle in row.kept {
                assert!(out.contains(needle), "{}: lost `{needle}`\n{out}", row.what);
            }
            for needle in row.gone {
                assert!(
                    !out.contains(needle),
                    "{}: kept `{needle}`\n{out}",
                    row.what
                );
            }
            for needle in row.once {
                assert_eq!(
                    out.matches(needle).count(),
                    1,
                    "{}: `{needle}` is not written exactly once\n{out}",
                    row.what
                );
            }
            toml::from_str::<Config>(&out)
                .unwrap_or_else(|e| panic!("{}: rewrite no longer loads: {e}\n{out}", row.what));

            // And the write after that (a `set_hotkey` press, say) must not
            // perturb a byte: a preserved key that moves or re-renders would
            // churn the file on every save.
            let path = dir.path().join("config.toml");
            write_config_to(&parse_config(&out), &path).unwrap();
            assert_eq!(
                out,
                fs::read_to_string(&path).unwrap(),
                "{}: the second write is not byte-stable",
                row.what
            );
        }
    }

    /// One `{}` inside an unknown section used to delete the whole section.
    ///
    /// An empty table contributes no leaf, so `PreservedKeys` has no node for
    /// it, `covers` read the missing node as "not confirmed-unknown", and the
    /// writer dropped `[bogus]` entire — the comment above it and both real
    /// keys with it. And because the rewritten file no longer held those keys,
    /// `unknown_config_keys` went quiet too: the tool warned about the data
    /// once, ate it, and then stopped mentioning it. That is issue #116's exact
    /// symptom, shipped by the fix for issue #116, and the write is byte-stable
    /// afterwards so nothing ever notices.
    #[test]
    fn a_stray_empty_table_does_not_delete_the_section_around_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n\
             # my notes about this section\n\
             [bogus]\nimportant = \"please keep me\"      # user data\n\
             other = 42\nempty_table = {}\n",
        );

        assert!(out.contains("# my notes about this section"), "{out}");
        assert!(
            out.contains("important = \"please keep me\"      # user data"),
            "{out}"
        );
        assert!(out.contains("other = 42"), "{out}");
        // The warning has to keep firing: losing the keys also lost the only
        // prompt to fix them.
        assert_eq!(
            unknown_config_keys(&out),
            vec!["bogus.important", "bogus.other"],
            "{out}"
        );
    }

    /// Every spelling of "a table with no leaves in it", and the blast radius
    /// each one used to have. All of these lost data before the vacuous rule in
    /// `subtree_is_preserved`.
    #[test]
    fn a_leafless_table_never_vetoes_its_enclosing_subtree() {
        let dir = tempfile::tempdir().unwrap();

        // A bare sub-table header with no keys under it.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[bogus]\nimportant = \"keep\"\n\n[bogus.emptysub]\n",
        );
        assert!(out.contains(r#"important = "keep""#), "{out}");
        assert!(out.contains("[bogus.emptysub]"), "{out}");

        // An empty table nested inside an inline one, two levels down.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[bogus]\nimportant = \"keep\"\na = { b = {} }\n",
        );
        assert!(out.contains(r#"important = "keep""#), "{out}");
        assert!(out.contains("a = { b = {} }"), "{out}");

        // An unknown subtree of a *known* section — `[tts]` and `[overlay]` are
        // in `fresh`, so the merge recurses into them and the stale filter sees
        // the sub-table on its own.
        for section in ["tts", "overlay"] {
            let out = rewrite_checked(
                &dir,
                &format!(
                    "[general]\nbackend = \"groq\"\n\n[{section}.bogus]\nkeep_me = 1\nt = {{}}\n"
                ),
            );
            assert!(out.contains("keep_me = 1"), "{section}: {out}");
            assert_eq!(
                unknown_config_keys(&out),
                vec![format!("{section}.bogus.keep_me")],
                "{section}: {out}"
            );
        }

        // Blast radius: the top-most unknown table went, its siblings did not.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[wipeme]\nx = 1\nt = {}\n\n[keepme]\ny = 2\n",
        );
        assert!(out.contains("[wipeme]") && out.contains("x = 1"), "{out}");
        assert!(out.contains("[keepme]") && out.contains("y = 2"), "{out}");
    }

    /// The other side of the vacuous rule: it is deliberately not applied by
    /// the top-level stale filter, so an empty table that is a *direct* key of
    /// a known section still goes. Keeping it there would mean keeping an empty
    /// `[asr]` too, beside the canonical `[asr-sidecar]` the writer emits, and
    /// serde rejects that file with `duplicate field`.
    #[test]
    fn an_empty_table_beside_a_known_sections_keys_is_still_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\nbogus_tbl = {}\nother_bogus = 3\n",
        );

        assert!(!out.contains("bogus_tbl"), "{out}");
        assert!(out.contains("other_bogus = 3"), "{out}");
    }

    /// The vacuous rule must not weaken the alias guard, which is a separate
    /// condition (`table_prunable`, not `covers`). An empty sub-table now lets
    /// `[asr]` *cover* itself, so the whole-table question is asked where it
    /// previously was not — and the answer still has to be "no".
    #[test]
    fn an_alias_section_with_an_empty_subtable_is_still_dropped_whole() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[asr]\nbogus = 1\n\n[asr.empty]\n",
        );

        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[asr]"), "{out}");
        assert!(!out.contains("[asr.empty]"), "{out}");
        assert_eq!(out.matches("[asr-sidecar]").count(), 1, "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // The shape that decides where the vacuous rule may be applied: an
        // aliased section that is *itself* leafless, beside an unknown key
        // elsewhere so the preserve set is non-empty and the stale filter
        // actually runs. Treating leafless tables as preserved at the top level
        // would keep this one next to the `[asr-sidecar]` the writer emits, and
        // serde rejects that with `duplicate field` — the daemon then falls
        // back to defaults and the user's whole config is gone.
        for contents in [
            // As a header, as a bare sub-table header, and as a root inline
            // table (which has to come before any section header to stay at the
            // root rather than becoming a key of `[general]`).
            "[general]\nbackend = \"groq\"\n\n[asr]\n\n[bogus]\nx = 1\n",
            "[general]\nbackend = \"groq\"\n\n[asr.empty]\n\n[bogus]\nx = 1\n",
            "asr = {}\n\n[general]\nbackend = \"groq\"\n\n[bogus]\nx = 1\n",
        ] {
            let spelling = contents;
            let out = rewrite_checked(&dir, contents);
            assert!(!out.contains("asr ="), "{spelling}: {out}");
            assert!(!out.contains("[asr]"), "{spelling}: {out}");
            assert!(!out.contains("[asr.empty]"), "{spelling}: {out}");
            assert_eq!(out.matches("[asr-sidecar]").count(), 1, "{spelling}: {out}");
            // The unrelated unknown section is untouched, so the drop above is
            // the alias guard doing its job, not the preserve set being empty.
            assert!(out.contains("[bogus]") && out.contains("x = 1"), "{out}");
        }
    }

    /// `overlay` is the only section with a sub-section, so it is the only one
    /// whose *root inline* spelling made `merge_item` bail to the catch-all —
    /// the fresh table is not flat — and replace the user's whole table with
    /// the fresh one, unknown keys and all. Every other section round-tripped
    /// this shape fine, which is why it took a nested fixture to find.
    #[test]
    fn a_root_inline_section_with_a_nested_inline_table_keeps_its_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();

        let out = rewrite_checked(
            &dir,
            "overlay = { theme = \"dark\", colors = { ring = \"#fff\", bogos = 1 } }\n\n\
             [general]\nbackend = \"groq\"\n",
        );
        assert!(out.contains("bogos = 1"), "{out}");
        assert!(out.contains(r##"ring = "#fff""##), "{out}");
        assert_eq!(
            unknown_config_keys(&out),
            vec!["overlay.colors.bogos"],
            "{out}"
        );
        // The user's inline spelling survives, so no header is synthesized —
        // which also disposes of the `[overlay ]` the conversion used to emit,
        // key decor and all.
        assert!(!out.contains("[overlay"), "{out}");

        // An unknown scalar in the outer inline table alongside the nested one.
        let out = rewrite_checked(
            &dir,
            "overlay = { theme = \"dark\", ov_bogus = 2, colors = { bogos = 3 } }\n\n\
             [general]\nbackend = \"groq\"\n",
        );
        assert!(out.contains("ov_bogus = 2"), "{out}");
        assert!(out.contains("bogos = 3"), "{out}");
        assert_eq!(
            unknown_config_keys(&out),
            vec!["overlay.colors.bogos", "overlay.ov_bogus"],
            "{out}"
        );
    }

    #[test]
    fn load_existing_config_from_reports_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        let ExistingConfig::Loaded { config, unknown } = load_existing_config_from(&path) else {
            panic!("fixture loads");
        };
        assert_eq!(config.general.backend, "groq");
        assert_eq!(
            unknown,
            vec![
                "bogus.foo",
                "bogus.nested.bar",
                "deepgram.bogus",
                "input.past",
                "llm_commands[1].stray",
            ]
        );

        // A path with nothing at it is `Missing`, and only that: the callers
        // key their "start from defaults" behavior off this variant alone
        // (issue #134).
        assert!(matches!(
            load_existing_config_from(&dir.path().join("absent.toml")),
            ExistingConfig::Missing
        ));
    }

    /// Issue #134: this file used to be indistinguishable from no file at all,
    /// so `whisrs config` opened on defaults and saved over it.
    #[test]
    fn config_that_does_not_deserialize_is_unusable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        let ExistingConfig::Unusable { kind, message: msg } = load_existing_config_from(&path)
        else {
            panic!("valid TOML that does not deserialize must be reported as unusable");
        };
        // Read fine, wrong shape: regenerating over it works, so the flows may
        // offer `whisrs setup`.
        assert_eq!(kind, UnusableKind::Undeserializable);
        // The path, so the user knows which file to open...
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        // ...and the serde error verbatim, which is the only thing that names
        // the offending key and value.
        assert!(
            msg.contains(r#"invalid type: string "2000", expected u64"#),
            "{msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_config_is_unusable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits and there is no portable way to make a
        // file unreadable to root, so skip rather than assert something that
        // only holds for an unprivileged user.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        let ExistingConfig::Unusable { kind, message: msg } = load_existing_config_from(&path)
        else {
            panic!("an unreadable file must not be reported as missing");
        };
        // And not as a shape problem either: this is the kind no rewrite can
        // fix, so both flows must refuse rather than send the user to
        // `whisrs setup` (issue #134).
        assert_eq!(kind, UnusableKind::Unreadable);
        assert!(msg.contains("cannot read config at"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    /// A directory at the config path reads as `Unreadable` too, not as a
    /// missing file: `read_to_string` fails with `IsADirectory`, and only
    /// `NotFound` is `Missing`.
    #[test]
    fn a_directory_at_the_config_path_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::create_dir(&path).unwrap();

        let ExistingConfig::Unusable { kind, message } = load_existing_config_from(&path) else {
            panic!("a directory at the config path must not be reported as missing");
        };
        assert_eq!(kind, UnusableKind::Unreadable);
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    /// These messages are the whole of what the user gets on this path, so pin
    /// their contents: the reason first and verbatim, the path to edit, the
    /// `.bak` the file lands in, and — for `whisrs config` — that nothing was
    /// touched.
    #[test]
    fn undeserializable_config_messages_name_the_path_the_backup_and_the_way_out() {
        let config_path = Path::new("/home/u/.config/whisrs/config.toml");
        let reason = "cannot parse config at /home/u/.config/whisrs/config.toml: \
                      invalid type: string \"2000\", expected u64";

        let refusal = unusable_config_refusal(UnusableKind::Undeserializable, reason, config_path);
        assert!(refusal.starts_with(reason), "{refusal}");
        assert!(refusal.contains("Nothing was changed"), "{refusal}");
        assert!(
            refusal.contains("Fix /home/u/.config/whisrs/config.toml by hand"),
            "{refusal}"
        );
        assert!(
            refusal.contains("/home/u/.config/whisrs/config.toml.bak"),
            "{refusal}"
        );
        // Regenerating over this file works, so the way out is offered.
        assert!(refusal.contains("whisrs setup"), "{refusal}");

        let warning = undeserializable_config_setup_warning(reason, config_path);
        assert!(warning.starts_with(reason), "{warning}");
        assert!(
            warning.contains("/home/u/.config/whisrs/config.toml.bak"),
            "{warning}"
        );
    }

    /// The other kind (issue #134): a file whose bytes cannot be read is one
    /// `write_config_to` cannot read either, so the one thing this message must
    /// never do is send the user to `whisrs setup` — that path is guaranteed to
    /// fail on the same read, after the whole wizard. It has to name the likely
    /// cause and the hand fix instead.
    #[test]
    fn the_unreadable_refusal_names_the_hand_fix_and_never_whisrs_setup() {
        let config_path = Path::new("/home/u/.config/whisrs/config.toml");
        let reason = "cannot read config at /home/u/.config/whisrs/config.toml: \
                      Permission denied (os error 13)";

        let refusal = unreadable_config_refusal(reason, config_path);
        assert!(refusal.starts_with(reason), "{refusal}");
        assert!(refusal.contains("Nothing was changed"), "{refusal}");
        assert!(
            refusal.contains("rewriting the file cannot help"),
            "{refusal}"
        );
        // The cause and the remedy, concretely.
        assert!(refusal.contains("ownership or permissions"), "{refusal}");
        assert!(
            refusal.contains("chown $USER /home/u/.config/whisrs/config.toml"),
            "{refusal}"
        );
        assert!(
            refusal.contains("chmod 600 /home/u/.config/whisrs/config.toml"),
            "{refusal}"
        );
        // And never the way out that cannot work. The text does name `whisrs
        // setup`, to say why it cannot help, so the absence of one phrasing
        // proves nothing — "try `whisrs setup` instead" would slip past it.
        // Assert the explanation is present instead: every sentence mentioning
        // the command has to be the one ruling it out.
        assert!(
            refusal.contains("`whisrs setup` writes the config by reading it first"),
            "the refusal must say why `whisrs setup` cannot help: {refusal}"
        );
        assert_eq!(
            refusal.matches("whisrs setup").count(),
            2,
            "every mention of `whisrs setup` must be part of ruling it out; a new one \
             is a way out that fails on the same read: {refusal}"
        );
        assert!(
            !refusal.contains(".bak"),
            "nothing gets backed up on this path, so promising a .bak is a lie: {refusal}"
        );

        // `whisrs config` refuses with exactly this text, so the two cannot
        // drift apart.
        assert_eq!(
            unusable_config_refusal(UnusableKind::Unreadable, reason, config_path),
            refusal
        );
    }

    /// The text of the function `source` declares at `header`, up to the next
    /// top-level item.
    fn function_body<'a>(source: &'a str, header: &str) -> &'a str {
        source
            .split(header)
            .nth(1)
            .unwrap_or_else(|| panic!("source does not define `{header}`"))
            .split("\n}\n")
            .next()
            .expect("function has a body")
    }

    /// `run_setup` is pure dialoguer IO and cannot be unit tested, so pin the
    /// one line that matters the way `edit.rs` pins its hotkey prompts
    /// (`edit_hotkeys_prompts_for_every_field`). Issue #116 was exactly this
    /// call being absent.
    #[test]
    fn run_setup_warns_about_unknown_keys_before_prompting() {
        let body = function_body(include_str!("setup.rs"), "pub fn run_setup(");
        let warn_at = body
            .find("print_unknown_keys_warning(&unknown)")
            .expect("run_setup never warns about unknown keys");
        // Before the prompt: the "Use existing" branch returns straight after
        // it, so a warning printed later would never be seen at all.
        let prompt_at = body.find("Select::new()").expect("run_setup prompts");
        assert!(
            warn_at < prompt_at,
            "the warning must be printed before the use-existing prompt"
        );
    }

    /// The three `run_setup` arms, sliced out of the source in the order they
    /// appear. The match is the whole of the issue #134 fix on this side and is
    /// pure dialoguer IO below it, so its control flow is only checkable as text.
    ///
    /// Comments are stripped first ([`source_without_comments`]): these arms
    /// carry long explanations, and a future one containing the word "return"
    /// would otherwise fail the assertion that an arm does not return.
    fn run_setup_unusable_arms() -> (String, String, usize, usize) {
        let body = function_body(include_str!("setup.rs"), "pub fn run_setup(");
        let unreadable_at = body
            .find("kind: UnusableKind::Unreadable,")
            .expect("run_setup does not handle an unreadable config");
        let undeserializable_at = body
            .find("kind: UnusableKind::Undeserializable,")
            .expect("run_setup does not handle an undeserializable config");
        let missing_at = body
            .find("ExistingConfig::Missing =>")
            .expect("run_setup does not handle a missing config");
        assert!(
            unreadable_at < undeserializable_at && undeserializable_at < missing_at,
            "this test slices the arms in source order: unreadable, undeserializable, missing"
        );
        (
            source_without_comments(&body[unreadable_at..undeserializable_at]),
            source_without_comments(&body[undeserializable_at..missing_at]),
            unreadable_at,
            undeserializable_at,
        )
    }

    /// Same technique for the issue #134 arm: `run_setup` must say that the file
    /// it found does not load *before* the wizard starts asking questions (it
    /// used to say nothing at all), and must then carry on — refusing would leave
    /// the user with no way to produce a config.
    #[test]
    fn run_setup_warns_about_an_undeserializable_config_then_continues() {
        let body = function_body(include_str!("setup.rs"), "pub fn run_setup(");
        let (_, arm, _, undeserializable_at) = run_setup_unusable_arms();

        assert!(
            arm.contains("undeserializable_config_setup_warning"),
            "run_setup must tell the user why the existing config cannot be used: {arm}"
        );
        // Falls through into the wizard: no early exit of any spelling, and no
        // substituting defaults for the file it could not read.
        for refusal in ["return", "bail!", "?;"] {
            assert!(
                !arm.contains(refusal),
                "setup must fall through into the wizard, not refuse (`{refusal}`): {arm}"
            );
        }
        assert!(
            !arm.contains("default_config()"),
            "setup builds its config from the wizard, not from defaults: {arm}"
        );
        // Before the first wizard question, so the warning is not buried under
        // the prompts it explains.
        let wizard_at = body
            .find("select_backend(None)")
            .expect("run_setup runs the backend wizard");
        assert!(
            undeserializable_at < wizard_at,
            "the warning must be printed before the wizard prompts"
        );
    }

    /// The other half of issue #134, and the one that cost the user a whole
    /// wizard: a config whose bytes cannot be read is a refusal, and it has to
    /// happen before the first question. Warning and continuing ran the entire
    /// wizard and then died in `write_config_to`'s own read, writing nothing and
    /// backing nothing up.
    #[test]
    fn run_setup_refuses_an_unreadable_config_before_the_first_question() {
        let body = function_body(include_str!("setup.rs"), "pub fn run_setup(");
        let (arm, _, unreadable_at, _) = run_setup_unusable_arms();

        assert!(
            arm.contains("bail!"),
            "an unreadable config must abort setup, not warn: {arm}"
        );
        assert!(
            arm.contains("unreadable_config_refusal"),
            "the refusal must carry the ownership/permissions guidance: {arm}"
        );
        // Not the message that promises a backup which cannot be written.
        assert!(
            !arm.contains("undeserializable_config_setup_warning"),
            "the unreadable arm must not promise a .bak: {arm}"
        );
        // Before the first question, so nothing the user types is thrown away.
        let wizard_at = body
            .find("select_backend(None)")
            .expect("run_setup runs the backend wizard");
        assert!(
            unreadable_at < wizard_at,
            "the refusal must come before the wizard prompts"
        );
    }

    /// A blank key with the env var already set must not tell the user to go
    /// set that var, and must not imply the daemon can see it.
    #[test]
    fn empty_key_notice_knows_when_the_env_var_is_set() {
        let set = empty_key_notice("WHISRS_GROQ_API_KEY", true);
        assert!(
            set.contains("WHISRS_GROQ_API_KEY is set in this shell")
                && set.contains("daemon must also see it")
                && !set.contains("Warning"),
            "{set}"
        );

        let unset = empty_key_notice("WHISRS_GROQ_API_KEY", false);
        assert!(
            unset.contains("Warning: empty API key")
                && unset.contains("or via WHISRS_GROQ_API_KEY")
                && !unset.contains("this shell"),
            "{unset}"
        );
    }
}
