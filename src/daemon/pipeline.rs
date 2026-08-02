use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use audio_silence_gate::{audio_gate_reason, AutoStopDetector, SILENCE_RMS_THRESHOLD};
use filler_remove::FillerFilter;
use prompt_echo::is_prompt_echo;
use whisrs::audio::capture::{AudioCaptureHandle, SAMPLE_RATE};
use whisrs::audio::feedback;
use whisrs::history::{self, HistoryEntry};
use whisrs::state::Action;
use whisrs::transcription::{TranscriptionBackend, TranscriptionConfig};
use whisrs::window::WindowTracker;
use whisrs::{Config, InjectorBackend, State};

use crate::context::{DaemonContext, DaemonState};
use crate::factory::get_model_for_backend;
use crate::injection::{inject_text, is_terminal_class, type_text_at_cursor};
use crate::notify::{send_notification, truncate_preview};

const TYPING_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum recording duration accepted by the gate. Anything shorter is almost
/// certainly an accidental hotkey tap.
const AUDIO_GATE_MIN_MS: u64 = 300;

/// Everything [`run_streaming_pipeline`] needs, bundled so the single call
/// site (`handle_toggle`'s Idle branch in `dictation.rs`) builds one value
/// instead of threading ~19 positional arguments.
pub(crate) struct StreamingPipelineParams {
    // Channels and shared handles.
    pub(crate) audio_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>,
    pub(crate) backend: Arc<dyn TranscriptionBackend>,
    pub(crate) daemon_state: Arc<Mutex<DaemonState>>,
    pub(crate) window_tracker: Arc<dyn WindowTracker>,
    pub(crate) state_tx: tokio::sync::watch::Sender<State>,
    pub(crate) cancel_flag: Arc<AtomicBool>,

    // Per-session values captured when recording started.
    pub(crate) config: TranscriptionConfig,
    pub(crate) window_id: Option<String>,
    pub(crate) language: String,

    // Config snapshot taken at spawn time.
    pub(crate) notify: bool,
    pub(crate) overlay_enabled: bool,
    pub(crate) silence_timeout_ms: u64,
    pub(crate) filler_enabled: bool,
    pub(crate) filler_words: Vec<String>,
    pub(crate) audio_feedback: bool,
    pub(crate) audio_feedback_volume: f32,
    pub(crate) backend_name: String,
    pub(crate) key_delay: Duration,
    pub(crate) injector_backend: InjectorBackend,
}

/// The streaming pipeline: reads audio in real-time, sends to API, types text.
/// Also monitors for silence auto-stop.
pub(crate) async fn run_streaming_pipeline(params: StreamingPipelineParams) -> Result<String> {
    let StreamingPipelineParams {
        mut audio_rx,
        backend,
        daemon_state,
        window_tracker,
        state_tx,
        cancel_flag,
        config,
        window_id,
        language,
        notify,
        overlay_enabled,
        silence_timeout_ms,
        filler_enabled,
        filler_words,
        audio_feedback,
        audio_feedback_volume,
        backend_name,
        key_delay,
        injector_backend,
    } = params;
    // State-progress toasts are noise when the overlay is on.
    let notify_state = notify && !overlay_enabled;
    let notify_error = notify;
    let pipeline_start = std::time::Instant::now();
    let (audio_tx, backend_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(256);
    let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);

    // Build the filler filter once for the lifetime of this pipeline so the
    // batch loop below isn't recompiling regexes on every typed delta.
    let filler_filter = if filler_enabled {
        Some(
            FillerFilter::new(&filler_words)
                .context("invalid custom filler word in configuration")?,
        )
    } else {
        None
    };

    // Spawn the transcription backend.
    let config_clone = config.clone();
    let backend_task = tokio::spawn(async move {
        backend
            .transcribe_stream(backend_rx, text_tx, &config_clone)
            .await
    });

    // Spawn a task that batches and types text as it arrives.
    // We collect deltas for a short window to avoid creating a new virtual
    // keyboard for every single word delta from the streaming API.
    let wid = window_id.clone();
    let typing_cancel = Arc::clone(&cancel_flag);
    let typing_task = tokio::spawn(async move {
        // Focus the original window before the first batch (only once).
        // Sequenced by the batcher awaiting each sink call, so a plain
        // atomic is enough to carry the flag into the 'static sink futures.
        let focused = Arc::new(AtomicBool::new(false));
        run_typing_batcher(text_rx, typing_cancel, filler_filter, move |text_to_type| {
            let wid = wid.clone();
            let tracker = Arc::clone(&window_tracker);
            let focused = Arc::clone(&focused);
            async move {
                // Focus the original window (only once, or re-focus if needed).
                if !focused.swap(true, Ordering::SeqCst) {
                    if let Some(wid) = &wid {
                        if let Err(e) = tracker.focus_window(wid) {
                            warn!("failed to refocus window {wid} before typing: {e}");
                        } else {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }

                info!("typing: {:?}", text_to_type);
                // Streaming deliberately bypasses `inject_text` / `[input]
                // paste`: partial deltas are typed as they arrive, and a
                // paste per delta would thrash the clipboard.
                match tokio::task::spawn_blocking(move || {
                    type_text_at_cursor(&text_to_type, key_delay, injector_backend)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("failed to type text: {e:#}"),
                    Err(e) => warn!("failed to join typing task: {e}"),
                }
            }
        })
        .await
    });

    // Forward audio from capture to backend, with auto-stop detection.
    let mut auto_stop =
        AutoStopDetector::new(SILENCE_RMS_THRESHOLD, silence_timeout_ms, SAMPLE_RATE);

    while let Some(chunk) = audio_rx.recv().await {
        // Check for auto-stop.
        if auto_stop.feed(&chunk) {
            info!("silence auto-stop triggered after {silence_timeout_ms}ms");
            if notify_state {
                send_notification("whisrs", "Auto-stopped (silence detected)");
            }

            // Trigger stop: signal the daemon state machine.
            // We stop the audio capture by closing the forwarding channel.
            // The daemon state will be updated when the streaming task finishes.
            let mut ds = daemon_state.lock().await;
            if ds.state_machine.state() == State::Recording {
                // Stop the audio capture.
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                // Transition to transcribing (pipeline is draining).
                if let Err(e) = ds.state_machine.transition(Action::Toggle) {
                    warn!("auto-stop state transition failed: {e}");
                } else {
                    let _ = state_tx.send(ds.state_machine.state());
                }
            }
            break;
        }

        // Forward to backend.
        if audio_tx.send(chunk).await.is_err() {
            break;
        }
    }

    // Drain remaining audio from the capture channel into the backend.
    while let Some(chunk) = audio_rx.recv().await {
        if audio_tx.send(chunk).await.is_err() {
            break;
        }
    }

    // Close the audio channel to signal end-of-stream to the backend.
    drop(audio_tx);

    // Wait for backend to finish.
    let mut stream_error: Option<String> = None;
    match backend_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let friendly = format_api_error(&e);
            error!("streaming transcription error: {friendly}");
            stream_error = Some(friendly);
        }
        Err(e) => {
            error!("streaming backend task panicked: {e}");
            stream_error = Some(format!("transcription task panicked: {e}"));
        }
    }

    // Wait for the typing side to observe the closed text channel and drain
    // any final batch. If that somehow gets stuck, abort it so the daemon can
    // return to Idle instead of staying in Transcribing forever.
    debug!("waiting for typing task to finish");
    let mut typing_task = typing_task;
    let full_text = tokio::select! {
        result = &mut typing_task => {
            match result {
                Ok(text) => text,
                Err(e) => {
                    warn!("typing task join failed during pipeline shutdown: {e}");
                    String::new()
                }
            }
        }
        _ = tokio::time::sleep(TYPING_DRAIN_TIMEOUT) => {
            warn!(
                "typing task did not finish within {:?}; aborting to unblock daemon state",
                TYPING_DRAIN_TIMEOUT
            );
            typing_task.abort();
            match typing_task.await {
                Ok(text) => text,
                Err(e) if e.is_cancelled() => String::new(),
                Err(e) => {
                    warn!("typing task reported an unexpected shutdown error after abort: {e}");
                    String::new()
                }
            }
        }
    };

    // Notify user about streaming errors. Errors always pop, even with the
    // overlay on — the overlay can't carry the failure detail.
    if let Some(err_msg) = &stream_error {
        if notify_error {
            if full_text.is_empty() {
                send_notification("whisrs", &format!("Transcription error: {err_msg}"));
            } else {
                send_notification(
                    "whisrs",
                    &format!("Transcription failed — partial text may have been typed.\n{err_msg}"),
                );
            }
        }
    }

    // Save to history if we got any text.
    if !full_text.is_empty() {
        let duration_secs = pipeline_start.elapsed().as_secs_f64();
        save_history_entry(&full_text, &backend_name, &language, duration_secs);
    }

    // If auto-stop happened, we need to transition to Idle.
    // This tail pairs with `handle_toggle`'s "pipeline already finalized" arm
    // in `dictation.rs`: when this TranscriptionDone runs first, the stop-
    // toggle's own TranscriptionDone becomes an expected invalid transition.
    let mut ds = daemon_state.lock().await;
    if ds.state_machine.state() == State::Transcribing {
        debug!("streaming pipeline transitioning daemon state back to idle");
        ds.state_machine.transition(Action::TranscriptionDone).ok();
        // Auto-stop ends the session without a stop-toggle — clear the
        // session language so it can't leak into the next recording.
        ds.session_language = None;
        let _ = state_tx.send(ds.state_machine.state());
        if audio_feedback {
            feedback::play_done(audio_feedback_volume);
        }
        if notify_state {
            let preview = truncate_preview(&full_text, 77);
            if !preview.is_empty() {
                send_notification("whisrs", &format!("Done: {preview}"));
            }
        }
    }

    Ok(full_text)
}

/// Batches streaming text deltas and hands them to `sink` (the key injector
/// in production). This is the single choke point through which all streamed
/// text reaches the cursor.
///
/// Deltas arriving within 150ms of each other are coalesced into one batch so
/// we don't create a new virtual keyboard per word delta. Returns the full
/// accumulated (typed) text. Exits when the text channel closes or `cancel`
/// is set.
///
/// The cancel flag is checked when a delta arrives and again right before a
/// batch reaches the sink, so a cancelled recording never types the text the
/// backend flushes on shutdown (a batch already inside the sink still
/// finishes). Exiting drops `text_rx`, which makes subsequent backend sends
/// fail fast and winds the detached backend task down.
///
/// The daemon text channel is intentionally append-only. For OpenAI realtime
/// this still feels token-by-token because the backend emits stable append
/// deltas. For Lemonade-compatible profiles, the protocol layer only forwards
/// completed utterances, so this loop naturally types phrase-sized chunks
/// without needing any replacement semantics.
async fn run_typing_batcher<F, Fut>(
    mut text_rx: tokio::sync::mpsc::Receiver<String>,
    cancel: Arc<AtomicBool>,
    filler_filter: Option<FillerFilter>,
    mut sink: F,
) -> String
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut full_text = String::new();
    let batch_delay = std::time::Duration::from_millis(150);

    loop {
        // Wait for the first delta (blocking).
        let first = text_rx.recv().await;
        let Some(first) = first else { break };

        // Cancelled while we were waiting: discard everything from here on.
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        // Collect this delta and any others that arrive within the batch window.
        let mut batch = first;
        while let Ok(Some(more)) = tokio::time::timeout(batch_delay, text_rx.recv()).await {
            batch.push_str(&more);
        }

        if batch.is_empty() {
            continue;
        }

        // Apply filler word removal if enabled.
        if let Some(filter) = filler_filter.as_ref() {
            batch = filter.apply(&batch);
            if batch.is_empty() {
                continue;
            }
        }

        // Add space separator between turns if needed.
        // Don't insert a space before punctuation streaming deltas,
        // as they arrive as bare tokens.
        let text_to_type = if full_text.is_empty()
            || batch.starts_with(' ')
            || full_text.ends_with(' ')
            || leads_with_punct(&batch)
        {
            batch
        } else {
            format!(" {batch}")
        };

        // Last gate before the sink: cancel may have arrived while this
        // batch was being collected.
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        full_text.push_str(&text_to_type);
        sink(text_to_type).await;
    }

    full_text
}

/// Where a batch transcription request originated. Selects the per-path
/// notification/log wording inside [`transcribe_batch_audio`]; the behavior
/// toggles live in [`BatchOptions`].
pub(crate) enum BatchOrigin<'a> {
    /// Plain dictation (`whisrs toggle` with a batch backend).
    Dictation,
    /// Command mode (`whisrs command`): selection + voice instruction.
    CommandMode,
    /// A named `[[llm_commands]]` hotkey. Carries the command name for
    /// toasts/logs — runtime config data, hence not a `&'static str` label.
    LlmCommand { name: &'a str },
}

/// Per-path switches for [`transcribe_batch_audio`].
///
/// File-local for now; the daemon module split moves this out later.
pub(crate) struct BatchOptions<'a> {
    /// Send the configured prompt/vocabulary hint via
    /// `build_transcription_config` instead of a bare config.
    pub(crate) use_prompt: bool,
    /// Save the recording for `whisrs transcribe-recovery` when the backend
    /// errors. Dictation only: command-path audio is an instruction, not
    /// content worth replaying later.
    pub(crate) save_recovery: bool,
    /// Drop transcripts that look like the model echoing the prompt back.
    /// Should be `true` whenever `use_prompt` is `true` — an echoed prompt is
    /// garbage whether it would be typed or fed to the LLM.
    pub(crate) check_prompt_echo: bool,
    /// Apply the configured filler-word filter. Dictation only for now.
    pub(crate) apply_filler: bool,
    /// Which flow is asking; picks the notification/log wording.
    pub(crate) origin: BatchOrigin<'a>,
}

/// Shared batch transcription flow: audio gate → WAV encode → backend
/// transcribe → prompt-echo check → filler removal.
///
/// `Ok(String::new())` means "nothing to act on" — the recording was gated,
/// the transcript was dropped as a prompt echo, or it came back empty — and
/// the relevant user-facing toast has already been sent with the calling
/// path's own wording (selected via `opts.origin`). `Err` is a real failure
/// (encode or backend); the caller formats and reports it, then unwinds.
pub(crate) async fn transcribe_batch_audio(
    samples: &[i16],
    context: &DaemonContext,
    language: &str,
    opts: &BatchOptions<'_>,
) -> Result<String> {
    use whisrs::audio::wav::encode_wav;

    // Skip the API call entirely when the recording is empty, too short, or
    // pure silence. Cloud Whisper variants (whisper-1, gpt-4o-*-transcribe)
    // hallucinate verbatim chunks of the supplied prompt when handed audio
    // with no speech, which whisrs would then type at the cursor — for a
    // multi-hundred-character prompt that means tens of seconds of garbage
    // typing on every accidental hotkey tap. Filtering here also saves the
    // round-trip cost.
    if let Some(reason) = audio_gate_reason(
        samples,
        SAMPLE_RATE,
        AUDIO_GATE_MIN_MS,
        SILENCE_RMS_THRESHOLD,
    ) {
        let label = reason.as_str();
        match &opts.origin {
            BatchOrigin::Dictation => {
                info!(
                    "skipping transcription: recording was {label} ({} samples)",
                    samples.len()
                );
                if context.notify_state() {
                    send_notification("whisrs", &format!("Skipped: recording was {label}"));
                }
            }
            BatchOrigin::CommandMode => {
                info!(
                    "command mode: skipping (recording was {label}, {} samples)",
                    samples.len()
                );
                if context.notify_error() {
                    send_notification("whisrs", &format!("Command mode: recording was {label}"));
                }
            }
            BatchOrigin::LlmCommand { name } => {
                info!(
                    "llm-command '{name}': skipping (recording was {label}, {} samples)",
                    samples.len()
                );
                if context.notify_state() {
                    send_notification("whisrs", &format!("Skipped: recording was {label}"));
                }
            }
        }
        return Ok(String::new());
    }

    // Progress toast for the command flows — after the gate, so a silent
    // recording only surfaces the skip toast above.
    match &opts.origin {
        BatchOrigin::Dictation => {}
        BatchOrigin::CommandMode => {
            if context.notify_state() {
                send_notification("whisrs", "Processing command...");
            }
        }
        BatchOrigin::LlmCommand { name } => {
            if context.notify_state() {
                send_notification("whisrs", &format!("Processing '{name}'..."));
            }
        }
    }

    let wav_data = encode_wav(samples)?;
    info!("encoded WAV: {} bytes", wav_data.len());

    let config = if opts.use_prompt {
        build_transcription_config(&context.config, language)
    } else {
        TranscriptionConfig {
            language: language.to_string(),
            model: get_model_for_backend(&context.config),
            prompt: None,
        }
    };

    let text = match context
        .transcription_backend
        .transcribe(&wav_data, &config)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            if opts.save_recovery {
                let friendly = format_api_error(&e);
                error!("transcription failed: {friendly}");
                // Save audio for recovery.
                use whisrs::audio::recovery;
                match recovery::save_recovery_audio(samples) {
                    Ok(path) => {
                        info!(
                            "audio saved for recovery: {} — retry with: whisrs transcribe-recovery",
                            path.display()
                        );
                        if context.notify_error() {
                            send_notification(
                                "whisrs",
                                &format!(
                                    "Transcription failed: {friendly}\nAudio saved to {}\nRetry with: whisrs transcribe-recovery",
                                    path.display()
                                ),
                            );
                        }
                    }
                    Err(re) => {
                        warn!("failed to save recovery audio: {re}");
                    }
                }
            }
            return Err(e);
        }
    };

    // Defence in depth against prompt-echo hallucinations: even with the
    // upstream gate, low-SNR recordings (tap microphones, very brief speech
    // followed by silence) sometimes squeak through and trigger the model to
    // regurgitate the prompt. Drop those before they reach the keyboard (or,
    // for llm-commands, the LLM).
    if opts.check_prompt_echo
        && config
            .prompt
            .as_deref()
            .is_some_and(|prompt| is_prompt_echo(&text, prompt))
    {
        warn!(
            "discarding likely prompt-echo response ({} chars) — see prompt_echo crate",
            text.len()
        );
        if context.notify_state() {
            send_notification(
                "whisrs",
                "Skipped: response looked like a prompt echo (no speech detected)",
            );
        }
        return Ok(String::new());
    }

    // Apply filler word removal if enabled.
    let text = if opts.apply_filler && context.config.general.remove_filler_words {
        let filter = FillerFilter::new(&context.config.general.filler_words)
            .context("invalid custom filler word in configuration")?;
        let cleaned = filter.apply(&text);
        if cleaned != text {
            info!(
                "filler removal: {} chars -> {} chars",
                text.len(),
                cleaned.len()
            );
        }
        cleaned
    } else {
        text
    };

    if text.is_empty() {
        match &opts.origin {
            BatchOrigin::Dictation => {
                info!("transcription returned empty text — nothing to type");
            }
            BatchOrigin::CommandMode => {
                if context.notify_error() {
                    send_notification("whisrs", "Could not understand instruction — try again");
                }
            }
            BatchOrigin::LlmCommand { .. } => {
                if context.notify_error() {
                    send_notification("whisrs", "Could not understand speech — try again");
                }
            }
        }
    }

    Ok(text)
}

/// Batch mode: collect all audio, transcribe in one shot, type result.
/// `language` is the resolved session language (per-toggle override or
/// config default) captured when recording started.
pub(crate) async fn process_recording_batch(
    capture: Option<AudioCaptureHandle>,
    window_id: Option<&str>,
    context: &DaemonContext,
    language: &str,
) -> Result<String> {
    let samples = match capture {
        Some(cap) => cap.stop_and_collect().await?,
        None => anyhow::bail!("no audio capture to collect"),
    };

    if samples.is_empty() {
        anyhow::bail!("no audio samples captured");
    }

    info!("collected {} audio samples", samples.len());

    let text = transcribe_batch_audio(
        &samples,
        context,
        language,
        &BatchOptions {
            use_prompt: true,
            save_recovery: true,
            check_prompt_echo: true,
            apply_filler: true,
            origin: BatchOrigin::Dictation,
        },
    )
    .await?;

    if text.is_empty() {
        // Gated, echo-dropped, or empty — the helper already said so.
        return Ok(text);
    }

    // Restore window focus.
    if let Some(wid) = window_id {
        if let Err(e) = context.window_tracker.focus_window(wid) {
            warn!("failed to restore window focus: {e}");
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    // Inject the text at the cursor — type keystrokes, or paste via the
    // clipboard when `[input] paste = true` (layout-independent).
    let text_clone = text.clone();
    let key_delay = std::time::Duration::from_millis(context.config.input.key_delay_ms);
    let injector_backend = context.config.input.backend;
    let paste = context.config.input.paste;
    let is_terminal = if paste {
        context
            .window_tracker
            .get_focused_window_class()
            .map(|c| is_terminal_class(&c))
            .unwrap_or(false)
    } else {
        false
    };
    match tokio::task::spawn_blocking(move || {
        inject_text(&text_clone, is_terminal, key_delay, injector_backend, paste)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("failed to inject text: {e:#}"),
        Err(e) => warn!("failed to join injection task: {e}"),
    }

    Ok(text)
}

pub(crate) fn format_no_microphone_error() -> String {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut msg = "No microphone found — no default audio input device available.".to_string();
    if let Ok(devices) = host.input_devices() {
        let names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
        if names.is_empty() {
            msg.push_str("\nNo audio input devices detected. Check that your microphone is connected and PipeWire/PulseAudio is running.");
        } else {
            msg.push_str("\nAvailable input devices:");
            for name in &names {
                msg.push_str(&format!("\n  - {name}"));
            }
            msg.push_str(
                "\nSet the device in ~/.config/whisrs/config.toml under [audio] device = \"...\"",
            );
        }
    }
    msg
}

pub(crate) fn format_api_error(err: &anyhow::Error) -> String {
    let msg = format!("{err}");
    if msg.contains("final transcription completion") {
        return "Realtime transcription timed out waiting for the server to finish after stop"
            .to_string();
    }
    if msg.contains("invalid API key") || msg.contains("401") {
        return "Invalid API key — check your config at ~/.config/whisrs/config.toml".to_string();
    }
    if msg.contains("rate limit") || msg.contains("429") {
        return "Rate limited — wait a moment and try again".to_string();
    }
    if msg.contains("error sending request")
        || msg.contains("dns error")
        || msg.contains("connection refused")
        || msg.contains("timed out")
        || msg.contains("ConnectError")
    {
        return "Cannot reach API — check your internet connection".to_string();
    }
    msg
}

pub(crate) fn build_transcription_config(config: &Config, language: &str) -> TranscriptionConfig {
    TranscriptionConfig {
        language: language.to_string(),
        model: get_model_for_backend(config),
        prompt: transcription_prompt(config.general.prompt.as_deref(), &config.general.vocabulary),
    }
}

/// Joins `prompt` and `vocabulary` with `". "`, prompt first. Blank prompts
/// are treated as absent; returns `None` only when both inputs are empty so
/// backends skip the hint entirely rather than receiving an empty string.
fn transcription_prompt(prompt: Option<&str>, vocabulary: &[String]) -> Option<String> {
    let prompt = prompt.map(str::trim).filter(|s| !s.is_empty());
    let vocab = if vocabulary.is_empty() {
        None
    } else {
        Some(vocabulary.join(", "))
    };
    match (prompt, vocab) {
        (Some(p), Some(v)) => Some(format!("{p}. {v}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

/// Save a transcription to the history file.
pub(crate) fn save_history_entry(text: &str, backend: &str, language: &str, duration_secs: f64) {
    let entry = HistoryEntry {
        timestamp: chrono::Local::now(),
        text: text.to_string(),
        backend: backend.to_string(),
        language: language.to_string(),
        duration_secs,
    };
    if let Err(e) = history::append_entry(&entry) {
        warn!("failed to save history entry: {e}");
    }
}

/// Returns `true` when the first character of `text` is a punctuation mark
/// that should not have a space inserted before it.
///
/// Covers ASCII sentence/clause terminators and closing brackets, their CJK
/// fullwidth equivalents, the ellipsis character, and closing curly quotes.
/// Opening brackets, opening curly quotes, and dashes are intentionally
/// excluded — they should retain a leading space.
fn leads_with_punct(text: &str) -> bool {
    text.starts_with([
        // ASCII
        '.', ',', '!', '?', ';', ':', ')', ']', '}', // CJK fullwidth equivalents
        '。', '，', '！', '？', '；', '：', '）', '】', '｝', // Ellipsis
        '…',  // Closing curly quotes (U+2019, U+201D)
        '\u{2019}', '\u{201D}',
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn transcription_prompt_neither() {
        assert_eq!(transcription_prompt(None, &[]), None);
    }

    #[test]
    fn transcription_prompt_vocab_only() {
        let vocab = vec!["whisrs".to_string(), "Hyprland".to_string()];
        assert_eq!(
            transcription_prompt(None, &vocab),
            Some("whisrs, Hyprland".to_string())
        );
    }

    #[test]
    fn transcription_prompt_prose_only() {
        assert_eq!(
            transcription_prompt(Some("Embedded Linux dictation."), &[]),
            Some("Embedded Linux dictation.".to_string())
        );
    }

    #[test]
    fn transcription_prompt_both_combined_with_separator() {
        let vocab = vec!["whisrs".to_string(), "Hyprland".to_string()];
        assert_eq!(
            transcription_prompt(Some("Embedded Linux dictation"), &vocab),
            Some("Embedded Linux dictation. whisrs, Hyprland".to_string())
        );
    }

    #[test]
    fn transcription_prompt_blank_prose_treated_as_absent() {
        let vocab = vec!["whisrs".to_string()];
        // Whitespace-only prompt must not bleed an empty "" into the output.
        assert_eq!(
            transcription_prompt(Some("   \t\n  "), &vocab),
            Some("whisrs".to_string())
        );
    }

    #[test]
    fn transcription_prompt_empty_string_with_empty_vocab_is_none() {
        assert_eq!(transcription_prompt(Some(""), &[]), None);
    }

    #[test]
    fn leads_with_punct_ascii_terminators_and_closers() {
        // Sentence/clause terminators and closing brackets that should not get
        // a leading space inserted before them.
        for p in [".", ",", "!", "?", ";", ":", ")", "]", "}"] {
            assert!(
                leads_with_punct(p),
                "expected leads_with_punct({p:?}) to be true",
            );
        }
    }

    #[test]
    fn leads_with_punct_ascii_followed_by_more_text() {
        // Helper should fire based on the first char, regardless of what follows.
        assert!(leads_with_punct(". And then"));
        assert!(leads_with_punct(", and"));
        assert!(leads_with_punct("?!"));
    }

    #[test]
    fn leads_with_punct_unicode_cjk_fullwidth() {
        // CJK fullwidth equivalents of ASCII punctuation.
        for p in ["。", "，", "！", "？", "；", "：", "）", "】", "｝"] {
            assert!(
                leads_with_punct(p),
                "expected leads_with_punct({p:?}) to be true",
            );
        }
    }

    #[test]
    fn leads_with_punct_unicode_ellipsis() {
        assert!(leads_with_punct("…"));
        assert!(leads_with_punct("… and then"));
    }

    #[test]
    fn leads_with_punct_unicode_closing_curly_quotes() {
        // U+2019 RIGHT SINGLE QUOTATION MARK
        assert!(leads_with_punct("\u{2019}"));
        // U+201D RIGHT DOUBLE QUOTATION MARK
        assert!(leads_with_punct("\u{201D}"));
    }

    #[test]
    fn leads_with_punct_leading_space_is_false() {
        // Whitespace prefix means the caller already has spacing — do not
        // suppress an additional one based on punctuation logic.
        assert!(!leads_with_punct(" hello"));
        assert!(!leads_with_punct(" ."));
    }

    #[test]
    fn leads_with_punct_non_punct_prefix_is_false() {
        assert!(!leads_with_punct("hello"));
        assert!(!leads_with_punct("a."));
        assert!(!leads_with_punct("1, 2, 3"));
    }

    #[test]
    fn leads_with_punct_empty_is_false() {
        assert!(!leads_with_punct(""));
    }

    #[test]
    fn leads_with_punct_opening_brackets_are_false() {
        // Opening brackets should retain a leading space.
        assert!(!leads_with_punct("("));
        assert!(!leads_with_punct("["));
        assert!(!leads_with_punct("{"));
    }

    #[test]
    fn leads_with_punct_opening_curly_quotes_are_false() {
        // U+2018 LEFT SINGLE QUOTATION MARK
        assert!(!leads_with_punct("\u{2018}"));
        // U+201C LEFT DOUBLE QUOTATION MARK
        assert!(!leads_with_punct("\u{201C}"));
    }

    #[test]
    fn format_api_error_preserves_realtime_flush_timeout_context() {
        let err = anyhow::anyhow!(
            "timed out waiting for final transcription completion from ws://example after commit"
        );
        assert_eq!(
            format_api_error(&err),
            "Realtime transcription timed out waiting for the server to finish after stop"
        );
    }

    /// Spawn `run_typing_batcher` with a recording sink; returns the text
    /// channel sender, the cancel flag, the sink log, and the join handle.
    #[allow(clippy::type_complexity)]
    fn spawn_test_batcher() -> (
        tokio::sync::mpsc::Sender<String>,
        Arc<AtomicBool>,
        Arc<StdMutex<Vec<String>>>,
        tokio::task::JoinHandle<String>,
    ) {
        let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let typed = Arc::new(StdMutex::new(Vec::<String>::new()));

        let sink_log = Arc::clone(&typed);
        let batcher = tokio::spawn(run_typing_batcher(
            text_rx,
            Arc::clone(&cancel),
            None,
            move |text| {
                let sink_log = Arc::clone(&sink_log);
                async move {
                    sink_log.lock().unwrap().push(text);
                }
            },
        ));

        (text_tx, cancel, typed, batcher)
    }

    /// Issue: `whisrs cancel` during a streaming recording still typed the
    /// phrase the backend flushed on shutdown. Once the cancel flag is set,
    /// nothing further may reach the sink and the loop must exit.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_discards_text_after_cancel() {
        let (text_tx, cancel, typed, batcher) = spawn_test_batcher();

        // A delta before cancel is batched and typed.
        text_tx.send("hello".to_string()).await.unwrap();
        // Paused clock: sleeping past the 150ms batch window deterministically
        // lets the batcher flush the batch to the sink.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(*typed.lock().unwrap(), vec!["hello".to_string()]);

        // Cancel, then simulate the backend flushing a trailing phrase.
        cancel.store(true, Ordering::SeqCst);
        text_tx
            .send(" flushed trailing phrase".to_string())
            .await
            .unwrap();

        // The sink never fires again and the loop exits, returning only the
        // text typed before cancel.
        let full_text = batcher.await.unwrap();
        assert_eq!(full_text, "hello");
        assert_eq!(*typed.lock().unwrap(), vec!["hello".to_string()]);

        // Exiting dropped text_rx, so backend sends now fail fast.
        assert!(text_tx.send("more".to_string()).await.is_err());
    }

    /// Cancel arriving while a batch is still being coalesced (inside the
    /// 150ms window) must drop that batch before it reaches the sink.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_drops_batch_cancelled_during_window() {
        let (text_tx, cancel, typed, batcher) = spawn_test_batcher();

        text_tx.send("hello".to_string()).await.unwrap();
        // Cancel mid-window, before the batch flushes at 150ms.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.store(true, Ordering::SeqCst);

        let full_text = batcher.await.unwrap();
        assert_eq!(full_text, "");
        assert!(typed.lock().unwrap().is_empty());
    }
}
