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

use crate::config::keys::mask_api_key;
use crate::config::types::unknown_keys_warning;
use crate::config::write::{
    load_existing_config, undeserializable_config_setup_warning, unreadable_config_refusal,
    ExistingConfig, UnusableKind,
};
use crate::llm::LlmConfig;
use crate::service::{ServiceManager, OPENRC_SERVICE, SYSTEMD_UNIT};
use crate::transcription::openai_realtime_protocol::openai_model_supports_languages;
use crate::{
    AsrSidecarConfig, AudioConfig, Config, DeepgramConfig, GeneralConfig, GroqConfig,
    InjectorBackend, InputConfig, LocalWhisperConfig, OpenAiCompatibleRealtimeConfig, OpenAiConfig,
    RestartOutcome,
};

/// The daemon saves a reprogrammed `[[llm_commands]]` instruction through
/// this path (`daemon/command_mode.rs`).
pub use crate::config::write::write_config;

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

/// Print the unknown-key warning for a config just loaded from disk, if there
/// is one. Called at load time by both interactive flows, so the user sees the
/// typo before deciding what to do about it.
pub(crate) fn print_unknown_keys_warning(unknown: &[String]) {
    if let Some(w) = unknown_keys_warning(&crate::config_path(), unknown) {
        println!("  {YELLOW}warning:{RESET} {w}");
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
        agents: Vec::new(),
        agent_overlay: None,
        replacements: Default::default(),
        recording: None,
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
/// `xtest` is the opt-in X11 alternative to uinput.
pub(crate) fn select_injector_backend(existing: Option<&Config>) -> Result<InjectorBackend> {
    const BACKENDS: &[InjectorBackend] = &[
        InjectorBackend::Auto,
        InjectorBackend::Uinput,
        InjectorBackend::WaylandVk,
        InjectorBackend::Xtest,
    ];
    let default_idx = existing
        .map(|cfg| match cfg.input.backend {
            InjectorBackend::Auto => 0,
            InjectorBackend::Uinput => 1,
            InjectorBackend::WaylandVk => 2,
            InjectorBackend::Xtest => 3,
        })
        .unwrap_or(0);

    println!();
    let selection = Select::new()
        .with_prompt("Select a keyboard-injection backend")
        .items(&[
            "Auto        (recommended — Wayland virtual keyboard, falls back to uinput)",
            "uinput      (evdev/uinput; layout-dependent on Wayland)",
            "wayland-vk  (force zwp_virtual_keyboard_v1 — fixes bilingual typing on Wayland)",
            "xtest       (force X11 XTEST — no dropped keys at any key delay)",
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
                let choices = [
                    "gpt-realtime-whisper (manual-commit, no prompt or vocabulary)",
                    "gpt-live-transcribe  (manual-commit, prompt, vocabulary and language hints)",
                    "gpt-4o-transcribe    (server-vad, prompt and vocabulary)",
                ];
                let current = existing
                    .and_then(|c| c.openai.as_ref())
                    .map(|o| o.model.as_str());
                let default = match current {
                    Some("gpt-live-transcribe") => 1,
                    Some("gpt-4o-transcribe") => 2,
                    _ => 0,
                };
                let selection = Select::new()
                    .with_prompt("Select OpenAI Realtime model")
                    .items(&choices)
                    .default(default)
                    .interact()
                    .context("failed to read realtime model selection")?;
                match selection {
                    1 => "gpt-live-transcribe",
                    2 => "gpt-4o-transcribe",
                    _ => "gpt-realtime-whisper",
                }
                .to_string()
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
                openai: Some(OpenAiConfig {
                    api_key,
                    languages: if backend == "openai-realtime"
                        && openai_model_supports_languages(&model)
                    {
                        existing
                            .and_then(|c| c.openai.as_ref())
                            .map(|o| o.languages.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    model,
                }),
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
                        let rule = "KERNEL==\"uinput\", SUBSYSTEM==\"misc\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\nKERNEL==\"uinput\", SUBSYSTEM==\"misc\", TEST==\"/usr/bin/setfacl\", RUN+=\"/usr/bin/setfacl -m g:input:rw /dev/$name\"";
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
    /// one line that matters in its source. Issue #116 was exactly this call
    /// being absent.
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
