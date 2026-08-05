//! Interactive onboarding flow for `whisrs setup`.
//!
//! Guides the user through selecting a backend, entering an API key,
//! choosing a language, testing the microphone, writing `config.toml`,
//! setting up uinput permissions, installing the systemd service,
//! and configuring keybindings.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Password, Select};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::llm::LlmConfig;
use crate::{
    AsrSidecarConfig, AudioConfig, Config, DeepgramConfig, GeneralConfig, GroqConfig,
    InjectorBackend, InputConfig, LocalWhisperConfig, OpenAiCompatibleRealtimeConfig, OpenAiConfig,
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

/// Try to load an existing config from disk.
pub(crate) fn load_existing_config() -> Option<Config> {
    let path = crate::config_path();
    if !path.exists() {
        return None;
    }
    let contents = fs::read_to_string(&path).ok()?;
    toml::from_str(&contents).ok()
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
    if let Some(existing_cfg) = load_existing_config() {
        println!(
            "  {GREEN}Found existing config{RESET} (backend: {BOLD}{}{RESET})",
            existing_cfg.general.backend
        );
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

    // 5b. Bottom recording overlay.
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

    // 8. Offer to install and enable the systemd service.
    setup_systemd_service();

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
            )?;
            let model = existing
                .and_then(|c| c.deepgram.as_ref())
                .map(|d| d.model.clone())
                .unwrap_or_else(|| "nova-3".to_string());
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

            Ok(BackendConfigSelection {
                asr_sidecar: Some(AsrSidecarConfig { url, model }),
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
        println!("  {YELLOW}Warning: empty API key — you can set it later in config.toml{RESET}");
    }
    Ok(key)
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
    use cpal::traits::{DeviceTrait, HostTrait};

    println!("\n{BOLD}Testing microphone...{RESET}");

    let host = cpal::default_host();
    match host.default_input_device() {
        Some(device) => {
            let name = device.name().unwrap_or_else(|_| "unknown".into());
            println!("  {GREEN}Microphone OK:{RESET} {name}");

            // Try to get a supported config to verify the device actually works.
            match device.default_input_config() {
                Ok(config) => {
                    println!(
                        "  {DIM}Format: {} Hz, {} channel(s){RESET}",
                        config.sample_rate().0,
                        config.channels()
                    );
                }
                Err(e) => {
                    println!("  {YELLOW}Warning: could not query device config: {e}{RESET}");
                }
            }
        }
        None => {
            println!("  {RED}No default audio input device found.{RESET}");

            // List available devices.
            if let Ok(devices) = host.input_devices() {
                let names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
                if names.is_empty() {
                    println!(
                        "  No input devices detected. Check that your microphone is connected"
                    );
                    println!("  and that PipeWire/PulseAudio is running.");
                } else {
                    println!("  Available input devices:");
                    for name in &names {
                        println!("    - {name}");
                    }
                    println!(
                        "  {DIM}Set the device in config.toml under [audio] device = \"...\"{RESET}"
                    );
                }
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
fn write_config_to(config: &Config, config_path: &Path) -> Result<()> {
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
    let output = match fs::read_to_string(config_path) {
        Ok(existing_str) => match existing_str.parse::<DocumentMut>() {
            Ok(mut existing) => {
                let fresh: DocumentMut = fresh_str
                    .parse()
                    .context("failed to reparse serialized config")?;
                merge_table(existing.as_table_mut(), fresh.as_table());
                existing.to_string()
            }
            Err(e) => {
                // Unparseable on-disk file: there is no layout to preserve, so
                // fall back to regenerating it from the struct (pre-#82
                // behavior). The broken file is the only copy of the user's
                // hand-edits, so save it as a private backup first (it may
                // hold API keys, hence 0600) and refuse to proceed if that
                // backup cannot be written.
                let file_name = config_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("config.toml");
                let backup_path = config_path.with_file_name(format!("{file_name}.bak"));
                write_private_file(&backup_path, &existing_str).with_context(|| {
                    format!(
                        "existing config {} is not valid TOML ({e}), and backing it up to {} \
                         failed; refusing to overwrite the only copy",
                        config_path.display(),
                        backup_path.display()
                    )
                })?;
                // `tracing` alone is invisible in the CLI flows (`whisrs
                // setup` / `whisrs config` install no subscriber that prints
                // warnings), so tell the user on stderr as well.
                tracing::warn!(
                    "existing config at {} is not valid TOML ({e}); rewriting it from scratch \
                     (backup saved to {})",
                    config_path.display(),
                    backup_path.display()
                );
                eprintln!(
                    "warning: existing config at {} is not valid TOML ({e}); rewriting it from \
                     scratch (backup saved to {})",
                    config_path.display(),
                    backup_path.display()
                );
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

    atomic_write(config_path, &output)
        .with_context(|| format!("failed to write config to {}", config_path.display()))
}

/// Sync `existing` (the user's on-disk TOML, decor intact) to hold exactly the
/// keys and values of `fresh` (the reserialized struct). Matching keys keep
/// their comments and formatting, recursing into sub-tables; keys missing from
/// `fresh` are removed; new keys are appended.
fn merge_table(existing: &mut Table, fresh: &Table) {
    // Drop keys the struct no longer carries. This intentionally also drops
    // keys `Config` never knew about (it has no catch-all field), matching the
    // previous full-rewrite behavior: the file always mirrors the struct.
    let stale: Vec<String> = existing
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !fresh.contains_key(key))
        .collect();
    for key in stale {
        existing.remove(&key);
    }

    for (key, fresh_item) in fresh.iter() {
        match existing.get_mut(key) {
            Some(existing_item) => merge_item(existing_item, fresh_item),
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

/// Merge one item of a table, dispatching on its structure.
fn merge_item(existing: &mut Item, fresh: &Item) {
    match (existing, fresh) {
        (Item::Table(existing), Item::Table(fresh)) => merge_table(existing, fresh),
        (Item::ArrayOfTables(existing), Item::ArrayOfTables(fresh)) => {
            merge_array_of_tables(existing, fresh)
        }
        // The user wrote a section as an inline table (`colors = { ... }`);
        // the serializer always produces a header table. Keep their spelling.
        (Item::Value(Value::InlineTable(existing)), Item::Table(fresh)) if is_flat(fresh) => {
            merge_inline_table(existing, fresh)
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
/// are appended; on-disk entries with no match are dropped.
fn merge_array_of_tables(existing: &mut ArrayOfTables, fresh: &ArrayOfTables) {
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
                merge_table(&mut table, fresh_table);
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

/// True when every entry of `table` is a plain value (no sub-tables).
fn is_flat(table: &Table) -> bool {
    table.iter().all(|(_, item)| item.is_value())
}

/// Sync a user-written inline table against the flat header table the
/// serializer produced for the same section.
fn merge_inline_table(existing: &mut toml_edit::InlineTable, fresh: &Table) {
    let stale: Vec<String> = existing
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !fresh.contains_key(key))
        .collect();
    for key in stale {
        existing.remove(&key);
    }
    for (key, fresh_item) in fresh.iter() {
        // `is_flat` guarantees every fresh item is a value.
        let Some(fresh_value) = fresh_item.as_value() else {
            continue;
        };
        match existing.get_mut(key) {
            Some(existing_value) => merge_value(existing_value, fresh_value),
            None => {
                existing.insert(key, fresh_value.clone());
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
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
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

/// Offer to install and enable the systemd user service.
fn setup_systemd_service() {
    println!("\n{BOLD}Systemd service...{RESET}");

    let user_service_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd/user");
    let dest = user_service_dir.join("whisrs.service");

    // Check if service is already installed.
    if dest.exists() {
        println!(
            "  {GREEN}Service already installed at {}{RESET}",
            dest.display()
        );

        // Check if it's already enabled.
        let enabled = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", "whisrs.service"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
            .unwrap_or(false);

        if enabled {
            println!("  {GREEN}Service is already enabled{RESET}");
            return;
        }
    }

    let choice = Select::new()
        .with_prompt("Enable whisrs daemon to start automatically?")
        .items(&[
            "Yes — install and enable systemd service",
            "No — I'll start it manually",
        ])
        .default(0)
        .interact();

    match choice {
        Ok(0) => {
            // Create the systemd user directory if needed.
            if let Err(e) = fs::create_dir_all(&user_service_dir) {
                println!(
                    "  {RED}Failed to create {}: {e}{RESET}",
                    user_service_dir.display()
                );
                return;
            }

            // Find the service file source.
            let service_src = find_contrib_file("whisrs.service");

            if let Some(src) = service_src {
                // Copy the service file.
                if let Err(e) = fs::copy(&src, &dest) {
                    println!("  {RED}Failed to copy service file: {e}{RESET}");
                    return;
                }
            } else {
                // Write the service file inline.
                let whisrsd_path = which_whisrsd();
                let service_content = format!(
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
                );
                if let Err(e) = fs::write(&dest, &service_content) {
                    println!("  {RED}Failed to write service file: {e}{RESET}");
                    return;
                }
            }

            println!("  {GREEN}Installed service to {}{RESET}", dest.display());

            // Reload and enable.
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let status = std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", "whisrs.service"])
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!("  {GREEN}Service enabled and started{RESET}");
                }
                _ => {
                    println!("  {YELLOW}Failed to enable service — you can do it manually:{RESET}");
                    println!("    systemctl --user enable --now whisrs.service");
                }
            }
        }
        _ => {
            println!("  {DIM}You can start the daemon manually: whisrsd &{RESET}");
            println!("  {DIM}Or enable the service later:{RESET}");
            println!("    cp contrib/whisrs.service ~/.config/systemd/user/");
            println!("    systemctl --user enable --now whisrs.service");
        }
    }
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

/// Ask the user whether to enable the bottom recording overlay, and on GNOME
/// offer to install the bundled Shell extension that renders it.
fn configure_overlay() -> (bool, Option<crate::OverlayConfig>) {
    println!("\n{BOLD}Recording overlay (optional)...{RESET}");
    println!("  {DIM}A small audio meter at the bottom of the screen while recording.{RESET}");

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
    println!("\n{GREEN}{BOLD}You're all set!{RESET}");
    println!();
    println!("  {DIM}Config:    ~/.config/whisrs/config.toml{RESET}");
    println!("  {DIM}Logs:      journalctl --user -u whisrs -f{RESET}");
    println!("  {DIM}Re-run:    whisrs setup (to change backend or settings){RESET}");
    println!();
    println!("  You can adjust all settings (filler words, audio feedback, silence");
    println!(
        "  timeout, etc.) by editing the config file or re-running {BOLD}whisrs setup{RESET}."
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
