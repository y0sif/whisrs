//! Interactive editor for `~/.config/whisrs/config.toml`.
//!
//! `whisrs config` opens a menu that lets the user jump to any section of the
//! config file, edit it, and on save writes a validated `config.toml` and
//! restarts the daemon if a user service is installed.
//!
//! This complements `whisrs setup` (the one-time onboarding wizard). `setup`
//! handles install-time concerns — mic test, udev rules, service install,
//! compositor keybinding — while `config` only edits the TOML.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::{Confirm, Editor, Input, Select};

use crate::config::{setup, vocabulary};
use crate::service::ServiceManager;
use crate::{Config, HotkeyConfig, RestartOutcome};

use setup::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

/// Top-level entry point for `whisrs config`.
///
/// Loads the existing config (or a fresh default if none exists), runs the
/// menu loop, and on save writes the file and triggers a daemon restart.
pub fn run_config_menu() -> Result<()> {
    println!("\n{BOLD}whisrs config{RESET} — edit ~/.config/whisrs/config.toml\n");

    let (mut config, fresh) = match setup::load_existing_config() {
        setup::ExistingConfig::Loaded { config, unknown } => {
            // Warn at load, not at save: the keys are about to be shown back to
            // the user as a config that looks fine (issue #116).
            setup::print_unknown_keys_warning(&unknown);
            (config, false)
        }
        // A file we cannot use is not a missing file (issue #134). Opening the
        // menu on defaults here and saving would rewrite the user's file from
        // those defaults, deleting every section the defaults leave unset —
        // API keys included. Refuse instead, and leave the file alone.
        //
        // Both kinds refuse; only the way out differs, which is
        // `unusable_config_refusal`'s job (a file we cannot read is one
        // `whisrs setup` cannot rewrite either).
        //
        // Returned as an error rather than printed: `src/cli/main.rs` renders
        // it as `config failed: {e:#}` and exits non-zero, so printing it here
        // too would show the same serde error twice.
        setup::ExistingConfig::Unusable { kind, message } => {
            anyhow::bail!(setup::unusable_config_refusal(
                kind,
                &message,
                &crate::config_path()
            ));
        }
        setup::ExistingConfig::Missing => {
            println!(
                "  {YELLOW}No config file found — starting from defaults.{RESET} \
                 Run {BOLD}whisrs setup{RESET} for the full onboarding flow."
            );
            (default_config(), true)
        }
    };

    let vocab_path = vocabulary::vocabulary_path();
    let config_toml_vocabulary = config.general.vocabulary.clone();
    let use_vocab_file = match vocabulary::load_vocabulary_file(&vocab_path) {
        Ok(Some(terms)) => {
            config.general.vocabulary =
                vocabulary::merge_vocabulary(std::mem::take(&mut config.general.vocabulary), terms);
            true
        }
        Ok(None) => false,
        Err(e) => {
            println!(
                "  {YELLOW}Could not read {}: {e} — its terms are not shown and \
                 vocabulary edits stay in config.toml.{RESET}",
                vocab_path.display()
            );
            false
        }
    };
    let vocabulary_baseline = VocabularyBaseline {
        use_file: use_vocab_file,
        config_toml: config_toml_vocabulary,
        merged: config.general.vocabulary.clone(),
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
            "Keyboard injection (key delay, modifier wait)",
            "Clipboard fallback (copy transcript to clipboard)",
            "Clipboard-only mode (no injection)",
            "Hotkeys",
            "Tray & overlay",
            "Recording hooks",
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
            4 => edit_vocabulary_and_prompt(&mut config, use_vocab_file)?,
            5 => edit_audio_device(&mut config)?,
            6 => edit_key_delay(&mut config)?,
            7 => edit_clipboard_fallback(&mut config)?,
            8 => edit_clipboard_only(&mut config)?,
            9 => edit_hotkeys(&mut config)?,
            10 => edit_tray_overlay(&mut config)?,
            11 => edit_media_hooks(&mut config)?,
            12 => edit_llm(&mut config)?,
            13 => edit_llm_commands(&mut config)?,
            14 => show_config(&config),
            15 => {
                if open_in_editor(&mut config)? {
                    // External edit already wrote the file; reload and skip the
                    // normal save path so we don't clobber formatting/comments
                    // (see open_in_editor).
                    println!("  {GREEN}Applied edits from $EDITOR.{RESET}");
                }
            }
            16 => {
                // separator — no-op
            }
            17 => {
                if save_and_restart(&config, fresh, &vocabulary_baseline)? {
                    return Ok(());
                }
                // Validation failed — fall through to next loop iteration,
                // preserving the in-memory `config` so the user can fix it.
            }
            18 => {
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
        hooks: None,
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
    current_key_summary_from(config, |name| std::env::var(name).ok())
}

/// [`current_key_summary`] with the environment passed in, so tests never
/// touch the process env. Mirrors the daemon's resolvers in `factory.rs`: a
/// `WHISRS_*_API_KEY` env var wins over `config.toml`. A key from the env is
/// labelled "this shell", since a daemon run by systemd takes its env from the
/// user manager and may not see it.
fn current_key_summary_from(config: &Config, env: impl Fn(&str) -> Option<String>) -> String {
    let from_env = |var: &str, key: &str| {
        format!(
            "{BOLD}{}{RESET} {DIM}({var}, this shell){RESET}",
            setup::mask_api_key(key)
        )
    };
    let (var, key) = match config.general.backend.as_str() {
        "groq" => (
            "WHISRS_GROQ_API_KEY",
            config.groq.as_ref().map(|g| g.api_key.as_str()),
        ),
        "deepgram" | "deepgram-streaming" => (
            "WHISRS_DEEPGRAM_API_KEY",
            config.deepgram.as_ref().map(|d| d.api_key.as_str()),
        ),
        "openai" | "openai-realtime" => (
            "WHISRS_OPENAI_API_KEY",
            config.openai.as_ref().map(|o| o.api_key.as_str()),
        ),
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
            // Unlike the other backends, the daemon trims both sides here and
            // treats a blank value as absent.
            let var = "WHISRS_ASR_SIDECAR_API_KEY";
            if let Some(key) = env(var).filter(|key| !key.trim().is_empty()) {
                return from_env(var, key.trim());
            }
            return match config
                .asr_sidecar
                .as_ref()
                .and_then(|s| s.api_key.as_deref())
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => format!("{BOLD}{}{RESET}", setup::mask_api_key(key)),
                None => format!("{DIM}(optional API key not set){RESET}"),
            };
        }
        _ => return format!("{YELLOW}not set{RESET}"),
    };
    if let Some(env_key) = env(var).filter(|key| !key.is_empty()) {
        return from_env(var, &env_key);
    }
    match key {
        Some(k) if !k.is_empty() => format!("{BOLD}{}{RESET}", setup::mask_api_key(k)),
        _ => format!("{YELLOW}not set{RESET}"),
    }
}

fn daemon_status_string() -> String {
    // We don't surface "failed/inactive" separately — the user only cares
    // about active vs not when deciding whether a restart is meaningful.
    if ServiceManager::detect().is_active() {
        format!("{GREEN}running{RESET}")
    } else {
        format!("{DIM}not running (or no service installed){RESET}")
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
        .with_prompt("Comma-separated filler words (leave blank for the built-in list)")
        .with_initial_text(config.general.filler_words.join(", "))
        .allow_empty(true)
        .interact_text()
        .context("failed to read filler word list")?;

    config.general.filler_words = parse_csv_list(&input);
    Ok(())
}

fn edit_vocabulary_and_prompt(config: &mut Config, use_vocab_file: bool) -> Result<()> {
    println!("\n  {BOLD}Vocabulary & prompt{RESET}");
    println!("  {DIM}Domain terms/names sent as a hint to the backend to improve accuracy.{RESET}");
    if use_vocab_file {
        println!(
            "  {DIM}Includes the terms from vocabulary.txt. Change the list and the whole \
             of it is written back there on save; leave it alone and both files stay as \
             they are.{RESET}"
        );
    }

    // No "Current vocabulary:" line: the prompt below seeds the editable
    // buffer with the same list, and printing it twice doubled the wrapped
    // rows on screen — enough to push this section's header out of view for a
    // list of any size.
    let input: String = Input::new()
        .with_prompt("Comma-separated vocabulary (leave blank to clear)")
        .with_initial_text(config.general.vocabulary.join(", "))
        .allow_empty(true)
        .interact_text()
        .context("failed to read vocabulary")?;
    config.general.vocabulary = parse_csv_list(&input);

    let prompt: String = Input::new()
        .with_prompt("Free-form prompt (style/register hints; leave blank to clear)")
        .with_initial_text(config.general.prompt.clone().unwrap_or_default())
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
    use crate::audio::device::{is_default_name, list_input_devices};

    println!("\n  {BOLD}Audio device{RESET}");
    println!("  Current: {}", config.audio.device);

    let devices = list_input_devices();
    if devices.is_empty() {
        println!("  {YELLOW}No input devices detected.{RESET}");
    }

    let mut items = vec!["System default".to_string()];
    items.extend(devices.iter().map(|d| {
        let monitor = if d.is_monitor() {
            " [system audio]"
        } else {
            ""
        };
        format!("{}  ({}){monitor}", d.description, d.id)
    }));
    items.push("Enter a name manually".to_string());

    let current = config.audio.device.trim();
    let default = if is_default_name(current) {
        0
    } else {
        devices
            .iter()
            .position(|d| d.id == current)
            .map(|i| i + 1)
            .unwrap_or(items.len() - 1)
    };

    let choice = Select::new()
        .with_prompt("Input device (restart the daemon to apply)")
        .items(&items)
        .default(default)
        .interact()
        .context("failed to read audio device choice")?;

    config.audio.device = if choice == 0 {
        "default".to_string()
    } else if let Some(d) = devices.get(choice - 1) {
        d.id.clone()
    } else {
        Input::new()
            .with_prompt("Audio device name (\"default\" to use system default)")
            .default(config.audio.device.clone())
            .interact_text()
            .context("failed to read audio device")?
    };
    Ok(())
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

    println!(
        "\n  {DIM}How long to wait for held Super/Alt/Ctrl/Shift keys to be released \
         before typing, with batch backends and in command mode. Still held after \
         this, the text is copied to the clipboard instead. 0 = don't wait: if a \
         modifier is held, the text goes straight to the clipboard. Streaming \
         backends always wait for the release.{RESET}"
    );
    let input: String = Input::new()
        .with_prompt("modifier_wait_ms")
        .default(config.input.modifier_wait_ms.to_string())
        .interact_text()
        .context("failed to read modifier wait")?;
    if let Ok(v) = input.parse::<u64>() {
        config.input.modifier_wait_ms = v;
    } else {
        println!("  {YELLOW}Not a number, left unchanged.{RESET}");
    }
    Ok(())
}

fn edit_clipboard_fallback(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Clipboard fallback{RESET}");
    println!(
        "  {DIM}Keep the final transcript in the system clipboard as a fallback, \\\
         alongside injecting it at the cursor. Use this if injection fails \\\
         silently or produces garbled text — paste and fix manually.{RESET}"
    );

    config.input.clipboard_fallback = Confirm::new()
        .with_prompt("Keep the transcript in the clipboard after dictation?")
        .default(config.input.clipboard_fallback)
        .interact()
        .unwrap_or(config.input.clipboard_fallback);
    Ok(())
}

fn edit_clipboard_only(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Clipboard-only mode{RESET}");
    println!(
        "  {DIM}Copy the transcript to the clipboard without injecting it at \
         the cursor — no keystrokes, no paste. Overrides paste and \
         clipboard fallback.{RESET}"
    );

    config.input.clipboard_only = Confirm::new()
        .with_prompt("Copy only to the clipboard (no injection)?")
        .default(config.input.clipboard_only)
        .interact()
        .unwrap_or(config.input.clipboard_only);
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

/// Prompt for a value that may be unset, seeded with the current one.
///
/// `with_initial_text` rather than `default`: it puts the current value in
/// the editable buffer, so Enter keeps it, editing amends it in place, and a
/// blank line genuinely unsets it. With `default` the buffer starts empty and
/// an empty line returns the default, so the value could only be cleared by
/// typing a space, which the label had to explain. See
/// `every_seeded_input_prompt_lets_a_blank_line_clear_it`.
fn prompt_optional_string(label: &str, current: &Option<String>) -> Result<Option<String>> {
    let current = current.clone().unwrap_or_default();
    let input: String = Input::new()
        .with_prompt(format!("{label} (blank to unset)"))
        .with_initial_text(current)
        .allow_empty(true)
        .interact_text()
        .with_context(|| format!("failed to read {label}"))?;
    Ok(if input.trim().is_empty() {
        None
    } else {
        Some(input)
    })
}

fn edit_media_hooks(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Recording hooks{RESET}");
    println!(
        "  {DIM}Pause MPRIS media that is playing (browsers, Spotify, VLC, MPV,\n   \
         KDE Connect) while dictating, and resume exactly those afterwards.\n   \
         Media you paused yourself is left alone.{RESET}"
    );

    let mut hooks = config.hooks.clone().unwrap_or_default();

    hooks.media_auto_pause = Confirm::new()
        .with_prompt("Pause playing MPRIS media while recording?")
        .default(hooks.media_auto_pause)
        .interact()
        .unwrap_or(hooks.media_auto_pause);

    hooks.on_record_start =
        prompt_optional_string("Shell command on record start", &hooks.on_record_start)?;
    hooks.on_record_stop =
        prompt_optional_string("Shell command on record stop", &hooks.on_record_stop)?;

    // Drop the whole section if nothing is configured — keeps the TOML clean.
    let any_set =
        hooks.media_auto_pause || hooks.on_record_start.is_some() || hooks.on_record_stop.is_some();
    config.hooks = if any_set { Some(hooks) } else { None };
    Ok(())
}

fn edit_tray_overlay(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Tray & overlay{RESET}");

    config.general.tray = Confirm::new()
        .with_prompt("Show system tray icon?")
        .default(config.general.tray)
        .interact()
        .unwrap_or(config.general.tray);

    config.general.overlay = Confirm::new()
        .with_prompt("Show recording overlay?")
        .default(config.general.overlay)
        .interact()
        .unwrap_or(config.general.overlay);

    if config.general.overlay {
        let theme = setup::pick_overlay_theme();
        let mut overlay_cfg = config.overlay.clone().unwrap_or_default();
        overlay_cfg.theme = theme;
        config.overlay = Some(overlay_cfg);
        println!(
            "  {DIM}Note: position, width/height and custom colors can be set by hand in config.toml.{RESET}"
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
        .with_initial_text(entry.set_hotkey.clone().unwrap_or_default())
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
    if let Some(s) = clone.asr_sidecar.as_mut() {
        s.api_key = s.api_key.as_ref().map(|key| setup::mask_api_key(key));
    }
    if let Some(l) = clone.llm.as_mut() {
        l.api_key = setup::mask_api_key(&l.api_key);
    }
    if let Some(t) = clone.tts.as_mut() {
        t.api_key = t.api_key.as_ref().map(|key| setup::mask_api_key(key));
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

/// Where the vocabulary stood when this `whisrs config` session started.
///
/// Two lists, because the editor works on the merged view while config.toml
/// only ever held its own half. A save that did not touch the vocabulary has to
/// put `config_toml` back, or the file's terms leak into config.toml and end up
/// stored twice.
struct VocabularyBaseline {
    /// `vocabulary.txt` exists and was read successfully. False when it is
    /// missing (feature not opted into) or unreadable, in which case the file
    /// is never written and vocabulary edits stay in config.toml.
    use_file: bool,
    /// `[general] vocabulary` as read from config.toml, before the merge.
    config_toml: Vec<String>,
    /// The merged list the vocabulary editor started from.
    merged: Vec<String>,
}

/// Whether this save should move the vocabulary into `vocabulary.txt`.
///
/// The migration is gated on an actual edit, not just on the file existing.
/// Rewriting on every save made two unrelated saves destructive: it blanked
/// `[general] vocabulary` in config.toml the first time anyone changed an
/// unrelated setting (so deleting `vocabulary.txt` later lost the terms), and
/// it flattened a hand-maintained `vocabulary.txt`, dropping its comments and
/// grouping, for a save that had nothing to do with vocabulary.
///
/// Gating on the edit is also what makes an empty `vocabulary.txt` behave. An
/// empty file is a legitimate way to opt in, and it stays untouched until the
/// user actually adds a term — at which point the file is written non-empty,
/// which is what the daemon-side merge requires.
///
/// The comparison is made on both lists *after* [`normalized_vocabulary`], so
/// it is like-for-like.
fn should_migrate_vocabulary(
    use_vocab_file: bool,
    baseline: &[String],
    current: &[String],
) -> bool {
    use_vocab_file && normalized_vocabulary(baseline) != normalized_vocabulary(current)
}

/// A vocabulary list as the editor would hand it back.
///
/// The baseline is the raw merged list; anything that came out of "Vocabulary &
/// prompt" has been through `join(", ")` and [`parse_csv_list`], and that round
/// trip is not the identity — it trims padding and splits on commas. Comparing
/// the two directly made an *unedited* pass through the editor look like an
/// edit: a config.toml term with padding (`vocabulary = [" whisrs "]`, which is
/// exactly what the templated-config setups this feature exists for produce) or
/// with a comma in it fired the destructive migration when the user opened the
/// section to change only the prompt and pressed Enter over the vocabulary
/// line. Normalizing both sides removes the difference the editor itself
/// introduced, leaving only the differences the user actually made.
///
/// The transform is idempotent — its output has no commas, no padding and no
/// empty entries — so normalizing a list the editor already produced is a
/// no-op, and the never-opened-the-editor path compares equal too.
fn normalized_vocabulary(terms: &[String]) -> Vec<String> {
    parse_csv_list(&terms.join(", "))
}

/// What `[general] vocabulary` config.toml should hold after this save.
///
/// The editor works on the merged view, so writing the in-memory list straight
/// out would copy `vocabulary.txt`'s terms into config.toml and store every one
/// of them twice. On a migration config.toml is emptied, since the file is now
/// the single store. On an untouched vocabulary it gets back exactly the half
/// it started with. With no `vocabulary.txt` in play there was no merge, so the
/// edited list is written as-is, which is the pre-feature behavior.
fn config_toml_vocabulary(vocab: &VocabularyBaseline, current: &[String]) -> Vec<String> {
    if should_migrate_vocabulary(vocab.use_file, &vocab.merged, current) {
        Vec::new()
    } else if vocab.use_file {
        vocab.config_toml.clone()
    } else {
        current.to_vec()
    }
}

/// Everything a save does about the vocabulary, decided in one place.
///
/// The two halves are not independent — blanking `[general] vocabulary` is only
/// safe *because* the same save wrote the terms to `vocabulary.txt` — so they
/// are computed together and handed to the writer as a finished plan. Splitting
/// the decision across the call site is what let a one-line slip there (write
/// the blanked list to the file, skip the file write, write the merged list to
/// config.toml) destroy every term the user had with every unit test still
/// green.
#[derive(Debug, PartialEq, Eq)]
struct VocabularySavePlan {
    /// The list `[general] vocabulary` is written with.
    config_toml: Vec<String>,
    /// The list `vocabulary.txt` is written with, or `None` to leave the file
    /// exactly as it is (including its comments and grouping).
    vocabulary_txt: Option<Vec<String>>,
}

/// Decide both halves of the save. Pure: it touches no disk.
fn vocabulary_save_plan(vocab: &VocabularyBaseline, current: &[String]) -> VocabularySavePlan {
    VocabularySavePlan {
        config_toml: config_toml_vocabulary(vocab, current),
        vocabulary_txt: should_migrate_vocabulary(vocab.use_file, &vocab.merged, current)
            .then(|| current.to_vec()),
    }
}

/// Execute a [`VocabularySavePlan`] and write config.toml, in that order.
///
/// The only place either store is written on save. `save_and_restart` calls
/// this and does nothing else with the vocabulary, so there is no second copy
/// of the decision for a call-site edit to get wrong.
///
/// Ordering: `vocabulary.txt` goes first because it is the direction that
/// cannot lose a term. If config.toml then fails to write, every term the user
/// kept is still on disk somewhere — the new list in `vocabulary.txt`, the old
/// one still in config.toml. What that does *not* guarantee is that the save
/// took effect: on a deletion config.toml keeps the removed term, and the next
/// daemon start merges it straight back in. The error context says which half
/// landed so the user is not left thinking the save rolled back cleanly.
fn write_config_and_vocabulary(
    config: &Config,
    vocab: &VocabularyBaseline,
    config_path: &Path,
    vocab_path: &Path,
) -> Result<Option<PathBuf>> {
    let plan = vocabulary_save_plan(vocab, &config.general.vocabulary);

    let mut vocabulary_written = None;
    if let Some(terms) = &plan.vocabulary_txt {
        vocabulary::write_vocabulary_file(vocab_path, terms)
            .with_context(|| format!("failed to write {}", vocab_path.display()))?;
        vocabulary_written = Some(vocab_path.to_path_buf());
    }

    let mut on_disk = config.clone();
    on_disk.general.vocabulary = plan.config_toml;

    if let Err(e) = setup::write_config_to(&on_disk, config_path) {
        return Err(match &vocabulary_written {
            Some(path) => e.context(format!(
                "failed to write config: your vocabulary edits were already saved to {}, \
                 but config.toml was not updated",
                path.display()
            )),
            None => e.context("failed to write config"),
        });
    }

    Ok(vocabulary_written)
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
///
/// The vocabulary is not decided here: [`vocabulary_save_plan`] decides and
/// [`write_config_and_vocabulary`] executes, and this function only reports
/// which paths were written.
fn save_and_restart(config: &Config, fresh: bool, vocab: &VocabularyBaseline) -> Result<bool> {
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

    // Both stores are written by one call, from one plan: see
    // `write_config_and_vocabulary`. Nothing about the vocabulary is decided
    // here.
    let path = crate::config_path();
    let vocabulary_written =
        write_config_and_vocabulary(config, vocab, &path, &vocabulary::vocabulary_path())?;

    if let Some(vocab_path) = &vocabulary_written {
        println!("\n  {GREEN}Wrote {}{RESET}", vocab_path.display());
    }
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
    let manager = ServiceManager::detect();
    match manager.restart() {
        RestartOutcome::Restarted => {
            println!("  {GREEN}Daemon restarted.{RESET}");
        }
        RestartOutcome::Failed => {
            let hint = manager.restart_hint().unwrap_or("restart");
            println!("  {RED}{hint} failed.{RESET}");
            match manager {
                ServiceManager::Systemd => {
                    println!("  {DIM}Check `journalctl --user -u whisrs -e` for details.{RESET}");
                }
                ServiceManager::OpenRc => {
                    println!(
                        "  {DIM}Check the daemon log under \
                         $XDG_STATE_HOME/whisrs/whisrsd.log for details.{RESET}"
                    );
                }
                ServiceManager::None => {}
            }
        }
        RestartOutcome::NoService => {
            println!(
                "  {DIM}No whisrs service installed — restart the daemon manually \
                 for the new config to take effect:{RESET}"
            );
            println!("    pkill whisrsd; sleep 0.2; whisrsd &");
        }
    }

    if fresh {
        println!(
            "\n  {DIM}This was a fresh config. Run {BOLD}whisrs setup{RESET}{DIM} once to install\n   \
             udev rules, the service unit, and a compositor keybinding.{RESET}"
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

    /// `Input::default` prints the current value as a hint but leaves the
    /// editable buffer empty, and on an empty line it returns that default
    /// rather than `""` — `permit_empty` is only consulted when no default is
    /// set (dialoguer 0.11, `prompts/input.rs:575`). Pairing it with
    /// `allow_empty(true)` is therefore a contradiction: the prompt advertises
    /// that a blank line clears the value, and a blank line keeps it. The same
    /// pairing is why a list could only be replaced, never amended — the
    /// buffer the user edits starts empty (#141).
    ///
    /// `with_initial_text` is the pairing that works: it seeds the editable
    /// buffer, so Enter keeps the value, typing amends it, and a blank line
    /// really does clear it. Driven through a real 80-column pty it also
    /// leaves one fewer orphaned row than `default` when the value wraps.
    ///
    /// Both directions are pinned, because the opposite slip is just as
    /// silent: drop `allow_empty(true)` from a seeded chain and `permit_empty`
    /// is false again, so dialoguer re-seeds the initial text and re-prompts
    /// forever on an empty line (`prompts/input.rs:303,320-324,589`) while
    /// the label still promises that blanking works.
    ///
    /// The prompting is interactive IO and cannot be unit tested, so this pins
    /// the builder shape instead. The chain count is asserted because the
    /// constructor needle is an exact substring: a site spelled
    /// `Input::with_theme(..)` or `Input::<String>::new()` would drop out of
    /// the scan with nothing noticing. Comments are stripped first — this file
    /// explains itself inline constantly, and a comment merely naming
    /// `.default(` would fail correct code. The needles are assembled at
    /// runtime so the test does not match its own source.
    #[test]
    fn every_seeded_input_prompt_lets_a_blank_line_clear_it() {
        let source = setup::source_without_comments(include_str!("edit.rs"));
        let ctor = format!("Input::{}()", "new");
        let default = format!(".{}(", "default");
        let initial = format!(".with_{}_text(", "initial");
        let allow_empty = format!(".{}(true)", "allow_empty");

        let chains: Vec<&str> = source
            .split(&ctor)
            .skip(1)
            .map(|chain| chain.split(".interact").next().unwrap())
            .collect();
        assert_eq!(
            chains.len(),
            17,
            "the scan found {} `Input` chains, not 17. Fewer means a \
             constructor spelling the needle misses, which makes this test \
             vacuous; more means a prompt was added — audit its chain against \
             the rules below, then bump this number",
            chains.len()
        );

        for (i, chain) in chains.iter().enumerate() {
            assert!(
                !(chain.contains(&default) && chain.contains(&allow_empty)),
                "Input chain {i} seeds a default and also permits an empty \
                 line, so its blank-clears-the-value label is false:\n{chain}"
            );
            assert!(
                !chain.contains(&initial) || chain.contains(&allow_empty),
                "Input chain {i} seeds the buffer but does not permit an \
                 empty line, so blanking it re-prompts forever:\n{chain}"
            );
        }
    }

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

    /// `run_config_menu` is pure dialoguer IO, so pin the call the same way:
    /// issue #116 was `whisrs config` loading a config with a typo'd key and
    /// saying nothing about it, then deleting the key on save.
    #[test]
    fn run_config_menu_warns_about_unknown_keys_at_load() {
        let source = include_str!("edit.rs");
        let body = source
            .split("pub fn run_config_menu(")
            .nth(1)
            .expect("edit.rs defines run_config_menu")
            .split("\n}\n")
            .next()
            .expect("run_config_menu has a body");

        let warn_at = body
            .find("print_unknown_keys_warning(&unknown)")
            .expect("run_config_menu never warns about unknown keys");
        // Before the menu, exactly as `run_setup` warns before its prompt: a
        // warning printed after the menu opens scrolls past unseen, and
        // "Discard & exit" returns without ever reaching it.
        let prompt_at = body.find("Select::new()").expect("run_config_menu prompts");
        assert!(
            warn_at < prompt_at,
            "the warning must be printed before the menu"
        );
    }

    /// Issue #134: a config that cannot be read is not a config that isn't
    /// there. Opening the menu on `default_config()` and saving rewrote the
    /// user's file from defaults, deleting every section they leave unset — API
    /// keys included. Pinned the same way, since the refusal is in interactive IO.
    #[test]
    fn run_config_menu_refuses_an_unusable_config_before_the_menu() {
        let source = include_str!("edit.rs");
        let body = source
            .split("pub fn run_config_menu(")
            .nth(1)
            .expect("edit.rs defines run_config_menu")
            .split("\n}\n")
            .next()
            .expect("run_config_menu has a body");

        let arm_at = body
            .find("setup::ExistingConfig::Unusable {")
            .expect("run_config_menu does not handle an unusable config");
        let missing_at = body
            .find("setup::ExistingConfig::Missing =>")
            .expect("run_config_menu does not handle a missing config");
        assert!(
            arm_at < missing_at,
            "this test slices the unusable arm as the text before the missing arm"
        );
        // Comments stripped: the arm explains itself at length, and the
        // assertions below search for code spellings a comment could also
        // contain (`default_config()`).
        let arm = setup::source_without_comments(&body[arm_at..missing_at]);

        // It refuses...
        assert!(
            arm.contains("anyhow::bail!"),
            "the unusable branch must return an error so the process exits non-zero: {arm}"
        );
        // ...and does not quietly substitute defaults, which is the bug.
        assert!(
            !arm.contains("default_config()"),
            "the unusable branch must not fall back to defaults: {arm}"
        );
        // And it refuses before the menu opens, so no edit can be made against
        // a config the user never had.
        let prompt_at = body.find("Select::new()").expect("run_config_menu prompts");
        assert!(
            arm_at < prompt_at,
            "the refusal must come before the menu is shown"
        );
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// No vocabulary.txt means no migration, however the list was edited: the
    /// terms stay in config.toml, which is the pre-feature behavior.
    #[test]
    fn without_a_vocabulary_file_nothing_migrates() {
        assert!(!should_migrate_vocabulary(
            false,
            &terms(&["whisrs"]),
            &terms(&["whisrs", "NixOS"])
        ));
    }

    /// The bug this gate exists for: a save that never touched the vocabulary
    /// used to rewrite vocabulary.txt flat and blank `[general] vocabulary` in
    /// config.toml. An unchanged list must leave both stores alone.
    #[test]
    fn an_unchanged_vocabulary_does_not_migrate() {
        // Equal by value, not the same Vec: the save path compares the list
        // the editor started from against the one it ends with.
        assert!(!should_migrate_vocabulary(
            true,
            &terms(&["whisrs", "Hyprland"]),
            &terms(&["whisrs", "Hyprland"])
        ));
    }

    /// An empty vocabulary.txt is a legitimate way to opt in, so its mere
    /// existence is not a reason to write anything. Nothing on either side, or
    /// terms that only ever lived in config.toml, both stay put.
    #[test]
    fn an_empty_vocabulary_file_alone_does_not_migrate() {
        assert!(!should_migrate_vocabulary(true, &[], &[]));
        assert!(!should_migrate_vocabulary(
            true,
            &terms(&["whisrs"]),
            &terms(&["whisrs"])
        ));
    }

    /// Adding, removing or reordering a term is an edit, and an edit is what
    /// moves the whole list into vocabulary.txt.
    #[test]
    fn any_vocabulary_edit_migrates() {
        let baseline = terms(&["whisrs", "Hyprland"]);
        for current in [
            terms(&["whisrs", "Hyprland", "NixOS"]),
            terms(&["whisrs"]),
            terms(&["Hyprland", "whisrs"]),
        ] {
            assert!(
                should_migrate_vocabulary(true, &baseline, &current),
                "editing {baseline:?} into {current:?} must migrate"
            );
        }
    }

    /// Deleting every term still migrates, so vocabulary.txt is written empty
    /// rather than left holding the terms the user just removed.
    #[test]
    fn clearing_the_vocabulary_migrates() {
        assert!(should_migrate_vocabulary(
            true,
            &terms(&["whisrs", "Hyprland"]),
            &[]
        ));
    }

    /// Casing is part of the term: Deepgram echoes each keyterm back in the
    /// casing configured, so `Whisrs` is a different term from `whisrs`.
    #[test]
    fn a_case_only_vocabulary_change_migrates() {
        assert!(should_migrate_vocabulary(
            true,
            &terms(&["whisrs"]),
            &terms(&["Whisrs"])
        ));
    }

    fn baseline(use_file: bool, config_toml: &[&str], file: &[&str]) -> VocabularyBaseline {
        VocabularyBaseline {
            use_file,
            config_toml: terms(config_toml),
            merged: crate::config::vocabulary::merge_vocabulary(terms(config_toml), terms(file)),
        }
    }

    /// The regression a naive "just don't migrate" fix introduces: the editor
    /// holds the merged list, so an untouched save must put config.toml's own
    /// half back rather than write the merge into it. Otherwise vocabulary.txt's
    /// terms are silently copied into config.toml and stored twice.
    #[test]
    fn an_untouched_save_leaves_config_toml_holding_only_its_own_terms() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let current = vocab.merged.clone();
        assert_eq!(
            config_toml_vocabulary(&vocab, &current),
            terms(&["whisrs"]),
            "NixOS lives in vocabulary.txt and must not be copied into config.toml"
        );
    }

    /// Once the user edits the list, vocabulary.txt becomes the single store
    /// and config.toml is emptied, so deleting a term cannot resurrect it.
    #[test]
    fn an_edited_vocabulary_empties_config_toml() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        assert!(
            config_toml_vocabulary(&vocab, &terms(&["whisrs", "NixOS", "Hyprland"])).is_empty()
        );
        assert!(config_toml_vocabulary(&vocab, &[]).is_empty());
    }

    /// With no vocabulary.txt there was no merge, so the edited list is what
    /// config.toml gets — the behavior from before the file existed.
    #[test]
    fn without_a_vocabulary_file_config_toml_keeps_the_edited_list() {
        let vocab = baseline(false, &["whisrs"], &[]);
        assert_eq!(
            config_toml_vocabulary(&vocab, &terms(&["whisrs", "Hyprland"])),
            terms(&["whisrs", "Hyprland"])
        );
    }

    /// One row of the save-time state table: what the two stores were, what the
    /// editor ended up holding, and what each store must be written with.
    struct PlanRow {
        what: &'static str,
        /// `vocabulary.txt` exists and was read.
        use_file: bool,
        /// `[general] vocabulary` as config.toml held it at startup.
        config_toml: &'static [&'static str],
        /// The terms `vocabulary.txt` held at startup.
        file: &'static [&'static str],
        /// The list in memory when the user hit "Save & exit".
        current: &'static [&'static str],
        /// What `[general] vocabulary` must be written with.
        expect_config_toml: &'static [&'static str],
        /// What `vocabulary.txt` must be written with, `None` to leave it be.
        expect_file: Option<&'static [&'static str]>,
    }

    /// The whole save-time decision, one row per reachable state.
    ///
    /// This is the table [`vocabulary_save_plan`] exists to satisfy. It is the
    /// only decision the save makes about the vocabulary, so a slip in
    /// `write_config_and_vocabulary` has nothing to reinterpret — see
    /// `a_migrating_save_writes_the_whole_list_to_the_file_and_blanks_config_toml`
    /// for the execution half.
    #[test]
    fn the_vocabulary_save_plan_covers_every_state() {
        let rows = [
            PlanRow {
                what: "no vocabulary.txt, vocabulary untouched",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "no vocabulary.txt, term added — stays in config.toml",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &["whisrs", "NixOS"],
                expect_file: None,
            },
            PlanRow {
                what: "no vocabulary.txt, list cleared",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &[],
                expect_config_toml: &[],
                expect_file: None,
            },
            PlanRow {
                what: "vocabulary.txt present, vocabulary untouched — neither store moves",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "term added — the whole merged list migrates to the file",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs", "NixOS", "Deepgram"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs", "NixOS", "Deepgram"]),
            },
            PlanRow {
                what: "term removed — the file gets the survivors, config.toml is blanked",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs"]),
            },
            PlanRow {
                what: "reordered — order is part of the list (Deepgram budgets in order)",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["NixOS", "whisrs"],
                expect_config_toml: &[],
                expect_file: Some(&["NixOS", "whisrs"]),
            },
            PlanRow {
                what: "case-only change — Deepgram echoes the casing configured",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["Whisrs", "NixOS"],
                expect_config_toml: &[],
                expect_file: Some(&["Whisrs", "NixOS"]),
            },
            PlanRow {
                what: "everything deleted — the file is written empty, not left holding terms",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &[],
                expect_config_toml: &[],
                expect_file: Some(&[]),
            },
            PlanRow {
                what: "empty vocabulary.txt, nothing anywhere — its existence is not an edit",
                use_file: true,
                config_toml: &[],
                file: &[],
                current: &[],
                expect_config_toml: &[],
                expect_file: None,
            },
            PlanRow {
                what: "empty vocabulary.txt, config.toml terms untouched — they stay put",
                use_file: true,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "empty vocabulary.txt, term added — now the file is written",
                use_file: true,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs", "NixOS"]),
            },
            PlanRow {
                // The editor round-trips through `join(", ")` + `parse_csv_list`,
                // which trims. A padded config.toml term must not read as an edit.
                what: "padded config.toml term, editor opened and left alone",
                use_file: true,
                config_toml: &[" whisrs "],
                file: &["NixOS"],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &[" whisrs "],
                expect_file: None,
            },
            PlanRow {
                // Same, for the other half of that round trip: the editor's
                // input format splits on commas.
                what: "comma-containing config.toml term, editor opened and left alone",
                use_file: true,
                config_toml: &["Claude, Code"],
                file: &["NixOS"],
                current: &["Claude", "Code", "NixOS"],
                expect_config_toml: &["Claude, Code"],
                expect_file: None,
            },
        ];

        for row in rows {
            let vocab = baseline(row.use_file, row.config_toml, row.file);
            let plan = vocabulary_save_plan(&vocab, &terms(row.current));
            assert_eq!(
                plan,
                VocabularySavePlan {
                    config_toml: terms(row.expect_config_toml),
                    vocabulary_txt: row.expect_file.map(terms),
                },
                "{}",
                row.what
            );
        }
    }

    /// A `whisrs config` session that only ever loaded the config: the baseline
    /// and the in-memory list are the same raw merged list, padding and all.
    /// Nothing may migrate.
    #[test]
    fn a_save_that_never_opened_the_vocabulary_editor_does_not_migrate() {
        let vocab = baseline(true, &[" whisrs ", "Claude, Code"], &["NixOS"]);
        let untouched = vocab.merged.clone();
        assert_eq!(
            vocabulary_save_plan(&vocab, &untouched).vocabulary_txt,
            None
        );
    }

    /// A config.toml + vocabulary.txt pair in a fresh temp dir, driven through
    /// the real save path. Returns the two paths and what the vocabulary write
    /// reported.
    fn run_save(
        vocab: &VocabularyBaseline,
        current: &[&str],
        vocabulary_txt: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, Option<PathBuf>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        if let Some(contents) = vocabulary_txt {
            fs::write(&vocab_path, contents).expect("seed vocabulary.txt");
        }

        let mut config = default_config();
        config.general.vocabulary = terms(current);
        let written = write_config_and_vocabulary(&config, vocab, &config_path, &vocab_path)
            .expect("save succeeds");

        (dir, config_path, vocab_path, written)
    }

    fn config_toml_terms(path: &Path) -> Vec<String> {
        let contents = fs::read_to_string(path).expect("config.toml exists");
        let parsed: Config = toml::from_str(&contents).expect("config.toml reparses");
        parsed.general.vocabulary
    }

    /// The composition, not the pieces: an edit must put the *whole merged
    /// list* in vocabulary.txt and an empty list in config.toml, in that order.
    ///
    /// Writing the blanked config.toml list to the file, skipping the file
    /// write, or writing the merged list into config.toml each destroy or
    /// duplicate the user's terms, and each is a one-line slip at this call
    /// site. All three fail here.
    #[test]
    fn a_migrating_save_writes_the_whole_list_to_the_file_and_blanks_config_toml() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let (_dir, config_path, vocab_path, written) = run_save(
            &vocab,
            &["whisrs", "NixOS", "Deepgram"],
            Some("# hand-written\nNixOS\n"),
        );

        assert_eq!(written.as_deref(), Some(vocab_path.as_path()));
        assert_eq!(
            vocabulary::load_vocabulary_file(&vocab_path).expect("readable"),
            Some(terms(&["whisrs", "NixOS", "Deepgram"])),
            "vocabulary.txt must hold every term the editor showed"
        );
        assert!(
            config_toml_terms(&config_path).is_empty(),
            "config.toml must be blanked so no term is stored twice"
        );
    }

    /// The other half of the composition: with no edit, neither store is
    /// touched. vocabulary.txt keeps its comments byte for byte and config.toml
    /// gets back exactly its own half of the list, not the merge.
    #[test]
    fn a_save_without_a_vocabulary_edit_leaves_both_stores_alone() {
        let seeded = "# Deepgram keyterms\n\n# proper nouns\nNixOS\n";
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let (_dir, config_path, vocab_path, written) =
            run_save(&vocab, &["whisrs", "NixOS"], Some(seeded));

        assert_eq!(written, None);
        assert_eq!(
            fs::read_to_string(&vocab_path).expect("readable"),
            seeded,
            "an untouched vocabulary must not cost the user their comments"
        );
        assert_eq!(
            config_toml_terms(&config_path),
            terms(&["whisrs"]),
            "config.toml keeps its own half; the file's terms must not leak in"
        );
    }

    /// FIX for the editor round trip: opening "Vocabulary & prompt" to change
    /// only the prompt and pressing Enter over the vocabulary line is not an
    /// edit, even when config.toml's terms have padding or a comma in them.
    /// Before this, `parse_csv_list` normalized them, the lists compared
    /// unequal, and a migration the user never asked for flattened their
    /// hand-maintained vocabulary.txt.
    #[test]
    fn an_unedited_pass_through_the_editor_does_not_migrate() {
        let seeded = "# grouped by project\nNixOS\n\n# people\nClaude\n";
        let vocab = baseline(true, &[" whisrs ", "Claude, Code"], &["NixOS", "Claude"]);
        // Exactly what the editor hands back when the user accepts the default.
        let echoed = normalized_vocabulary(&vocab.merged);
        let echoed: Vec<&str> = echoed.iter().map(String::as_str).collect();

        let (_dir, config_path, vocab_path, written) = run_save(&vocab, &echoed, Some(seeded));

        assert_eq!(written, None, "an unedited pass must not write the file");
        assert_eq!(fs::read_to_string(&vocab_path).expect("readable"), seeded);
        assert_eq!(
            config_toml_terms(&config_path),
            terms(&[" whisrs ", "Claude, Code"]),
            "config.toml must keep its terms verbatim"
        );
    }

    /// Deleting a term is the case blanking config.toml exists for: the term
    /// must be in neither store afterwards, so the daemon's startup merge
    /// cannot bring it back. This is the resurrection scenario end to end —
    /// save, then re-run the merge the daemon does.
    #[test]
    fn a_deleted_term_cannot_resurrect_from_config_toml() {
        let vocab = baseline(true, &["whisrs", "ghost"], &["NixOS"]);
        let (_dir, config_path, vocab_path, _) =
            run_save(&vocab, &["whisrs", "NixOS"], Some("NixOS\n"));

        let from_file = vocabulary::load_vocabulary_file(&vocab_path)
            .expect("readable")
            .expect("written");
        let merged = vocabulary::merge_vocabulary(config_toml_terms(&config_path), from_file);
        assert_eq!(
            merged,
            terms(&["whisrs", "NixOS"]),
            "the daemon must load exactly what the editor showed after the delete"
        );
    }

    /// A term starting with `#` survives the migration. It used to be written
    /// as a line the parser reads as a comment, so it disappeared from
    /// vocabulary.txt in the same save that blanked config.toml — gone from
    /// both stores, silently.
    #[test]
    fn a_hash_leading_term_survives_the_migration() {
        let vocab = baseline(true, &["#1", "NixOS"], &[]);
        let (_dir, config_path, vocab_path, _) =
            run_save(&vocab, &["#1", "NixOS", "Deepgram"], Some(""));

        let from_file = vocabulary::load_vocabulary_file(&vocab_path)
            .expect("readable")
            .expect("written");
        let merged = vocabulary::merge_vocabulary(config_toml_terms(&config_path), from_file);
        assert_eq!(merged, terms(&["#1", "NixOS", "Deepgram"]));
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

    /// Every `Config` field whose section carries an `api_key`, discovered from
    /// the struct definitions rather than a hand-written list. `[tts]` was
    /// missed by `render_masked_toml` for six releases precisely because the
    /// set of key-bearing sections lived only in that function's body.
    fn key_bearing_config_fields() -> Vec<String> {
        let types_src = include_str!("types.rs");

        let mut with_key: Vec<&str> = Vec::new();
        for src in [types_src, include_str!("../llm.rs")] {
            for chunk in src.split("\npub struct ").skip(1) {
                let name = chunk
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default();
                let body = chunk.split("\n}").next().unwrap_or_default();
                if body.contains("pub api_key") {
                    with_key.push(name);
                }
            }
        }
        assert_eq!(
            with_key.len(),
            7,
            "the struct scan found {with_key:?} — it only sees column-0 `pub struct` \
             in types.rs and llm.rs, so a key-bearing struct that moved elsewhere is \
             invisible here and would pass vacuously"
        );

        let config_body = types_src
            .split("\npub struct Config {")
            .nth(1)
            .expect("types.rs defines Config")
            .split("\n}")
            .next()
            .expect("Config has a body");

        let mut fields = Vec::new();
        for line in config_body.lines() {
            let Some(rest) = line.trim().strip_prefix("pub ") else {
                continue;
            };
            let Some((name, ty)) = rest.split_once(':') else {
                continue;
            };
            if with_key.iter().any(|s| ty.contains(s)) {
                fields.push(name.trim().to_string());
            }
        }
        fields
    }

    /// `render_masked_toml` masks a hand-written list of sections, so a new
    /// key-bearing section is silently printed in cleartext under a heading
    /// that promises "masked". Assert the function touches every section the
    /// struct definitions say carries a key.
    ///
    /// Structural only: it proves each section is *mentioned*, not that the
    /// mention masks anything. `a_populated_config_renders_no_cleartext_key`
    /// is the semantic half, and neither test replaces the other.
    #[test]
    fn render_masked_toml_covers_every_key_bearing_section() {
        let source = include_str!("edit.rs");
        let body = source
            .split("fn render_masked_toml(")
            .nth(1)
            .expect("edit.rs defines render_masked_toml")
            .split("\nfn ")
            .next()
            .expect("render_masked_toml has a body");

        let fields = key_bearing_config_fields();
        for field in &fields {
            // The trailing dot is load-bearing: `clone.openai` is a prefix of
            // `clone.openai_compatible_realtime`, so an unanchored match let a
            // deleted `[openai]` arm pass this test.
            assert!(
                body.contains(&format!("clone.{field}.")),
                "render_masked_toml never masks `[{field}] api_key`, so \
                 `whisrs config` prints it in cleartext under \"Current config (masked)\""
            );
        }
        assert_eq!(
            fields.len(),
            7,
            "expected 7 key-bearing sections, found {fields:?} — update this count \
             deliberately once the new section is masked above"
        );
    }

    /// The value half: a config with a key in every section must render with
    /// no secret surviving, and with every `api_key` still present and masked.
    /// A section dropped from serialization would otherwise pass the scan test.
    #[test]
    fn a_populated_config_renders_no_cleartext_key() {
        let config: Config = toml::from_str(
            r#"
[deepgram]
api_key = "deepgram-SECRET-aaaa"
[groq]
api_key = "groq-SECRET-bbbb"
[openai]
api_key = "openai-SECRET-cccc"
[asr-sidecar]
api_key = "sidecar-SECRET-dddd"
[openai-compatible-realtime]
api_key = "realtime-SECRET-eeee"
[llm]
api_key = "llm-SECRET-ffff"
[tts]
api_key = "tts-SECRET-gggg"
"#,
        )
        .expect("fixture parses");

        let rendered = render_masked_toml(&config).expect("renders");

        assert!(
            !rendered.contains("SECRET"),
            "a key survived masking:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("api_key").count(),
            7,
            "every section must still print a masked api_key:\n{rendered}"
        );
        for tail in ["aaaa", "bbbb", "cccc", "dddd", "eeee", "ffff", "gggg"] {
            assert!(
                rendered.contains(&format!("****{tail}")),
                "`{tail}` section is missing its masked key:\n{rendered}"
            );
        }
    }

    /// Parse `toml` and summarize its key with `env` standing in for the
    /// process environment, which these tests never touch.
    fn key_summary(toml: &str, env: &[(&str, &str)]) -> String {
        let config: Config = toml::from_str(toml).expect("fixture parses");
        current_key_summary_from(&config, |name| {
            env.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| value.to_string())
        })
    }

    /// One row per keyed backend family, including an alias of each shared
    /// section: backend, section name, and the env var the daemon reads.
    const KEYED_BACKENDS: &[(&str, &str, &str)] = &[
        ("groq", "groq", "WHISRS_GROQ_API_KEY"),
        ("deepgram", "deepgram", "WHISRS_DEEPGRAM_API_KEY"),
        ("deepgram-streaming", "deepgram", "WHISRS_DEEPGRAM_API_KEY"),
        ("openai", "openai", "WHISRS_OPENAI_API_KEY"),
        ("openai-realtime", "openai", "WHISRS_OPENAI_API_KEY"),
        ("asr-sidecar", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
        ("asr", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
        ("vibevoice", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
    ];

    #[test]
    fn key_summary_shows_an_env_key_without_a_config_key() {
        for &(backend, section, var) in KEYED_BACKENDS {
            let absent = format!("[general]\nbackend = \"{backend}\"\n");
            let empty = format!("{absent}[{section}]\napi_key = \"\"\n");
            for toml in [absent, empty] {
                let summary = key_summary(&toml, &[(var, "env-key-9876")]);
                assert!(
                    summary.contains("****9876") && summary.contains(var),
                    "{backend}"
                );
                assert!(summary.contains("this shell"), "{backend}");
            }
        }
    }

    #[test]
    fn key_summary_prefers_the_env_key_like_the_daemon() {
        for &(backend, section, var) in KEYED_BACKENDS {
            let toml = format!(
                "[general]\nbackend = \"{backend}\"\n[{section}]\napi_key = \"file-key-1234\"\n"
            );
            let both = key_summary(&toml, &[(var, "env-key-9876")]);
            assert!(
                both.contains("****9876") && both.contains(var) && !both.contains("1234"),
                "{backend}"
            );

            let file_only = key_summary(&toml, &[]);
            assert!(
                file_only.contains("****1234") && !file_only.contains(var),
                "{backend}"
            );
        }
    }

    #[test]
    fn key_summary_without_any_key_is_unchanged() {
        for &(backend, section, var) in KEYED_BACKENDS {
            let summary = key_summary(&format!("[general]\nbackend = \"{backend}\"\n"), &[]);
            let expected = if section == "asr-sidecar" {
                "(optional API key not set)"
            } else {
                "not set"
            };
            assert!(summary.contains(expected), "{backend}");
            assert!(!summary.contains(var), "{backend}");
        }
    }

    /// The daemon's blank-env rules differ by backend: groq only skips an
    /// empty string, so whitespace counts as set, while the sidecar trims.
    #[test]
    fn key_summary_follows_each_backends_blank_env_rule() {
        let groq = "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"file-key-1234\"\n";
        let empty = key_summary(groq, &[("WHISRS_GROQ_API_KEY", "")]);
        assert!(empty.contains("****1234"));
        let spaces = key_summary(groq, &[("WHISRS_GROQ_API_KEY", "     ")]);
        assert!(spaces.contains("WHISRS_GROQ_API_KEY"));

        let sidecar = "[general]\nbackend = \"asr-sidecar\"\n\
                       [asr-sidecar]\napi_key = \"file-key-1234\"\n";
        let spaces = key_summary(sidecar, &[("WHISRS_ASR_SIDECAR_API_KEY", "   ")]);
        assert!(spaces.contains("****1234") && !spaces.contains("WHISRS_"));
        let padded = key_summary(
            sidecar,
            &[("WHISRS_ASR_SIDECAR_API_KEY", " env-key-9876\n")],
        );
        assert!(padded.contains("****9876"));
    }
}
