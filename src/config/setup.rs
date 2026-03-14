//! Interactive onboarding flow for `whisrs setup`.
//!
//! Guides the user through selecting a backend, entering an API key,
//! choosing a language, testing the microphone, and writing `config.toml`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use dialoguer::{Input, Password, Select};

use crate::{AudioConfig, Config, GeneralConfig, GroqConfig, LocalConfig, OpenAiConfig};

// ANSI color codes.
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Backend choices presented to the user.
const BACKEND_CHOICES: &[&str] = &[
    "Groq          (free, fast, cloud)",
    "OpenAI Realtime (best streaming, cloud)",
    "OpenAI REST     (simple, cloud)",
    "Local           (offline, whisper.cpp)",
];

/// Map selection index to backend string used in config.
const BACKEND_VALUES: &[&str] = &["groq", "openai-realtime", "openai", "local"];

/// Run the full interactive setup flow.
///
/// This function does NOT require the daemon to be running.
pub fn run_setup() -> Result<()> {
    println!("\n{BOLD}whisrs setup{RESET} — interactive onboarding\n");

    // 1. Select backend.
    let backend = select_backend()?;

    // 2. API key (for cloud backends).
    let (groq_config, openai_config, local_config) = configure_backend(&backend)?;

    // 3. Language.
    let language = select_language()?;

    // 4. Test microphone.
    test_microphone();

    // 5. Build and write config.
    let config = Config {
        general: GeneralConfig {
            backend,
            language,
            silence_timeout_ms: 2000,
            notify: true,
        },
        audio: AudioConfig {
            device: "default".to_string(),
        },
        groq: groq_config,
        openai: openai_config,
        local: local_config,
    };

    let config_path = write_config(&config)?;
    println!(
        "\n{GREEN}Config written to {}{RESET}",
        config_path.display()
    );

    // 6. Check uinput permissions.
    check_uinput_permissions();

    // 7. Print next steps.
    print_next_steps();

    Ok(())
}

/// Prompt the user to select a transcription backend.
fn select_backend() -> Result<String> {
    let selection = Select::new()
        .with_prompt("Select a transcription backend")
        .items(BACKEND_CHOICES)
        .default(0)
        .interact()
        .context("failed to read backend selection")?;

    let backend = BACKEND_VALUES[selection].to_string();
    println!("  {DIM}Selected: {backend}{RESET}");
    Ok(backend)
}

/// Configure the selected backend (API key or model path).
fn configure_backend(
    backend: &str,
) -> Result<(
    Option<GroqConfig>,
    Option<OpenAiConfig>,
    Option<LocalConfig>,
)> {
    match backend {
        "groq" => {
            let api_key = prompt_api_key(
                "Groq API key",
                "Get one free at https://console.groq.com/keys",
            )?;
            Ok((
                Some(GroqConfig {
                    api_key,
                    model: "whisper-large-v3-turbo".to_string(),
                }),
                None,
                None,
            ))
        }
        "openai-realtime" | "openai" => {
            let api_key = prompt_api_key(
                "OpenAI API key",
                "Get one at https://platform.openai.com/api-keys",
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
            Ok((None, Some(OpenAiConfig { api_key, model }), None))
        }
        "local" => {
            let default_model_path = dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("whisrs/models/ggml-base.en.bin");

            let model_path: String = Input::new()
                .with_prompt("Model file path")
                .default(default_model_path.to_string_lossy().to_string())
                .interact_text()
                .context("failed to read model path")?;

            if !std::path::Path::new(&model_path).exists() {
                println!("  {YELLOW}Warning: model file not found at {model_path}{RESET}");
                println!(
                    "  {DIM}Download a model from https://huggingface.co/ggerganov/whisper.cpp/tree/main{RESET}"
                );
                println!(
                    "  {DIM}Recommended: ggml-base.en.bin (142 MB) for good accuracy/speed{RESET}"
                );
            }

            Ok((None, None, Some(LocalConfig { model_path })))
        }
        _ => Ok((None, None, None)),
    }
}

/// Prompt for an API key using hidden input.
fn prompt_api_key(prompt: &str, hint: &str) -> Result<String> {
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

/// Ask the user for their preferred language.
fn select_language() -> Result<String> {
    let language: String = Input::new()
        .with_prompt("Language (ISO 639-1 code, or \"auto\" for auto-detect)")
        .default("en".to_string())
        .interact_text()
        .context("failed to read language")?;
    Ok(language)
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

/// Check if /dev/uinput is accessible and print guidance if not.
fn check_uinput_permissions() {
    use std::fs::OpenOptions;

    println!("\n{BOLD}Checking uinput permissions...{RESET}");

    match OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => {
            println!("  {GREEN}uinput access: OK{RESET}");
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                println!("  {RED}Cannot open /dev/uinput — permission denied.{RESET}");
                println!();
                println!("  Fix with one of:");
                println!();
                println!("  1. Add yourself to the input group:");
                println!("     sudo usermod -aG input $USER");
                println!("     # Then log out and log back in");
                println!();
                println!("  2. Install the udev rule (included in contrib/):");
                println!("     sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/");
                println!("     sudo udevadm control --reload-rules");
                println!("     sudo udevadm trigger");
            } else {
                println!("  {YELLOW}Cannot open /dev/uinput: {e}{RESET}");
            }
        }
    }
}

/// Print the final "you're ready" message with next steps.
fn print_next_steps() {
    println!("\n{GREEN}{BOLD}You're ready!{RESET}");
    println!();
    println!("  Start the daemon:");
    println!("    whisrsd &");
    println!("  Or enable the systemd service:");
    println!("    systemctl --user enable --now whisrs.service");
    println!();
    println!("  Then bind {BOLD}whisrs toggle{RESET} to a hotkey in your WM/DE.");
    println!();
    println!("  {DIM}Config: ~/.config/whisrs/config.toml{RESET}");
    println!("  {DIM}Logs:   RUST_LOG=debug whisrsd{RESET}");
    println!();
}
