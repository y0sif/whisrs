use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::feedback;
use whisrs::state::Action;
use whisrs::{validate_language_override, Response, State};

use crate::context::{DaemonContext, DaemonState};
use crate::notify::{send_notification, truncate_preview};
use crate::pipeline::{
    build_transcription_config, format_no_microphone_error, process_recording_batch,
    run_streaming_pipeline, save_history_entry,
};

pub(crate) async fn handle_toggle(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    language: Option<String>,
) -> Response {
    let mut ds = daemon_state.lock().await;
    let current_state = ds.state_machine.state();

    match current_state {
        State::Idle => {
            // Fail fast on a bad `-l` override before any capture or state
            // transition — otherwise the user records a whole session and
            // only finds out when the backend rejects the language.
            let language = match validate_toggle_language(language) {
                Ok(lang) => lang,
                Err(response) => return response,
            };

            // Resolve the session language once: per-toggle override, else
            // config default. Persisted in `DaemonState` below so the
            // stop-toggle can apply it to the batch path and history too.
            let session_language = resolve_language(language, &context.config.general.language);

            // Capture focused window before recording.
            let window_id = match context.window_tracker.get_focused_window() {
                Ok(id) => {
                    info!("captured source window: {id}");
                    Some(id)
                }
                Err(e) => {
                    warn!("failed to capture focused window: {e}");
                    None
                }
            };

            // Start recording.
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

            // For streaming backends: start the streaming pipeline immediately.
            // Audio flows in real-time from microphone → API → text at cursor.
            if context.transcription_backend.supports_streaming() {
                let audio_rx = match capture.take_receiver() {
                    Some(rx) => rx,
                    None => {
                        return Response::Error {
                            message: "failed to get audio receiver".to_string(),
                        }
                    }
                };

                let config = build_transcription_config(&context.config, &session_language);

                let backend = Arc::clone(&context.transcription_backend);
                let wid = window_id.clone();
                let ctx_notify = context.notify;
                let ctx_overlay = context.overlay_enabled;
                let window_tracker_for_pipeline = Arc::clone(&context.window_tracker);
                // Restore focus before starting the pipeline.
                let wid_for_focus = wid.clone();

                // Spawn a task to:
                // 1. Run auto-stop detection + forward audio to transcription
                // 2. Run transcription backend
                // 3. Type text as it arrives
                let silence_timeout = context.config.general.silence_timeout_ms;
                let ds_ref = Arc::clone(&daemon_state);
                let filler_enabled = context.config.general.remove_filler_words;
                let filler_words = context.config.general.filler_words.clone();
                let pipeline_audio_feedback = context.config.general.audio_feedback;
                let pipeline_audio_volume = context.config.general.audio_feedback_volume;

                let pipeline_backend_name = context.config.general.backend.clone();
                let pipeline_language = session_language.clone();
                let pipeline_state_tx = context.state_tx.clone();
                let pipeline_key_delay =
                    std::time::Duration::from_millis(context.config.input.key_delay_ms);
                let pipeline_injector_backend = context.config.input.backend;

                // Per-recording cancel flag: lets `handle_cancel` stop the
                // typing loop, which keeps running detached when the outer
                // pipeline future is aborted.
                let cancel_flag = Arc::new(AtomicBool::new(false));
                let pipeline_cancel = Arc::clone(&cancel_flag);

                let task = tokio::spawn(async move {
                    run_streaming_pipeline(
                        audio_rx,
                        backend,
                        config,
                        wid,
                        ctx_notify,
                        ctx_overlay,
                        silence_timeout,
                        ds_ref,
                        window_tracker_for_pipeline,
                        filler_enabled,
                        filler_words,
                        pipeline_audio_feedback,
                        pipeline_audio_volume,
                        pipeline_backend_name,
                        pipeline_language,
                        pipeline_state_tx,
                        pipeline_key_delay,
                        pipeline_injector_backend,
                        pipeline_cancel,
                    )
                    .await
                });

                ds.streaming_task = Some(task);
                ds.streaming_cancel = Some(cancel_flag);

                // Focus the window now (so text goes to the right place from the start).
                if let Some(wid) = &wid_for_focus {
                    if let Err(e) = context.window_tracker.focus_window(wid) {
                        warn!("failed to pre-focus window: {e}");
                    }
                }
            }

            ds.audio_capture = Some(capture);
            ds.recording_window_id = window_id;
            ds.recording_started_at = Some(std::time::Instant::now());
            ds.session_language = Some(session_language);

            match ds.state_machine.transition(Action::Toggle) {
                Ok(new_state) => {
                    info!("started recording");
                    // Broadcast recording state for tray.
                    let _ = context.state_tx.send(new_state);
                    if context.config.general.audio_feedback {
                        feedback::play_start(context.config.general.audio_feedback_volume);
                    }
                    if context.notify_state() {
                        send_notification("whisrs", "Recording...");
                    }
                    Response::Ok { state: new_state }
                }
                Err(e) => {
                    ds.audio_capture = None;
                    ds.recording_window_id = None;
                    ds.streaming_task = None;
                    ds.streaming_cancel = None;
                    ds.session_language = None;
                    Response::Error {
                        message: e.to_string(),
                    }
                }
            }
        }
        State::Recording => {
            // Stop recording.
            match ds.state_machine.transition(Action::Toggle) {
                Ok(_) => {
                    info!("stopped recording, transitioning to transcribing");
                    // Broadcast transcribing state for tray.
                    let _ = context.state_tx.send(State::Transcribing);
                    if let Some(level_tx) = &context.overlay_level_tx {
                        let _ = level_tx.send(0.0);
                    }
                    if context.config.general.audio_feedback {
                        feedback::play_stop(context.config.general.audio_feedback_volume);
                    }
                    if context.notify_state() {
                        send_notification("whisrs", "Transcribing...");
                    }

                    let capture = ds.audio_capture.take();
                    let window_id = ds.recording_window_id.take();
                    let streaming_task = ds.streaming_task.take();
                    // Normal stop: drop the cancel flag untriggered so the
                    // pipeline drains and types the remaining text.
                    ds.streaming_cancel = None;
                    let recording_started_at = ds.recording_started_at.take();
                    // The language this session was started with. Consuming it
                    // here ends the language session.
                    let session_language =
                        ds.take_session_language(&context.config.general.language);

                    // A `-l` on the stop-press comes too late to change the
                    // session — tell the user instead of silently dropping it.
                    if let Some(requested) = &language {
                        if *requested != session_language {
                            warn!(
                                "language override '{requested}' ignored — this session was \
                                 started with '{session_language}'; pass -l on the toggle that \
                                 starts recording"
                            );
                        }
                    }

                    // Release lock before slow operations.
                    drop(ds);

                    let result = if let Some(task) = streaming_task {
                        // Streaming path: stop capture to close the channel,
                        // then wait for the pipeline to drain and finish.
                        if let Some(mut cap) = capture {
                            cap.stop();
                            tokio::task::spawn_blocking(move || drop(cap));
                        }
                        match task.await {
                            Ok(Ok(text)) => Ok(text),
                            Ok(Err(e)) => Err(e),
                            Err(e) => Err(anyhow::anyhow!("streaming task panicked: {e}")),
                        }
                    } else {
                        // Batch path: collect all audio, then transcribe with
                        // the session language (not the config default).
                        process_recording_batch(
                            capture,
                            window_id.as_deref(),
                            &context,
                            &session_language,
                        )
                        .await
                    };

                    // Transition back to Idle.
                    let duration_secs = recording_started_at
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0);
                    let mut ds = daemon_state.lock().await;
                    match ds.state_machine.transition(Action::TranscriptionDone) {
                        Ok(new_state) => {
                            // Broadcast idle state for tray.
                            let _ = context.state_tx.send(new_state);
                            if let Some(level_tx) = &context.overlay_level_tx {
                                let _ = level_tx.send(0.0);
                            }
                            match result {
                                Ok(text) => {
                                    info!("transcription complete: {} chars", text.len());
                                    if !text.is_empty() {
                                        save_history_entry(
                                            &text,
                                            &context.config.general.backend,
                                            &session_language,
                                            duration_secs,
                                        );
                                    }
                                    if context.config.general.audio_feedback {
                                        feedback::play_done(
                                            context.config.general.audio_feedback_volume,
                                        );
                                    }
                                    if context.notify_state() {
                                        let preview = truncate_preview(&text, 77);
                                        send_notification("whisrs", &format!("Done: {preview}"));
                                    }
                                    Response::Ok { state: new_state }
                                }
                                Err(e) => {
                                    error!("transcription failed: {e:#}");
                                    if context.notify_error() {
                                        send_notification(
                                            "whisrs",
                                            &format!("Transcription failed: {e}"),
                                        );
                                    }
                                    Response::Ok { state: new_state }
                                }
                            }
                        }
                        // For STREAMING backends, run_streaming_pipeline's tail
                        // (pipeline.rs) often already moved Transcribing → Idle,
                        // so this second TranscriptionDone is invalid. That is
                        // not an error — the pipeline finalized everything
                        // (history, done-tone, toast). Report success without
                        // double-saving or double-notifying.
                        Err(_) if ds.state_machine.state() == State::Idle => {
                            let _ = context.state_tx.send(State::Idle);
                            if let Some(level_tx) = &context.overlay_level_tx {
                                let _ = level_tx.send(0.0);
                            }
                            match &result {
                                Ok(text) => {
                                    info!(
                                        "transcription complete (pipeline-finalized): {} chars",
                                        text.len()
                                    )
                                }
                                Err(e) => error!("transcription failed: {e:#}"),
                            }
                            Response::Ok { state: State::Idle }
                        }
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    }
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        State::Transcribing => {
            if let Some(requested) = &language {
                warn!(
                    "language override '{requested}' ignored — daemon is still transcribing \
                     the previous session"
                );
            }
            Response::Error {
                message: "cannot toggle while transcribing".to_string(),
            }
        }
        // Recording is refused while read-aloud is active.
        State::Synthesizing | State::Speaking => Response::Error {
            message: "cannot toggle while reading aloud — cancel read-aloud first".to_string(),
        },
    }
}

pub(crate) async fn handle_cancel(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    let mut ds = daemon_state.lock().await;

    // Stop any in-progress TTS playback regardless of recording state.
    let stopped_tts = if let Some(stop) = ds.tts_stop.take() {
        stop.store(true, std::sync::atomic::Ordering::Release);
        info!("cancel: stopped TTS playback");
        true
    } else {
        false
    };

    match ds.state_machine.transition(Action::Cancel) {
        Ok(new_state) => {
            if let Some(mut capture) = ds.audio_capture.take() {
                capture.stop();
                tokio::task::spawn_blocking(move || drop(capture));
            }
            // Stop the typing loop BEFORE aborting the pipeline. Aborting
            // only drops the outer pipeline future; the detached backend and
            // typing tasks keep running, the backend treats the dropped audio
            // channel as a normal end-of-stream and flushes the trailing
            // phrase — without this flag the typing task would type it.
            if let Some(cancel) = ds.streaming_cancel.take() {
                cancel.store(true, Ordering::SeqCst);
            }
            if let Some(task) = ds.streaming_task.take() {
                task.abort();
            }
            ds.recording_window_id = None;
            ds.session_language = None;
            if let Some(level_tx) = &context.overlay_level_tx {
                let _ = level_tx.send(0.0);
            }
            info!("cancelled recording");
            if context.notify_state() {
                send_notification("whisrs", "Recording cancelled");
            }
            Response::Ok { state: new_state }
        }
        // Nothing was recording. If we still interrupted TTS playback, that's a
        // successful cancel — report the current (Idle) state rather than the
        // invalid-transition error.
        Err(_) if stopped_tts => Response::Ok {
            state: ds.state_machine.state(),
        },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

/// Resolve the effective session language: the per-toggle `override_lang` when
/// present, otherwise the configured default.
fn resolve_language(override_lang: Option<String>, default_lang: &str) -> String {
    override_lang.unwrap_or_else(|| default_lang.to_string())
}

/// Validate a per-toggle language override before recording starts.
///
/// Returns the normalized override on success, or the error `Response` to
/// send back to the CLI. Called on the start-press before any capture or
/// state transition so a bad `-l` value (e.g. `english`) is rejected up
/// front instead of failing a whole session at transcription time. The
/// config default (`general.language`) is deliberately not validated.
fn validate_toggle_language(language: Option<String>) -> Result<Option<String>, Response> {
    match language {
        None => Ok(None),
        Some(lang) => match validate_language_override(&lang) {
            Ok(normalized) => Ok(Some(normalized)),
            Err(message) => {
                warn!("rejected language override: {message}");
                Err(Response::Error { message })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whisrs::Config;

    #[test]
    fn resolve_language_prefers_override() {
        assert_eq!(resolve_language(Some("pl".to_string()), "en"), "pl");
    }

    #[test]
    fn resolve_language_falls_back_to_default() {
        assert_eq!(resolve_language(None, "en"), "en");
    }

    /// Mirrors handle_toggle's start-press: validation runs first and its
    /// error response is returned before the Idle→Recording transition, so
    /// a bad `-l` override never starts a recording.
    #[test]
    fn start_press_rejects_invalid_language_override_before_recording() {
        let ds = DaemonState::new();

        let response = validate_toggle_language(Some("english".to_string())).unwrap_err();
        assert!(matches!(response, Response::Error { .. }));

        // handle_toggle returns that response without touching the state
        // machine — the daemon stays idle and no capture is started.
        assert_eq!(ds.state_machine.state(), State::Idle);
    }

    /// Valid overrides are normalized (whisper.cpp needs lowercase codes);
    /// a plain toggle passes through untouched.
    #[test]
    fn start_press_normalizes_valid_language_override() {
        assert_eq!(
            validate_toggle_language(Some("PL".to_string())).unwrap(),
            Some("pl".to_string())
        );
        assert_eq!(validate_toggle_language(None).unwrap(), None);
    }

    /// Minimal config with a batch (non-streaming) backend and an `en` default
    /// language, mirroring the setup where issue reviews caught the batch path
    /// ignoring `-l`.
    fn batch_config() -> Config {
        toml::from_str(
            r#"
            [general]
            backend = "groq"
            language = "en"
            "#,
        )
        .unwrap()
    }

    /// The batch path must transcribe with the language the session was
    /// started with (`toggle -l pl`), not the config default. Mirrors the
    /// handle_toggle flow: the start-press persists the resolved language in
    /// `DaemonState`; the stop-press consumes it and builds the batch
    /// `TranscriptionConfig` from it.
    #[test]
    fn batch_stop_uses_session_language_override() {
        let config = batch_config();
        let mut ds = DaemonState::new();

        // Start press with `-l pl`: resolve and persist the session language.
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.session_language = Some(resolve_language(
            Some("pl".to_string()),
            &config.general.language,
        ));

        // Stop press: the batch path consumes the stored session language.
        ds.state_machine.transition(Action::Toggle).unwrap();
        let session_language = ds.take_session_language(&config.general.language);
        assert_eq!(session_language, "pl");

        // This is the config `process_recording_batch` sends to the backend
        // (and the language `save_history_entry` records).
        let tc = build_transcription_config(&config, &session_language);
        assert_eq!(tc.language, "pl");
    }

    /// Consuming the session language ends the language session: the next
    /// recording must fall back to the config default, not inherit `pl`.
    #[test]
    fn session_language_does_not_leak_into_next_session() {
        let config = batch_config();
        let mut ds = DaemonState::new();

        ds.session_language = Some("pl".to_string());
        assert_eq!(ds.take_session_language(&config.general.language), "pl");

        // Session over — a plain toggle now uses the default again.
        assert_eq!(ds.take_session_language(&config.general.language), "en");
    }

    /// Without an override the batch path keeps using the config default.
    #[test]
    fn batch_stop_defaults_to_config_language() {
        let config = batch_config();
        let mut ds = DaemonState::new();

        ds.session_language = Some(resolve_language(None, &config.general.language));
        let session_language = ds.take_session_language(&config.general.language);
        let tc = build_transcription_config(&config, &session_language);
        assert_eq!(tc.language, "en");
    }
}
