//! Interactive onboarding flow for `whisrs setup`.
//!
//! Guides the user through selecting a backend, entering an API key,
//! choosing a language, testing the microphone, writing `config.toml`,
//! setting up uinput permissions, installing the systemd service,
//! and configuring keybindings.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Password, Select};

use crate::llm::LlmConfig;
use crate::{
    AudioConfig, Config, DeepgramConfig, GeneralConfig, GroqConfig, LocalWhisperConfig,
    OpenAiConfig,
};

// ANSI color codes.
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Backend choices presented to the user.
const BACKEND_CHOICES: &[&str] = &[
    "Groq               (free, fast, cloud)",
    "Deepgram Streaming (free credits, true streaming, cloud)",
    "Deepgram REST      (free credits, simple, cloud)",
    "OpenAI Realtime    (best streaming, cloud)",
    "OpenAI REST        (simple, cloud)",
    "Local              (offline, no API key needed)",
];

/// Map selection index to backend string used in config.
const BACKEND_VALUES: &[&str] = &[
    "groq",
    "deepgram-streaming",
    "deepgram",
    "openai-realtime",
    "openai",
    "local",
];

/// Whisper model choices (name, file size, description).
const WHISPER_MODEL_CHOICES: &[&str] = &[
    "tiny.en    (75 MB,  decent accuracy, very fast)",
    "base.en    (142 MB, good accuracy, real-time)  <- recommended",
    "small.en   (466 MB, very good accuracy, slower)",
];
const WHISPER_MODEL_NAMES: &[&str] = &["tiny.en", "base.en", "small.en"];

/// Try to load an existing config from disk.
fn load_existing_config() -> Option<Config> {
    let path = crate::config_path();
    if !path.exists() {
        return None;
    }
    let contents = fs::read_to_string(&path).ok()?;
    toml::from_str(&contents).ok()
}

/// Mask an API key for display, showing only the last 4 characters.
fn mask_api_key(key: &str) -> String {
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
    let (deepgram_config, groq_config, openai_config, local_whisper_config) =
        configure_backend(&backend, None)?;

    // 3. Language.
    let language = select_language(None)?;

    // 4. Test microphone.
    test_microphone();

    // 5. Extra options.
    let (remove_filler_words, audio_feedback) = configure_extras()?;

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
            tray: true,
        },
        audio: AudioConfig {
            device: "default".to_string(),
        },
        deepgram: deepgram_config,
        groq: groq_config,
        openai: openai_config,
        local_whisper: local_whisper_config,
        local_vosk: None,
        local_parakeet: None,
        llm: llm_config,
        hotkeys: None,
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
fn select_backend(existing: Option<&Config>) -> Result<String> {
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
                _ if b.starts_with("local") => 5,
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
#[allow(clippy::type_complexity)]
fn configure_backend(
    backend: &str,
    existing: Option<&Config>,
) -> Result<(
    Option<DeepgramConfig>,
    Option<GroqConfig>,
    Option<OpenAiConfig>,
    Option<LocalWhisperConfig>,
)> {
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
            Ok((Some(DeepgramConfig { api_key, model }), None, None, None))
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
            Ok((None, Some(GroqConfig { api_key, model }), None, None))
        }
        "openai-realtime" | "openai" => {
            let existing_key = existing.and_then(|c| c.openai.as_ref()).map(|o| &o.api_key);
            let api_key = prompt_api_key_with_existing(
                "OpenAI API key",
                "Get one at https://platform.openai.com/api-keys",
                existing_key,
            )?;
            let model = if backend == "openai-realtime" {
                "gpt-4o-mini-transcribe".to_string()
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
            Ok((None, None, Some(OpenAiConfig { api_key, model }), None))
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
            Ok((None, None, None, Some(LocalWhisperConfig { model_path })))
        }
        _ => Ok((None, None, None, None)),
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
fn prompt_api_key_with_existing(
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

/// Common languages with their ISO 639-1 codes.
const LANGUAGE_CHOICES: &[(&str, &str)] = &[
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
fn select_language(existing: Option<&Config>) -> Result<String> {
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
fn write_config(config: &Config) -> Result<PathBuf> {
    let config_path = crate::config_path();
    let config_dir = config_path
        .parent()
        .expect("config path should have a parent directory");

    // Create the config directory if it doesn't exist.
    fs::create_dir_all(config_dir)
        .with_context(|| format!("failed to create config directory {}", config_dir.display()))?;

    // Serialize config to TOML.
    let toml_str = toml::to_string_pretty(config).context("failed to serialize config to TOML")?;

    // Write the file.
    fs::write(&config_path, &toml_str)
        .with_context(|| format!("failed to write config to {}", config_path.display()))?;

    // Set permissions to 0600 (owner read/write only) since it may contain API keys.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&config_path, perms)
            .with_context(|| format!("failed to set permissions on {}", config_path.display()))?;
    }

    Ok(config_path)
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
                     PassEnvironment=HYPRLAND_INSTANCE_SIGNATURE SWAYSOCK WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR\n\
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

/// LLM provider choices for command mode.
const LLM_PROVIDER_CHOICES: &[&str] = &[
    "OpenAI         (recommended)",
    "Groq           (fast, free tier)",
    "OpenRouter     (many models, free options)",
    "Google Gemini  (generous free tier)",
    "Skip           (configure later in config.toml)",
];

/// LLM provider API URLs.
const LLM_PROVIDER_URLS: &[&str] = &[
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
fn configure_llm() -> Result<Option<LlmConfig>> {
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
fn select_llm_model(provider_idx: usize) -> Result<String> {
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
