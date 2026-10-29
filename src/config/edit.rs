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

use anyhow::{Context, Result};
use dialoguer::{Confirm, Editor, Input, Select};

use crate::config::keys::{self, KeySummary};
use crate::config::vocabulary::{self, VocabularyBaseline};
use crate::config::{setup, write};
use crate::service::ServiceManager;
use crate::{
    AgentOverlayConfig, AgentOverlayCorner, AgentOverlayStyle, Config, HotkeyConfig, RestartOutcome,
};
#[cfg(feature = "recording")]
use crate::{DiarizationMode, RecordingConfig};

use setup::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

/// Top-level entry point for `whisrs config`.
///
/// Loads the existing config (or a fresh default if none exists), runs the
/// menu loop, and on save writes the file and triggers a daemon restart.
pub fn run_config_menu() -> Result<()> {
    println!("\n{BOLD}whisrs config{RESET} — edit ~/.config/whisrs/config.toml\n");

    let (mut config, fresh) = match write::load_existing_config() {
        write::ExistingConfig::Loaded { config, unknown } => {
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
        write::ExistingConfig::Unusable { kind, message } => {
            anyhow::bail!(write::unusable_config_refusal(
                kind,
                &message,
                &crate::config_path()
            ));
        }
        write::ExistingConfig::Missing => {
            println!(
                "  {YELLOW}No config file found — starting from defaults.{RESET} \
                 Run {BOLD}whisrs setup{RESET} for the full onboarding flow."
            );
            (default_config(), true)
        }
    };

    let vocab_path = vocabulary::vocabulary_path();
    let (vocabulary_baseline, unreadable) = VocabularyBaseline::load(&mut config, &vocab_path);
    if let Some(e) = unreadable {
        println!(
            "  {YELLOW}Could not read {}: {e} \u{2014} its terms are not shown and \
             vocabulary edits stay in config.toml.{RESET}",
            vocab_path.display()
        );
    }
    let use_vocab_file = vocabulary_baseline.uses_file();

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
            "Dictation hooks",
            "Command mode (LLM)",
            "Custom LLM commands",
            "Recording",
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
            14 => edit_recording(&mut config)?,
            15 => show_config(&config),
            16 => {
                if open_in_editor(&mut config)? {
                    // External edit already wrote the file; reload and skip the
                    // normal save path so we don't clobber formatting/comments
                    // (see open_in_editor).
                    println!("  {GREEN}Applied edits from $EDITOR.{RESET}");
                }
            }
            17 => {
                // separator — no-op
            }
            18 => {
                if save_and_restart(&config, fresh, &vocabulary_baseline)? {
                    return Ok(());
                }
                // Validation failed — fall through to next loop iteration,
                // preserving the in-memory `config` so the user can fix it.
            }
            19 => {
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
        agents: Vec::new(),
        agent_overlay: None,
        replacements: Default::default(),
        recording: None,
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
    render_key_summary(keys::key_summary(config))
}

/// A [`KeySummary`] in the menu's colors.
fn render_key_summary(summary: KeySummary) -> String {
    match summary {
        KeySummary::Env { var, masked } => {
            format!("{BOLD}{masked}{RESET} {DIM}({var}, this shell){RESET}")
        }
        KeySummary::File { masked } => format!("{BOLD}{masked}{RESET}"),
        KeySummary::NotNeeded => {
            format!("{DIM}(local backend \u{2014} no API key needed){RESET}")
        }
        KeySummary::Optional(what) => format!("{DIM}(optional {what} not set){RESET}"),
        KeySummary::NotSet => format!("{YELLOW}not set{RESET}"),
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

    config.general.filler_words = vocabulary::parse_csv_list(&input);
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
    config.general.vocabulary = vocabulary::parse_csv_list(&input);

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
    // Every spelling of "default" lands on the preset, not on "manual".
    let current = if is_default_name(&config.audio.device) {
        "default".to_string()
    } else {
        config.audio.device.clone()
    };
    let picked = select_input_device(
        "Input device (restart the daemon to apply)",
        &[("System default".to_string(), Some("default".to_string()))],
        &devices,
        Some(&current),
        "Audio device name (\"default\" to use system default)",
    )?;
    config.audio.device = picked.unwrap_or_else(|| "default".to_string());
    Ok(())
}

/// Pick an input device: a few fixed `presets` (label, stored value, where
/// `None` means "unset the key"), then every detected device, then manual
/// entry. `current` preselects the matching row; a value that matches neither
/// a preset nor a device preselects manual entry.
fn select_input_device(
    prompt: &str,
    presets: &[(String, Option<String>)],
    devices: &[crate::audio::device::InputDeviceInfo],
    current: Option<&str>,
    manual_prompt: &str,
) -> Result<Option<String>> {
    if devices.is_empty() {
        println!("  {YELLOW}No matching devices detected.{RESET}");
    }

    let mut items: Vec<String> = presets.iter().map(|(label, _)| label.clone()).collect();
    items.extend(devices.iter().map(|d| {
        let monitor = if d.is_monitor() {
            " [system audio]"
        } else {
            ""
        };
        format!("{}  ({}){monitor}", d.description, d.id)
    }));
    items.push("Enter a name manually".to_string());
    let manual = items.len() - 1;

    let default = if let Some(i) = presets.iter().position(|(_, v)| v.as_deref() == current) {
        i
    } else {
        current
            .and_then(|c| devices.iter().position(|d| d.id == c))
            .map(|i| i + presets.len())
            .unwrap_or(manual)
    };

    let choice = Select::new()
        .with_prompt(prompt)
        .items(&items)
        .default(default)
        .interact()
        .context("failed to read audio device choice")?;

    Ok(if let Some((_, value)) = presets.get(choice) {
        value.clone()
    } else if let Some(d) = devices.get(choice - presets.len()) {
        Some(d.id.clone())
    } else {
        let name: String = Input::new()
            .with_prompt(manual_prompt)
            .default(current.unwrap_or("default").to_string())
            .interact_text()
            .context("failed to read audio device")?;
        Some(name.trim().to_string())
    })
}

/// Batch backends the recording feature can transcribe with. The realtime backends
/// are left out on purpose: they only stream, and a recording is transcribed
/// from files after it ends. `deepgram-streaming` is left out because a
/// recording uses Deepgram's REST API either way.
#[cfg(feature = "recording")]
const RECORDING_BACKENDS: [&str; 5] =
    ["groq", "openai", "deepgram", "asr-sidecar", "local-whisper"];

#[cfg(feature = "recording")]
fn edit_recording(config: &mut Config) -> Result<()> {
    use crate::audio::device::list_input_devices;

    println!("\n  {BOLD}Recording{RESET}");
    println!(
        "  {DIM}Records your mic and the system audio as separate tracks, then\n   \
         transcribes them after you stop. Start and stop it with\n   \
         `whisrs recording` or the [hotkeys] recording binding.{RESET}"
    );

    let mut recording = RecordingConfig::effective(config);

    // Backend.
    let mut backend_items = vec![format!("Same as dictation ({})", config.general.backend)];
    backend_items.extend(RECORDING_BACKENDS.iter().map(|b| b.to_string()));
    let backend_default = match recording.backend.as_deref() {
        None => 0,
        Some(b) => RECORDING_BACKENDS
            .iter()
            .position(|m| *m == b)
            .map(|i| i + 1)
            .unwrap_or(0),
    };
    let choice = Select::new()
        .with_prompt("Transcription backend for recordings (batch backends only)")
        .items(&backend_items)
        .default(backend_default)
        .interact()
        .context("failed to read recording backend")?;
    recording.backend = if choice == 0 {
        None
    } else {
        Some(RECORDING_BACKENDS[choice - 1].to_string())
    };

    // Speaker name.
    recording.user_name = prompt_optional_string(
        "Your name in transcripts, defaults to your login name",
        &recording.user_name,
    )?;

    // Languages. `whisrs recording start -l` and friends override these for
    // one recording.
    recording.language = prompt_optional_language(
        &format!(
            "Language of both tracks (e.g. de, auto), defaults to [general] language ({})",
            config.general.language
        ),
        &recording.language,
    )?;
    recording.mic_language = prompt_optional_language(
        "Language of your microphone, defaults to the one above",
        &recording.mic_language,
    )?;
    recording.system_language = prompt_optional_language(
        "Language of the system audio, defaults to the one above",
        &recording.system_language,
    )?;

    // Speakers.
    let modes = [
        (
            DiarizationMode::Auto,
            "Auto: the diarization sidecar when its URL is set, else Deepgram",
        ),
        (
            DiarizationMode::Backend,
            "The transcription backend (Deepgram, or an asr-sidecar that returns speakers)",
        ),
        (
            DiarizationMode::Sidecar,
            "A diarization sidecar (a local server at diarization_url)",
        ),
        (DiarizationMode::Off, "Off: one speaker per track"),
    ];
    let mode_labels: Vec<&str> = modes.iter().map(|(_, label)| *label).collect();
    let mode_default = modes
        .iter()
        .position(|(mode, _)| *mode == recording.diarization)
        .unwrap_or(0);
    let choice = Select::new()
        .with_prompt("Tell the voices on the system audio apart")
        .items(&mode_labels)
        .default(mode_default)
        .interact()
        .context("failed to read speaker separation")?;
    recording.diarization = modes[choice].0;
    if matches!(
        recording.diarization,
        DiarizationMode::Auto | DiarizationMode::Sidecar
    ) {
        let required = recording.diarization == DiarizationMode::Sidecar;
        recording.diarization_url = prompt_optional_url(
            "Diarization sidecar URL, e.g. http://127.0.0.1:8770/diarize",
            &recording.diarization_url,
            required,
        )?;
        if recording.diarization_url.is_some() {
            recording.num_speakers = Input::new()
                .with_prompt("How many voices on the system audio (0 = let the sidecar decide)")
                .default(recording.num_speakers)
                .validate_with(|n: &u32| crate::recording::validate_num_speakers(*n).map(|_| ()))
                .interact_text()
                .context("failed to read the number of voices")?;
        }
    }
    recording.shared_mic = Confirm::new()
        .with_prompt("Several people share your mic (tell its voices apart too)?")
        .default(recording.shared_mic)
        .interact()
        .context("failed to read shared mic")?;
    recording.name_speakers = Confirm::new()
        .with_prompt("Name the speakers with the [llm] (sends excerpts of each transcript to it)?")
        .default(recording.name_speakers)
        .interact()
        .context("failed to read speaker naming")?;
    recording.summary = Confirm::new()
        .with_prompt("Summarize each transcript with the [llm] (sends the whole transcript to it)?")
        .default(recording.summary)
        .interact()
        .context("failed to read the summary setting")?;

    // Devices.
    let devices = list_input_devices();
    let (monitors, mics): (Vec<_>, Vec<_>) = devices.into_iter().partition(|d| d.is_monitor());

    recording.mic_device = select_input_device(
        "Microphone",
        &[
            (
                format!("Same as [audio] device ({})", config.audio.device),
                None,
            ),
            ("System default".to_string(), Some("default".to_string())),
        ],
        &mics,
        recording.mic_device.as_deref(),
        "Microphone device name",
    )?;

    // `"default"` is stored as unset: both mean the default monitor.
    let system_current = match recording.system_device.as_deref() {
        Some("default") => None,
        other => other,
    };
    recording.system_device = select_input_device(
        "System audio (what your computer plays)",
        &[
            ("Default monitor (what you hear)".to_string(), None),
            ("None (mic only)".to_string(), Some("none".to_string())),
        ],
        &monitors,
        system_current,
        "System audio source name (usually ends in .monitor)",
    )?;

    // Limits and storage.
    recording.max_duration_minutes = Input::new()
        .with_prompt("Stop recording automatically after (minutes, 1-2160)")
        .default(recording.max_duration_minutes)
        .validate_with(|v: &u32| {
            if (1..=2160).contains(v) {
                Ok(())
            } else {
                Err("must be between 1 and 2160")
            }
        })
        .interact_text()
        .context("failed to read max duration")?;

    recording.audio_retention_days = Input::new()
        .with_prompt("Keep recording audio for (days; 0 = delete once transcribed, -1 = forever)")
        .default(recording.audio_retention_days)
        .interact_text()
        .context("failed to read audio retention")?;

    recording.dir = prompt_optional_string(
        "Recordings folder, defaults to ~/.local/share/whisrs/recordings",
        &recording.dir,
    )?;

    config.recording = apply_recording_edit(config.recording.take(), recording);
    Ok(())
}

/// What the Recording row says in a build without the recording feature.
#[cfg(not(feature = "recording"))]
const RECORDING_MISSING: &str = "this whisrs was built without the recording feature, so \
     there is nothing to set here, and [recording] is kept as it is. Use a release build, or \
     rebuild with `cargo install whisrs --features recording`.";

/// The Recording row of a build without the recording feature: no question
/// to ask, only why.
#[cfg(not(feature = "recording"))]
fn edit_recording(_config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Recording{RESET}");
    println!("  {YELLOW}{RECORDING_MISSING}{RESET}");
    Ok(())
}

/// Decide what to store after the recording editor ran. An existing
/// `[recording]` section is always kept (even if every value is back at its
/// default); a config without one only gains it when the user changed
/// something, so walking through the editor does not add an empty section.
#[cfg(feature = "recording")]
fn apply_recording_edit(
    current: Option<RecordingConfig>,
    edited: RecordingConfig,
) -> Option<RecordingConfig> {
    if current.is_some() || edited != RecordingConfig::default() {
        Some(edited)
    } else {
        None
    }
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
         before each typed key, with batch backends and in command mode. Typing pauses \
         while one is held; still held after this, the rest of the text is copied to \
         the clipboard instead. 0 = don't wait: if a modifier is held, the rest goes \
         straight to the clipboard. Streaming backends always wait for the \
         release.{RESET}"
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
    // Named apart from dictation, whose hotkey is `toggle`. A build without
    // the recording feature ignores the binding, so it is not asked for, and
    // `hotkeys` keeps it as it was.
    #[cfg(feature = "recording")]
    {
        let recording_prompt = "Recording hotkey (mic + system audio)";
        hotkeys.recording = prompt_optional_string(recording_prompt, &hotkeys.recording)?;
    }

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
        recording,
    } = hotkeys;
    toggle.is_some()
        || cancel.is_some()
        || command.is_some()
        || speak.is_some()
        || recording.is_some()
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

/// [`prompt_optional_string`] for an `http(s)://` URL: re-asked until it
/// parses, and, when `required`, until it is given. An empty line always
/// reaches the validator (`allow_empty`), which refuses it when required.
#[cfg(feature = "recording")]
fn prompt_optional_url(
    label: &str,
    current: &Option<String>,
    required: bool,
) -> Result<Option<String>> {
    let input: String = Input::new()
        .with_prompt(if required {
            label.to_string()
        } else {
            format!("{label} (blank to unset)")
        })
        .with_initial_text(current.clone().unwrap_or_default())
        .allow_empty(true)
        .validate_with(move |input: &String| match parse_optional_url(input) {
            Ok(None) if required => Err("a URL is needed here".to_string()),
            other => other.map(|_| ()),
        })
        .interact_text()
        .with_context(|| format!("failed to read {label}"))?;
    Ok(parse_optional_url(&input).unwrap_or(None))
}

/// A URL typed into the editor: blank is unset, anything else must be an
/// `http://` or `https://` URL, stored trimmed.
#[cfg(feature = "recording")]
fn parse_optional_url(input: &str) -> std::result::Result<Option<String>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let url = reqwest::Url::parse(input).map_err(|e| format!("not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("use http:// or https://, not {}://", url.scheme()));
    }
    Ok(Some(input.to_string()))
}

/// [`prompt_optional_string`] for a language: re-asked until it passes the
/// rule `-l` follows, and stored normalized.
#[cfg(feature = "recording")]
fn prompt_optional_language(label: &str, current: &Option<String>) -> Result<Option<String>> {
    let input: String = Input::new()
        .with_prompt(format!("{label} (blank to unset)"))
        .with_initial_text(current.clone().unwrap_or_default())
        .allow_empty(true)
        .validate_with(|input: &String| parse_optional_language(input).map(|_| ()))
        .interact_text()
        .with_context(|| format!("failed to read {label}"))?;
    Ok(parse_optional_language(&input).unwrap_or(None))
}

/// A language typed into the editor: blank is unset, anything else must
/// pass [`crate::validate_language_override`] and is stored as it returns it.
#[cfg(feature = "recording")]
fn parse_optional_language(input: &str) -> std::result::Result<Option<String>, String> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    crate::validate_language_override(input).map(Some)
}

fn edit_media_hooks(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Dictation hooks{RESET}");
    println!(
        "  {DIM}Pause MPRIS media that is playing (browsers, Spotify, VLC, MPV,\n   \
         KDE Connect) while dictating, and resume exactly those afterwards.\n   \
         Media you paused yourself is left alone.{RESET}"
    );

    let mut hooks = config.hooks.clone().unwrap_or_default();

    hooks.media_auto_pause = Confirm::new()
        .with_prompt("Pause playing MPRIS media while dictating?")
        .default(hooks.media_auto_pause)
        .interact()
        .unwrap_or(hooks.media_auto_pause);

    hooks.on_record_start =
        prompt_optional_string("Shell command on dictation start", &hooks.on_record_start)?;
    hooks.on_record_stop =
        prompt_optional_string("Shell command on dictation stop", &hooks.on_record_stop)?;

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

    // The agent overlay draws only with [[agents]] entries.
    if !config.agents.is_empty() {
        edit_agent_overlay(config);
    }
    Ok(())
}

/// The agent overlay's styles, as the editor offers them.
const AGENT_OVERLAY_STYLES: [(AgentOverlayStyle, &str); 2] = [
    (
        AgentOverlayStyle::Orb,
        "Orb   (rings and arcs in the state's color)",
    ),
    (
        AgentOverlayStyle::Face,
        "Face  (a small face whose eyes and mouth show the state)",
    ),
];

/// The agent overlay's corners, as the editor offers them.
const AGENT_OVERLAY_CORNERS: [(AgentOverlayCorner, &str); 4] = [
    (AgentOverlayCorner::BottomRight, "Bottom right"),
    (AgentOverlayCorner::BottomLeft, "Bottom left"),
    (AgentOverlayCorner::TopRight, "Top right"),
    (AgentOverlayCorner::TopLeft, "Top left"),
];

/// `[agent_overlay]` style and corner, while it is shown (it follows the
/// recording overlay unless `enabled` is set). Left out of the file while
/// it holds only the defaults.
fn edit_agent_overlay(config: &mut Config) {
    let mut agent_overlay = config.agent_overlay.clone().unwrap_or_default();
    if !agent_overlay.enabled(config.general.overlay) {
        println!(
            "  {DIM}Note: the agent overlay is off. `[agent_overlay] enabled = true` in config.toml shows it.{RESET}"
        );
        return;
    }
    println!(
        "
  {BOLD}Agent overlay{RESET}"
    );
    println!(
        "  {DIM}A row per agent at work, in a screen corner: what it does and for how long.{RESET}"
    );
    let style = pick_option(
        "Agent overlay style",
        &AGENT_OVERLAY_STYLES,
        agent_overlay.style(),
    );
    let corner = pick_option(
        "Agent overlay corner",
        &AGENT_OVERLAY_CORNERS,
        agent_overlay.corner(),
    );
    agent_overlay.style = style.as_str().to_string();
    agent_overlay.corner = corner.as_str().to_string();
    config.agent_overlay =
        (agent_overlay != AgentOverlayConfig::default()).then_some(agent_overlay);
}

/// Pick one of `options` (value, label), starting at `current`.
fn pick_option<T: Copy + PartialEq>(prompt: &str, options: &[(T, &str)], current: T) -> T {
    let default = options
        .iter()
        .position(|(value, _)| *value == current)
        .unwrap_or(0);
    let labels: Vec<&str> = options.iter().map(|(_, label)| *label).collect();
    let choice = Select::new()
        .with_prompt(prompt)
        .items(&labels)
        .default(default)
        .interact()
        .unwrap_or(default);
    options[choice].0
}

fn edit_llm(config: &mut Config) -> Result<()> {
    println!("\n  {BOLD}Command mode (LLM){RESET}");
    println!("  {DIM}Select text + hotkey + speak instruction → LLM rewrites it in place.{RESET}");

    let current_label = config
        .llm
        .as_ref()
        .map(|l| format!("{} ({})", l.model, keys::mask_api_key(&l.api_key)))
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
    match keys::render_masked_toml(config) {
        Ok(s) => {
            for line in s.lines() {
                println!("    {line}");
            }
        }
        Err(e) => println!("  {RED}Failed to render config: {e}{RESET}"),
    }
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
///
/// The vocabulary is not decided here: `vocabulary_save_plan` decides and
/// [`vocabulary::write_config_and_vocabulary`] executes, and this function
/// only reports which paths were written.
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
    let vocabulary_written = vocabulary::write_config_and_vocabulary(
        config,
        vocab,
        &path,
        &vocabulary::vocabulary_path(),
    )?;

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
            22,
            "the scan found {} `Input` chains, not 22. Fewer means a \
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
            recording: Some("e".into()),
        };
        let json = serde_json::to_value(&all_set).expect("HotkeyConfig serializes");
        json.as_object()
            .expect("HotkeyConfig is a struct")
            .keys()
            .cloned()
            .collect()
    }

    /// A language typed into the recording editor: blank unsets it, a valid
    /// one is stored normalized, anything else is re-asked.
    #[cfg(feature = "recording")]
    #[test]
    fn recording_editor_languages_follow_the_l_rule() {
        assert_eq!(parse_optional_language("  "), Ok(None));
        assert_eq!(parse_optional_language("DE"), Ok(Some("de".into())));
        assert_eq!(parse_optional_language("auto"), Ok(Some("auto".into())));
        assert!(parse_optional_language("german").is_err());
    }

    /// Walking through the recording editor without changing anything must not
    /// add a `[recording]` section; any change, or an existing section, keeps it.
    #[cfg(feature = "recording")]
    #[test]
    fn recording_section_is_written_only_when_used() {
        assert_eq!(apply_recording_edit(None, RecordingConfig::default()), None);
        assert_eq!(parse_optional_url("  "), Ok(None));
        assert_eq!(
            parse_optional_url(" http://127.0.0.1:8770/diarize "),
            Ok(Some("http://127.0.0.1:8770/diarize".to_string()))
        );
        assert!(parse_optional_url("ws://127.0.0.1:8770").is_err());
        assert!(parse_optional_url("127.0.0.1:8770").is_err());

        let changed = RecordingConfig {
            user_name: Some("Ada".into()),
            ..Default::default()
        };
        assert_eq!(apply_recording_edit(None, changed.clone()), Some(changed));

        assert_eq!(
            apply_recording_edit(Some(RecordingConfig::default()), RecordingConfig::default()),
            Some(RecordingConfig::default())
        );
    }

    /// The menu labels and the `match selection` arms are two hand-kept lists;
    /// pin that the recording row lands on its own handler.
    #[test]
    fn recording_menu_row_dispatches_to_edit_recording() {
        let source = include_str!("edit.rs");
        let body = source
            .split("pub fn run_config_menu(")
            .nth(1)
            .expect("edit.rs defines run_config_menu")
            .split("\n}\n")
            .next()
            .expect("run_config_menu has a body");
        let labels: Vec<&str> = body
            .split("let choices = &[")
            .nth(1)
            .expect("menu choices")
            .split("];")
            .next()
            .expect("menu choices end")
            .lines()
            .filter_map(|l| l.trim().strip_prefix('"'))
            .filter_map(|l| l.strip_suffix("\","))
            .collect();
        let index = labels
            .iter()
            .position(|l| *l == "Recording")
            .expect("menu has a Recording row");
        assert!(
            body.contains(&format!("{index} => edit_recording(&mut config)?")),
            "Recording is row {index} but that arm does not call edit_recording"
        );
        let last = labels.len() - 1;
        assert!(
            body.contains(&format!(
                "{last} => {{\n                println!(\"\\n  {{DIM}}Discarded"
            )),
            "the last row ({last}) must be Discard & exit"
        );
    }

    /// The agent overlay's questions are asked only with agents configured,
    /// and offer every style and corner there is.
    #[test]
    fn agent_overlay_questions_need_agents() {
        let source = include_str!("edit.rs");
        let body = source
            .split("fn edit_tray_overlay(")
            .nth(1)
            .expect("edit.rs defines edit_tray_overlay")
            .split("\nfn ")
            .next()
            .expect("edit_tray_overlay has a body");
        let gate = body
            .find("if !config.agents.is_empty() {")
            .expect("the agent overlay is not gated on agents");
        let asked = body
            .find("edit_agent_overlay(config)")
            .expect("edit_tray_overlay never asks about the agent overlay");
        assert!(gate < asked);

        for style in [AgentOverlayStyle::Orb, AgentOverlayStyle::Face] {
            assert!(AGENT_OVERLAY_STYLES.iter().any(|(s, _)| *s == style));
        }
        for corner in ["bottom-right", "bottom-left", "top-right", "top-left"] {
            let corner = AgentOverlayCorner::parse(corner).unwrap();
            assert!(AGENT_OVERLAY_CORNERS.iter().any(|(c, _)| *c == corner));
        }
        // The default is the first offered, so a fresh config starts on it.
        assert_eq!(AGENT_OVERLAY_STYLES[0].0, AgentOverlayStyle::default());
        assert_eq!(AGENT_OVERLAY_CORNERS[0].0, AgentOverlayCorner::default());
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
            .find("write::ExistingConfig::Unusable {")
            .expect("run_config_menu does not handle an unusable config");
        let missing_at = body
            .find("write::ExistingConfig::Missing =>")
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
                "recording" => hotkeys.recording = Some("Super+Shift+M".into()),
                other => panic!("unhandled hotkey field `{other}` — add it to this test"),
            }
            assert!(
                any_hotkey_set(&hotkeys),
                "a lone `{field}` binding was treated as an empty section and dropped"
            );
        }
    }

    /// The menu header shows each [`KeySummary`] as it always has: the
    /// summary moved to `keys.rs`, its text did not change.
    #[test]
    fn key_summary_renders_the_menu_text() {
        let cases = [
            (
                KeySummary::Env {
                    var: "WHISRS_GROQ_API_KEY",
                    masked: "****9876".into(),
                },
                format!("{BOLD}****9876{RESET} {DIM}(WHISRS_GROQ_API_KEY, this shell){RESET}"),
            ),
            (
                KeySummary::File {
                    masked: "****1234".into(),
                },
                format!("{BOLD}****1234{RESET}"),
            ),
            (
                KeySummary::NotNeeded,
                format!("{DIM}(local backend \u{2014} no API key needed){RESET}"),
            ),
            (
                KeySummary::Optional("bearer token"),
                format!("{DIM}(optional bearer token not set){RESET}"),
            ),
            (
                KeySummary::Optional("API key"),
                format!("{DIM}(optional API key not set){RESET}"),
            ),
            (KeySummary::NotSet, format!("{YELLOW}not set{RESET}")),
        ];
        for (summary, expected) in cases {
            assert_eq!(render_key_summary(summary.clone()), expected, "{summary:?}");
        }
    }
}
