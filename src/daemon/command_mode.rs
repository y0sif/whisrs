use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use audio_silence_gate::{AutoStopDetector, SILENCE_RMS_THRESHOLD};
use whisrs::audio::capture::{AudioCaptureHandle, SAMPLE_RATE};
use whisrs::audio::feedback;
use whisrs::llm;
use whisrs::state::{Action, StateMachine};
use whisrs::{Config, Response, State};

use crate::context::{CommandModeContext, DaemonContext, DaemonState, LlmCommandContext};
use crate::injection::{clear_line_via_keyboard, inject_text, is_terminal_class};
use crate::notify::{send_notification, truncate_preview};
use crate::pipeline::{
    format_api_error, format_no_microphone_error, save_history_entry, transcribe_batch_audio,
    BatchOptions,
};
use crate::selection::{acquire_selected_text, capture_selection};

/// Claim a command-session context (command mode or llm-command) when audio
/// collection ends, moving the state machine Recording → Transcribing.
///
/// Returns `None` — leaving the state machine *and* the context slot
/// untouched — when the session no longer exists: `handle_cancel` takes the
/// context and moves the machine to Idle under the same daemon-state lock
/// the caller holds here, so a missing context (or a machine that is not in
/// `Recording`) means the user threw this recording away. The caller must
/// then bail without transcribing, calling the LLM, injecting, or
/// transitioning — and without touching any other `DaemonState` field: by
/// the time a stale task gets the lock, those fields may already belong to
/// a session that started after the cancel. That is why this borrows the
/// slot instead of consuming a pre-taken value, and why
/// [`claim_command_mode_session`] / [`claim_llm_command_session`] gate every
/// neighbor take on a successful claim.
///
/// Ordering matters: transitioning *before* checking the context was issues
/// #81/#90 — from a post-cancel Idle, `Toggle` is a valid transition back
/// into Recording, so the dead session ran to completion and its final
/// `TranscriptionDone` (invalid from Recording) was swallowed, wedging the
/// daemon in Recording until restart. Claiming first closes the race: both
/// sides serialize on the daemon-state lock, so either cancel wins (context
/// gone → bail, machine stays Idle) or the background wins (machine moves to
/// Transcribing, from which `Cancel` is an invalid transition, so the
/// pipeline owns the state until its finalizer's `TranscriptionDone`).
fn claim_session<T>(state_machine: &mut StateMachine, ctx: &mut Option<T>) -> Option<T> {
    if ctx.is_none() || state_machine.state() != State::Recording {
        return None;
    }
    // From Recording, Toggle → Transcribing cannot fail.
    let _ = state_machine.transition(Action::Toggle);
    ctx.take()
}

/// The claim block of [`command_mode_background`]: claim the session via
/// [`claim_session`], then — only after the claim succeeded — take the
/// neighbor state that belongs to this recording (the capture handle, in
/// case a stop path didn't already drop it, e.g. the input device vanished
/// and the channel closed on its own).
///
/// The gating is the point: a stale background task can reach this after
/// `handle_cancel` discarded its session AND a new session has already
/// started. Taking `audio_capture` before knowing the claim succeeded would
/// strip the new session's live capture. On bail this mutates nothing.
fn claim_command_mode_session(ds: &mut DaemonState) -> Option<CommandModeContext> {
    let ctx = claim_session(&mut ds.state_machine, &mut ds.command_mode)?;
    ds.audio_capture.take(); // ensure capture is dropped
    Some(ctx)
}

/// The claim block of [`llm_command_background`]: claim the session via
/// [`claim_session`], then — only after the claim succeeded — take the
/// neighbor state that belongs to this recording (capture handle, source
/// window, start time).
///
/// Same gating as [`claim_command_mode_session`]: a stale background that
/// bails must not strip `audio_capture` / `recording_window_id` /
/// `recording_started_at` from a just-started new session.
fn claim_llm_command_session(
    ds: &mut DaemonState,
) -> Option<(
    LlmCommandContext,
    Option<String>,
    Option<std::time::Instant>,
)> {
    let ctx = claim_session(&mut ds.state_machine, &mut ds.llm_command)?;
    ds.audio_capture.take(); // ensure capture is dropped
    let window_id = ds.recording_window_id.take();
    let started_at = ds.recording_started_at.take();
    Some((ctx, window_id, started_at))
}

/// Command mode toggle: first call copies selection and starts recording,
/// second call stops recording and kicks off transcription → LLM → inject.
/// Also auto-stops on silence.
pub(crate) async fn handle_command_mode(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    let current_state = {
        let ds = daemon_state.lock().await;
        ds.state_machine.state()
    };

    match current_state {
        State::Recording => {
            // Second press: check if we're in command mode recording.
            let is_command_mode = {
                let ds = daemon_state.lock().await;
                ds.command_mode.is_some()
            };
            if !is_command_mode {
                return Response::Error {
                    message: "recording is active but not in command mode — use toggle or cancel"
                        .to_string(),
                };
            }
            // Stop recording — the background task will detect the channel close.
            let mut ds = daemon_state.lock().await;
            if let Some(mut capture) = ds.audio_capture.take() {
                capture.stop();
                tokio::task::spawn_blocking(move || drop(capture));
            }
            info!("command mode: manual stop");
            Response::Ok {
                state: State::Recording,
            }
        }
        State::Idle => {
            // First press: copy selection and start recording.
            command_mode_start(daemon_state, context).await
        }
        State::Transcribing => Response::Error {
            message: "cannot start command mode while transcribing".to_string(),
        },
        // Command mode is a recording flow — refused while read-aloud is active.
        State::Synthesizing | State::Speaking => Response::Error {
            message: "cannot start command mode while reading aloud — cancel read-aloud first"
                .to_string(),
        },
    }
}

/// Command mode first press: copy selection, start recording, spawn background processor.
async fn command_mode_start(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    // Get LLM config.
    let llm_config = context.config.llm.clone().unwrap_or_default();

    // Step 1: Capture the selected text. Command mode prefers the primary
    // selection (the X highlight, distinct from the Ctrl+C clipboard), which
    // needs no key simulation and leaves the clipboard untouched. When the
    // primary selection is empty it falls back to a simulated Ctrl+C, sharing
    // the same capture path as read-aloud, so command mode still works on apps
    // and compositors that don't populate the primary selection. The LLM result
    // is later injected through the same wrapper dictation uses: typed by
    // default, pasted when `[input] paste` is set. Either way it replaces the
    // active selection in GUI apps and lands at the prompt cursor in terminals.
    info!("command mode: getting selected text");
    let selected_text = match capture_selection(&context).await {
        Ok(text) => text,
        Err(message) => return Response::Error { message },
    };

    info!(
        "command mode: got {} chars of selected text",
        selected_text.len()
    );

    // Step 2: Start recording voice instruction.
    if context.config.general.audio_feedback {
        feedback::play_start(context.config.general.audio_feedback_volume);
    }

    let mut capture =
        match AudioCaptureHandle::start_with_level_tx(context.overlay_level_tx.clone()) {
            Ok(c) => c,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to start audio capture: {e}"),
                };
            }
        };

    let audio_rx = capture.take_receiver();

    // Store state.
    {
        let mut ds = daemon_state.lock().await;
        if let Err(e) = ds.state_machine.transition(Action::Toggle) {
            return Response::Error {
                message: format!("state transition failed: {e}"),
            };
        }
        ds.audio_capture = Some(capture);
        ds.recording_started_at = Some(std::time::Instant::now());
        ds.command_mode = Some(CommandModeContext {
            selected_text,
            llm_config,
        });
    }

    if context.notify_state() {
        send_notification(
            "whisrs",
            "Command mode: speak your instruction... (press again to stop)",
        );
    }

    // Spawn background task: collect audio (with auto-stop), then process.
    let ds_ref = Arc::clone(&daemon_state);
    let ctx = Arc::clone(&context);
    tokio::spawn(async move {
        command_mode_background(audio_rx, ds_ref, ctx).await;
    });

    Response::Ok {
        state: State::Recording,
    }
}

/// Background task: collects audio until channel closes (manual stop or auto-stop),
/// then transcribes the instruction, sends to LLM, and injects the result.
///
/// Thin wrapper: collects the recording, claims the session context via
/// [`claim_session`] (bailing out if `whisrs cancel` already discarded the
/// session), runs the fallible pipeline in [`command_mode_background_inner`],
/// then funnels every surviving outcome through one finalize block.
async fn command_mode_background(
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>>,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) {
    let silence_timeout = context.config.general.silence_timeout_ms;
    let mut auto_stop = AutoStopDetector::new(SILENCE_RMS_THRESHOLD, silence_timeout, SAMPLE_RATE);
    let mut all_samples: Vec<i16> = Vec::new();

    // Collect audio until silence auto-stop or channel close (manual stop).
    if let Some(mut rx) = audio_rx {
        while let Some(chunk) = rx.recv().await {
            all_samples.extend_from_slice(&chunk);
            if auto_stop.feed(&chunk) {
                info!("command mode: silence auto-stop");
                // Stop capture.
                let mut ds = daemon_state.lock().await;
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                break;
            }
        }
    }

    // Claim the session context and enter Transcribing. `handle_cancel`
    // takes the context under this same lock, so whoever locks first wins:
    // if cancel got there first the context is gone and the machine is
    // already Idle — bail below without transcribing, calling the LLM,
    // injecting, or transitioning (issues #81/#90). Every neighbor take
    // (the capture handle) happens inside the helper, gated on a successful
    // claim, so a bail cannot strip state from a session that started after
    // the cancel. The lock is released before the slow transcribe/LLM
    // awaits in the inner fn.
    let cmd_ctx = {
        let mut ds = daemon_state.lock().await;
        claim_command_mode_session(&mut ds)
    };

    let Some(cmd_ctx) = cmd_ctx else {
        // Cancelled: `handle_cancel` already moved the machine to Idle (its
        // dispatcher broadcasts the state and it resets the overlay level),
        // so there is nothing to finalize — the recording is discarded.
        info!("command mode: session cancelled — discarding recording");
        return;
    };

    if let Err(e) = command_mode_background_inner(&all_samples, cmd_ctx, &context).await {
        let friendly = format_api_error(&e);
        error!("command mode: {e:#}");
        if context.notify_error() {
            send_notification("whisrs", &format!("Command failed: {friendly}"));
        }
    }

    // Single finalize path — success, benign skip, and error all land here.
    // Deliberate improvement over the old per-site unwinds: the state
    // broadcast + overlay reset now run on error paths too, so the tray can
    // no longer be left showing a stale state after a failure.
    // `claim_session` put the machine in Transcribing and no other action is
    // valid from there, so this TranscriptionDone cannot fail.
    let mut ds = daemon_state.lock().await;
    let _ = ds.state_machine.transition(Action::TranscriptionDone);
    let _ = context.state_tx.send(ds.state_machine.state());
    if let Some(level_tx) = &context.overlay_level_tx {
        let _ = level_tx.send(0.0);
    }
}

/// Fallible body of [`command_mode_background`]: stop-feedback, gate +
/// transcribe (via [`transcribe_batch_audio`]), LLM rewrite, inject.
///
/// Early `Ok(())` returns are benign skips whose toast the helper already
/// sent; `Err` is a real failure the outer wrapper formats
/// (`format_api_error`) and reports. Takes no daemon-state lock, so nothing
/// is held across the transcribe/LLM awaits.
async fn command_mode_background_inner(
    all_samples: &[i16],
    cmd_ctx: CommandModeContext,
    context: &DaemonContext,
) -> Result<()> {
    if context.config.general.audio_feedback {
        feedback::play_stop(context.config.general.audio_feedback_volume);
    }

    let instruction = transcribe_batch_audio(
        all_samples,
        context,
        &context.config.general.language,
        &BatchOptions::command_mode(),
    )
    .await?;

    if instruction.is_empty() {
        // Gated or unintelligible — the helper already toasted.
        return Ok(());
    }

    info!("command mode: instruction = {:?}", instruction);

    // Send to LLM.
    let result =
        llm::rewrite_text(&cmd_ctx.llm_config, &cmd_ctx.selected_text, &instruction).await?;

    // Inject the result at the cursor through the same policy wrapper the
    // dictation path uses. By default that means typing keystrokes through
    // the evdev / Wayland virtual-keyboard pipeline: the AltGr / dead-key
    // work in xkb-type covers accented and non-ASCII output end-to-end,
    // including in terminals where Ctrl+V is interpreted as a control
    // character rather than paste, so the result never touches the clipboard.
    // (The capture side may still fall back to a simulated Ctrl+C when the
    // primary selection is empty; see `capture_selection`.)
    //
    // `[input] paste = true` switches command mode to clipboard paste for the
    // same reason it does for dictation: on compositors without the Wayland
    // virtual-keyboard protocol (e.g. KWin) uinput keycodes are decoded
    // through the target window's active XKB layout and can come out garbled,
    // and the clipboard is layout-independent. `is_terminal` only picks
    // Ctrl+Shift+V over Ctrl+V there; nothing else keys off it. It can only
    // ever be true under Hyprland and Niri: `get_focused_window_class()`
    // defaults to `None` for every other tracker (KWin, GNOME, Sway, X11),
    // so terminals get plain Ctrl+V there. Same gap as the dictation path
    // above; tracked in issues #70 and #71.
    //
    // GUI apps: typing (or pasting) while text is selected replaces the
    // selection. That is text-widget behavior in GTK, Qt and Electron, and it
    // holds on Wayland as well, so nothing extra is needed there.
    //
    // Terminals: a mouse highlight is a visual overlay, not an editable
    // selection, so injecting at the cursor would *append* to the line that is
    // already at the prompt. We clear it first with Ctrl+A / Ctrl+K, then
    // inject, so the result replaces the highlighted command instead of being
    // tacked onto it. The clear is best-effort: if it fails we still inject,
    // because appending the LLM result beats losing it.
    //
    // `is_terminal` can only ever be true where
    // `WindowTracker::get_focused_window_class()` is actually implemented,
    // which is Hyprland (`src/window/hyprland.rs:59`) and Niri
    // (`src/window/niri.rs:79`). `src/window/mod.rs:23` defaults it to `None`,
    // so on KWin, GNOME, Sway and X11 it stays false and terminals there fall
    // back to plain injection at the cursor (and to plain Ctrl+V on the paste
    // branch). Tracked in issues #70 and #71; not fixed here.
    info!("command mode: injecting {} chars", result.len());
    let text_clone = result.clone();
    let key_delay = std::time::Duration::from_millis(context.config.input.key_delay_ms);
    let injector_backend = context.config.input.backend;
    let paste = context.config.input.paste;
    let is_terminal = context
        .window_tracker
        .get_focused_window_class()
        .map(|c| is_terminal_class(&c, &context.config.input.terminal_classes))
        .unwrap_or(false);
    match tokio::task::spawn_blocking(move || {
        if is_terminal {
            if let Err(e) = clear_line_via_keyboard(key_delay, injector_backend) {
                warn!("command mode: failed to clear terminal line, injecting anyway: {e:#}");
            }
        }
        inject_text(&text_clone, is_terminal, key_delay, injector_backend, paste)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("command mode: failed to inject text: {e:#}"),
        Err(e) => warn!("command mode: injection task panicked: {e}"),
    }

    if context.config.general.audio_feedback {
        feedback::play_done(context.config.general.audio_feedback_volume);
    }
    if context.notify_state() {
        send_notification("whisrs", &format!("Command applied: {instruction}"));
    }

    Ok(())
}

/// Handle a named `[[llm_commands]]` hotkey. A toggle-recording flavor of
/// plain dictation: first press starts recording, second press stops it and
/// runs the transcribed text through the command's fixed instruction via the
/// LLM before typing the result at the cursor. Unlike command mode, there's
/// no selection or clipboard involved.
pub(crate) async fn handle_llm_command(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    name: String,
) -> Response {
    let current_state = {
        let ds = daemon_state.lock().await;
        ds.state_machine.state()
    };

    match current_state {
        State::Recording => {
            // Second press: only stop if an llm-command session is active —
            // mirrors handle_command_mode's own-flag check.
            let is_llm_command = {
                let ds = daemon_state.lock().await;
                ds.llm_command.is_some()
            };
            if !is_llm_command {
                return Response::Error {
                    message: "recording is active but not for an llm-command — use toggle, \
                              command, or cancel"
                        .to_string(),
                };
            }
            let mut ds = daemon_state.lock().await;
            if let Some(mut capture) = ds.audio_capture.take() {
                capture.stop();
                tokio::task::spawn_blocking(move || drop(capture));
            }
            info!("llm-command '{name}': manual stop");
            Response::Ok {
                state: State::Recording,
            }
        }
        State::Idle => {
            let entry = context
                .config
                .llm_commands
                .iter()
                .find(|e| e.name == name)
                .cloned();
            match entry {
                Some(entry) => llm_command_start(daemon_state, context, entry).await,
                None => Response::Error {
                    message: format!("no llm_commands entry named '{name}' — check config.toml"),
                },
            }
        }
        State::Transcribing => Response::Error {
            message: "cannot start llm-command while transcribing".to_string(),
        },
        State::Synthesizing | State::Speaking => Response::Error {
            message: "cannot start llm-command while reading aloud — cancel read-aloud first"
                .to_string(),
        },
    }
}

/// Reprogram a named LLM command from the current selection (its `set_hotkey`).
///
/// Synchronous: no recording, no LLM, no typing. Captures the highlighted text
/// and stores it as the command's new instruction — applied live via the
/// daemon override map and persisted to `config.toml` for the next start.
pub(crate) async fn handle_set_llm_instruction(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    name: String,
) -> Response {
    // Only when idle — don't interfere with an active recording / read-aloud.
    let state = {
        let ds = daemon_state.lock().await;
        ds.state_machine.state()
    };
    if state != State::Idle {
        return Response::Error {
            message: format!("cannot set an instruction while {state:?} — finish or cancel first"),
        };
    }

    // The command must exist (its hotkey was registered from config).
    if !context.config.llm_commands.iter().any(|e| e.name == name) {
        return Response::Error {
            message: format!("no llm_commands entry named '{name}' — check config.toml"),
        };
    }

    let Some(instruction) = acquire_selected_text(&context).await else {
        // Not gated by `notify_state()`: this path never records, so the
        // overlay (which would otherwise justify suppressing toasts) never
        // shows — without a toast there'd be no feedback at all.
        if context.notify {
            send_notification(
                "whisrs",
                &format!("'{name}': select the instruction text first"),
            );
        }
        return Response::Ok { state: State::Idle };
    };

    {
        let mut ds = daemon_state.lock().await;
        ds.llm_instruction_overrides
            .insert(name.clone(), instruction.clone());
    }

    if let Err(e) = persist_llm_instruction(&name, &instruction) {
        warn!("llm-command '{name}': failed to persist new instruction to config: {e:#}");
    }

    info!(
        "llm-command '{name}': instruction reprogrammed ({} chars)",
        instruction.len()
    );
    // Audible + toast confirmation. Not gated by `notify_state()` (see above):
    // the set path shows no overlay, so this is the only feedback the user gets.
    if context.config.general.audio_feedback {
        feedback::play_done(context.config.general.audio_feedback_volume);
    }
    if context.notify {
        let preview = truncate_preview(&instruction, 77);
        send_notification("whisrs", &format!("'{name}' set to: {preview}"));
    }

    Response::Ok { state: State::Idle }
}

/// Persist a reprogrammed instruction to `config.toml`: re-read the on-disk
/// config, update the matching entry, write it back. Best-effort — the
/// in-memory override already applies to the running daemon.
fn persist_llm_instruction(name: &str, instruction: &str) -> anyhow::Result<()> {
    let path = whisrs::config_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut config: Config =
        toml::from_str(&contents).context("failed to parse config for instruction update")?;
    let entry = config
        .llm_commands
        .iter_mut()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("entry '{name}' not present in config file"))?;
    entry.instruction = instruction.to_string();
    whisrs::config::setup::write_config(&config)?;
    Ok(())
}

/// Named LLM command, first press: capture the focused window, start
/// recording, spawn the background processor. Mirrors `handle_toggle`'s Idle
/// branch (batch path only — this feature doesn't support streaming
/// backends, same as command mode).
async fn llm_command_start(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    entry: llm::LlmCommandConfig,
) -> Response {
    let llm_config = context.config.llm.clone().unwrap_or_default();

    // Capture focused window before recording, like plain dictation — the
    // result is typed at the cursor, not pasted over a selection.
    let window_id = match context.window_tracker.get_focused_window() {
        Ok(id) => Some(id),
        Err(e) => {
            warn!("failed to capture focused window: {e}");
            None
        }
    };

    if context.config.general.audio_feedback {
        feedback::play_start(context.config.general.audio_feedback_volume);
    }

    let mut capture =
        match AudioCaptureHandle::start_with_level_tx(context.overlay_level_tx.clone()) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{e}");
                let friendly = if msg.contains("no default audio input device") {
                    format_no_microphone_error()
                } else {
                    format!("Failed to start audio capture: {e}")
                };
                error!("{friendly}");
                return Response::Error { message: friendly };
            }
        };

    let audio_rx = capture.take_receiver();

    {
        let mut ds = daemon_state.lock().await;
        if let Err(e) = ds.state_machine.transition(Action::Toggle) {
            return Response::Error {
                message: format!("state transition failed: {e}"),
            };
        }
        ds.audio_capture = Some(capture);
        ds.recording_window_id = window_id;
        ds.recording_started_at = Some(std::time::Instant::now());
        // Prefer a runtime override set via `set_hotkey`; else the configured
        // instruction.
        let instruction = ds
            .llm_instruction_overrides
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| entry.instruction.clone());
        ds.llm_command = Some(LlmCommandContext {
            name: entry.name.clone(),
            instruction,
            llm_config,
        });
    }

    if context.notify_state() {
        send_notification(
            "whisrs",
            &format!("Recording for '{}'... (press again to stop)", entry.name),
        );
    }

    let ds_ref = Arc::clone(&daemon_state);
    let ctx = Arc::clone(&context);
    tokio::spawn(async move {
        llm_command_background(audio_rx, ds_ref, ctx).await;
    });

    Response::Ok {
        state: State::Recording,
    }
}

/// Background task: collects audio until channel closes (manual stop or
/// auto-stop), transcribes it, applies the command's fixed instruction via
/// the LLM (reusing the same `llm::rewrite_text` call as command mode, just
/// with the roles swapped — the dictated text is the "selected text" and the
/// preset instruction is the "voice instruction"), and types the result at
/// the cursor.
///
/// Thin wrapper: collects the recording, claims the session context via
/// [`claim_session`] (bailing out if `whisrs cancel` already discarded the
/// session), runs the fallible pipeline in [`llm_command_background_inner`],
/// then funnels every surviving outcome through one finalize block.
async fn llm_command_background(
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>>,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) {
    let silence_timeout = context.config.general.silence_timeout_ms;
    let mut auto_stop = AutoStopDetector::new(SILENCE_RMS_THRESHOLD, silence_timeout, SAMPLE_RATE);
    let mut all_samples: Vec<i16> = Vec::new();

    if let Some(mut rx) = audio_rx {
        while let Some(chunk) = rx.recv().await {
            all_samples.extend_from_slice(&chunk);
            if auto_stop.feed(&chunk) {
                info!("llm-command: silence auto-stop");
                let mut ds = daemon_state.lock().await;
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                break;
            }
        }
    }

    // Claim the session context and enter Transcribing. `handle_cancel`
    // takes the context under this same lock, so whoever locks first wins:
    // if cancel got there first the context is gone and the machine is
    // already Idle — bail below without transcribing, calling the LLM,
    // injecting, or transitioning (issues #81/#90). Every neighbor take
    // (capture handle, source window, start time) happens inside the
    // helper, gated on a successful claim, so a bail cannot strip state
    // from a session that started after the cancel. The lock is released
    // before the slow transcribe/LLM awaits in the inner fn.
    let claimed = {
        let mut ds = daemon_state.lock().await;
        claim_llm_command_session(&mut ds)
    };

    let Some((cmd_ctx, window_id, recording_started_at)) = claimed else {
        // Cancelled: `handle_cancel` already moved the machine to Idle (its
        // dispatcher broadcasts the state and it resets the overlay level),
        // so there is nothing to finalize — the recording is discarded.
        info!("llm-command: session cancelled — discarding recording");
        return;
    };

    let name = cmd_ctx.name.clone();
    if let Err(e) = llm_command_background_inner(
        &all_samples,
        cmd_ctx,
        window_id,
        recording_started_at,
        &context,
    )
    .await
    {
        let friendly = format_api_error(&e);
        error!("llm-command '{name}': {e:#}");
        if context.notify_error() {
            send_notification("whisrs", &format!("'{name}' failed: {friendly}"));
        }
    }

    // Single finalize path — success, benign skip, and error all land here.
    // As in `command_mode_background`, the state broadcast + overlay reset
    // now deliberately run on error paths too (they used to be success-only),
    // so the tray can no longer be left showing a stale state after a failure.
    // `claim_session` put the machine in Transcribing and no other action is
    // valid from there, so this TranscriptionDone cannot fail.
    let mut ds = daemon_state.lock().await;
    let _ = ds.state_machine.transition(Action::TranscriptionDone);
    let _ = context.state_tx.send(ds.state_machine.state());
    if let Some(level_tx) = &context.overlay_level_tx {
        let _ = level_tx.send(0.0);
    }
}

/// Fallible body of [`llm_command_background`]: stop-feedback, gate +
/// transcribe (via [`transcribe_batch_audio`]), LLM rewrite, inject, history.
///
/// Early `Ok(())` returns are benign skips that already surfaced their own
/// toast; `Err` is a real failure the outer wrapper formats
/// (`format_api_error`) and reports. Takes no daemon-state lock, so nothing
/// is held across the transcribe/LLM awaits.
async fn llm_command_background_inner(
    all_samples: &[i16],
    cmd_ctx: LlmCommandContext,
    window_id: Option<String>,
    recording_started_at: Option<std::time::Instant>,
    context: &DaemonContext,
) -> Result<()> {
    if context.config.general.audio_feedback {
        feedback::play_stop(context.config.general.audio_feedback_volume);
    }

    let text = transcribe_batch_audio(
        all_samples,
        context,
        &context.config.general.language,
        &BatchOptions::llm_command(&cmd_ctx.name),
    )
    .await?;

    if text.is_empty() {
        // Gated, echo-dropped, or unintelligible — the helper already toasted.
        return Ok(());
    }

    info!(
        "llm-command '{}': transcribed {} chars",
        cmd_ctx.name,
        text.len()
    );

    let result = llm::rewrite_text(&cmd_ctx.llm_config, &text, &cmd_ctx.instruction).await?;

    if result.is_empty() {
        if context.notify_error() {
            send_notification(
                "whisrs",
                &format!("'{}': LLM returned empty text", cmd_ctx.name),
            );
        }
        return Ok(());
    }

    // Restore window focus, then inject the result at the cursor — keystrokes,
    // or clipboard paste when `[input] paste = true` (layout-independent).
    if let Some(wid) = &window_id {
        if let Err(e) = context.window_tracker.focus_window(wid) {
            warn!("failed to restore window focus: {e}");
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    let result_clone = result.clone();
    let key_delay = std::time::Duration::from_millis(context.config.input.key_delay_ms);
    let injector_backend = context.config.input.backend;
    let paste = context.config.input.paste;
    let is_terminal = if paste {
        context
            .window_tracker
            .get_focused_window_class()
            .map(|c| is_terminal_class(&c, &context.config.input.terminal_classes))
            .unwrap_or(false)
    } else {
        false
    };
    match tokio::task::spawn_blocking(move || {
        inject_text(
            &result_clone,
            is_terminal,
            key_delay,
            injector_backend,
            paste,
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(
            "llm-command '{}': failed to inject text: {e:#}",
            cmd_ctx.name
        ),
        Err(e) => warn!(
            "llm-command '{}': failed to join injection task: {e}",
            cmd_ctx.name
        ),
    }

    let duration_secs = recording_started_at
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    save_history_entry(
        &result,
        &format!("llm:{}", cmd_ctx.name),
        &context.config.general.language,
        duration_secs,
    );

    if context.config.general.audio_feedback {
        feedback::play_done(context.config.general.audio_feedback_volume);
    }
    if context.notify_state() {
        let preview = truncate_preview(&result, 77);
        send_notification("whisrs", &format!("'{}' done: {preview}", cmd_ctx.name));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_machine() -> StateMachine {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm
    }

    /// Normal stop: the context survived, so the session is claimed (the
    /// slot is consumed) and the machine moves to Transcribing — from which
    /// the finalizer's TranscriptionDone must succeed.
    #[test]
    fn claim_session_moves_recording_to_transcribing() {
        let mut sm = recording_machine();
        let mut ctx = Some("ctx");
        assert_eq!(claim_session(&mut sm, &mut ctx), Some("ctx"));
        assert_eq!(ctx, None, "a successful claim consumes the context slot");
        assert_eq!(sm.state(), State::Transcribing);
        assert_eq!(
            sm.transition(Action::TranscriptionDone).unwrap(),
            State::Idle
        );
    }

    /// Cancel won the race: `handle_cancel` took the context and moved the
    /// machine to Idle before the background task got the lock. The claim
    /// must bail and leave Idle untouched — the old blind Toggle re-entered
    /// Recording here and wedged the daemon (issues #81/#90).
    #[test]
    fn claim_session_bails_after_cancel_without_touching_idle() {
        let mut sm = StateMachine::new(); // post-cancel: Idle
        let mut ctx: Option<&str> = None;
        assert_eq!(claim_session(&mut sm, &mut ctx), None);
        assert_eq!(sm.state(), State::Idle);
    }

    /// Defence in depth: even if a context somehow survived, a machine that
    /// is not in Recording is never transitioned — the pipeline bails rather
    /// than wedging, and the bail leaves the context slot in place (a bail
    /// must not mutate anything).
    #[test]
    fn claim_session_bails_when_not_recording_even_with_context() {
        let mut sm = StateMachine::new(); // Idle
        let mut ctx = Some("ctx");
        assert_eq!(claim_session(&mut sm, &mut ctx), None);
        assert_eq!(sm.state(), State::Idle);
        assert_eq!(ctx, Some("ctx"), "a bail must not consume the context slot");
    }

    /// The full cancelled-session sequence at the state-machine level:
    /// cancel puts the machine in Idle, the woken background task claims
    /// nothing, and a hypothetical late TranscriptionDone is rejected
    /// without leaving Idle. This is the exact sequence from issue #81's
    /// repro (`llm-command german` → `cancel`).
    #[test]
    fn cancelled_session_cannot_wedge_the_machine() {
        let mut sm = recording_machine();
        sm.transition(Action::Cancel).unwrap(); // handle_cancel → Idle
        let mut ctx: Option<()> = None;
        assert_eq!(claim_session(&mut sm, &mut ctx), None);
        assert!(sm.transition(Action::TranscriptionDone).is_err());
        assert_eq!(sm.state(), State::Idle);
    }

    /// User cancels a command session, then starts a new plain dictation
    /// before the stale background task wakes. The stale task finds its
    /// context gone and must not disturb the new session's Recording state.
    #[test]
    fn stale_background_does_not_disturb_a_new_dictation() {
        let mut sm = recording_machine();
        sm.transition(Action::Cancel).unwrap(); // → Idle
        sm.transition(Action::Toggle).unwrap(); // new dictation → Recording
        let mut ctx: Option<()> = None;
        assert_eq!(claim_session(&mut sm, &mut ctx), None);
        assert_eq!(sm.state(), State::Recording);
    }

    /// Build a `DaemonState` in the residual-steal-window scenario: an
    /// llm-command session was cancelled (`handle_cancel` took the context
    /// and capture, machine → Idle), then a NEW plain dictation started and
    /// repopulated the per-session fields before the stale background task
    /// got the lock.
    fn state_with_new_dictation_after_cancel() -> DaemonState {
        let mut ds = DaemonState::new();

        // Old session: recording, then `whisrs cancel` discards it.
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.state_machine.transition(Action::Cancel).unwrap();
        // (`discard_recording_session` left command_mode / llm_command /
        // recording_* all None — DaemonState::new() starts that way.)

        // New dictation starts before the stale task wakes.
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.recording_window_id = Some("0xnew".to_string());
        ds.recording_started_at = Some(std::time::Instant::now());
        ds.session_language = Some("pl".to_string());
        // In real life `audio_capture` now holds the new session's live
        // handle; it needs a real input device, so it stays `None` here.
        // The claim helpers gate its take behind the same success check as
        // the fields asserted below.
        ds
    }

    /// The residual steal window from the #81/#90 review: a stale
    /// llm-command background bails (its context is gone) while a new
    /// session's `recording_window_id` / `recording_started_at` are already
    /// populated. The old claim block took those fields BEFORE checking the
    /// claim, stripping them from the new session. The bail must leave
    /// every field untouched and the machine in Recording.
    #[test]
    fn stale_llm_command_bail_leaves_new_session_state_untouched() {
        let mut ds = state_with_new_dictation_after_cancel();
        let started_at = ds.recording_started_at;

        assert!(claim_llm_command_session(&mut ds).is_none());

        assert_eq!(ds.state_machine.state(), State::Recording);
        assert_eq!(ds.recording_window_id.as_deref(), Some("0xnew"));
        assert_eq!(ds.recording_started_at, started_at);
        assert_eq!(ds.session_language.as_deref(), Some("pl"));
        assert!(ds.audio_capture.is_none());
    }

    /// Same steal-window scenario for the command-mode claim block: the
    /// stale bail must not touch the new session's fields (its old take of
    /// `audio_capture` before the claim check would have dropped the new
    /// session's live capture).
    #[test]
    fn stale_command_mode_bail_leaves_new_session_state_untouched() {
        let mut ds = state_with_new_dictation_after_cancel();
        let started_at = ds.recording_started_at;

        assert!(claim_command_mode_session(&mut ds).is_none());

        assert_eq!(ds.state_machine.state(), State::Recording);
        assert_eq!(ds.recording_window_id.as_deref(), Some("0xnew"));
        assert_eq!(ds.recording_started_at, started_at);
        assert_eq!(ds.session_language.as_deref(), Some("pl"));
        assert!(ds.audio_capture.is_none());
    }

    /// The success half of the llm-command claim helper: an uncancelled
    /// stop claims the context, hands back this session's window id and
    /// start time, clears those fields, and enters Transcribing.
    #[test]
    fn llm_command_claim_takes_own_session_state_on_success() {
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.llm_command = Some(LlmCommandContext {
            name: "german".to_string(),
            instruction: "translate to German".to_string(),
            llm_config: Default::default(),
        });
        ds.recording_window_id = Some("0xmine".to_string());
        ds.recording_started_at = Some(std::time::Instant::now());

        let (ctx, window_id, started_at) =
            claim_llm_command_session(&mut ds).expect("live session must be claimed");

        assert_eq!(ctx.name, "german");
        assert_eq!(window_id.as_deref(), Some("0xmine"));
        assert!(started_at.is_some());
        assert_eq!(ds.state_machine.state(), State::Transcribing);
        assert!(ds.llm_command.is_none());
        assert!(ds.recording_window_id.is_none());
        assert!(ds.recording_started_at.is_none());
    }

    /// A plain `whisrs toggle` on a command-mode / llm-command recording:
    /// handle_toggle's stop arm takes the session over as plain dictation
    /// and must clear BOTH command slots under its lock (via
    /// `discard_command_sessions`). The background task's claim then bails
    /// without consuming anything — its bail is mutation-free by design —
    /// so without the stop-arm clear the stale `Some` slot would satisfy
    /// the second-press gates above during a later unrelated dictation and
    /// silently steal its capture.
    #[test]
    fn toggle_stop_clears_command_slots_so_stale_claim_bails() {
        let mut ds = DaemonState::new();

        // Command session recording. Both slots filled to pin that the
        // stop arm clears both (in real life only one is ever Some).
        ds.state_machine.transition(Action::Toggle).unwrap(); // → Recording
        ds.command_mode = Some(CommandModeContext {
            selected_text: "selected".to_string(),
            llm_config: Default::default(),
        });
        ds.llm_command = Some(LlmCommandContext {
            name: "german".to_string(),
            instruction: "translate to German".to_string(),
            llm_config: Default::default(),
        });

        // Plain toggle-stop: Recording → Transcribing plus the takeover
        // clear — the two steps handle_toggle's stop arm runs under one
        // lock.
        ds.state_machine.transition(Action::Toggle).unwrap();
        crate::dictation::discard_command_sessions(&mut ds, "toggle-stop");

        assert!(
            ds.command_mode.is_none(),
            "toggle-stop must clear command_mode"
        );
        assert!(
            ds.llm_command.is_none(),
            "toggle-stop must clear llm_command"
        );

        // The command background wakes later (its audio channel closed on
        // the toggle-stop's capture take) and must bail cleanly: slot gone,
        // machine no longer in Recording, nothing mutated.
        assert!(claim_command_mode_session(&mut ds).is_none());
        assert!(claim_llm_command_session(&mut ds).is_none());
        assert_eq!(ds.state_machine.state(), State::Transcribing);
    }
}
