use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use audio_silence_gate::{
    speech_gate, AutoStopDetector, GateReason, HeardFrame, Levels, VadConfig,
};
use filler_remove::FillerFilter;
use prompt_echo::is_prompt_echo;
use whisrs::audio::capture::{AudioCaptureHandle, SAMPLE_RATE};
use whisrs::audio::quality;
use whisrs::history::HistoryEntry;
use whisrs::llm;
use whisrs::replacements::Replacer;
use whisrs::state::Action;
use whisrs::transcription::{
    transcription_prompt, StreamText, TranscriptionBackend, TranscriptionConfig,
};
use whisrs::window::WindowTracker;
use whisrs::{Config, State};
use xkb_type::ClipboardBackend;

use crate::context::{DaemonContext, DaemonState, StateBus};
use crate::factory::get_model_for_backend;
use crate::injection::{
    deliver_streaming_delta, fold_line_breaks_at_terminal, inject_text, is_terminal_window,
    notify_copied_for_held_modifier, sanitize_for_injection, Injection, KeystrokeSettings,
    ModifierWaitTracker, StreamingDelivery,
};
use crate::notify::{send_notification, truncate_preview};

const TYPING_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum recording duration accepted by the gate. Anything shorter is almost
/// certainly an accidental hotkey tap.
const AUDIO_GATE_MIN_MS: u64 = 300;

/// Wall-clock budget for the toggle-path LLM post-processing call
/// (`[general] llm_post_process`). `llm::rewrite_text` sets no client-side
/// timeout, so without this a hung endpoint would strand the daemon in
/// `Transcribing` and the dictation would never land. On expiry the raw
/// transcript is injected instead.
const LLM_POST_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything [`run_streaming_pipeline`] needs, bundled so the single call
/// site (`handle_toggle`'s Idle branch in `dictation.rs`) builds one value
/// instead of threading ~19 positional arguments.
pub(crate) struct StreamingPipelineParams {
    // Channels and shared handles.
    pub(crate) audio_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>,
    pub(crate) backend: Arc<dyn TranscriptionBackend>,
    pub(crate) daemon_state: Arc<Mutex<DaemonState>>,
    pub(crate) window_tracker: Arc<dyn WindowTracker>,
    pub(crate) state_tx: StateBus,
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
    /// `[replacements]`, applied to each batch after filler removal.
    pub(crate) replacements: std::collections::BTreeMap<String, String>,
    pub(crate) audio_feedback: bool,
    pub(crate) audio_feedback_volume: f32,
    pub(crate) backend_name: String,
    /// `[input]` key delay, backend and modifier wait (#154).
    pub(crate) keys: KeystrokeSettings,
    /// `[input] clipboard_fallback`: leave the final transcript in the
    /// clipboard as a manual-fix fallback for silent injection failures.
    pub(crate) clipboard_fallback: bool,
    /// `[input] clipboard_only`: copy-only dictation — the transcript is
    /// written to the clipboard and never typed at the cursor.
    pub(crate) clipboard_only: bool,
    /// `[input] terminal_classes`, for folding line breaks in deltas typed
    /// into a terminal ([`fold_line_breaks_at_terminal`]).
    pub(crate) terminal_classes: Vec<String>,
    /// `[general] history`: save the transcript for `whisrs log`.
    pub(crate) history: bool,
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
        replacements,
        audio_feedback,
        audio_feedback_volume,
        backend_name,
        keys,
        clipboard_fallback,
        clipboard_only,
        terminal_classes,
        history,
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
    let replacer =
        Replacer::new(&replacements).context("invalid [replacements] key in configuration")?;

    // Asked before the backend moves into its task: the batcher joins what
    // it sends by this.
    let stream_text = backend.stream_text();

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
    // Second handle on the same flag: `typing_cancel` is moved into the
    // batcher, and the clipboard write below still has to know whether the
    // run ended in a cancel.
    let clipboard_cancel = Arc::clone(&cancel_flag);
    // Third handle: a delta waiting on a held modifier (#154) polls it, so
    // `whisrs cancel` ends that wait and discards the delta.
    let sink_cancel = Arc::clone(&cancel_flag);
    // Time deltas spend waiting on a held modifier, kept out of the drain
    // timeout below.
    let modifier_waits = Arc::new(ModifierWaitTracker::default());
    let sink_modifier_waits = Arc::clone(&modifier_waits);
    // Marked by the batcher before it types the first text: from then on
    // the no-speech timeout below never fires.
    let text_typed = Arc::new(TextLatch::default());
    let batcher_typed = Arc::clone(&text_typed);
    let typing_task = tokio::spawn(async move {
        // Focus the original window before the first batch (only once).
        // Sequenced by the batcher awaiting each sink call, so a plain
        // atomic is enough to carry the flag into the 'static sink futures.
        let focused = Arc::new(AtomicBool::new(false));
        // Whether the target is a terminal, resolved at most once per session
        // and only when a delta first holds a line break to fold, so a session
        // without one never reads the window class (see
        // `fold_line_breaks_at_terminal`).
        let at_terminal = Arc::new(std::sync::OnceLock::<bool>::new());
        let terminal_classes = Arc::new(terminal_classes);
        let full_text = run_typing_batcher(
            text_rx,
            stream_text,
            typing_cancel,
            batcher_typed,
            filler_filter,
            replacer,
            move |text_to_type| {
                let wid = wid.clone();
                let tracker = Arc::clone(&window_tracker);
                let focused = Arc::clone(&focused);
                let at_terminal = Arc::clone(&at_terminal);
                let terminal_classes = Arc::clone(&terminal_classes);
                let cancel = Arc::clone(&sink_cancel);
                let modifier_waits = Arc::clone(&sink_modifier_waits);
                async move {
                    // Under `clipboard_only` nothing is typed, so nothing is
                    // folded, as on the batch path.
                    let mut text_to_type = text_to_type;
                    if !clipboard_only {
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

                        if let Some(folded) =
                            owned(fold_line_breaks_at_terminal(&text_to_type, || {
                                *at_terminal.get_or_init(|| {
                                    is_terminal_window(
                                        tracker.get_focused_window_class().as_deref(),
                                        &terminal_classes,
                                    )
                                })
                            }))
                        {
                            warn!(
                                "folded line breaks in dictation: the focused window is a terminal"
                            );
                            text_to_type = folded;
                        }

                        // Streaming deliberately bypasses `inject_text` / `[input]
                        // paste`: partial deltas are typed as they arrive, and a
                        // paste per delta would thrash the clipboard. A held
                        // modifier pauses the delta before its next key, uncapped,
                        // until it is released or the session is cancelled
                        // (#154, #171).
                        let delta = text_to_type.clone();
                        match tokio::task::spawn_blocking(move || {
                            deliver_streaming_delta(&delta, keys, &cancel, &modifier_waits)
                                .map(|delivery| (delivery, delta))
                        })
                        .await
                        {
                            Ok(Ok((StreamingDelivery::Typed, text))) => {
                                debug!("typed: {text:?}");
                            }
                            Ok(Ok((StreamingDelivery::Cancelled { typed }, _))) => {
                                info!(
                                    "cancelled while a modifier key was held; typed {typed} \
                                     bytes of the delta, discarded the rest"
                                );
                            }
                            Ok(Err(e)) => warn!("failed to type text: {e:#}"),
                            Err(e) => warn!("failed to join typing task: {e}"),
                        }
                    }
                    text_to_type
                }
            },
        )
        .await;

        write_streaming_clipboard(
            &full_text,
            clipboard_fallback,
            clipboard_only,
            clipboard_cancel.load(Ordering::SeqCst),
            Arc::from(xkb_type::default_clipboard()),
        )
        .await;

        full_text
    });

    // Forward audio from capture to backend, with auto-stop detection
    // (`[audio] vad`, the close profile: one user near the mic). A timeout of
    // 0 never fires (#178), and turns the no-speech timeout off too.
    let mut auto_stop = AutoStopDetector::with_vad(
        VadConfig::close(config.vad),
        silence_timeout_ms,
        SAMPLE_RATE,
    );
    let mut no_speech = NoSpeechTimeout::new(silence_timeout_ms, "dictation");
    let mut nothing_heard = false;

    while let Some(chunk) = audio_rx.recv().await {
        let silence_reached = auto_stop.feed(&chunk);
        // Nothing heard (`no_speech_timeout`): not a frame of voice for that
        // long, auto-stop not armed, and no text typed yet. Stop, and discard
        // the session as a cancel does (the flag keeps the typing loop from
        // typing whatever the backend still sends, and the tail below drops
        // the transcript). A stop press that got here first owns the session,
        // and text typed while this waited for the lock keeps it: the latch
        // decides that once, against the typing loop.
        let heard = Heard::by(&auto_stop).with_text(text_typed.is_typed());
        if no_speech.feed(chunk.len(), heard) {
            let mut ds = daemon_state.lock().await;
            if ds.state_machine.state() == State::Recording && text_typed.close() {
                if let Some(flag) = &ds.streaming_cancel {
                    if Arc::ptr_eq(flag, &cancel_flag) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                if let Err(e) = ds.state_machine.transition(Action::Toggle) {
                    warn!("no-speech stop state transition failed: {e}");
                } else {
                    state_tx.publish(&ds);
                }
                nothing_heard = true;
                break;
            }
        }

        // Check for auto-stop.
        if silence_reached {
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
                    state_tx.publish(&ds);
                }
            }
            break;
        }

        // Forward to backend.
        if audio_tx.send(chunk).await.is_err() {
            break;
        }
    }
    // Everything up to the stop: the drain below is the capture's last few
    // chunks.
    let stream_levels = auto_stop.levels();
    let heard_speech = auto_stop.has_speech();
    // A voice the level floors rejected: named as the batch gate names it.
    let too_quiet = auto_stop.too_quiet();
    info!(
        "dictation audio: {}, {}",
        describe_levels(&stream_levels),
        no_speech.heard_label()
    );

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
    // return to Idle instead of staying in Transcribing forever. A delta
    // waiting on a held modifier (#154) is not stuck: that time does not
    // count, and the budget restarts once the modifier is released. Only
    // `whisrs cancel` ends such a wait early.
    debug!("waiting for typing task to finish");
    let mut typing_task = typing_task;
    let drain_start = std::time::Instant::now();
    let full_text = loop {
        // While a delta waits on a modifier there is no deadline yet: look
        // again one budget later.
        let wake = modifier_waits
            .drain_deadline(drain_start, TYPING_DRAIN_TIMEOUT)
            .unwrap_or_else(|| std::time::Instant::now() + TYPING_DRAIN_TIMEOUT);
        tokio::select! {
            result = &mut typing_task => {
                break match result {
                    Ok(text) => text,
                    Err(e) => {
                        warn!("typing task join failed during pipeline shutdown: {e}");
                        String::new()
                    }
                };
            }
            _ = tokio::time::sleep_until(wake.into()) => {
                let expired = modifier_waits
                    .drain_deadline(drain_start, TYPING_DRAIN_TIMEOUT)
                    .is_some_and(|deadline| deadline <= std::time::Instant::now());
                if !expired {
                    debug!("typing task is waiting for a held modifier key; still draining");
                    continue;
                }
                warn!(
                    "typing task did not finish within {TYPING_DRAIN_TIMEOUT:?}; aborting to \
                     unblock daemon state"
                );
                typing_task.abort();
                break match typing_task.await {
                    Ok(text) => text,
                    Err(e) if e.is_cancelled() => String::new(),
                    Err(e) => {
                        warn!("typing task reported an unexpected shutdown error after abort: {e}");
                        String::new()
                    }
                };
            }
        }
    };

    // `whisrs cancel` after stop (#154): the session is discarded, like a
    // cancel during recording. No history entry, no done cue. Every writer of
    // the flag holds the daemon-state lock and first finds it in
    // `streaming_cancel`, so reading it and clearing that slot under one
    // lock decides the outcome for good: a cancel that lands later finds no
    // flag and gets the plain "nothing to cancel" error instead of a
    // "Dictation cancelled" toast followed by this session's history entry
    // and done cue.
    let cancelled = {
        daemon_state
            .lock()
            .await
            .release_streaming_cancel(&cancel_flag);
        cancel_flag.load(Ordering::SeqCst)
    };
    let full_text = if cancelled {
        info!("streaming session cancelled after stop; discarding its transcript");
        String::new()
    } else {
        full_text
    };

    if nothing_heard {
        let after = no_speech_timeout(silence_timeout_ms).unwrap_or(NO_SPEECH_TIMEOUT);
        let message = BatchOrigin::Dictation.nothing_heard_message(after, too_quiet.as_ref());
        info!("{message}: the dictation is discarded");
        if notify_state {
            send_notification("whisrs", &message);
        }
    }

    // Notify user about streaming errors. Errors always pop, even with the
    // overlay on — the overlay can't carry the failure detail.
    if let Some(err_msg) = &stream_error {
        if notify_error && !cancelled {
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

    // Nothing typed, and no error or cancel to say why: name what the audio
    // was, as a batch skip does (a voice the level floors rejected is "too
    // quiet", with its level). Audio the detector accepted speech in is not
    // why, so that stays a log line.
    if full_text.is_empty() && stream_error.is_none() && !cancelled {
        let skip = match too_quiet {
            Some(levels) => Some((GateReason::TooQuiet, levels)),
            None => quality::stream_skip_reason(config.vad, &stream_levels, heard_speech)
                .map(|reason| (reason, stream_levels)),
        };
        match skip {
            Some((reason, levels)) => {
                let message = nothing_typed_message(reason, &levels);
                info!("{message}");
                if notify_state {
                    send_notification("whisrs", &message);
                }
            }
            None => info!("nothing typed: the backend returned no text"),
        }
    }

    // Save to history if we got any text.
    if !full_text.is_empty() {
        let duration_secs = pipeline_start.elapsed().as_secs_f64();
        save_history_entry(history, &full_text, &backend_name, &language, duration_secs);
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
        state_tx.publish(&ds);
        if audio_feedback && !cancelled {
            play_done_cue(audio_feedback_volume);
        }
        if notify_state && !cancelled {
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
/// we don't create a new virtual keyboard per word delta. `stream_text` says
/// how two of them join: `Phrases` by the same rule that joins a batch to the
/// text before it ([`needs_space_between`]), `Fragments` verbatim. Phrases
/// queue up whenever the sink is busy (a held modifier, slow typing), so they
/// often share a batch.
///
/// Each batch is sanitized ([`sanitize_for_injection`]) before it reaches the
/// sink. The sink returns the text it handed on, which can differ from what
/// it was given (line breaks folded at a terminal), and the batcher returns
/// all of it accumulated: the transcript the clipboard copy and history get,
/// so they hold exactly what was typed. Exits when the text channel closes or
/// `cancel` is set.
///
/// `text_typed` is marked ([`TextLatch::mark_typed`]) before the sink types
/// the first non-blank batch, after filler removal, `[replacements]` and
/// sanitizing: from then on the audio loop's no-speech timeout never fires
/// ([`NoSpeechTimeout`]). A delta those leave nothing of (a lone "um") marks
/// nothing. When the timeout claimed the session first, nothing is typed.
///
/// The cancel flag is checked when a delta arrives and again right before a
/// batch reaches the sink, so a cancelled recording never types the text the
/// backend flushes on shutdown (a batch already inside the sink still
/// finishes). Exiting drops `text_rx`, which makes subsequent backend sends
/// fail fast and winds the detached backend task down.
///
/// The daemon text channel is intentionally append-only. For OpenAI realtime
/// this still feels token-by-token because the backend emits stable append
/// deltas (`Fragments`). For Lemonade-compatible profiles, the protocol layer
/// only forwards completed utterances (`Phrases`), so this loop naturally
/// types phrase-sized chunks without needing any replacement semantics.
pub(crate) async fn run_typing_batcher<F, Fut>(
    mut text_rx: tokio::sync::mpsc::Receiver<String>,
    stream_text: StreamText,
    cancel: Arc<AtomicBool>,
    text_typed: Arc<TextLatch>,
    filler_filter: Option<FillerFilter>,
    replacer: Option<Replacer>,
    mut sink: F,
) -> String
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = String>,
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
            if stream_text == StreamText::Phrases && needs_space_between(&batch, &more) {
                batch.push(' ');
            }
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

        // Apply `[replacements]`. Per batch, so a phrase split across two
        // batches is not matched.
        if let Some(replacer) = replacer.as_ref() {
            batch = replacer.apply(&batch);
            if batch.is_empty() {
                continue;
            }
        }

        // Strip control characters here, not only at the keyboard, so the
        // clipboard copy and history built from `full_text` get none either.
        if let Some(sanitized) = owned(sanitize_for_injection(&batch)) {
            batch = sanitized;
            if batch.is_empty() {
                continue;
            }
        }

        // Add space separator between turns if needed.
        // Don't insert a space before punctuation streaming deltas,
        // as they arrive as bare tokens.
        let text_to_type = if needs_space_between(&full_text, &batch) {
            format!(" {batch}")
        } else {
            batch
        };

        // Last gate before the sink: cancel may have arrived while this
        // batch was being collected.
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        // Typed text holds the no-speech timeout off for good, marked before
        // the sink types it so the timeout never fires after a typed word.
        // Refused once the timeout claimed the session (it sets `cancel` as
        // well): nothing more is typed.
        if !text_to_type.trim().is_empty() && !text_typed.mark_typed() {
            break;
        }

        let handed_on = sink(text_to_type).await;
        full_text.push_str(&handed_on);
    }

    full_text
}

/// Leave the streaming run's final transcript in the clipboard — or
/// deliberately leave the clipboard alone.
///
/// `[input] clipboard_fallback` / `[input] clipboard_only`: the full
/// accumulated transcript is written to the clipboard once the recording
/// stops — as a manual-fix fallback for silent injection failures, or as the
/// only output in copy-only mode (see
/// [`whisrs::InputConfig::clipboard_fallback`] /
/// [`whisrs::InputConfig::clipboard_only`]). Skipped when nothing was typed
/// — copying an empty string would clobber the user's clipboard for nothing.
/// "Nothing" is an empty transcript, not an empty typing run: under
/// `clipboard_only` nothing is ever typed and the copy is still the point.
///
/// `whisrs cancel` discards: it produces no clipboard output either, which is
/// what `cancelled` carries in. The batch path already behaves that way for
/// free (cancel throws the audio away and `process_recording_batch` never
/// runs), so without that check the same keypress would leave text in the
/// clipboard on a streaming backend and nothing on a batch one — the
/// streaming/batch divergence class from #54.
///
/// Returns `()`: a copy failure is only warned about, never propagated. The
/// caller is the typing task, whose value is the transcript itself.
///
/// `clipboard` is an `Arc` because the write happens inside `spawn_blocking`,
/// whose closure must be `'static + Send` — a `&dyn ClipboardBackend` cannot
/// cross that boundary. The production call site wraps
/// `xkb_type::default_clipboard()`; tests pass a double.
async fn write_streaming_clipboard(
    text: &str,
    clipboard_fallback: bool,
    clipboard_only: bool,
    cancelled: bool,
    clipboard: Arc<dyn ClipboardBackend>,
) {
    if (!clipboard_fallback && !clipboard_only) || text.is_empty() || cancelled {
        return;
    }

    let text = text.to_string();
    match tokio::task::spawn_blocking(move || clipboard.set_text(&text)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("failed to set clipboard fallback: {e:#}"),
        Err(e) => warn!("failed to join clipboard fallback task: {e}"),
    }
}

/// A capture that hears nothing for this long, or for
/// `[general] silence_timeout_ms` when that is longer, stops and is
/// discarded: dictation (both paths), command mode, `[[llm_commands]]` and
/// agent voice turns, never a recording. Every frame a voice is heard in
/// starts the wait again, until auto-stop arms; once transcript text is
/// typed it never fires, and `silence_timeout_ms = 0` (auto-stop off) turns
/// it off too.
pub(crate) const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a capture may hear nothing before it stops ([`NO_SPEECH_TIMEOUT`]
/// or the auto-stop timeout, the longer), `None` with auto-stop off.
pub(crate) fn no_speech_timeout(silence_timeout_ms: u64) -> Option<Duration> {
    (silence_timeout_ms != 0)
        .then(|| NO_SPEECH_TIMEOUT.max(Duration::from_millis(silence_timeout_ms)))
}

/// Whether a streaming session has typed text, shared by its typing loop
/// ([`run_typing_batcher`]) and its audio loop's no-speech stop. One atomic
/// decides between them: the loop marks text typed before its sink types it,
/// and the stop claims the session only while nothing was, so no batch is
/// typed into a session the stop then discards, and the stop never fires
/// after a typed word.
#[derive(Debug, Default)]
pub(crate) struct TextLatch(AtomicU8);

impl TextLatch {
    const OPEN: u8 = 0;
    const TYPED: u8 = 1;
    const CLOSED: u8 = 2;

    /// The typing loop, before its sink types non-blank text: true when it
    /// may type, false once the no-speech stop claimed the session.
    pub(crate) fn mark_typed(&self) -> bool {
        matches!(
            self.0
                .compare_exchange(Self::OPEN, Self::TYPED, Ordering::SeqCst, Ordering::SeqCst),
            Ok(_) | Err(Self::TYPED)
        )
    }

    /// Text was typed (or is being typed): the no-speech stop is off.
    pub(crate) fn is_typed(&self) -> bool {
        self.0.load(Ordering::SeqCst) == Self::TYPED
    }

    /// The no-speech stop, under the daemon lock: true when it may discard
    /// the session, nothing having been typed, and from then on nothing is.
    pub(crate) fn close(&self) -> bool {
        self.0
            .compare_exchange(Self::OPEN, Self::CLOSED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// What a capture has heard by the end of the samples fed to
/// [`NoSpeechTimeout::feed`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Heard {
    /// The first and the latest frame the auto-stop detector heard a voice
    /// in ([`AutoStopDetector::first_heard`], [`AutoStopDetector::last_heard`]).
    pub(crate) first: Option<HeardFrame>,
    pub(crate) last: Option<HeardFrame>,
    /// Auto-stop armed: from then on it, not this, ends the capture.
    pub(crate) armed: bool,
    /// Transcript text was typed (streaming only, [`TextLatch::is_typed`]).
    pub(crate) text: bool,
}

impl Heard {
    /// What `auto_stop` heard, without transcript text.
    pub(crate) fn by(auto_stop: &AutoStopDetector) -> Self {
        Self {
            first: auto_stop.first_heard(),
            last: auto_stop.last_heard(),
            armed: auto_stop.has_speech(),
            text: false,
        }
    }

    /// The same, with transcript text when `text`.
    pub(crate) fn with_text(self, text: bool) -> Self {
        Self { text, ..self }
    }
}

/// What a capture heard first, for its log line.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HeardBy {
    /// A frame the auto-stop detector heard a voice in.
    Detector(HeardFrame),
    /// Transcript text.
    Transcript,
}

/// Seconds of capture at sample `at`.
fn seconds_at(at: u64) -> f64 {
    at as f64 / f64::from(SAMPLE_RATE)
}

/// The no-speech timeout of one capture, counted in its audio from the
/// first chunk, or from the end of the latest frame the auto-stop detector
/// heard a voice in ([`AutoStopDetector::heard_voice`]: any frame it calls
/// speech before its level floors and the count that arms auto-stop, so a
/// voice too quiet to arm it counts, and under `[audio] vad = "rms"` any
/// 16 ms over -50.5 dBFS). A sound switching on (a fan, dishes, a knock) can
/// count as heard too; it only starts the wait again. The timeout never fires
/// once auto-stop armed, nor once transcript text is typed (streaming,
/// [`TextLatch`]): typed text keeps the session to the stop press or to
/// auto-stop. Each capture logs the first thing it heard, once.
pub(crate) struct NoSpeechTimeout {
    /// Samples after which it fires; `None` when off.
    limit: Option<u64>,
    fed: u64,
    /// The detector's first and latest heard frames, as last fed.
    first: Option<HeardFrame>,
    last: Option<HeardFrame>,
    /// Where the first transcript text arrived: it never fires after it.
    text_at: Option<u64>,
    /// The first thing heard was logged.
    logged: bool,
    fired: bool,
    /// Names the flow in the log ("dictation", "command mode").
    label: String,
}

impl NoSpeechTimeout {
    pub(crate) fn new(silence_timeout_ms: u64, label: &str) -> Self {
        // Saturating, as `AutoStopDetector` counts its own timeout: a huge
        // `silence_timeout_ms` must not wrap to a limit the first chunk
        // reaches.
        let limit = no_speech_timeout(silence_timeout_ms)
            .map(|after| (after.as_millis() as u64).saturating_mul(u64::from(SAMPLE_RATE)) / 1000);
        Self {
            limit,
            fed: 0,
            first: None,
            last: None,
            text_at: None,
            logged: false,
            fired: false,
            label: label.to_string(),
        }
    }

    /// Count `samples` more of the capture; `heard`: what was heard by the
    /// end of them. True once, on the chunk that completes the timeout with
    /// nothing heard since the start or the latest heard frame, auto-stop
    /// not armed and no transcript text.
    pub(crate) fn feed(&mut self, samples: usize, heard: Heard) -> bool {
        self.fed += samples as u64;
        self.first = heard.first;
        self.last = heard.last;
        if heard.text && self.text_at.is_none() {
            self.text_at = Some(self.fed);
        }
        let first = match (heard.first, self.text_at) {
            (Some(frame), _) => Some(HeardBy::Detector(frame)),
            (None, Some(_)) => Some(HeardBy::Transcript),
            (None, None) => None,
        };
        if let Some(first) = first.filter(|_| !self.logged) {
            self.logged = true;
            self.log_first(first);
        }
        let since = heard.last.map_or(0, |frame| frame.end_sample());
        let reached = !self.fired
            && !heard.armed
            && self.text_at.is_none()
            && self
                .limit
                .is_some_and(|l| self.fed.saturating_sub(since) >= l);
        self.fired |= reached;
        reached
    }

    /// The info line for the first thing heard.
    fn log_first(&self, first: HeardBy) {
        let label = &self.label;
        match first {
            HeardBy::Detector(frame) => {
                let rule = if frame.soft { "soft rule, " } else { "" };
                info!(
                    "{label}: heard at {:.2} s (detector, {rule}score {:.2}, {:.1} dBFS)",
                    seconds_at(frame.start_sample),
                    frame.score,
                    frame.dbfs
                );
            }
            HeardBy::Transcript => info!(
                "{label}: heard at {:.2} s (transcript)",
                seconds_at(self.text_at.unwrap_or(self.fed))
            ),
        }
    }

    /// What was heard, for the turn's levels line: "heard at 4.15 s",
    /// "heard at 0.35 to 8.20 s" (the first and latest heard frames) or
    /// "heard nothing", then ", text at 9.50 s" when transcript text came.
    pub(crate) fn heard_label(&self) -> String {
        let mut label = match (self.first, self.last) {
            (Some(first), Some(last)) if last.start_sample > first.start_sample => format!(
                "heard at {:.2} to {:.2} s",
                seconds_at(first.start_sample),
                seconds_at(last.start_sample)
            ),
            (Some(first), _) => format!("heard at {:.2} s", seconds_at(first.start_sample)),
            (None, _) => "heard nothing".to_string(),
        };
        if let Some(at) = self.text_at {
            label.push_str(&format!(", text at {:.2} s", seconds_at(at)));
        }
        label
    }
}

/// `after` for a toast: whole seconds, else one decimal ("12.5 s").
fn seconds_label(after: Duration) -> String {
    let ms = after.as_millis();
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", after.as_secs_f64())
    }
}

/// Where a batch transcription request originated. Selects the per-path
/// notification/log wording inside [`transcribe_batch_audio`]; the behavior
/// toggles live in [`BatchOptions`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BatchOrigin<'a> {
    /// Plain dictation (`whisrs toggle` with a batch backend).
    Dictation,
    /// Command mode (`whisrs command`): selection + voice instruction.
    CommandMode,
    /// A named `[[llm_commands]]` hotkey. Carries the command name for
    /// toasts/logs — runtime config data, hence not a `&'static str` label.
    LlmCommand { name: &'a str },
    /// An agent's voice turn (`[[agents]]`). Carries the agent name.
    Agent { name: &'a str },
}

/// `message` with the advice for `reason` after it ([`quality::skip_hint`]).
fn with_skip_hint(message: String, reason: GateReason, levels: &Levels) -> String {
    match quality::skip_hint(reason, levels) {
        Some(hint) => format!("{message}. {hint}"),
        None => message,
    }
}

/// The toast for a streaming dictation that typed nothing because of its
/// audio: the batch skip toast's label and advice.
fn nothing_typed_message(reason: GateReason, levels: &Levels) -> String {
    with_skip_hint(
        format!(
            "Nothing typed: the audio was {}",
            quality::skip_label(reason, levels)
        ),
        reason,
        levels,
    )
}

impl BatchOrigin<'_> {
    /// Toast text when the audio gate rejects the audio (`label` is the
    /// gate's reason, e.g. "too short" or "silent", with its level). It
    /// never says "recording": that is the recording feature's word.
    fn gate_skip_message(&self, label: &str) -> String {
        match self {
            BatchOrigin::Dictation | BatchOrigin::LlmCommand { .. } => {
                format!("Skipped: the audio was {label}")
            }
            BatchOrigin::CommandMode => format!("Command mode: the audio was {label}"),
            BatchOrigin::Agent { name } => {
                format!("Agent '{name}': the audio was {label}, nothing was sent")
            }
        }
    }

    /// Toast text when a capture stopped because nothing was heard for
    /// `after` ([`no_speech_timeout`]), worded as the origin's skip toasts
    /// are. `too_quiet`: what it heard before was a voice the level floors
    /// rejected ([`AutoStopDetector::too_quiet`]), which the toast names
    /// instead, with its level.
    pub(crate) fn nothing_heard_message(
        &self,
        after: Duration,
        too_quiet: Option<&Levels>,
    ) -> String {
        let what = match too_quiet {
            Some(levels) => format!(
                "the audio was {}",
                quality::skip_label(GateReason::TooQuiet, levels)
            ),
            None => format!("nothing heard for {}", seconds_label(after)),
        };
        match self {
            BatchOrigin::Dictation | BatchOrigin::LlmCommand { .. } => format!("Stopped: {what}"),
            BatchOrigin::CommandMode => format!("Command mode: {what}, stopped"),
            BatchOrigin::Agent { name } => format!("Agent '{name}': {what}, stopped"),
        }
    }

    /// Whether this origin's skip toasts show: dictation's and
    /// `[[llm_commands]]`' only without the overlay (they are progress),
    /// command mode's and agents' always (the turn was asked for).
    fn notifies_skips(&self, context: &DaemonContext) -> bool {
        match self {
            BatchOrigin::Dictation | BatchOrigin::LlmCommand { .. } => context.notify_state(),
            BatchOrigin::CommandMode | BatchOrigin::Agent { .. } => context.notify_error(),
        }
    }

    /// The skip toast for `reason`: [`Self::gate_skip_message`] with the level
    /// that explains it ([`quality::skip_label`]), and for silent audio the
    /// advice after it.
    fn gate_skip_toast(&self, reason: GateReason, levels: &Levels) -> String {
        with_skip_hint(
            self.gate_skip_message(&quality::skip_label(reason, levels)),
            reason,
            levels,
        )
    }

    /// Progress toast shown once the gate passes. `None` for dictation, which
    /// stays quiet until text lands.
    fn processing_message(&self) -> Option<String> {
        match self {
            BatchOrigin::Dictation => None,
            BatchOrigin::CommandMode => Some("Processing command...".to_string()),
            BatchOrigin::LlmCommand { name } => Some(format!("Processing '{name}'...")),
            BatchOrigin::Agent { name } => Some(format!("Sending to '{name}'...")),
        }
    }

    /// Error toast when the backend returns an empty transcript. `None` for
    /// dictation, which only logs.
    fn empty_result_message(&self) -> Option<&'static str> {
        match self {
            BatchOrigin::Dictation => None,
            BatchOrigin::CommandMode => Some("Could not understand instruction — try again"),
            BatchOrigin::LlmCommand { .. } => Some("Could not understand speech — try again"),
            BatchOrigin::Agent { .. } => {
                Some("Could not understand the instruction, nothing was sent. Try again.")
            }
        }
    }
}

/// Say that a capture stopped with nothing heard ([`no_speech_timeout`]):
/// a log line, and the toast under the origin's skip-toast rule. `too_quiet`
/// as for [`BatchOrigin::nothing_heard_message`].
pub(crate) fn report_nothing_heard(
    context: &DaemonContext,
    origin: &BatchOrigin<'_>,
    too_quiet: Option<&Levels>,
) {
    let after =
        no_speech_timeout(context.config.general.silence_timeout_ms).unwrap_or(NO_SPEECH_TIMEOUT);
    let message = origin.nothing_heard_message(after, too_quiet);
    info!("{message}: the audio is discarded");
    if origin.notifies_skips(context) {
        send_notification("whisrs", &message);
    }
}

/// Toast text when a transcript is dropped as a prompt echo. Origin-independent:
/// the same wording is used by every batch path.
const PROMPT_ECHO_SKIP_TOAST: &str =
    "Skipped: response looked like a prompt echo (no speech detected)";

/// Per-path switches for [`transcribe_batch_audio`].
#[derive(Debug, PartialEq, Eq)]
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
    /// Apply `[replacements]`. Dictation only, like `apply_filler`.
    pub(crate) apply_replacements: bool,
    /// Which flow is asking; picks the notification/log wording.
    pub(crate) origin: BatchOrigin<'a>,
}

impl<'a> BatchOptions<'a> {
    /// Plain dictation: everything on — prompt, recovery save, echo check,
    /// filler removal.
    pub(crate) fn dictation() -> BatchOptions<'static> {
        BatchOptions {
            use_prompt: true,
            save_recovery: true,
            check_prompt_echo: true,
            apply_filler: true,
            apply_replacements: true,
            origin: BatchOrigin::Dictation,
        }
    }

    /// Command mode (`whisrs command`): the recording is a spoken instruction,
    /// not content — no prompt (so no echo check), no recovery save, no filler
    /// removal.
    pub(crate) fn command_mode() -> BatchOptions<'static> {
        BatchOptions {
            use_prompt: false,
            save_recovery: false,
            check_prompt_echo: false,
            apply_filler: false,
            apply_replacements: false,
            origin: BatchOrigin::CommandMode,
        }
    }

    /// A named `[[llm_commands]]` hotkey. Prompted like dictation — and the
    /// transcript is LLM input here: an echoed prompt would be silently
    /// rewritten into plausible garbage, so the echo check matters just as
    /// much as on the dictation path. Still no recovery save or filler
    /// removal: the audio is an instruction, not content.
    pub(crate) fn llm_command(name: &'a str) -> BatchOptions<'a> {
        BatchOptions {
            use_prompt: true,
            save_recovery: false,
            check_prompt_echo: true,
            apply_filler: false,
            apply_replacements: false,
            origin: BatchOrigin::LlmCommand { name },
        }
    }

    /// An agent's voice turn: the transcript is the agent's instruction.
    /// Prompted (project vocabulary helps) and echo-checked like an
    /// llm-command, with `[replacements]` applied, since a word fix-up the
    /// user configured matters as much to the agent as on screen. No
    /// recovery save or filler removal.
    pub(crate) fn agent(name: &'a str) -> BatchOptions<'a> {
        BatchOptions {
            use_prompt: true,
            save_recovery: false,
            check_prompt_echo: true,
            apply_filler: false,
            apply_replacements: true,
            origin: BatchOrigin::Agent { name },
        }
    }

    /// `config.prompt` is set for every backend, including ones that never
    /// transmit it — hence the separate `backend_sends_prompt` gate. The
    /// caller reads that flag off `TranscriptionBackend::sends_prompt` per
    /// request, since the realtime backends answer it from the model.
    fn should_drop_as_echo(
        &self,
        backend_sends_prompt: bool,
        prompt: Option<&str>,
        text: &str,
    ) -> bool {
        self.check_prompt_echo
            && backend_sends_prompt
            && prompt.is_some_and(|prompt| is_prompt_echo(text, prompt))
    }
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
    // holds no speech. Cloud Whisper variants (whisper-1, gpt-4o-*-transcribe)
    // hallucinate verbatim chunks of the supplied prompt when handed audio
    // with no speech, which whisrs would then type at the cursor — for a
    // multi-hundred-character prompt that means tens of seconds of garbage
    // typing on every accidental hotkey tap. Filtering here also saves the
    // round-trip cost. The detector is `[audio] vad` with the close profile
    // (one user near the mic), run off the async worker: a long buffer is
    // a few hundred ms of model time.
    let vad = VadConfig::close(context.config.audio.vad());
    let gate = whisrs::audio::block_off_runtime(|| {
        speech_gate(samples, SAMPLE_RATE, AUDIO_GATE_MIN_MS, &vad)
    });
    // The turn's one info line with its levels: command mode,
    // `[[llm_commands]]` and agent turns logged theirs while recording
    // (`collect_instruction_audio`), batch dictation has only this one.
    if opts.origin == BatchOrigin::Dictation {
        info!("dictation audio: {}", describe_levels(&gate.levels));
    } else {
        debug!("audio gate: {}", describe_levels(&gate.levels));
    }
    if let Some(reason) = gate.reason {
        let label = quality::skip_label(reason, &gate.levels);
        let toast = opts.origin.gate_skip_toast(reason, &gate.levels);
        match &opts.origin {
            BatchOrigin::Dictation => info!(
                "skipping transcription: recording was {label} ({} samples)",
                samples.len()
            ),
            BatchOrigin::CommandMode => info!(
                "command mode: skipping (recording was {label}, {} samples)",
                samples.len()
            ),
            BatchOrigin::LlmCommand { name } => info!(
                "llm-command '{name}': skipping (recording was {label}, {} samples)",
                samples.len()
            ),
            BatchOrigin::Agent { name } => info!(
                "agent '{name}': skipping (recording was {label}, {} samples)",
                samples.len()
            ),
        }
        if opts.origin.notifies_skips(context) {
            send_notification("whisrs", &toast);
        }
        return Ok(String::new());
    }

    // Progress toast for the command flows — after the gate, so a silent
    // recording only surfaces the skip toast above. An agent's is left out
    // only while an agent overlay shows (the pill never shows agent mode).
    if let Some(msg) = opts.origin.processing_message() {
        let notify = match opts.origin {
            BatchOrigin::Agent { .. } => context.notify_agent_state(),
            _ => context.notify_state(),
        };
        if notify {
            send_notification("whisrs", &msg);
        }
    }

    let wav_data = encode_wav(samples)?;
    info!("encoded WAV: {} bytes", wav_data.len());

    let config = batch_transcription_config(&context.config, language, opts.use_prompt);

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
    if opts.should_drop_as_echo(
        context.transcription_backend.sends_prompt(&config),
        config.prompt.as_deref(),
        &text,
    ) {
        warn!(
            "discarding likely prompt-echo response ({} chars) — see prompt_echo crate",
            text.len()
        );
        if context.notify_state() {
            send_notification("whisrs", PROMPT_ECHO_SKIP_TOAST);
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

    let text = if opts.apply_replacements {
        match Replacer::new(&context.config.replacements)
            .context("invalid [replacements] key in configuration")?
        {
            Some(replacer) => replacer.apply(&text),
            None => text,
        }
    } else {
        text
    };

    if text.is_empty() {
        match opts.origin.empty_result_message() {
            // Dictation: nothing to toast, just log.
            None => info!("transcription returned empty text — nothing to type"),
            Some(msg) => {
                if context.notify_error() {
                    send_notification("whisrs", msg);
                }
            }
        }
    }

    Ok(text)
}

/// The instruction a finished transcript should be post-processed with, or
/// `None` when the toggle path must inject it unmodified.
///
/// `None` covers both "`llm_post_process` is off" (the default) and "on but
/// the instruction is blank" — an empty instruction buys a round trip and a
/// non-deterministic rewrite for nothing. `Config::validate` warns about the
/// second case at startup.
fn llm_post_process_instruction(config: &Config) -> Option<&str> {
    if !config.general.llm_post_process {
        return None;
    }
    let instruction = config.general.llm_instruction.trim();
    (!instruction.is_empty()).then_some(instruction)
}

/// What a finished dictation produced: the text that reached the cursor, and
/// whether `[general] llm_post_process` is what produced it.
///
/// The flag is carried out of the pipeline purely so `whisrs log` can tell a
/// post-processed dictation from a raw one (see [`history_backend_tag`]). It
/// is `true` only when the LLM actually returned the words that were typed,
/// never for a fallback: with the flag off, a blank instruction, an API error,
/// a timeout or an empty completion, the entry holds the raw transcript and
/// labelling it post-processed would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictationOutcome {
    pub(crate) text: String,
    pub(crate) post_processed: bool,
}

impl DictationOutcome {
    /// Text that reached the cursor untouched by the LLM. Also what the
    /// streaming path always produces: it types partials as they arrive and
    /// never holds a whole transcript to post-process.
    pub(crate) fn raw(text: String) -> Self {
        Self {
            text,
            post_processed: false,
        }
    }
}

/// The history `backend` field for a finished dictation.
///
/// A raw dictation keeps the bare backend name, byte for byte what has always
/// been recorded, so existing history semantics do not shift. A dictation the
/// LLM rewrote gets a `+llm` suffix (`groq` becomes `groq+llm`), so `whisrs
/// log` distinguishes the LLM's words from the microphone's.
///
/// The suffix mirrors the llm-command path's `llm:<name>` tag, which reads as
/// "the LLM produced this". It is a suffix rather than that prefix for two
/// reasons: the transcription backend still matters here (it produced the LLM's
/// input, whereas command mode never logs it), and `llm:<backend>` would be
/// indistinguishable from an `[[llm_commands]]` entry that happens to be named
/// after a backend.
pub(crate) fn history_backend_tag(backend: &str, post_processed: bool) -> String {
    if post_processed {
        format!("{backend}+llm")
    } else {
        backend.to_string()
    }
}

/// Pick the text to inject after a post-processing attempt. `rewritten` is
/// `None` when the call failed or timed out, and a blank completion counts the
/// same: the original transcript wins in every one of those cases.
///
/// This is where the toggle path deliberately diverges from the llm-command
/// path (`llm_command_background_inner`), which propagates the LLM error and
/// injects nothing. There the user pressed a hotkey to run *that command*, so
/// a failure means the command did not happen; here they pressed plain
/// dictate, and losing a whole dictation to a flaky endpoint would be a
/// regression against `llm_post_process = false`.
/// The completion goes through [`llm::clean_llm_output`], the same cleaning
/// the llm-command path uses: line endings normalized, a wrapping code fence
/// stripped, the ends trimmed. A trailing newline typed at a shell prompt
/// submits the line, and unlike the llm-command hotkey this runs on every
/// dictation, so a chatty model would append one every time. Line breaks
/// *inside* the reply are folded later, at injection, once the target is known.
fn post_processed_or_raw(original: String, rewritten: Option<String>) -> DictationOutcome {
    match rewritten.map(|text| llm::clean_llm_output(&text)) {
        Some(text) if !text.is_empty() => DictationOutcome {
            text,
            post_processed: true,
        },
        _ => DictationOutcome::raw(original),
    }
}

/// Run a finished transcript through the shared `[llm]` backend when
/// `[general] llm_post_process` is on, returning the text to inject.
///
/// Fails soft on every path — flag off, blank instruction, missing `[llm]`
/// section, API error, timeout, empty completion — by returning `text`
/// unchanged, so a post-processing problem never costs the user their words.
/// Reuses `llm::rewrite_text` exactly as the llm-command path does, with the
/// dictated text as the "selected text" and the configured instruction as the
/// "voice instruction".
async fn apply_llm_post_process(text: String, context: &DaemonContext) -> DictationOutcome {
    let Some(instruction) = llm_post_process_instruction(&context.config) else {
        return DictationOutcome::raw(text);
    };

    info!(
        "llm post-processing: sending {} chars to the LLM",
        text.len()
    );
    // Same resolution as command mode: an absent [llm] section falls back to
    // the defaults, where an empty api_key sends `rewrite_text` to the
    // WHISRS_OPENAI_API_KEY / WHISRS_GROQ_API_KEY env vars.
    let llm_config = context.config.llm.clone().unwrap_or_default();

    let rewritten = match tokio::time::timeout(
        LLM_POST_PROCESS_TIMEOUT,
        llm::rewrite_text(&llm_config, &text, instruction),
    )
    .await
    {
        Ok(Ok(rewritten)) => {
            if rewritten.trim().is_empty() {
                warn!("llm post-processing returned empty text — injecting the raw transcript");
                if context.notify_error() {
                    send_notification(
                        "whisrs",
                        "LLM post-processing returned nothing — typing the raw transcript",
                    );
                }
            }
            Some(rewritten)
        }
        Ok(Err(e)) => {
            let friendly = format_api_error(&e);
            error!("llm post-processing failed: {e:#}");
            if context.notify_error() {
                send_notification(
                    "whisrs",
                    &format!("LLM post-processing failed: {friendly}\nTyping the raw transcript"),
                );
            }
            None
        }
        Err(_elapsed) => {
            let secs = LLM_POST_PROCESS_TIMEOUT.as_secs();
            error!("llm post-processing timed out after {secs}s");
            if context.notify_error() {
                send_notification(
                    "whisrs",
                    &format!(
                        "LLM post-processing timed out after {secs}s\nTyping the raw transcript"
                    ),
                );
            }
            None
        }
    };

    post_processed_or_raw(text, rewritten)
}

/// Batch mode: collect all audio, transcribe in one shot, type result.
/// `language` is the resolved session language (per-toggle override or
/// config default) captured when recording started.
pub(crate) async fn process_recording_batch(
    capture: Option<AudioCaptureHandle>,
    window_id: Option<&str>,
    context: &DaemonContext,
    language: &str,
) -> Result<DictationOutcome> {
    let samples = match capture {
        Some(cap) => cap.stop_and_collect().await?,
        None => anyhow::bail!("no audio capture to collect"),
    };
    process_batch_samples(samples, window_id, context, language).await
}

/// [`process_recording_batch`] on audio already collected (batch dictation
/// collects on its own task, which watches for nothing heard).
pub(crate) async fn process_batch_samples(
    samples: Vec<i16>,
    window_id: Option<&str>,
    context: &DaemonContext,
    language: &str,
) -> Result<DictationOutcome> {
    if samples.is_empty() {
        anyhow::bail!("no audio samples captured");
    }

    info!("collected {} audio samples", samples.len());

    let text =
        transcribe_batch_audio(&samples, context, language, &BatchOptions::dictation()).await?;

    if text.is_empty() {
        // Gated, echo-dropped, or empty — the helper already said so.
        return Ok(DictationOutcome::raw(text));
    }

    // `[general] llm_post_process`: rewrite the finished transcript through
    // the shared `[llm]` backend before it reaches the cursor (issue #85).
    // Batch-only by design — the streaming pipeline types partials as they
    // arrive and never holds a whole transcript, so this must NOT be wired
    // into `run_streaming_pipeline`; the usual "wire it into both paths" rule
    // is inverted here and `Config::validate` warns instead. The rewritten
    // text is what this function returns, so history records what was actually
    // typed, tagged by `history_backend_tag` with whether the LLM produced it.
    let outcome = apply_llm_post_process(text, context).await;

    let keys = KeystrokeSettings::from_config(&context.config.input);
    let paste = context.config.input.paste;
    let clipboard_fallback = context.config.input.clipboard_fallback;
    let clipboard_only = context.config.input.clipboard_only;

    // Restore window focus so the injection lands where the user was typing.
    // Skipped under `[input] clipboard_only`: nothing is injected, so raising
    // the old window would only yank focus away from wherever the user moved
    // to (and burn 100 ms doing it). Matches the streaming path's refocus gate.
    if !clipboard_only {
        if let Some(wid) = window_id {
            if let Err(e) = context.window_tracker.focus_window(wid) {
                warn!("failed to restore window focus: {e}");
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }

    // Inject the text at the cursor — type keystrokes, or paste via the
    // clipboard when `[input] paste = true` (layout-independent).
    let terminal_classes = &context.config.input.terminal_classes;
    let is_terminal = if paste {
        is_terminal_window(
            context.window_tracker.get_focused_window_class().as_deref(),
            terminal_classes,
        )
    } else {
        false
    };

    // No line break reaches a terminal, where it would be an Enter: a
    // transcript never legitimately holds one, and with `llm_post_process` a
    // steered model could otherwise add one on every dictation. The class is
    // only read for text that has a line break to fold (and reused when
    // `paste` already read it). Under `clipboard_only` nothing is typed, so
    // nothing is folded. `outcome.text` takes the sanitized, folded form so
    // history records what was typed.
    let mut outcome = outcome;
    if let Some(sanitized) = owned(sanitize_for_injection(&outcome.text)) {
        outcome.text = sanitized;
    }
    if !clipboard_only {
        let at_terminal = || {
            if paste {
                is_terminal
            } else {
                is_terminal_window(
                    context.window_tracker.get_focused_window_class().as_deref(),
                    terminal_classes,
                )
            }
        };
        if let Some(folded) = owned(fold_line_breaks_at_terminal(&outcome.text, at_terminal)) {
            warn!("folded line breaks in dictation: the focused window is a terminal");
            outcome.text = folded;
        }
    }
    let text_clone = outcome.text.clone();
    match tokio::task::spawn_blocking(move || {
        inject_text(
            &text_clone,
            is_terminal,
            keys,
            paste,
            clipboard_fallback,
            clipboard_only,
        )
    })
    .await
    {
        Ok(Ok(Injection::Delivered)) => {}
        Ok(Ok(Injection::CopiedForHeldModifier(copied))) => {
            if context.notify_error() {
                notify_copied_for_held_modifier(&copied);
            }
        }
        Ok(Err(e)) => warn!("failed to inject text: {e:#}"),
        Err(e) => warn!("failed to join injection task: {e}"),
    }

    Ok(outcome)
}

/// The new text when a `Cow`-returning filter changed something, else `None`,
/// so the caller can overwrite the very string the filter borrowed.
fn owned(text: Cow<'_, str>) -> Option<String> {
    match text {
        Cow::Owned(s) => Some(s),
        Cow::Borrowed(_) => None,
    }
}

/// `device` is the configured `[audio] device`: a stock `"default"` lists
/// ALSA devices only, so this error path never opens a PulseAudio connection.
pub(crate) fn format_no_microphone_error(device: &str) -> String {
    let mut msg = "No microphone found: no default audio input device available.".to_string();
    let devices =
        whisrs::audio::block_off_runtime(|| whisrs::audio::device::list_input_devices_for(device));
    if devices.is_empty() {
        msg.push_str("\nNo audio input devices detected. Check that your microphone is connected and PipeWire/PulseAudio is running.");
    } else {
        msg.push_str("\nAvailable input devices:");
        for d in &devices {
            msg.push_str(&format!("\n  - {} ({})", d.id, d.description));
        }
        msg.push_str(
            "\nSet the device in ~/.config/whisrs/config.toml under [audio] device = \"...\"",
        );
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

/// One line of a stream's measured levels, for the log.
pub(crate) fn describe_levels(levels: &Levels) -> String {
    let db = |v: Option<f32>| v.map_or_else(|| "none".to_string(), |v| format!("{v:.1}"));
    format!(
        "{:.2} s, speech {} dBFS ({:.0}% of frames), background {} dBFS, SNR {} dB, peak \
         {:.1} dBFS, clipped {:.2}%",
        levels.duration_ms as f64 / 1000.0,
        db(levels.speech_dbfs),
        levels.speech_share * 100.0,
        db(levels.noise_dbfs),
        db(levels.snr_db),
        levels.peak_dbfs,
        levels.clipped_share * 100.0
    )
}

pub(crate) fn build_transcription_config(config: &Config, language: &str) -> TranscriptionConfig {
    TranscriptionConfig {
        language: language.to_string(),
        model: get_model_for_backend(config),
        prompt: transcription_prompt(config.general.prompt.as_deref(), &config.general.vocabulary),
        keyterms: config.general.vocabulary.clone(),
        diarize: false,
        vad: config.audio.vad(),
        soft_voices: true,
    }
}

/// Pick the [`TranscriptionConfig`] for one batch run.
///
/// `use_prompt = false` is the command-mode opt-out: the recording is a spoken
/// instruction, not content, so it gets neither the prompt hint nor the
/// vocabulary keyterms. `keyterms: Vec::new()` is the same opt-out as
/// `prompt: None` — biasing "make this shorter" toward the dictation
/// vocabulary is wrong for exactly the reason the prompt is withheld.
fn batch_transcription_config(
    config: &Config,
    language: &str,
    use_prompt: bool,
) -> TranscriptionConfig {
    if use_prompt {
        build_transcription_config(config, language)
    } else {
        TranscriptionConfig {
            language: language.to_string(),
            model: get_model_for_backend(config),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: config.audio.vad(),
            soft_voices: true,
        }
    }
}

/// Save a transcription to the history file, unless `[general] history` is
/// off. `enabled` is a parameter, not a lookup, so no caller can skip the check.
pub(crate) fn save_history_entry(
    enabled: bool,
    text: &str,
    backend: &str,
    language: &str,
    duration_secs: f64,
) {
    if !enabled {
        return;
    }
    let entry = HistoryEntry {
        timestamp: chrono::Local::now(),
        text: text.to_string(),
        backend: backend.to_string(),
        language: language.to_string(),
        duration_secs,
    };
    #[cfg(not(test))]
    if let Err(e) = whisrs::history::append_entry(&entry) {
        warn!("failed to save history entry: {e}");
    }
    #[cfg(test)]
    SAVED_HISTORY.with(|saved| saved.borrow_mut().push(entry.text));
}

#[cfg(test)]
thread_local! {
    /// Test seams: the history entries this thread saved and the done cues
    /// it played, neither written to the real history nor played.
    static SAVED_HISTORY: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static DONE_CUES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The texts this thread saved to history since the last call.
#[cfg(test)]
pub(crate) fn take_saved_history() -> Vec<String> {
    SAVED_HISTORY.with(|saved| std::mem::take(&mut *saved.borrow_mut()))
}

/// The done cues this thread played since the last call.
#[cfg(test)]
pub(crate) fn take_done_cues() -> usize {
    DONE_CUES.with(|cues| cues.replace(0))
}

/// The done cue of a dictation; test builds count it instead.
fn play_done_cue(volume: f32) {
    #[cfg(not(test))]
    whisrs::audio::feedback::play_done(volume);
    #[cfg(test)]
    {
        let _ = volume;
        DONE_CUES.with(|cues| cues.set(cues.get() + 1));
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

/// Whether streamed `next` needs a space to follow `before`: not when either
/// is empty, not when the boundary already has one, and not before closing
/// punctuation ([`leads_with_punct`]). The one rule for joining a batch to
/// the text already typed and, for `Phrases`, two deltas inside a batch.
fn needs_space_between(before: &str, next: &str) -> bool {
    !(before.is_empty()
        || next.is_empty()
        || before.ends_with(' ')
        || next.starts_with(' ')
        || leads_with_punct(next))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use audio_silence_gate::Vad;
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

    /// A default config with the given vocabulary, for the keyterm routing
    /// tests. Every `Config` field is `#[serde(default)]`, so an empty
    /// document deserializes to the defaults.
    fn config_with_vocabulary(vocabulary: &[&str]) -> Config {
        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.vocabulary = vocabulary.iter().map(|t| t.to_string()).collect();
        config
    }

    #[test]
    fn dictation_batch_config_carries_vocabulary_as_keyterms() {
        let config = config_with_vocabulary(&["whisrs", "Hyprland"]);
        let built = batch_transcription_config(&config, "en", BatchOptions::dictation().use_prompt);
        assert_eq!(built.keyterms, vec!["whisrs", "Hyprland"]);
        assert!(built.prompt.is_some());
    }

    #[test]
    fn command_mode_batch_config_sends_no_keyterms() {
        // Regression guard: keyterms must ride the same opt-out as `prompt`.
        // A spoken instruction is not content, so biasing it toward the
        // dictation vocabulary is wrong for the same reason the prompt is
        // withheld — see `BatchOptions::command_mode`.
        let config = config_with_vocabulary(&["whisrs", "Hyprland"]);
        let built =
            batch_transcription_config(&config, "en", BatchOptions::command_mode().use_prompt);
        assert!(
            built.keyterms.is_empty(),
            "command mode leaked the dictation vocabulary: {:?}",
            built.keyterms
        );
        assert!(built.prompt.is_none());
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

    /// The text channel sender, the cancel flag, the sink log, and the join
    /// handle of a test batcher.
    type TestBatcher = (
        tokio::sync::mpsc::Sender<String>,
        Arc<AtomicBool>,
        Arc<StdMutex<Vec<String>>>,
        tokio::task::JoinHandle<String>,
    );

    /// Spawn `run_typing_batcher` for a phrase backend with a recording sink.
    fn spawn_test_batcher() -> TestBatcher {
        spawn_test_batcher_with(StreamText::Phrases, str::to_string)
    }

    /// [`spawn_test_batcher`] for a `stream_text` backend, with a sink that
    /// hands on `deliver(text)` instead of `text`, the way the production
    /// sink folds line breaks.
    fn spawn_test_batcher_with(
        stream_text: StreamText,
        deliver: fn(&str) -> String,
    ) -> TestBatcher {
        let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let typed = Arc::new(StdMutex::new(Vec::<String>::new()));

        let sink_log = Arc::clone(&typed);
        let batcher = tokio::spawn(run_typing_batcher(
            text_rx,
            stream_text,
            Arc::clone(&cancel),
            Arc::new(TextLatch::default()),
            None,
            None,
            move |text| {
                let sink_log = Arc::clone(&sink_log);
                async move {
                    sink_log.lock().unwrap().push(text.clone());
                    deliver(&text)
                }
            },
        ));

        (text_tx, cancel, typed, batcher)
    }

    /// The streaming clipboard copy and history are built from what the
    /// batcher returns, so it must be the text the sink actually typed:
    /// control characters stripped before the sink sees them, and whatever
    /// the sink changed (line breaks folded at a terminal) kept.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_returns_the_sanitized_text_the_sink_handed_on() {
        let (text_tx, _cancel, typed, batcher) =
            spawn_test_batcher_with(StreamText::Phrases, |text| {
                crate::injection::fold_line_breaks_at_terminal(text, || true).into_owned()
            });

        text_tx.send("ls\u{1b}\r\nrm x".to_string()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // A batch of nothing but control characters never reaches the sink.
        text_tx.send("\u{1b}\u{8}".to_string()).await.unwrap();
        drop(text_tx);

        let full_text = batcher.await.unwrap();
        assert_eq!(*typed.lock().unwrap(), vec!["ls\nrm x".to_string()]);
        assert_eq!(full_text, "ls rm x");
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

    /// `[replacements]` reach the streaming path: the sink, and so the
    /// clipboard copy and history, get the replaced text.
    /// The batcher marks text typed only for a batch it hands to the sink,
    /// after filler removal: a lone "um" marks nothing, "hello" does. Once
    /// the no-speech stop closed the latch, nothing is typed.
    #[tokio::test]
    async fn typing_batcher_marks_only_typed_text() {
        let run = |latch: Arc<TextLatch>, deltas: &'static [&'static str]| async move {
            let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
            let batcher = tokio::spawn(run_typing_batcher(
                text_rx,
                StreamText::Phrases,
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&latch),
                Some(FillerFilter::new::<&str>(&[]).unwrap()),
                None,
                |text| async move { text },
            ));
            for delta in deltas {
                text_tx.send(delta.to_string()).await.unwrap();
            }
            drop(text_tx);
            batcher.await.unwrap()
        };
        let latch = Arc::new(TextLatch::default());
        assert_eq!(run(Arc::clone(&latch), &["um"]).await, "");
        assert!(!latch.is_typed(), "a filler alone is not typed text");
        assert!(latch.close(), "the stop may still claim the session");

        let latch = Arc::new(TextLatch::default());
        assert_eq!(run(Arc::clone(&latch), &["um", "hello"]).await, "hello");
        assert!(latch.is_typed());
        assert!(!latch.close(), "typed text holds the stop off");

        let latch = Arc::new(TextLatch::default());
        assert!(latch.close());
        assert_eq!(run(Arc::clone(&latch), &["hello"]).await, "");
        assert!(!latch.is_typed());
    }

    #[tokio::test]
    async fn typing_batcher_applies_replacements() {
        let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
        let table = [("package build".to_string(), "PKGBUILD".to_string())].into();
        let replacer = Replacer::new(&table).unwrap();
        let batcher = tokio::spawn(run_typing_batcher(
            text_rx,
            StreamText::Phrases,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TextLatch::default()),
            None,
            replacer,
            |text| async move { text },
        ));
        text_tx
            .send("edit the Package Build file".to_string())
            .await
            .unwrap();
        drop(text_tx);
        assert_eq!(batcher.await.unwrap(), "edit the PKGBUILD file");
    }

    /// Send `deltas` back to back (one batch window) to a `stream_text`
    /// batcher; return what reached the sink and the transcript.
    async fn type_in_one_batch(stream_text: StreamText, deltas: &[&str]) -> (Vec<String>, String) {
        let (text_tx, _cancel, typed, batcher) =
            spawn_test_batcher_with(stream_text, str::to_string);
        for delta in deltas {
            text_tx.send(delta.to_string()).await.unwrap();
        }
        drop(text_tx);
        let full_text = batcher.await.unwrap();
        let typed = typed.lock().unwrap().clone();
        (typed, full_text)
    }

    #[test]
    fn needs_space_between_rule() {
        assert!(needs_space_between("a", "b"));
        assert!(needs_space_between("a.", "(b)"));
        assert!(!needs_space_between("", "b"), "nothing before");
        assert!(!needs_space_between("a", ""), "nothing to separate");
        assert!(!needs_space_between("a ", "b"));
        assert!(!needs_space_between("a", " b"));
        assert!(!needs_space_between("a", ", b"));
        assert!(!needs_space_between("a", "\u{201D}"));
    }

    /// Phrase backends (Deepgram streaming, Groq, local whisper) send
    /// trimmed phrases: two in one batch window used to be glued ("itcopies").
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_spaces_phrases_in_one_batch() {
        let (typed, full_text) = type_in_one_batch(StreamText::Phrases, &["a", "b"]).await;
        assert_eq!(typed, ["a b"]);
        assert_eq!(full_text, "a b");
    }

    #[tokio::test(start_paused = true)]
    async fn typing_batcher_puts_no_space_before_closing_punctuation_in_a_batch() {
        let (typed, full_text) =
            type_in_one_batch(StreamText::Phrases, &["word", ", next", "."]).await;
        assert_eq!(typed, ["word, next."]);
        assert_eq!(full_text, "word, next.");
    }

    /// A space already at the boundary, on either side, is not doubled.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_adds_no_second_space_between_phrases() {
        let (typed, full_text) =
            type_in_one_batch(StreamText::Phrases, &["one ", "two", " three"]).await;
        assert_eq!(typed, ["one two three"]);
        assert_eq!(full_text, "one two three");
    }

    /// OpenAI realtime append deltas split words and carry their own spaces.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_glues_fragments_in_one_batch() {
        let (typed, full_text) =
            type_in_one_batch(StreamText::Fragments, &["Deep", "gram", " is", " fast"]).await;
        assert_eq!(typed, ["Deepgram is fast"]);
        assert_eq!(full_text, "Deepgram is fast");
    }

    /// The real trigger: phrases that arrive while the sink is still typing
    /// (or waiting on a held modifier) queue up and come out as one batch.
    #[tokio::test(start_paused = true)]
    async fn typing_batcher_spaces_phrases_queued_while_the_sink_is_busy() {
        let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
        let release = Arc::new(tokio::sync::Notify::new());
        let typed = Arc::new(StdMutex::new(Vec::<String>::new()));

        let sink_release = Arc::clone(&release);
        let sink_log = Arc::clone(&typed);
        let batcher = tokio::spawn(run_typing_batcher(
            text_rx,
            StreamText::Phrases,
            Arc::new(AtomicBool::new(false)),
            Arc::new(TextLatch::default()),
            None,
            None,
            move |text| {
                let release = Arc::clone(&sink_release);
                let sink_log = Arc::clone(&sink_log);
                async move {
                    let first = {
                        let mut log = sink_log.lock().unwrap();
                        log.push(text.clone());
                        log.len() == 1
                    };
                    // The first batch blocks the sink until released.
                    if first {
                        release.notified().await;
                    }
                    text
                }
            },
        ));

        text_tx.send("it".to_string()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(*typed.lock().unwrap(), ["it"], "the sink is now busy");

        text_tx
            .send("copies to clipboard.".to_string())
            .await
            .unwrap();
        text_tx.send("Okay.".to_string()).await.unwrap();
        release.notify_one();
        drop(text_tx);

        let full_text = batcher.await.unwrap();
        assert_eq!(
            *typed.lock().unwrap(),
            ["it", " copies to clipboard. Okay."]
        );
        assert_eq!(full_text, "it copies to clipboard. Okay.");
    }

    // ── BatchOptions / BatchOrigin decision logic ───────────────────────

    /// Pins the exact option set each production caller uses (dictation in
    /// `process_recording_batch`, the two command flows in `command_mode.rs`).
    /// A flipped flag here is a behavior change on a real path — in
    /// particular, recovery-save and filler removal are dictation-only.
    #[test]
    fn batch_options_canonical_sets() {
        let cases = [
            (
                "dictation",
                BatchOptions::dictation(),
                BatchOptions {
                    use_prompt: true,
                    save_recovery: true,
                    check_prompt_echo: true,
                    apply_filler: true,
                    apply_replacements: true,
                    origin: BatchOrigin::Dictation,
                },
            ),
            (
                "command_mode",
                BatchOptions::command_mode(),
                BatchOptions {
                    use_prompt: false,
                    save_recovery: false,
                    check_prompt_echo: false,
                    apply_filler: false,
                    apply_replacements: false,
                    origin: BatchOrigin::CommandMode,
                },
            ),
            (
                "llm_command",
                BatchOptions::llm_command("proofread"),
                BatchOptions {
                    use_prompt: true,
                    save_recovery: false,
                    check_prompt_echo: true,
                    apply_filler: false,
                    apply_replacements: false,
                    origin: BatchOrigin::LlmCommand { name: "proofread" },
                },
            ),
        ];
        for (name, actual, expected) in cases {
            assert_eq!(actual, expected, "option set for {name}");
        }
    }

    /// `secs` seconds of 50 ms capture chunks of digital silence, a loud
    /// square wave (speech to the RMS test) from `speech_at` seconds on.
    fn capture_chunks(secs: usize, speech_at: Option<usize>) -> Vec<Vec<i16>> {
        let loud: Vec<i16> = (0..800)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect();
        (0..secs * 20)
            .map(|c| match speech_at {
                Some(at) if c >= at * 20 => loud.clone(),
                _ => vec![0; 800],
            })
            .collect()
    }

    /// A synthetic voice at an RMS of `dbfs`: a 120 Hz pulse train through
    /// three formants of four vowels in turn, in 0.25 s syllables. earshot
    /// calls it speech at -40 dBFS; at -62 dBFS it calls a frame here and
    /// there speech, never enough to arm auto-stop.
    fn synthetic_voice(secs: usize, dbfs: f64) -> Vec<i16> {
        formant_voice(secs, dbfs, 3, 0.25, 0.5)
    }

    /// [`synthetic_voice`] with F4 and F5 too, in 0.2 s syllables held at
    /// full level between ramps. earshot hears this one under close's
    /// -65 dBFS floor: at -70 dBFS the gate calls it too quiet, its voiced
    /// frames at -68 dBFS (too quiet from -68 to -73 dBFS, never armed).
    pub(crate) fn quiet_voice(secs: usize, dbfs: f64) -> Vec<i16> {
        formant_voice(secs, dbfs, 5, 0.2, 0.15)
    }

    /// A 120 Hz pulse train through the first `formants` (3 to 5) formants
    /// of four vowels in turn, in syllables of `syllable_secs` voiced for
    /// 80% of it, scaled to an RMS of `dbfs`. Each syllable rises and falls
    /// over `ramp` of its voiced part (0.5: a sine bump).
    fn formant_voice(
        secs: usize,
        dbfs: f64,
        formants: usize,
        syllable_secs: f64,
        ramp: f64,
    ) -> Vec<i16> {
        use std::f64::consts::PI;
        let rate = f64::from(SAMPLE_RATE);
        let vowels = [
            [700.0, 1220.0, 2600.0, 3300.0, 3750.0],
            [300.0, 2300.0, 3000.0, 3500.0, 4200.0],
            [500.0, 1000.0, 2500.0, 3400.0, 3900.0],
            [400.0, 2000.0, 2600.0, 3300.0, 4000.0],
        ];
        let bandwidths = [80.0, 100.0, 150.0, 250.0, 300.0];
        let mut phase = 0.0;
        let mut state = [[0.0f64; 2]; 5];
        let voice: Vec<f64> = (0..secs * SAMPLE_RATE as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let syllable = (t / syllable_secs) as usize;
                let within = (t % syllable_secs) / syllable_secs / 0.8;
                let envelope = if within < 1.0 {
                    (PI / 2.0 * (within.min(1.0 - within) / ramp).min(1.0)).sin()
                } else {
                    0.0
                };
                phase += (120.0 + 15.0 * (2.0 * PI * 0.7 * t).sin()) / rate;
                let mut y = if phase >= 1.0 {
                    phase -= 1.0;
                    envelope
                } else {
                    0.0
                };
                let vowel = vowels[syllable % vowels.len()];
                for ((past, f), bandwidth) in
                    state.iter_mut().zip(vowel).zip(bandwidths).take(formants)
                {
                    let r: f64 = (-PI * bandwidth / rate).exp();
                    let v = y + 2.0 * r * (2.0 * PI * f / rate).cos() * past[0] - r * r * past[1];
                    *past = [v, past[0]];
                    y = v;
                }
                y
            })
            .collect();
        let rms = (voice.iter().map(|x| x * x).sum::<f64>() / voice.len() as f64).sqrt();
        let gain = 10f64.powf(dbfs / 20.0) * f64::from(i16::MAX) / rms;
        voice
            .iter()
            .map(|x| (x * gain).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    /// The chunks the no-speech timeout fires on, fed as the capture loops
    /// feed it (the auto-stop detector first, then what it heard).
    fn no_speech_fires(vad: Vad, silence_timeout_ms: u64, chunks: &[Vec<i16>]) -> Vec<usize> {
        let mut auto_stop =
            AutoStopDetector::with_vad(VadConfig::close(vad), silence_timeout_ms, SAMPLE_RATE);
        let mut no_speech = NoSpeechTimeout::new(silence_timeout_ms, "test");
        (0..chunks.len())
            .filter(|&c| {
                auto_stop.feed(&chunks[c]);
                no_speech.feed(chunks[c].len(), Heard::by(&auto_stop))
            })
            .collect()
    }

    /// Nothing heard, no frame and no text.
    const NOTHING: Heard = Heard {
        first: None,
        last: None,
        armed: false,
        text: false,
    };

    /// A heard frame of 256 samples at `start_sample`.
    fn heard_frame(start_sample: u64, soft: bool) -> HeardFrame {
        HeardFrame {
            start_sample,
            len: 256,
            score: 0.31,
            dbfs: -48.6,
            soft,
        }
    }

    /// The detector heard frames `first` to `last` (sample starts).
    fn heard_between(first: u64, last: u64) -> Heard {
        Heard {
            first: Some(heard_frame(first, false)),
            last: Some(heard_frame(last, false)),
            ..NOTHING
        }
    }

    /// 15 s of steady rumble at -54 dBFS, a quiet room's background: white
    /// noise through a one-pole low-pass at about 160 Hz, seeded.
    fn room_noise(secs: usize) -> Vec<i16> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut low = 0.0f64;
        let noise: Vec<f64> = (0..secs * SAMPLE_RATE as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let white = 2.0 * ((state >> 11) as f64 / (1u64 << 53) as f64) - 1.0;
                low += 0.06 * (white - low);
                low
            })
            .collect();
        let rms = (noise.iter().map(|x| x * x).sum::<f64>() / noise.len() as f64).sqrt();
        let gain = 10f64.powf(-54.0 / 20.0) * f64::from(i16::MAX) / rms;
        noise.iter().map(|x| (x * gain).round() as i16).collect()
    }

    /// `secs` s of digital silence with a knock at each of `at` (seconds):
    /// 32 ms at -49.4 dBFS, inside one 50 ms chunk. The RMS test hears a
    /// 16 ms frame of it, while the chunk stays under the threshold, so
    /// auto-stop never arms: a knock on the desk, or a door.
    fn knocks(secs: usize, at: &[f64]) -> Vec<Vec<i16>> {
        let mut samples = vec![0i16; secs * SAMPLE_RATE as usize];
        for &t in at {
            let start = (t * f64::from(SAMPLE_RATE)) as usize;
            assert_eq!(start % 800, 0, "a knock starts a chunk");
            for (i, s) in samples[start..start + 512].iter_mut().enumerate() {
                *s = if i % 2 == 0 { 111 } else { -111 };
            }
        }
        samples.chunks(800).map(<[i16]>::to_vec).collect()
    }

    /// Nothing heard: it fires once, on the chunk that completes 10 s (the
    /// 200th), with either detector, over digital silence and (earshot)
    /// over a quiet room's rumble.
    #[test]
    fn no_speech_timeout_fires_at_10_s_with_no_speech() {
        for vad in [Vad::Earshot, Vad::Rms] {
            assert_eq!(
                no_speech_fires(vad, 2000, &capture_chunks(30, None)),
                [199],
                "{vad:?}"
            );
        }
        let room: Vec<Vec<i16>> = room_noise(15).chunks(800).map(<[i16]>::to_vec).collect();
        assert_eq!(no_speech_fires(Vad::Earshot, 2000, &room), [199]);
        let mut timeout = NoSpeechTimeout::new(2000, "test");
        assert!(!timeout.feed(159_999, NOTHING));
        assert!(timeout.feed(1, NOTHING));
        assert!(!timeout.feed(800, NOTHING), "once");
    }

    /// Every heard frame starts the wait again: knocks at 0.2, 4 and 8 s,
    /// each heard and none arming, stop the session 10 s after the last
    /// one (its frame ends at 8.03 s), on the chunk that completes 18.05 s.
    /// Heard as a latch, the rule before, they kept it recording to the
    /// stop press.
    #[test]
    fn heard_sounds_slide_the_no_speech_timeout() {
        assert_eq!(
            no_speech_fires(Vad::Rms, 2000, &knocks(30, &[0.2, 4.0, 8.0])),
            [360]
        );
        // One knock at 4 s: 14.05 s.
        assert_eq!(no_speech_fires(Vad::Rms, 2000, &knocks(30, &[4.0])), [280]);

        // Counted from the end of the latest heard frame.
        let mut timeout = NoSpeechTimeout::new(2000, "test");
        assert!(!timeout.feed(9 * 16_000, heard_between(8 * 16_000, 8 * 16_000)));
        assert!(!timeout.feed(16_000, heard_between(8 * 16_000, 9 * 16_000)));
        let after = 9 * 16_000 + 256 + 160_000;
        assert!(!timeout.feed(
            after - 1 - 10 * 16_000,
            heard_between(8 * 16_000, 9 * 16_000)
        ));
        assert!(timeout.feed(1, heard_between(8 * 16_000, 9 * 16_000)));
    }

    /// Once auto-stop armed, or once transcript text came, it never fires,
    /// however long nothing is heard after.
    #[test]
    fn no_speech_timeout_never_fires_once_armed_or_typed() {
        let mut chunks = capture_chunks(9, None);
        chunks.extend(capture_chunks(1, Some(0)));
        chunks.extend(capture_chunks(30, None));
        assert_eq!(
            no_speech_fires(Vad::Rms, 2000, &chunks),
            Vec::<usize>::new()
        );

        let armed = Heard {
            armed: true,
            ..heard_between(9 * 16_000, 9 * 16_000)
        };
        let mut timeout = NoSpeechTimeout::new(2000, "test");
        assert!(!timeout.feed(9 * 16_000, NOTHING));
        assert!(!timeout.feed(800, armed));
        assert!(!timeout.feed(30 * 16_000, armed));

        let mut timeout = NoSpeechTimeout::new(2000, "test");
        assert!(!timeout.feed(9 * 16_000, NOTHING));
        assert!(!timeout.feed(800, NOTHING.with_text(true)));
        assert!(!timeout.feed(30 * 16_000, NOTHING), "text latches for good");
    }

    /// What the turn's levels line says was heard.
    #[test]
    fn heard_label_names_the_first_and_latest_heard_frames() {
        let mut timeout = NoSpeechTimeout::new(2000, "test");
        timeout.feed(800, NOTHING);
        assert_eq!(timeout.heard_label(), "heard nothing");
        timeout.feed(3200, heard_between(3328, 3328));
        assert_eq!(timeout.heard_label(), "heard at 0.21 s");
        timeout.feed(16_000 * 8, heard_between(3328, 128_000));
        assert_eq!(timeout.heard_label(), "heard at 0.21 to 8.00 s");
        timeout.feed(16_000, heard_between(3328, 128_000).with_text(true));
        assert_eq!(
            timeout.heard_label(),
            "heard at 0.21 to 8.00 s, text at 9.25 s"
        );
        let mut timeout = NoSpeechTimeout::new(2000, "test");
        timeout.feed(8000, NOTHING.with_text(true));
        assert_eq!(timeout.heard_label(), "heard nothing, text at 0.50 s");
    }

    /// A voice that starts at 8.5 s, too quiet ever to arm auto-stop
    /// (earshot hears a frame of it here and there), is heard before 10 s
    /// and keeps being heard: it is never cut off. 10 s after its last
    /// heard frame the session stops. The same voice at -40 dBFS arms, and
    /// then the timeout never fires.
    #[test]
    fn a_soft_voice_from_8_5_s_is_never_cut_off() {
        let start = 17 * SAMPLE_RATE as usize / 2;
        let voice_end = start + 15 * SAMPLE_RATE as usize;
        let mut samples = vec![0i16; start];
        samples.extend(synthetic_voice(15, -62.0));
        samples.extend(vec![0i16; 15 * SAMPLE_RATE as usize]);
        let chunks: Vec<Vec<i16>> = samples.chunks(800).map(<[i16]>::to_vec).collect();
        let fired = no_speech_fires(Vad::Earshot, 2000, &chunks);
        assert_eq!(fired.len(), 1, "{fired:?}");
        let at = (fired[0] + 1) * 800;
        assert!(at > voice_end, "cut off at {:.2} s", seconds_at(at as u64));
        // Within a chunk and a frame of 10 s after it.
        assert!(
            at <= voice_end + 10 * SAMPLE_RATE as usize + 800 + 256,
            "stopped at {:.2} s",
            seconds_at(at as u64)
        );

        let mut samples = vec![0i16; start];
        samples.extend(synthetic_voice(4, -40.0));
        samples.extend(vec![0i16; 15 * SAMPLE_RATE as usize]);
        let chunks: Vec<Vec<i16>> = samples.chunks(800).map(<[i16]>::to_vec).collect();
        assert!(no_speech_fires(Vad::Earshot, 2000, &chunks).is_empty());
    }

    /// A voice earshot hears but never long enough to arm auto-stop (soft,
    /// or under the level floor) is heard all the same: the timeout never
    /// cuts it off. Arming, the rule before, would have fired at 10 s.
    #[test]
    fn a_voice_too_quiet_to_arm_auto_stop_holds_the_timeout_off() {
        let voice = synthetic_voice(12, -62.0);
        let chunks: Vec<Vec<i16>> = voice.chunks(800).map(<[i16]>::to_vec).collect();
        assert!(no_speech_fires(Vad::Earshot, 2000, &chunks).is_empty());

        let mut auto_stop =
            AutoStopDetector::with_vad(VadConfig::close(Vad::Earshot), 2000, SAMPLE_RATE);
        let mut heard_at = None;
        for (c, chunk) in chunks.iter().enumerate() {
            auto_stop.feed(chunk);
            if heard_at.is_none() && auto_stop.heard_voice() {
                heard_at = Some(c);
            }
        }
        assert!(!auto_stop.has_speech(), "it never arms");
        assert!(
            heard_at.is_some_and(|c| c < 20),
            "heard at chunk {heard_at:?}"
        );

        // The same voice at -40 dBFS arms (the generator is a voice to it).
        let mut auto_stop =
            AutoStopDetector::with_vad(VadConfig::close(Vad::Earshot), 2000, SAMPLE_RATE);
        auto_stop.feed(&synthetic_voice(2, -40.0));
        assert!(auto_stop.has_speech());
    }

    /// `silence_timeout_ms = 0` turns auto-stop off, and this with it.
    #[test]
    fn no_speech_timeout_is_off_with_auto_stop_off() {
        for vad in [Vad::Earshot, Vad::Rms] {
            assert!(no_speech_fires(vad, 0, &capture_chunks(60, None)).is_empty());
        }
        assert_eq!(no_speech_timeout(0), None);
    }

    /// The timeout is 10 s, or the auto-stop timeout when that is longer:
    /// `silence_timeout_ms = 20000` gives 20 s (the 400th chunk).
    #[test]
    fn no_speech_timeout_is_at_least_the_auto_stop_timeout() {
        assert_eq!(no_speech_timeout(2000), Some(Duration::from_secs(10)));
        assert_eq!(no_speech_timeout(10_000), Some(Duration::from_secs(10)));
        assert_eq!(no_speech_timeout(20_000), Some(Duration::from_secs(20)));
        assert_eq!(
            no_speech_timeout(12_500),
            Some(Duration::from_millis(12_500))
        );
        for vad in [Vad::Earshot, Vad::Rms] {
            assert_eq!(
                no_speech_fires(vad, 20_000, &capture_chunks(30, None)),
                [399],
                "{vad:?}"
            );
        }
        let mut timeout = NoSpeechTimeout::new(12_500, "test");
        assert!(!timeout.feed(199_999, NOTHING));
        assert!(timeout.feed(1, NOTHING));
    }

    /// A huge `silence_timeout_ms` saturates the limit in samples: 2^60 ms
    /// times 16 kHz would wrap to exactly 0 (a stop on the first chunk), and
    /// a debug build would panic.
    #[test]
    fn no_speech_timeout_saturates_a_huge_auto_stop_timeout() {
        for ms in [1 << 60, u64::MAX] {
            let mut timeout = NoSpeechTimeout::new(ms, "test");
            assert!(!timeout.feed(800, NOTHING), "{ms}");
            assert!(!timeout.feed(30 * 16_000, NOTHING), "{ms}");
        }
    }

    #[test]
    fn nothing_heard_message_per_origin() {
        let ten = Duration::from_secs(10);
        let cases = [
            (BatchOrigin::Dictation, "Stopped: nothing heard for 10 s"),
            (
                BatchOrigin::CommandMode,
                "Command mode: nothing heard for 10 s, stopped",
            ),
            (
                BatchOrigin::LlmCommand { name: "proofread" },
                "Stopped: nothing heard for 10 s",
            ),
            (
                BatchOrigin::Agent { name: "jarvis" },
                "Agent 'jarvis': nothing heard for 10 s, stopped",
            ),
        ];
        for (origin, expected) in cases {
            assert_eq!(
                origin.nothing_heard_message(ten, None),
                expected,
                "{origin:?}"
            );
        }
        // The length is the one that applied.
        assert_eq!(
            BatchOrigin::Dictation.nothing_heard_message(Duration::from_secs(20), None),
            "Stopped: nothing heard for 20 s"
        );
        assert_eq!(
            BatchOrigin::CommandMode.nothing_heard_message(Duration::from_millis(12_500), None),
            "Command mode: nothing heard for 12.5 s, stopped"
        );

        // A voice the level floors rejected came before: named, with its
        // level.
        let levels = Levels {
            speech_dbfs: Some(-68.0),
            noise_dbfs: None,
            snr_db: None,
            clipped_share: 0.0,
            dc_offset: 0.0,
            speech_share: 0.0,
            peak_dbfs: -60.0,
            duration_ms: 13_000,
        };
        let cases = [
            (
                BatchOrigin::Dictation,
                "Stopped: the audio was too quiet (speech at -68 dB)",
            ),
            (
                BatchOrigin::CommandMode,
                "Command mode: the audio was too quiet (speech at -68 dB), stopped",
            ),
            (
                BatchOrigin::LlmCommand { name: "proofread" },
                "Stopped: the audio was too quiet (speech at -68 dB)",
            ),
            (
                BatchOrigin::Agent { name: "jarvis" },
                "Agent 'jarvis': the audio was too quiet (speech at -68 dB), stopped",
            ),
        ];
        for (origin, expected) in cases {
            assert_eq!(
                origin.nothing_heard_message(ten, Some(&levels)),
                expected,
                "{origin:?}"
            );
        }
    }

    /// A streaming backend that sends `text` once it has received `after`
    /// samples, and no other text.
    struct ScriptedStream {
        text_after: Option<(usize, &'static str)>,
    }

    #[async_trait::async_trait]
    impl TranscriptionBackend for ScriptedStream {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            unreachable!("a streaming session never sends a file")
        }

        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> Result<whisrs::transcription::Transcript> {
            unreachable!("a streaming session never sends a file")
        }

        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }

        async fn transcribe_stream(
            &self,
            mut audio_rx: tokio::sync::mpsc::Receiver<whisrs::audio::AudioChunk>,
            text_tx: tokio::sync::mpsc::Sender<String>,
            _config: &TranscriptionConfig,
        ) -> Result<()> {
            let mut received = 0;
            let mut pending = self.text_after;
            while let Some(chunk) = audio_rx.recv().await {
                received += chunk.len();
                if let Some((_, text)) = pending.filter(|&(after, _)| received >= after) {
                    pending = None;
                    let _ = text_tx.send(text.to_string()).await;
                }
            }
            Ok(())
        }
    }

    /// What [`streaming_run`] saw.
    #[derive(Debug)]
    struct StreamingRun {
        /// The chunks sent before the session left Recording on its own, and
        /// the state it went to.
        stopped: Option<(usize, State)>,
        text: String,
        cancelled: bool,
        /// The pipeline released its cancel slot.
        released: bool,
        state: State,
        toasts: Vec<String>,
        history: Vec<String>,
        done_cues: usize,
    }

    /// How [`streaming_run`] ends a session still recording after its audio.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum End {
        /// The stop press, then `whisrs cancel` while it drains (#154), so
        /// nothing reaches the clipboard.
        Cancel,
        /// The stop press alone. Only for a session the backend sends no
        /// text: under `clipboard_only` its text would reach the clipboard.
        Stop,
    }

    /// `secs` s of digital silence.
    fn silence(secs: usize) -> Vec<i16> {
        vec![0; secs * SAMPLE_RATE as usize]
    }

    /// A streaming dictation over `samples` in 50 ms chunks, paced in
    /// (paused) real time, on [`ScriptedStream`]. Toasts, history and the
    /// done cue are on (recorded, not shown, by the test seams), the overlay
    /// off, and nothing is typed (`clipboard_only`). A session still
    /// recording at the end is ended as `end` says.
    async fn streaming_run(
        text_after: Option<(usize, &'static str)>,
        silence_timeout_ms: u64,
        samples: &[i16],
        end: End,
    ) -> StreamingRun {
        streaming_run_with(
            text_after,
            silence_timeout_ms,
            samples,
            end,
            RunOpts::default(),
        )
        .await
    }

    /// What else a [`streaming_run_with`] does.
    #[derive(Debug, Clone, Copy, Default)]
    struct RunOpts {
        /// Filler removal on (the default list).
        filler: bool,
        /// Hold the daemon-state lock over sending this chunk (index) and
        /// 300 ms after it: a no-speech stop it completes waits for the lock
        /// while the typing loop goes on.
        hold_lock_at: Option<usize>,
    }

    /// [`streaming_run`] with `opts`.
    async fn streaming_run_with(
        text_after: Option<(usize, &'static str)>,
        silence_timeout_ms: u64,
        samples: &[i16],
        end: End,
        opts: RunOpts,
    ) -> StreamingRun {
        if end == End::Stop {
            assert!(
                text_after.is_none(),
                "a stop without the cancel sends no text"
            );
        }
        crate::notify::take_sent_toasts();
        take_saved_history();
        take_done_cues();
        let config: Config = toml::from_str("").unwrap();
        let daemon_state = Arc::new(tokio::sync::Mutex::new(DaemonState::new()));
        let cancel_flag = Arc::new(AtomicBool::new(false));
        {
            let mut ds = daemon_state.lock().await;
            ds.state_machine.transition(Action::Toggle).unwrap();
            ds.streaming_cancel = Some(Arc::clone(&cancel_flag));
            ds.session_language = Some("en".into());
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = tokio::spawn(run_streaming_pipeline(StreamingPipelineParams {
            audio_rx: rx,
            backend: Arc::new(ScriptedStream { text_after }),
            daemon_state: Arc::clone(&daemon_state),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            state_tx: StateBus::new(),
            cancel_flag: Arc::clone(&cancel_flag),
            config: build_transcription_config(&config, "en"),
            window_id: None,
            language: "en".into(),
            notify: true,
            overlay_enabled: false,
            silence_timeout_ms,
            filler_enabled: opts.filler,
            filler_words: Vec::new(),
            replacements: Default::default(),
            audio_feedback: true,
            audio_feedback_volume: 0.5,
            backend_name: "test".into(),
            keys: KeystrokeSettings::from_config(&config.input),
            clipboard_fallback: false,
            clipboard_only: true,
            terminal_classes: Vec::new(),
            history: true,
        }));
        let mut stopped = None;
        for (c, chunk) in samples.chunks(800).enumerate() {
            if opts.hold_lock_at == Some(c) {
                let held = daemon_state.lock().await;
                tx.send(chunk.to_vec()).unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                drop(held);
            } else {
                tx.send(chunk.to_vec()).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let state = daemon_state.lock().await.state_machine.state();
            if state != State::Recording {
                stopped = Some((c + 1, state));
                break;
            }
        }
        if stopped.is_none() {
            // The stop press, then (`End::Cancel`) `whisrs cancel` while it
            // drains.
            let mut ds = daemon_state.lock().await;
            ds.state_machine.transition(Action::Toggle).unwrap();
            if end == End::Cancel {
                ds.streaming_cancel
                    .take()
                    .unwrap()
                    .store(true, Ordering::SeqCst);
            }
        }
        // The capture's channel closes, as a stopped capture's does.
        drop(tx);
        let text = pipeline.await.unwrap().unwrap();
        let ds = daemon_state.lock().await;
        StreamingRun {
            stopped,
            text,
            cancelled: cancel_flag.load(Ordering::SeqCst),
            released: ds.streaming_cancel.is_none(),
            state: ds.state_machine.state(),
            toasts: crate::notify::take_sent_toasts(),
            history: take_saved_history(),
            done_cues: take_done_cues(),
        }
    }

    /// Nothing heard for 10 s: the pipeline stops the session itself
    /// (Recording to Transcribing on the 200th chunk), sets its cancel flag,
    /// says so, saves nothing, plays no done cue and ends in Idle.
    #[tokio::test(start_paused = true)]
    async fn a_streaming_dictation_that_hears_nothing_stops_at_10_s() {
        let run = streaming_run(None, 2000, &silence(15), End::Cancel).await;
        assert_eq!(run.stopped, Some((200, State::Transcribing)), "{run:?}");
        assert!(run.cancelled && run.released, "{run:?}");
        assert_eq!(run.toasts, ["Stopped: nothing heard for 10 s"]);
        assert!(run.history.is_empty());
        assert_eq!(run.done_cues, 0);
        assert_eq!((run.state, run.text.as_str()), (State::Idle, ""));
    }

    /// The first transcript text holds the timeout off for good: a delta at
    /// 9.5 s, with nothing the detector calls speech, keeps the session
    /// recording to the stop press at 25 s, past any deadline.
    #[tokio::test(start_paused = true)]
    async fn a_streaming_delta_before_10_s_holds_the_timeout_off() {
        let run = streaming_run(Some((152_000, "hello")), 2000, &silence(25), End::Cancel).await;
        assert_eq!(run.stopped, None, "{run:?}");
        assert!(run.toasts.is_empty(), "{:?}", run.toasts);
        // Ended by the cancel: discarded.
        assert_eq!((run.state, run.text.as_str()), (State::Idle, ""));
        assert!(run.history.is_empty());

        // Without it, the same session stops at 10 s.
        let run = streaming_run(Some((200_000, "hello")), 2000, &silence(15), End::Cancel).await;
        assert_eq!(run.stopped, Some((200, State::Transcribing)), "{run:?}");
    }

    /// Only text that is typed counts: a lone "um" at 9.5 s, which filler
    /// removal leaves nothing of, keeps nothing on, and the session stops
    /// at 10 s as if no text had come. "hello" there keeps it recording.
    #[tokio::test(start_paused = true)]
    async fn a_delta_filler_removal_empties_does_not_hold_the_timeout_off() {
        let filler = RunOpts {
            filler: true,
            ..RunOpts::default()
        };
        let run = streaming_run_with(
            Some((152_000, "um")),
            2000,
            &silence(15),
            End::Cancel,
            filler,
        )
        .await;
        assert_eq!(run.stopped, Some((200, State::Transcribing)), "{run:?}");
        assert_eq!(run.toasts, ["Stopped: nothing heard for 10 s"]);
        let run = streaming_run_with(
            Some((152_000, "hello")),
            2000,
            &silence(15),
            End::Cancel,
            filler,
        )
        .await;
        assert_eq!(run.stopped, None, "{run:?}");
    }

    /// Text typed while the no-speech stop waits for the daemon-state lock
    /// keeps the session: the stop completes on the 200th chunk, but the
    /// lock is held while the typing loop types "hello" (sent after the
    /// 199th), and under it the stop finds the text typed. It never
    /// discards a typed word.
    #[tokio::test(start_paused = true)]
    async fn text_typed_while_the_stop_waits_for_the_lock_keeps_the_session() {
        let held = RunOpts {
            hold_lock_at: Some(199),
            ..RunOpts::default()
        };
        let run = streaming_run_with(
            Some((159_200, "hello")),
            2000,
            &silence(15),
            End::Cancel,
            held,
        )
        .await;
        assert_eq!(run.stopped, None, "{run:?}");
        assert!(run.toasts.is_empty(), "{:?}", run.toasts);
    }

    /// `silence_timeout_ms = 20000`: the session hears nothing for 20 s
    /// before it stops, and the toast says 20 s.
    #[tokio::test(start_paused = true)]
    async fn a_longer_auto_stop_timeout_lengthens_the_no_speech_timeout() {
        let run = streaming_run(None, 20_000, &silence(25), End::Cancel).await;
        assert_eq!(run.stopped, Some((400, State::Transcribing)), "{run:?}");
        assert_eq!(run.toasts, ["Stopped: nothing heard for 20 s"]);
        assert_eq!(run.state, State::Idle);
    }

    /// A voice the level floor rejects all of (12 s of [`quiet_voice`] at
    /// -70 dBFS) is heard while it lasts, so the session records past 10 s
    /// to the stop press, never armed; then the toast calls it too quiet with
    /// the voice's level, as the batch gate would. The stream's own levels
    /// say only "without speech", which a too-quiet voice outranks.
    #[tokio::test(start_paused = true)]
    async fn a_too_quiet_streaming_dictation_says_too_quiet() {
        let run = streaming_run(None, 2000, &quiet_voice(12, -70.0), End::Stop).await;
        assert_eq!(run.stopped, None, "{run:?}");
        assert!(!run.cancelled && run.released, "{run:?}");
        assert_eq!(
            run.toasts,
            ["Nothing typed: the audio was too quiet (speech at -68 dB)"]
        );
        assert!(run.history.is_empty());
        assert_eq!((run.state, run.text.as_str()), (State::Idle, ""));
    }

    /// The same voice for 3 s, then 12 s of nothing: the session stops 10 s
    /// after its last heard frame, never armed, and the toast calls what it
    /// heard too quiet. Discarded as for nothing heard.
    #[tokio::test(start_paused = true)]
    async fn a_too_quiet_voice_then_nothing_stops_and_says_too_quiet() {
        let mut samples = quiet_voice(3, -70.0);
        samples.extend(silence(12));
        let run = streaming_run(None, 2000, &samples, End::Cancel).await;
        let (chunks, state) = run.stopped.expect("stopped");
        assert_eq!(state, State::Transcribing);
        // Past 10 s (the latch's deadline had it heard nothing), within 10 s
        // of the voice's end.
        assert!((201..=261).contains(&chunks), "{run:?}");
        assert!(run.cancelled && run.released, "{run:?}");
        assert_eq!(
            run.toasts,
            ["Stopped: the audio was too quiet (speech at -68 dB)"]
        );
        assert!(run.history.is_empty());
        assert_eq!(run.done_cues, 0);
        assert_eq!((run.state, run.text.as_str()), (State::Idle, ""));
    }

    /// Per-origin gate-skip toast wording: command mode announces itself,
    /// dictation and llm-commands share the plain skip message.
    #[test]
    fn gate_skip_message_per_origin() {
        let cases = [
            (BatchOrigin::Dictation, "Skipped: the audio was silent"),
            (
                BatchOrigin::CommandMode,
                "Command mode: the audio was silent",
            ),
            (
                BatchOrigin::LlmCommand { name: "proofread" },
                "Skipped: the audio was silent",
            ),
        ];
        for (origin, expected) in cases {
            assert_eq!(origin.gate_skip_message("silent"), expected, "{origin:?}");
        }
        // The speech detector's reasons read the same way on every origin.
        use audio_silence_gate::GateReason;
        for reason in [GateReason::TooQuiet, GateReason::NoSpeech] {
            let label = reason.as_str();
            for (origin, expected) in [
                (
                    BatchOrigin::Dictation,
                    format!("Skipped: the audio was {label}"),
                ),
                (
                    BatchOrigin::CommandMode,
                    format!("Command mode: the audio was {label}"),
                ),
                (
                    BatchOrigin::LlmCommand { name: "proofread" },
                    format!("Skipped: the audio was {label}"),
                ),
                (
                    BatchOrigin::Agent { name: "jarvis" },
                    format!("Agent 'jarvis': the audio was {label}, nothing was sent"),
                ),
            ] {
                assert_eq!(origin.gate_skip_message(label), expected, "{origin:?}");
            }
        }
        assert_eq!(
            BatchOrigin::Dictation.gate_skip_message(GateReason::NoSpeech.as_str()),
            "Skipped: the audio was without speech"
        );
    }

    /// The skip toast names its reason with a level, on every origin, and
    /// silent audio asks about a muted mic after it.
    #[test]
    fn gate_skip_toast_names_a_level_per_reason_and_origin() {
        use audio_silence_gate::GateReason;
        let levels = Levels {
            speech_dbfs: Some(-68.0),
            noise_dbfs: Some(-48.0),
            snr_db: Some(-20.0),
            clipped_share: 0.0,
            dc_offset: 0.0,
            speech_share: 0.0,
            peak_dbfs: -82.0,
            duration_ms: 210,
        };
        let cases = [
            (
                GateReason::Silent,
                "silent (peak -82 dB)",
                ". Is the mic muted?",
            ),
            (GateReason::TooQuiet, "too quiet (speech at -68 dB)", ""),
            (
                GateReason::NoSpeech,
                "without speech (background at -48 dB)",
                "",
            ),
            (GateReason::TooShort, "too short (0.2 s)", ""),
        ];
        for (reason, label, hint) in cases {
            for (origin, expected) in [
                (
                    BatchOrigin::Dictation,
                    format!("Skipped: the audio was {label}{hint}"),
                ),
                (
                    BatchOrigin::CommandMode,
                    format!("Command mode: the audio was {label}{hint}"),
                ),
                (
                    BatchOrigin::LlmCommand { name: "proofread" },
                    format!("Skipped: the audio was {label}{hint}"),
                ),
                (
                    BatchOrigin::Agent { name: "jarvis" },
                    format!("Agent 'jarvis': the audio was {label}, nothing was sent{hint}"),
                ),
            ] {
                assert_eq!(
                    origin.gate_skip_toast(reason, &levels),
                    expected,
                    "{origin:?}"
                );
            }
        }
        // The examples the toasts were designed from.
        assert_eq!(
            BatchOrigin::Dictation.gate_skip_toast(GateReason::Silent, &levels),
            "Skipped: the audio was silent (peak -82 dB). Is the mic muted?"
        );
        assert_eq!(
            BatchOrigin::Agent { name: "jarvis" }.gate_skip_toast(GateReason::TooQuiet, &levels),
            "Agent 'jarvis': the audio was too quiet (speech at -68 dB), nothing was sent"
        );
        // A streaming dictation that typed nothing says the same.
        assert_eq!(
            nothing_typed_message(GateReason::NoSpeech, &levels),
            "Nothing typed: the audio was without speech (background at -48 dB)"
        );
        assert_eq!(
            nothing_typed_message(GateReason::Silent, &levels),
            "Nothing typed: the audio was silent (peak -82 dB). Is the mic muted?"
        );
        // With `vad` = rms a quiet room is silent too: no muted-mic hint.
        let quiet_room = Levels {
            peak_dbfs: -44.0,
            ..levels
        };
        assert_eq!(
            BatchOrigin::Dictation.gate_skip_toast(GateReason::Silent, &quiet_room),
            "Skipped: the audio was silent (peak -44 dB)"
        );
        assert_eq!(
            nothing_typed_message(GateReason::Silent, &quiet_room),
            "Nothing typed: the audio was silent (peak -44 dB)"
        );
    }

    /// Progress toast after the gate: dictation stays quiet; the command
    /// flows announce what they are doing, llm-commands by name.
    #[test]
    fn processing_message_per_origin() {
        let cases = [
            (BatchOrigin::Dictation, None),
            (
                BatchOrigin::CommandMode,
                Some("Processing command...".to_string()),
            ),
            (
                BatchOrigin::LlmCommand { name: "proofread" },
                Some("Processing 'proofread'...".to_string()),
            ),
        ];
        for (origin, expected) in cases {
            assert_eq!(origin.processing_message(), expected, "{origin:?}");
        }
    }

    /// Empty-transcript wording: dictation only logs; command mode blames the
    /// instruction, llm-commands the speech.
    #[test]
    fn empty_result_message_per_origin() {
        let cases = [
            (BatchOrigin::Dictation, None),
            (
                BatchOrigin::CommandMode,
                Some("Could not understand instruction — try again"),
            ),
            (
                BatchOrigin::LlmCommand { name: "proofread" },
                Some("Could not understand speech — try again"),
            ),
        ];
        for (origin, expected) in cases {
            assert_eq!(origin.empty_result_message(), expected, "{origin:?}");
        }
    }

    /// The echo-skip toast wording is origin-independent — every batch path
    /// sends the same message.
    #[test]
    fn prompt_echo_skip_toast_wording() {
        assert_eq!(
            PROMPT_ECHO_SKIP_TOAST,
            "Skipped: response looked like a prompt echo (no speech detected)"
        );
    }

    /// The prompt-echo guard needs all three conditions to fire: the calling
    /// path enables the check, the backend actually transmits the prompt, and
    /// a prompt exists.
    ///
    /// The llm-command path (whose transcript is LLM input) checks, while
    /// command mode (which sends no prompt) never does. The backend gate is
    /// issue #133: `config.prompt` is built for every backend, but Deepgram
    /// and `openai-compatible-realtime` never put it on the wire, so without
    /// that condition genuine speech resembling the user's own
    /// `[general] vocabulary` was discarded as an echo of something the model
    /// never saw. `openai-realtime` joins them on gpt-realtime-whisper, the
    /// one model that takes no prompt, which is why the flag is read per
    /// request rather than per backend.
    #[test]
    fn prompt_echo_guard_conditions() {
        let prompt = "Embedded Linux dictation about Hyprland and whisrs";
        // Contained verbatim in the prompt → `is_prompt_echo` fires.
        let echoed = "Embedded Linux dictation";
        // Unrelated short utterance → never an echo.
        let genuine = "hello world";

        let cases = [
            (
                "dictation: echo with prompt drops",
                BatchOptions::dictation(),
                true,
                Some(prompt),
                echoed,
                true,
            ),
            (
                "dictation: no prompt sent, nothing to echo",
                BatchOptions::dictation(),
                true,
                None,
                echoed,
                false,
            ),
            (
                "dictation: genuine speech passes",
                BatchOptions::dictation(),
                true,
                Some(prompt),
                genuine,
                false,
            ),
            (
                "dictation: promptless backend, vocabulary-shaped speech passes (#133)",
                BatchOptions::dictation(),
                false,
                Some(prompt),
                echoed,
                false,
            ),
            (
                "llm_command: echo with prompt drops",
                BatchOptions::llm_command("proofread"),
                true,
                Some(prompt),
                echoed,
                true,
            ),
            (
                "llm_command: promptless backend, echo-shaped text passes (#133)",
                BatchOptions::llm_command("proofread"),
                false,
                Some(prompt),
                echoed,
                false,
            ),
            (
                "command_mode: check disabled, echo-shaped text passes",
                BatchOptions::command_mode(),
                true,
                Some(prompt),
                echoed,
                false,
            ),
        ];
        for (name, opts, backend_sends_prompt, prompt, text, expect_drop) in cases {
            assert_eq!(
                opts.should_drop_as_echo(backend_sends_prompt, prompt, text),
                expect_drop,
                "{name}"
            );
        }
    }

    // ── The #133 gate is wired into the batch path ──────────────────────
    //
    // `should_drop_as_echo` is covered above and the per-backend answers are
    // covered next to each backend, but neither proves `transcribe_batch_audio`
    // ever *asks* the backend. Hardcoding `true` at that call site — which
    // disables the whole fix — left every one of those tests green. That is the
    // failure shape of #71 all over again, so the two tests below drive the real
    // function and flip nothing but `sends_prompt()`.

    /// A backend that returns a fixed transcript and answers `sends_prompt()`
    /// however the test asks it to. Nothing else about the batch path cares.
    struct StubBackend {
        text: &'static str,
        sends_prompt: bool,
    }

    #[async_trait::async_trait]
    impl TranscriptionBackend for StubBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> anyhow::Result<String> {
            Ok(self.text.to_string())
        }

        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            self.sends_prompt
        }

        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }

        // The batch path never asks for segments; recordings have their own
        // fakes in `whisrs::recording::process`.
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> anyhow::Result<whisrs::transcription::Transcript> {
            unreachable!("the dictation batch path does not request segments")
        }

        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    /// Terms a user would put in `[general] vocabulary`. They are what
    /// `batch_transcription_config` turns into the prompt.
    const WIRING_VOCABULARY: [&str; 4] = ["Hyprland", "whisrs", "Wayland", "systemd"];

    /// Genuine speech that normalizes to a substring of the prompt built from
    /// `WIRING_VOCABULARY`, so `is_prompt_echo` flags it. Someone dictating a
    /// run of their own vocabulary terms produces exactly this shape — which
    /// is what issue #133 reported.
    const VOCABULARY_SHAPED_SPEECH: &str = "Hyprland, whisrs, Wayland";

    /// `notify: false` keeps `send_notification` (which spawns a D-Bus thread)
    /// out of the picture on both branches. `[audio] vad = "rms"`: the test
    /// audio is a square wave, loud but no voice, which earshot would gate.
    fn context_with_backend(backend: StubBackend) -> DaemonContext {
        let mut config = config_with_vocabulary(&WIRING_VOCABULARY);
        config.audio.vad = "rms".to_string();
        DaemonContext {
            config,
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            transcription_backend: Arc::new(backend),
            notify: false,
            state_tx: StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            #[cfg(feature = "recording")]
            recording: crate::recording::RecordingRuntime::new().0,
            agents: crate::agent::AgentRuntime::disabled(),
            agent_board: tokio::sync::watch::channel(Vec::new()).0,
            agent_overlay_shown: AtomicBool::new(false),
        }
    }

    /// Half a second of alternating full-ish amplitude: past `AUDIO_GATE_MIN_MS`
    /// and far above `SILENCE_RMS_THRESHOLD`, so the upstream gate lets the
    /// buffer reach the backend instead of short-circuiting to `Ok("")`.
    fn audio_that_passes_the_gate() -> Vec<i16> {
        (0..SAMPLE_RATE as usize / 2)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect()
    }

    #[tokio::test]
    async fn batch_path_keeps_echo_shaped_text_from_a_promptless_backend() {
        let context = context_with_backend(StubBackend {
            text: VOCABULARY_SHAPED_SPEECH,
            sends_prompt: false,
        });

        let text = transcribe_batch_audio(
            &audio_that_passes_the_gate(),
            &context,
            "en",
            &BatchOptions::dictation(),
        )
        .await
        .expect("the stub backend never fails");

        assert_eq!(
            text, VOCABULARY_SHAPED_SPEECH,
            "the backend never sent the prompt, so this cannot be an echo of it (#133) — \
             does `transcribe_batch_audio` still pass `sends_prompt()` to `should_drop_as_echo`?"
        );
    }

    #[tokio::test]
    async fn batch_path_drops_echo_shaped_text_from_a_prompting_backend() {
        // Same audio, same transcript, same options — only the backend's
        // answer changes, which pins the flag as the thing being consulted.
        let context = context_with_backend(StubBackend {
            text: VOCABULARY_SHAPED_SPEECH,
            sends_prompt: true,
        });

        let text = transcribe_batch_audio(
            &audio_that_passes_the_gate(),
            &context,
            "en",
            &BatchOptions::dictation(),
        )
        .await
        .expect("the stub backend never fails");

        assert!(
            text.is_empty(),
            "a backend that does send the prompt must still have echoes filtered, got {text:?}"
        );
    }

    /// The gate runs `[audio] vad`: hiss at -40 dBFS reaches the backend
    /// with the RMS test and is skipped as "without speech" with earshot.
    #[tokio::test]
    async fn batch_gate_runs_the_configured_vad() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let hiss: Vec<i16> = (0..2 * SAMPLE_RATE as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % 1141) as i16 - 570
            })
            .collect();
        let mut context = context_with_backend(StubBackend {
            text: "heard",
            sends_prompt: false,
        });
        let opts = BatchOptions::dictation();
        let sent = transcribe_batch_audio(&hiss, &context, "en", &opts).await;
        assert_eq!(sent.unwrap(), "heard");
        context.config.audio.vad = "earshot".to_string();
        let gated = transcribe_batch_audio(&hiss, &context, "en", &opts).await;
        assert_eq!(gated.unwrap(), "", "earshot heard speech in hiss");
        let gate = speech_gate(
            &hiss,
            SAMPLE_RATE,
            AUDIO_GATE_MIN_MS,
            &VadConfig::close(context.config.audio.vad()),
        );
        assert_eq!(gate.reason, Some(audio_silence_gate::GateReason::NoSpeech));
    }

    /// `[replacements]` is wired into the batch path, not just the streaming
    /// batcher, and stays off for command mode, whose transcript is an
    /// instruction for the LLM rather than text to type.
    #[tokio::test]
    async fn batch_path_applies_replacements_to_dictation_only() {
        const SPOKEN: &str = "edit the package build file";
        let mut context = context_with_backend(StubBackend {
            text: SPOKEN,
            sends_prompt: false,
        });
        context
            .config
            .replacements
            .insert("package build".to_string(), "PKGBUILD".to_string());

        let dictated = transcribe_batch_audio(
            &audio_that_passes_the_gate(),
            &context,
            "en",
            &BatchOptions::dictation(),
        )
        .await
        .expect("the stub backend never fails");
        assert_eq!(dictated, "edit the PKGBUILD file");

        let instruction = transcribe_batch_audio(
            &audio_that_passes_the_gate(),
            &context,
            "en",
            &BatchOptions::command_mode(),
        )
        .await
        .expect("the stub backend never fails");
        assert_eq!(instruction, SPOKEN);
    }

    // ── Toggle-path LLM post-processing (issue #85) ─────────────────────

    /// Parse a config from TOML, with a `[general] backend` already set so
    /// only the keys under test have to be spelled out.
    fn config_from(general_extra: &str) -> Config {
        toml::from_str(&format!("[general]\nbackend = \"groq\"\n{general_extra}\n"))
            .expect("test config parses")
    }

    /// The flag is opt-in: an untouched config never post-processes, whatever
    /// the (defaulted) instruction says.
    #[test]
    fn llm_post_process_off_by_default() {
        let config = config_from("");
        assert!(!config.general.llm_post_process);
        assert!(llm_post_process_instruction(&config).is_none());
    }

    /// Flag on: the trimmed instruction is what gets sent to the LLM.
    #[test]
    fn llm_post_process_instruction_is_trimmed() {
        let config =
            config_from("llm_post_process = true\nllm_instruction = \"  Translate to German.  \"");
        assert_eq!(
            llm_post_process_instruction(&config),
            Some("Translate to German.")
        );
    }

    /// Flag on with the key omitted still has something to apply — the built
    /// in cleanup instruction — so the flag alone is a working configuration.
    #[test]
    fn llm_post_process_uses_default_instruction_when_key_omitted() {
        let config = config_from("llm_post_process = true");
        let instruction = llm_post_process_instruction(&config).expect("default instruction");
        assert!(
            instruction.contains("Return only the corrected text"),
            "unexpected default instruction: {instruction}"
        );
    }

    /// A blank instruction disables post-processing rather than sending an
    /// empty instruction to the LLM.
    #[test]
    fn llm_post_process_blank_instruction_skips() {
        let config = config_from("llm_post_process = true\nllm_instruction = \"   \"");
        assert!(llm_post_process_instruction(&config).is_none());
    }

    /// The failure contract: only a non-blank completion may replace the
    /// transcript. Errors, timeouts (both `None`) and blank completions all
    /// keep the user's words — the toggle path never loses a dictation to a
    /// post-processing problem.
    ///
    /// The `post_processed` flag tracks the same split, because it is what
    /// `whisrs log` is tagged from: a fallback holds the raw transcript, so it
    /// must not be labelled as the LLM's work.
    #[test]
    fn post_processed_or_raw_falls_back_to_the_transcript() {
        let raw = || "hello world".to_string();
        let cases = [
            (
                "rewrite wins",
                Some("Hello, world.".to_string()),
                "Hello, world.",
                true,
            ),
            ("llm error or timeout", None, "hello world", false),
            (
                "empty completion",
                Some(String::new()),
                "hello world",
                false,
            ),
            (
                "whitespace-only completion",
                Some("  \n ".to_string()),
                "hello world",
                false,
            ),
            (
                "trailing newline is trimmed, it would submit the line in a terminal",
                Some("Hello, world.\n".to_string()),
                "Hello, world.",
                true,
            ),
            (
                "surrounding whitespace is trimmed",
                Some("\n  Hello, world.  \n\n".to_string()),
                "Hello, world.",
                true,
            ),
            (
                "a wrapping fence is stripped, as on the llm-command path",
                Some("```text\nHello, world.\n```".to_string()),
                "Hello, world.",
                true,
            ),
            (
                "CRLF is normalized so no bare CR is typed as Return",
                Some("Hello,\r\nworld.".to_string()),
                "Hello,\nworld.",
                true,
            ),
        ];
        for (name, rewritten, expected, post_processed) in cases {
            let outcome = post_processed_or_raw(raw(), rewritten);
            assert_eq!(outcome.text, expected, "{name}");
            assert_eq!(
                outcome.post_processed, post_processed,
                "{name}: post_processed must be true only when the LLM produced the text \
                 that was typed"
            );
        }
    }

    /// A dictation that was not post-processed keeps the bare backend name it
    /// has always been logged under, byte for byte. Anything else would shift
    /// the meaning of every pre-existing history entry.
    #[test]
    fn history_backend_tag_is_unchanged_for_raw_dictation() {
        for backend in ["groq", "deepgram", "openai", "local-whisper", "asr-sidecar"] {
            assert_eq!(history_backend_tag(backend, false), backend);
        }
    }

    /// A post-processed dictation is tagged `<backend>+llm`, so `whisrs log`
    /// shows which entries hold the LLM's words rather than the microphone's.
    #[test]
    fn history_backend_tag_marks_post_processed_dictation() {
        assert_eq!(history_backend_tag("groq", true), "groq+llm");
        assert_eq!(history_backend_tag("deepgram", true), "deepgram+llm");
    }

    /// The tag must not collide with the llm-command path's `llm:<name>`
    /// entries (`command_mode.rs`), which name a configured command rather
    /// than a transcription backend. Keeping the backend in front and the
    /// marker behind means neither form can be mistaken for the other, even
    /// when an `[[llm_commands]]` entry is named after a backend.
    #[test]
    fn history_backend_tag_does_not_collide_with_llm_command_tags() {
        let post_processed = history_backend_tag("groq", true);
        assert!(!post_processed.starts_with("llm:"));
        assert_ne!(post_processed, "llm:groq");
    }

    /// The streaming path can only ever produce raw dictation, so its
    /// constructor must never claim otherwise.
    #[test]
    fn dictation_outcome_raw_is_not_post_processed() {
        let outcome = DictationOutcome::raw("hello".to_string());
        assert!(!outcome.post_processed);
        assert_eq!(
            history_backend_tag("openai-realtime", outcome.post_processed),
            "openai-realtime"
        );
    }

    /// Scripted clipboard double, copied verbatim from the `injection.rs`
    /// tests. Deliberately a third copy of that exact shape rather than a
    /// fourth shape or a shared helper: this crate has no test-support
    /// module, and adding one would mean reworking the doubles in
    /// `injection.rs` and `selection.rs`, which this change has no business
    /// touching. `texts` is the sequence of `get_text` results
    /// (each call consumes the front entry; the last entry is sticky),
    /// `writes` records every `set_text` so tests can assert the
    /// fallback/restore policy byte-for-byte — including that nothing is
    /// ever written. An empty `texts` list makes any `get_text` panic, which
    /// is itself an assertion in the streaming tests below (the streaming
    /// path must never read the clipboard).
    struct ScriptedClipboard {
        texts: StdMutex<Vec<Option<String>>>,
        writes: StdMutex<Vec<String>>,
        fail_writes: bool,
    }

    impl ScriptedClipboard {
        fn new(texts: &[Option<&str>]) -> Self {
            Self {
                texts: StdMutex::new(texts.iter().map(|t| t.map(String::from)).collect()),
                writes: StdMutex::new(Vec::new()),
                fail_writes: false,
            }
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl ClipboardBackend for ScriptedClipboard {
        fn get_text(&self) -> anyhow::Result<String> {
            let mut texts = self.texts.lock().unwrap();
            assert!(!texts.is_empty(), "get_text called with no scripted result");
            let head = if texts.len() > 1 {
                texts.remove(0)
            } else {
                texts[0].clone()
            };
            head.ok_or_else(|| anyhow::anyhow!("clipboard holds non-text content"))
        }

        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            if self.fail_writes {
                return Err(anyhow::anyhow!("mock clipboard write failure"));
            }
            self.writes.lock().unwrap().push(text.to_string());
            Ok(())
        }

        fn get_primary_selection(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    /// The plain stop path with `[input] clipboard_fallback` on: exactly one
    /// copy of the final transcript, never a partial delta and never a
    /// duplicate. The empty script is itself an assertion — the streaming
    /// path must never *read* the clipboard, so any read panics.
    #[tokio::test]
    async fn streaming_stop_copies_the_final_transcript_once() {
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        write_streaming_clipboard(
            "hello world",
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
            /* cancelled = */ false,
            clipboard.clone(),
        )
        .await;

        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    /// `[input] clipboard_only` reaches the same copy on its own, with the
    /// fallback off: in copy-only mode the clipboard write is the entire
    /// output of the dictation, so gating it on `clipboard_fallback` would
    /// silently produce nothing at all.
    #[tokio::test]
    async fn streaming_clipboard_only_copies_the_final_transcript() {
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        write_streaming_clipboard(
            "hello world",
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ true,
            /* cancelled = */ false,
            clipboard.clone(),
        )
        .await;

        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    /// `whisrs cancel` discards. The transcript is non-empty and the
    /// fallback is on, so `cancelled` is the only thing standing between
    /// this run and a clipboard write — which is the point: the batch path
    /// produces nothing on cancel, and a streaming backend that copied
    /// anyway would be the streaming/batch divergence class from #54.
    #[tokio::test]
    async fn streaming_cancel_copies_nothing() {
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        write_streaming_clipboard(
            "hello world",
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
            /* cancelled = */ true,
            clipboard.clone(),
        )
        .await;

        assert!(
            clipboard.writes().is_empty(),
            "a cancelled run must leave the clipboard exactly as it found it"
        );
    }

    /// A recording that transcribed to nothing (silence, an accidental
    /// tap) must not overwrite whatever the user already had copied. The
    /// fallback is on and the run was not cancelled, so the empty string is
    /// the only reason to skip.
    #[tokio::test]
    async fn streaming_empty_transcript_never_clobbers_the_clipboard() {
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        write_streaming_clipboard(
            "",
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
            /* cancelled = */ false,
            clipboard.clone(),
        )
        .await;

        assert!(
            clipboard.writes().is_empty(),
            "an empty transcript must not clobber the user's clipboard"
        );
    }

    /// The default configuration: both flags off, a perfectly ordinary
    /// successful dictation. Typing already delivered the text, so the
    /// clipboard is none of our business.
    #[tokio::test]
    async fn streaming_without_fallback_never_touches_the_clipboard() {
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        write_streaming_clipboard(
            "hello world",
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ false,
            /* cancelled = */ false,
            clipboard.clone(),
        )
        .await;

        assert!(
            clipboard.writes().is_empty(),
            "clipboard must stay untouched when neither clipboard mode is on"
        );
    }
}
