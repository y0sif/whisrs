//! Interactive editor for `~/.config/whisrs/config.toml`.
//!
//! `whisrs config` opens a menu that lets the user jump to any section of the
//! config file, edit it, and on save writes a validated `config.toml` and
//! restarts the daemon if the systemd user unit is loaded.
//!
//! This complements `whisrs setup` (the one-time onboarding wizard). `setup`
//! handles install-time concerns — mic test, udev rules, systemd install,
//! compositor keybinding — while `config` only edits the TOML.

use std::fs;
use std::process::Command as StdCommand;

use anyhow::{Context, Result};
use dialoguer::{Confirm, Editor, Input, Select};

use crate::config::setup;
use crate::{Config, HotkeyConfig, RestartOutcome};

use setup::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

/// Top-level entry point for `whisrs config`.
///
/// Loads the existing config (or a fresh default if none exists), runs the
/// menu loop, and on save writes the file and triggers a daemon restart.
pub fn run_config_menu() -> Result<()> {
    println!("\n{BOLD}whisrs config{RESET} — edit ~/.config/whisrs/config.toml\n");

    let (mut config, fresh) = match setup::load_existing_config() {
        Some(cfg) => (cfg, false),
        None => {
            println!(
                "  {YELLOW}No config file found — starting from defaults.{RESET} \
                 Run {BOLD}whisrs setup{RESET} for the full onboarding flow."
            );
            (default_config(), true)
        }
    };

    loop {
        print_summary(&config);

        // TODO(tts): add a "Text-to-speech (read aloud)" menu entry here
        // (enable / backend / model / voice / url) once the read-aloud feature
        // stabilizes. Skipped for now to keep this change focused; the [tts]
        // section can still be edited via "Open in $EDITOR".
        let choices = &[
            "Backend & API keys",
            "Language",
            "Behavior (silence timeout, notifications, audio feedback)",
            "Filler words",
            "Vocabulary & prompt",
            "Audio device",
            "Keyboard injection (key delay)",
            "Hotkeys",
            "Tray & overlay",
            "Command mode (LLM)",
            "Custom LLM commands",
            "Show full config (masked)",
            "Open in $EDITOR",
            "─────────",
            "Save & exit",
            "Discard & exit",
        ];

        let selection = Select::new()
            .with_prompt("What do you want to change?")
            .items(choices)
            .default(0)
            .interact()
            .context("failed to read menu selection")?;

        match selection {
            0 => edit_backend(&mut config)?,
            1 => edit_language(&mut config)?,
            2 => edit_behavior(&mut config)?,
            3 => edit_filler_words(&mut config)?,
            4 => edit_vocabulary_and_prompt(&mut config)?,
            5 => edit_audio_device(&mut config)?,
            6 => edit_key_delay(&mut config)?,
            7 => edit_hotkeys(&mut config)?,
            8 => edit_tray_overlay(&mut config)?,
            9 => edit_llm(&mut config)?,
            10 => edit_llm_commands(&mut config)?,
            11 => show_config(&config),
            12 => {
                if open_in_editor(&mut config)? {
                    // External edit already wrote the file; reload and skip the
                    // normal save path so we don't clobber formatting/comments
                    // the user might have added.
                    println!("  {GREEN}Applied edits from $EDITOR.{RESET}");
                }
            }
            13 => {
                // separator — no-op
            }
            14 => {
                if save_and_restart(&config, fresh)? {
                    return Ok(());
                }
                // Validation failed — fall through to next loop iteration,
                // preserving the in-memory `config` so the user can fix it.
            }
            15 => {
                println!("\n  {DIM}Discarded changes.{RESET}");
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
}

/// Build a fresh Config from defaults — used when no `config.toml` exists yet.
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
        overlay: None,
        llm_commands: Vec::new(),
    }
}

/// Print the current state header above the menu so the user can see at a
/// glance what backend/language/daemon-status they're working with.
fn print_summary(config: &Config) {
    println!("\n  {BOLD}Current settings:{RESET}");
    println!(
        "    Backend:  {BOLD}{}{RESET}    Language: {BOLD}{}{RESET}",
        config.general.backend, config.general.language
    );
    let key_status = current_key_summary(config);
    println!("    API key:  {key_status}");
    println!("    Daemon:   {}", daemon_status_string());
    println!();
}

/// Summarize whether the active backend has an API key configured. Shows the
/// last 4 chars so the user can tell which key they're looking at without
/// leaking the full secret.
fn current_key_summary(config: &Config) -> String {
    let key = match config.general.backend.as_str() {
        "groq" => config.groq.as_ref().map(|g| g.api_key.as_str()),
        "deepgram" | "deepgram-streaming" => config.deepgram.as_ref().map(|d| d.api_key.as_str()),
        "openai" | "openai-realtime" => config.openai.as_ref().map(|o| o.api_key.as_str()),
        "local-whisper" | "local" | "local-vosk" | "local-parakeet" => {
            return format!("{DIM}(local backend — no API key needed){RESET}");
        }
        "openai-compatible-realtime" => {
            return match config
                .openai_compatible_realtime
                .as_ref()
                .and_then(|r| r.api_key.as_deref())
                .filter(|key| !key.is_empty())
            {
                Some(key) => format!("{BOLD}{}{RESET}", setup::mask_api_key(key)),
                None => format!("{DIM}(optional bearer token not set){RESET}"),
            };
        }
        "asr-sidecar" | "asr" | "vibevoice" => {
            return format!("{DIM}(sidecar backend — no API key needed){RESET}");
        }
        _ => None,
    };
    match key {
        Some(k) if !k.is_empty() => format!("{BOLD}{}{RESET}", setup::mask_api_key(k)),
        _ => format!("{YELLOW}not set{RESET}"),
    }
}

fn daemon_status_string() -> String {
    // `is-active` exits 0 when running. We don't surface "failed/inactive"
    // separately — the user only cares about active vs not when deciding
    // whether a restart is meaningful.
    let active = StdCommand::new("systemctl")
        .args(["--user", "is-active", "whisrs.service"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false);
    if active {
        format!("{GREEN}running{RESET}")
    } else {
        format!("{DIM}not running (or no systemd unit){RESET}")
    }
}

// ---------------------------------------------------------------------------
// Section editors
// ---------------------------------------------------------------------------

fn edit_backend(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Backend & API keys{RESET}");
    let new_backend = setup::select_backend(Some(config))?;

    let backend_config = setup::configure_backend(&new_backend, Some(config))?;

    // Only overwrite the section the user just edited. Other backend sections
    // are preserved so the user can switch back without re-entering a key.
    config.general.backend = new_backend;
    if backend_config.deepgram.is_some() {
        config.deepgram = backend_config.deepgram;
    }
    if backend_config.groq.is_some() {
        config.groq = backend_config.groq;
    }
    if backend_config.openai.is_some() {
        config.openai = backend_config.openai;
    }
    if backend_config.local_whisper.is_some() {
        config.local_whisper = backend_config.local_whisper;
    }
    if backend_config.asr_sidecar.is_some() {
        config.asr_sidecar = backend_config.asr_sidecar;
    }
    if backend_config.openai_compatible_realtime.is_some() {
        config.openai_compatible_realtime = backend_config.openai_compatible_realtime;
    }
    Ok(())
}

fn edit_language(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Language{RESET}");
    config.general.language = setup::select_language(Some(config))?;
    Ok(())
}

fn edit_behavior(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Behavior{RESET}");

    let timeout: String = Input::new()
        .with_prompt("Silence timeout (ms) — 0 disables auto-stop")
        .default(config.general.silence_timeout_ms.to_string())
        .interact_text()
        .context("failed to read silence timeout")?;
    if let Ok(t) = timeout.parse::<u64>() {
        config.general.silence_timeout_ms = t;
    } else {
        println!("  {YELLOW}Not a number — left unchanged.{RESET}");
    }

    config.general.notify = Confirm::new()
        .with_prompt("Enable desktop notifications?")
        .default(config.general.notify)
        .interact()
        .unwrap_or(config.general.notify);

    config.general.audio_feedback = Confirm::new()
        .with_prompt("Enable audio feedback (tones on start/stop)?")
        .default(config.general.audio_feedback)
        .interact()
        .unwrap_or(config.general.audio_feedback);

    if config.general.audio_feedback {
        let vol: String = Input::new()
            .with_prompt("Audio feedback volume (0.0 to 1.0)")
            .default(format!("{:.2}", config.general.audio_feedback_volume))
            .interact_text()
            .context("failed to read volume")?;
        if let Ok(v) = vol.parse::<f32>() {
            config.general.audio_feedback_volume = v.clamp(0.0, 1.0);
        } else {
            println!("  {YELLOW}Not a number — left unchanged.{RESET}");
        }
    }

    Ok(())
}

fn edit_filler_words(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Filler words{RESET}");

    config.general.remove_filler_words = Confirm::new()
        .with_prompt("Remove filler words (\"um\", \"uh\", ...) from transcriptions?")
        .default(config.general.remove_filler_words)
        .interact()
        .unwrap_or(config.general.remove_filler_words);

    if !config.general.remove_filler_words {
        return Ok(());
    }

    let current = if config.general.filler_words.is_empty() {
        "(built-in list)".to_string()
    } else {
        config.general.filler_words.join(", ")
    };
    println!("  {DIM}Current custom list: {current}{RESET}");

    let edit_list = Confirm::new()
        .with_prompt("Edit custom filler list? (empty = use built-in defaults)")
        .default(false)
        .interact()
        .unwrap_or(false);
    if !edit_list {
        return Ok(());
    }

    let input: String = Input::new()
        .with_prompt("Comma-separated filler words (leave blank to clear)")
        .default(config.general.filler_words.join(", "))
        .allow_empty(true)
        .interact_text()
        .context("failed to read filler word list")?;

    config.general.filler_words = parse_csv_list(&input);
    Ok(())
}

fn edit_vocabulary_and_prompt(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Vocabulary & prompt{RESET}");
    println!("  {DIM}Domain terms/names sent as a hint to the backend to improve accuracy.{RESET}");

    let current = if config.general.vocabulary.is_empty() {
        "(empty)".to_string()
    } else {
        config.general.vocabulary.join(", ")
    };
    println!("  Current vocabulary: {current}");

    let input: String = Input::new()
        .with_prompt("Comma-separated vocabulary (leave blank to clear)")
        .default(config.general.vocabulary.join(", "))
        .allow_empty(true)
        .interact_text()
        .context("failed to read vocabulary")?;
    config.general.vocabulary = parse_csv_list(&input);

    let current_prompt = config.general.prompt.as_deref().unwrap_or("(none)");
    println!("  Current prompt: {current_prompt}");
    let prompt: String = Input::new()
        .with_prompt("Free-form prompt (style/register hints; leave blank to clear)")
        .default(config.general.prompt.clone().unwrap_or_default())
        .allow_empty(true)
        .interact_text()
        .context("failed to read prompt")?;
    config.general.prompt = if prompt.trim().is_empty() {
        None
    } else {
        Some(prompt)
    };

    Ok(())
}

fn edit_audio_device(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Audio device{RESET}");

    let devices = list_input_devices();
    if devices.is_empty() {
        println!("  {YELLOW}No input devices detected.{RESET}");
    } else {
        println!("  {DIM}Detected input devices:{RESET}");
        for d in &devices {
            println!("    - {d}");
        }
    }

    let new_device: String = Input::new()
        .with_prompt("Audio device name (\"default\" to use system default)")
        .default(config.audio.device.clone())
        .interact_text()
        .context("failed to read audio device")?;
    config.audio.device = new_device;
    Ok(())
}

fn list_input_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host()
        .input_devices()
        .map(|iter| iter.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn edit_key_delay(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Keyboard injection{RESET}");
    println!(
        "  {DIM}Delay between simulated keystrokes. Raise this if characters are dropped \
         by TUI apps that read stdin in raw mode (e.g. Claude Code).{RESET}"
    );

    let input: String = Input::new()
        .with_prompt("key_delay_ms")
        .default(config.input.key_delay_ms.to_string())
        .interact_text()
        .context("failed to read key delay")?;
    if let Ok(v) = input.parse::<u64>() {
        config.input.key_delay_ms = v;
    } else {
        println!("  {YELLOW}Not a number — left unchanged.{RESET}");
    }
    Ok(())
}

fn edit_hotkeys(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Hotkeys{RESET}");
    println!(
        "  {DIM}Key combo strings (e.g. \"Super+Shift+D\"). Whether these bind globally\n   \
         depends on your compositor — most users let the compositor invoke\n   \
         `whisrs toggle` instead.{RESET}"
    );

    // Prompt for every field, in struct order. A field left out of this list
    // is silently destroyed: the editor rewrites the whole `[hotkeys]` table
    // from this struct, and `any_hotkey_set` below drops the table entirely
    // when the prompted fields all come back blank — taking the unprompted
    // ones with it. That was live data loss for `speak`: editing hotkeys and
    // clearing toggle/cancel/command deleted a configured read-aloud binding
    // the editor never showed.
    let mut hotkeys = config.hotkeys.clone().unwrap_or_default();
    hotkeys.toggle = prompt_optional_string("Toggle hotkey", &hotkeys.toggle)?;
    hotkeys.cancel = prompt_optional_string("Cancel hotkey", &hotkeys.cancel)?;
    hotkeys.command = prompt_optional_string("Command-mode hotkey", &hotkeys.command)?;
    hotkeys.speak = prompt_optional_string("Read-aloud hotkey", &hotkeys.speak)?;

    // Drop the whole section if every field is empty — keeps the TOML clean.
    config.hotkeys = if any_hotkey_set(&hotkeys) {
        Some(hotkeys)
    } else {
        None
    };
    Ok(())
}

/// Whether any hotkey in the section is bound.
///
/// Must consider every field of [`HotkeyConfig`]: a field missed here reads as
/// "the section is empty" and deletes the user's other bindings along with it.
fn any_hotkey_set(hotkeys: &HotkeyConfig) -> bool {
    let HotkeyConfig {
        toggle,
        cancel,
        command,
        speak,
    } = hotkeys;
    toggle.is_some() || cancel.is_some() || command.is_some() || speak.is_some()
}

fn prompt_optional_string(label: &str, current: &Option<String>) -> Result<Option<String>> {
    let default = current.clone().unwrap_or_default();
    let input: String = Input::new()
        // Enter keeps the shown default, so an existing value cannot be cleared
        // by submitting an empty line. Type a space to unset one.
        .with_prompt(format!("{label} (space to unset)"))
        .default(default)
        .allow_empty(true)
        .interact_text()
        .with_context(|| format!("failed to read {label}"))?;
    Ok(if input.trim().is_empty() {
        None
    } else {
        Some(input)
    })
}

fn edit_tray_overlay(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Tray & overlay{RESET}");

    config.general.tray = Confirm::new()
        .with_prompt("Show system tray icon?")
        .default(config.general.tray)
        .interact()
        .unwrap_or(config.general.tray);

    config.general.overlay = Confirm::new()
        .with_prompt("Show bottom recording overlay?")
        .default(config.general.overlay)
        .interact()
        .unwrap_or(config.general.overlay);

    if config.general.overlay {
        let theme = setup::pick_overlay_theme();
        let mut overlay_cfg = config.overlay.clone().unwrap_or_default();
        overlay_cfg.theme = theme;
        config.overlay = Some(overlay_cfg);
        println!(
            "  {DIM}Note: width/height and custom colors can be set by hand in config.toml.{RESET}"
        );
    }
    Ok(())
}

fn edit_llm(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Command mode (LLM){RESET}");
    println!("  {DIM}Select text + hotkey + speak instruction → LLM rewrites it in place.{RESET}");

    let current_label = config
        .llm
        .as_ref()
        .map(|l| format!("{} ({})", l.model, setup::mask_api_key(&l.api_key)))
        .unwrap_or_else(|| "not configured".to_string());
    println!("  Current: {current_label}");

    let choice = Select::new()
        .with_prompt("LLM configuration")
        .items(&[
            "Configure / replace",
            "Disable (remove [llm] section)",
            "Keep current",
        ])
        .default(0)
        .interact()
        .context("failed to read LLM choice")?;

    match choice {
        0 => {
            // Reuse the same picker the setup flow uses — same model lists,
            // same provider URLs, same masking.
            let new_llm = setup::configure_llm()?;
            if let Some(llm) = new_llm {
                config.llm = Some(llm);
            }
        }
        1 => {
            config.llm = None;
            println!("  {GREEN}LLM removed.{RESET}");
        }
        _ => {}
    }
    Ok(())
}

fn edit_llm_commands(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Custom LLM commands{RESET}");
    println!(
        "  {DIM}Each entry gets its own hotkey: dictate, the LLM applies a fixed\n   \
         instruction to what you said, and the result is typed at the cursor —\n   \
         a toggle-recording flavor of plain dictation, not command mode (no\n   \
         selection needed). Uses the same [llm] configuration as command mode.{RESET}"
    );

    loop {
        if config.llm_commands.is_empty() {
            println!("  {DIM}(none configured){RESET}");
        } else {
            println!();
            for (i, entry) in config.llm_commands.iter().enumerate() {
                println!(
                    "    {}. {BOLD}{}{RESET}  [{}]  {}",
                    i + 1,
                    entry.name,
                    entry.hotkey,
                    truncate_for_menu(&entry.instruction)
                );
            }
        }

        let mut choices = vec!["Add new".to_string()];
        if !config.llm_commands.is_empty() {
            choices.push("Edit an entry".to_string());
            choices.push("Remove an entry".to_string());
        }
        choices.push("Done".to_string());
        let done_index = choices.len() - 1;

        let selection = Select::new()
            .with_prompt("Custom LLM commands")
            .items(&choices)
            .default(done_index)
            .interact()
            .context("failed to read menu selection")?;

        match choices[selection].as_str() {
            "Add new" => add_llm_command(config)?,
            "Edit an entry" => edit_one_llm_command(config)?,
            "Remove an entry" => remove_llm_command(config)?,
            _ => return Ok(()), // "Done"
        }
    }
}

fn add_llm_command(config: &mut Config) -> Result<()> {
    let name: String = Input::new()
        .with_prompt("Name (identifier, e.g. \"translate-de\")")
        .interact_text()
        .context("failed to read name")?;
    if config.llm_commands.iter().any(|e| e.name == name) {
        println!("  {YELLOW}An entry named '{name}' already exists — edit it instead.{RESET}");
        return Ok(());
    }
    let hotkey: String = Input::new()
        .with_prompt("Hotkey (e.g. \"Super+Shift+T\")")
        .interact_text()
        .context("failed to read hotkey")?;
    let set_hotkey_raw: String = Input::new()
        .with_prompt(
            "Set-hotkey — reprogram this command from selected text (optional, blank to skip)",
        )
        .allow_empty(true)
        .interact_text()
        .context("failed to read set_hotkey")?;
    let set_hotkey = Some(set_hotkey_raw.trim().to_string()).filter(|s| !s.is_empty());
    let instruction: String = Input::new()
        .with_prompt("Instruction applied to the dictated text")
        .interact_text()
        .context("failed to read instruction")?;

    config.llm_commands.push(crate::llm::LlmCommandConfig {
        name,
        hotkey,
        set_hotkey,
        instruction,
    });
    println!("  {GREEN}Added.{RESET}");
    Ok(())
}

fn edit_one_llm_command(config: &mut Config) -> Result<()> {
    let Some(idx) = select_llm_command(config, "Edit which entry?")? else {
        return Ok(());
    };

    let entry = config.llm_commands[idx].clone();
    let name: String = Input::new()
        .with_prompt("Name")
        .default(entry.name)
        .interact_text()
        .context("failed to read name")?;
    let hotkey: String = Input::new()
        .with_prompt("Hotkey")
        .default(entry.hotkey)
        .interact_text()
        .context("failed to read hotkey")?;
    let set_hotkey_raw: String = Input::new()
        .with_prompt("Set-hotkey (reprogram from selection; blank for none)")
        .default(entry.set_hotkey.clone().unwrap_or_default())
        .allow_empty(true)
        .interact_text()
        .context("failed to read set_hotkey")?;
    let set_hotkey = Some(set_hotkey_raw.trim().to_string()).filter(|s| !s.is_empty());
    let instruction: String = Input::new()
        .with_prompt("Instruction")
        .default(entry.instruction)
        .interact_text()
        .context("failed to read instruction")?;

    config.llm_commands[idx] = crate::llm::LlmCommandConfig {
        name,
        hotkey,
        set_hotkey,
        instruction,
    };
    println!("  {GREEN}Updated.{RESET}");
    Ok(())
}

fn remove_llm_command(config: &mut Config) -> Result<()> {
    let Some(idx) = select_llm_command(config, "Remove which entry?")? else {
        return Ok(());
    };
    let removed = config.llm_commands.remove(idx);
    println!("  {GREEN}Removed '{}'.{RESET}", removed.name);
    Ok(())
}

/// Show a picker over current entries plus a trailing "Cancel". Returns
/// `None` when the user cancels.
fn select_llm_command(config: &Config, prompt: &str) -> Result<Option<usize>> {
    let mut items: Vec<String> = config
        .llm_commands
        .iter()
        .map(|e| format!("{} ({})", e.name, e.hotkey))
        .collect();
    items.push("Cancel".to_string());
    let cancel_index = items.len() - 1;

    let selection = Select::new()
        .with_prompt(prompt)
        .items(&items)
        .default(0)
        .interact()
        .context("failed to read selection")?;

    Ok(if selection == cancel_index {
        None
    } else {
        Some(selection)
    })
}

/// Truncate an instruction string for the summary menu line.
fn truncate_for_menu(s: &str) -> String {
    const MAX: usize = 50;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

// ---------------------------------------------------------------------------
// Show / external editor / save
// ---------------------------------------------------------------------------

fn show_config(config: &Config) {
    println!("\n  {BOLD}Current config (masked){RESET}\n");
    match render_masked_toml(config) {
        Ok(s) => {
            for line in s.lines() {
                println!("    {line}");
            }
        }
        Err(e) => println!("  {RED}Failed to render config: {e}{RESET}"),
    }
}

/// Serialize a copy of the config with API keys replaced by `****<last4>`.
///
/// We clone before masking so the in-memory edit buffer keeps real keys.
fn render_masked_toml(config: &Config) -> Result<String> {
    let mut clone = config.clone();
    if let Some(d) = clone.deepgram.as_mut() {
        d.api_key = setup::mask_api_key(&d.api_key);
    }
    if let Some(g) = clone.groq.as_mut() {
        g.api_key = setup::mask_api_key(&g.api_key);
    }
    if let Some(o) = clone.openai.as_mut() {
        o.api_key = setup::mask_api_key(&o.api_key);
    }
    if let Some(r) = clone.openai_compatible_realtime.as_mut() {
        r.api_key = r.api_key.as_ref().map(|key| setup::mask_api_key(key));
    }
    if let Some(l) = clone.llm.as_mut() {
        l.api_key = setup::mask_api_key(&l.api_key);
    }
    toml::to_string_pretty(&clone).context("failed to serialize config")
}

/// Open the current config in $EDITOR. Returns `true` when the user saved
/// edits, in which case the in-memory config is replaced by what's on disk.
///
/// We write the current in-memory config to a temp string first so the editor
/// session sees the user's pending changes (not stale on-disk content).
fn open_in_editor(config: &mut Config) -> Result<bool> {
    let toml_str = toml::to_string_pretty(config).context("failed to serialize config")?;
    let edited = Editor::new()
        .extension(".toml")
        .edit(&toml_str)
        .context("failed to open editor")?;
    let Some(edited) = edited else {
        println!("  {DIM}Editor exited without saving.{RESET}");
        return Ok(false);
    };
    match toml::from_str::<Config>(&edited) {
        Ok(new_config) => {
            *config = new_config;
            Ok(true)
        }
        Err(e) => {
            println!("  {RED}Edited TOML is invalid: {e}{RESET}");
            println!("  {YELLOW}Changes from $EDITOR discarded.{RESET}");
            Ok(false)
        }
    }
}

/// Validate, write, and restart. Called only from the "Save & exit" branch.
///
/// Returns `Ok(true)` on a successful save (caller should exit the menu) and
/// `Ok(false)` when validation failed (caller should return to the menu while
/// preserving the in-memory edit buffer).
///
/// `fresh` is true when we created the config from defaults (no file on disk
/// at startup) — in that case we point the user at `whisrs setup` for the
/// permissions/systemd/keybinding bits we deliberately skipped.
fn save_and_restart(config: &Config, fresh: bool) -> Result<bool> {
    match config.validate() {
        Ok(warnings) => {
            for w in warnings {
                println!("  {YELLOW}warning:{RESET} {w}");
            }
        }
        Err(e) => {
            println!("\n  {RED}Cannot save — config is invalid:{RESET}");
            println!("    {e}");
            println!("  {DIM}Fix the issue and try again, or pick \"Discard & exit\".{RESET}");
            // Signal the caller to re-enter the menu without losing the
            // in-memory edits the user has already made this session.
            return Ok(false);
        }
    }

    let path = setup::write_config(config).context("failed to write config")?;
    println!("\n  {GREEN}Wrote {}{RESET}", path.display());

    // Permissions are set to 0600 by write_config(); double-check for the
    // case where a previous run created the file with a different umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }

    println!("\n  Restarting daemon to pick up new config...");
    match crate::restart_daemon_via_systemd() {
        RestartOutcome::Restarted => {
            println!("  {GREEN}Daemon restarted.{RESET}");
        }
        RestartOutcome::Failed => {
            println!("  {RED}systemctl --user restart whisrs.service failed.{RESET}");
            println!("  {DIM}Check `journalctl --user -u whisrs -e` for details.{RESET}");
        }
        RestartOutcome::NoSystemdUnit => {
            println!(
                "  {DIM}No whisrs.service user unit detected — restart the daemon manually \
                 for the new config to take effect:{RESET}"
            );
            println!("    pkill whisrsd; sleep 0.2; whisrsd &");
        }
    }

    if fresh {
        println!(
            "\n  {DIM}This was a fresh config. Run {BOLD}whisrs setup{RESET}{DIM} once to install\n   \
             udev rules, the systemd unit, and a compositor keybinding.{RESET}"
        );
    }
    Ok(true)
}

/// Parse a comma-separated list, trimming whitespace and dropping empty entries.
fn parse_csv_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field name of `[hotkeys]`, taken from serde rather than a hand-
    /// written list so a new field cannot be forgotten here too.
    fn hotkey_field_names() -> Vec<String> {
        let all_set = HotkeyConfig {
            toggle: Some("a".into()),
            cancel: Some("b".into()),
            command: Some("c".into()),
            speak: Some("d".into()),
        };
        let json = serde_json::to_value(&all_set).expect("HotkeyConfig serializes");
        json.as_object()
            .expect("HotkeyConfig is a struct")
            .keys()
            .cloned()
            .collect()
    }

    /// The editor rewrites the whole `[hotkeys]` table from the struct, so a
    /// field it never prompts for is blanked on save. Assert the prompt list
    /// covers every field — this is the half of the bug a value test cannot
    /// see, since the prompting itself is interactive IO.
    #[test]
    fn edit_hotkeys_prompts_for_every_field() {
        let source = include_str!("edit.rs");
        let body = source
            .split("fn edit_hotkeys(")
            .nth(1)
            .expect("edit.rs defines edit_hotkeys")
            .split("\nfn ")
            .next()
            .expect("edit_hotkeys has a body");

        for field in hotkey_field_names() {
            assert!(
                body.contains(&format!("hotkeys.{field} = prompt_optional_string")),
                "edit_hotkeys never prompts for `{field}`, so editing hotkeys deletes it"
            );
        }
    }

    /// A section with nothing bound is dropped, keeping the TOML clean.
    #[test]
    fn an_empty_hotkey_section_is_dropped() {
        assert!(!any_hotkey_set(&HotkeyConfig::default()));
    }

    /// Any single binding keeps the section. Before this was widened, a set
    /// `speak` with the other three blank read as empty and the binding was
    /// deleted on save.
    #[test]
    fn any_single_binding_keeps_the_section() {
        for field in hotkey_field_names() {
            let mut hotkeys = HotkeyConfig::default();
            match field.as_str() {
                "toggle" => hotkeys.toggle = Some("Super+Shift+D".into()),
                "cancel" => hotkeys.cancel = Some("Super+Shift+Escape".into()),
                "command" => hotkeys.command = Some("Super+Shift+C".into()),
                "speak" => hotkeys.speak = Some("Super+Shift+R".into()),
                other => panic!("unhandled hotkey field `{other}` — add it to this test"),
            }
            assert!(
                any_hotkey_set(&hotkeys),
                "a lone `{field}` binding was treated as an empty section and dropped"
            );
        }
    }
}
