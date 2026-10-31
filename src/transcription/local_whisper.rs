//! Local whisper.cpp transcription backend via `whisper-rs`.
//!
//! Streaming uses silence-delimited phrase segmentation by default
//! (`segmentation = "silence"`): incoming audio is split into phrases at
//! natural pauses (>= `phrase_silence_ms` of silence) and each phrase is
//! decoded exactly once — no overlapping windows, no deduplication, and no
//! feedback of prior output. Feeding each window's transcription back as
//! `initial_prompt` was the root cause of the repetition/invented-text loops
//! in issue #55, so the only prompt ever passed to the decoder is the static
//! vocabulary prompt from the user's config.
//!
//! Pause-free speech is force-cut by the splitter before it outgrows
//! whisper's 30 s context window (soft cap at a genuinely silent frame, hard
//! ceiling regardless); the phrase after such a cut is decoded on its own,
//! and its spurious leading capital — whisper capitalizes each decode
//! independently — is lowercased (see
//! [`Phrase::continuation`](super::phrase_split::Phrase::continuation)).
//!
//! The legacy sliding-window mode (`segmentation = "window"`) keeps the
//! 8s/2s overlapping windows with text-based n-gram overlap removal
//! (`asr_dedup::TextDedup`) for lower-latency partial output.
//!
//! `segmentation = "none"` turns streaming off: [`supports_streaming`]
//! returns `false`, so the daemon records the whole session and hands it to
//! [`transcribe`] in one call when recording stops. Nothing is typed while
//! you talk, and the batch-only dictation features (`[general]
//! llm_post_process`, `[input] paste`) apply.
//!
//! Every [`transcribe`] call longer than one decode window (28 s) is cut at
//! pauses (or at 28 s in continuous speech) and decoded a window at a time,
//! so no window's text can set off a loop in the next (issue #208). That is
//! dictation in `"none"`, and command mode and `[[llm_commands]]` whatever
//! `segmentation` says, since they always take the batch path.
//!
//! Both streaming modes create one decoder state per stream and reuse it
//! across decodes; see [`create_state`].
//!
//! [`supports_streaming`]: TranscriptionBackend::supports_streaming
//! [`transcribe`]: TranscriptionBackend::transcribe

use std::sync::Arc;

use asr_dedup::TextDedup;
use async_trait::async_trait;
use audio_silence_gate::{speech_gate, Vad, VadConfig};
use prompt_echo::is_prompt_echo;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::phrase_split::{
    decode_windows, dictation_windows, DecodeWindow, PhraseSplitter, MAX_DECODE_WINDOW_SECS,
};
use super::verbose_json::is_no_speech;
use super::{
    decode_wav_samples, samples_to_ms, Segment, StreamText, Transcript, TranscriptionBackend,
    TranscriptionConfig, LOCAL_CHUNK_SECS,
};
use crate::audio::AudioChunk;

/// Sliding window parameters for the legacy "window" streaming mode.
const WINDOW_SECS: usize = 8;
const STEP_SECS: usize = 2;
const SAMPLE_RATE: usize = 16_000;
const WINDOW_SAMPLES: usize = WINDOW_SECS * SAMPLE_RATE;
const STEP_SAMPLES: usize = STEP_SECS * SAMPLE_RATE;
/// Shorter initial window for faster first result.
const INITIAL_WINDOW_SECS: usize = 4;
const INITIAL_WINDOW_SAMPLES: usize = INITIAL_WINDOW_SECS * SAMPLE_RATE;

/// whisper.cpp produces no output for buffers shorter than ~1 s of audio;
/// short phrases are zero-padded up to this length before decoding.
const MIN_DECODE_SAMPLES: usize = SAMPLE_RATE + SAMPLE_RATE / 10; // 1.1 s

/// How streaming audio is segmented before decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SegmentationMode {
    /// Split at natural pauses; decode each phrase exactly once (default).
    #[default]
    Silence,
    /// Legacy overlapping sliding window with text-based dedup.
    Window,
    /// No streaming: decode the whole recording after it stops, in one call
    /// up to 28 s and a window at a time past that (config value `"none"`).
    Unsegmented,
}

impl SegmentationMode {
    /// Parse a config string; unknown values warn and fall back to `Silence`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "silence" => Self::Silence,
            "window" => Self::Window,
            "none" => Self::Unsegmented,
            other => {
                warn!("unknown local-whisper segmentation {other:?}, using \"silence\"");
                Self::Silence
            }
        }
    }
}

/// Local whisper.cpp transcription backend.
pub struct LocalWhisperBackend {
    ctx: Option<Arc<whisper_rs::WhisperContext>>,
    model_path: String,
    segmentation: SegmentationMode,
    phrase_silence_ms: u64,
}

impl LocalWhisperBackend {
    /// Create a new local whisper backend, eagerly loading the model.
    ///
    /// Defaults to silence-delimited phrase segmentation; see
    /// [`Self::with_segmentation`].
    pub fn new(model_path: String) -> Self {
        Self::with_flash_attn(model_path, false)
    }

    /// Like [`Self::new`], with whisper.cpp flash attention on or off.
    ///
    /// It is a context parameter, so it is fixed when the model loads. It
    /// disables DTW token timestamps, which whisrs does not use.
    pub fn with_flash_attn(model_path: String, flash_attn: bool) -> Self {
        let ctx = match Self::load_model(&model_path, flash_attn) {
            Ok(ctx) => {
                info!("loaded whisper model from {model_path}");
                Some(Arc::new(ctx))
            }
            Err(e) => {
                warn!("failed to load whisper model from {model_path}: {e}");
                None
            }
        };
        Self {
            ctx,
            model_path,
            segmentation: SegmentationMode::default(),
            // Same default as `crate::LocalWhisperConfig` — lib.rs is the
            // single source of truth for segmentation defaults.
            phrase_silence_ms: crate::default_phrase_silence_ms(),
        }
    }

    /// Configure the streaming segmentation strategy.
    ///
    /// `mode` is `"silence"` (default), `"window"` or `"none"`;
    /// `phrase_silence_ms` is how much continuous silence ends a phrase in
    /// silence mode, and (capped at 900 ms) where recordings and batch calls
    /// past 28 s are cut.
    pub fn with_segmentation(mut self, mode: &str, phrase_silence_ms: u64) -> Self {
        self.segmentation = SegmentationMode::parse(mode);
        self.phrase_silence_ms = phrase_silence_ms.max(1);
        self
    }

    fn load_model(path: &str, flash_attn: bool) -> anyhow::Result<whisper_rs::WhisperContext> {
        if !std::path::Path::new(path).exists() {
            anyhow::bail!("model file not found: {path}. Run 'whisrs setup' to download a model.");
        }

        let mut params = whisper_rs::WhisperContextParameters::default();
        params.flash_attn(flash_attn);

        whisper_rs::WhisperContext::new_with_params(path, params)
            .map_err(|e| anyhow::anyhow!("failed to initialize whisper context: {e}"))
    }

    /// Silence-delimited streaming: split audio into phrases at pauses and
    /// decode each phrase exactly once, in FIFO order.
    async fn stream_phrases(
        &self,
        ctx: &Arc<whisper_rs::WhisperContext>,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        // The close profile: one user near the mic, so a TV under them is
        // not typed.
        let mut splitter = PhraseSplitter::new(
            SAMPLE_RATE,
            VadConfig::close(config.vad),
            self.phrase_silence_ms,
        );
        let mut state = create_state(ctx)?;

        while let Some(chunk) = audio_rx.recv().await {
            for phrase in splitter.feed(&chunk) {
                state = decode_phrase(
                    ctx,
                    state,
                    phrase.samples,
                    phrase.continuation,
                    config,
                    &text_tx,
                )
                .await?;
            }
        }

        // End of stream: flush the trailing in-progress phrase (if any),
        // exactly once. No rewind, no overlap re-send.
        if let Some(phrase) = splitter.flush() {
            decode_phrase(
                ctx,
                state,
                phrase.samples,
                phrase.continuation,
                config,
                &text_tx,
            )
            .await?;
        }

        Ok(())
    }

    /// Legacy sliding-window streaming with text-based overlap removal.
    async fn stream_windows(
        &self,
        ctx: &Arc<whisper_rs::WhisperContext>,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let mut buffer: Vec<i16> = Vec::new();
        let mut dedup = TextDedup::new();
        let mut next_process_at = INITIAL_WINDOW_SAMPLES;
        let mut last_processed_end: usize = 0;
        // Static vocabulary prompt only — never prior transcription output.
        let prompt = config.prompt.clone().filter(|p| !p.is_empty());
        let mut state = create_state(ctx)?;

        while let Some(chunk) = audio_rx.recv().await {
            buffer.extend_from_slice(&chunk);

            while buffer.len() >= next_process_at {
                let window_size = if last_processed_end == 0 {
                    INITIAL_WINDOW_SAMPLES.min(buffer.len())
                } else {
                    WINDOW_SAMPLES.min(buffer.len())
                };

                let window_end = next_process_at;
                let window_start = window_end.saturating_sub(window_size);
                let window = buffer[window_start..window_end].to_vec();

                // Skip windows without speech.
                if holds_speech(&window, config.vad) {
                    let samples_f32 = i16_to_f32(&window);
                    let lang = config.language.clone();
                    let is_multilingual = ctx.is_multilingual();
                    let prompt = prompt.clone();
                    let mut decode_state = state;

                    match tokio::task::spawn_blocking(move || {
                        let result = run_whisper_inference(
                            &mut decode_state,
                            &samples_f32,
                            &lang,
                            is_multilingual,
                            prompt.as_deref(),
                        );
                        (decode_state, result)
                    })
                    .await
                    {
                        Ok((returned, Ok(full_text))) => {
                            state = returned;
                            // Use text-based dedup to extract only the new portion.
                            let new_text = dedup.filter_text(&full_text);
                            if !new_text.trim().is_empty() {
                                debug!("streaming window produced: {:?}", new_text);
                                text_tx.send(new_text).await.ok();
                            }
                        }
                        Ok((stale, Err(e))) => {
                            warn!("whisper window inference failed: {e}");
                            // Free the failed state before allocating its
                            // replacement, so recovery never needs two states
                            // resident at once.
                            drop(stale);
                            state = create_state(ctx)?;
                        }
                        Err(e) => {
                            warn!("whisper task panicked: {e}");
                            // The state died with the panicked task; make a
                            // fresh one so later windows can still decode.
                            state = create_state(ctx)?;
                        }
                    }
                } else {
                    debug!(
                        "skipping silent window at samples {}..{}",
                        window_start, window_end
                    );
                }

                last_processed_end = window_end;
                next_process_at += STEP_SAMPLES;
            }
        }

        // Process remaining audio not covered by the last window.
        if buffer.len() > last_processed_end {
            let remaining_start = if buffer.len() - last_processed_end < SAMPLE_RATE {
                last_processed_end.saturating_sub(WINDOW_SAMPLES / 4)
            } else {
                last_processed_end
            };
            let remaining = &buffer[remaining_start..];

            if holds_speech(remaining, config.vad) {
                let samples_f32 = i16_to_f32(remaining);
                let lang = config.language.clone();
                let is_multilingual = ctx.is_multilingual();
                let prompt = prompt.clone();
                // Final decode of the stream — the state is consumed.
                let mut decode_state = state;

                match tokio::task::spawn_blocking(move || {
                    let result = run_whisper_inference(
                        &mut decode_state,
                        &samples_f32,
                        &lang,
                        is_multilingual,
                        prompt.as_deref(),
                    );
                    (decode_state, result)
                })
                .await
                {
                    Ok((_, Ok(full_text))) => {
                        let new_text = dedup.filter_text(&full_text);
                        if !new_text.trim().is_empty() {
                            text_tx.send(new_text).await.ok();
                        }
                    }
                    Ok((_, Err(e))) => warn!("whisper final inference failed: {e}"),
                    Err(e) => warn!("whisper final task panicked: {e}"),
                }
            }
        }

        Ok(())
    }

    /// Unsegmented mode over a stream: buffer everything, decode it at the
    /// end in one call or a window at a time as
    /// [`TranscriptionBackend::transcribe`] does, and send the text once. Up
    /// to 28 s the call is [`decode_phrase`], which only logs a failed decode
    /// where `transcribe` returns the error.
    ///
    /// The daemon never gets here, since this mode reports
    /// `supports_streaming() == false` and dictation takes the batch path
    /// instead. It exists so a direct `transcribe_stream` caller still gets
    /// batch semantics (one send) rather than a silent fallback to phrase
    /// streaming.
    async fn stream_unsegmented(
        &self,
        ctx: &Arc<whisper_rs::WhisperContext>,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let mut buffer: Vec<i16> = Vec::new();
        while let Some(chunk) = audio_rx.recv().await {
            buffer.extend_from_slice(&chunk);
        }

        // A whole session: a few hundred ms of model time for minutes of it.
        if !crate::audio::block_off_runtime(|| holds_speech(&buffer, config.vad)) {
            return Ok(());
        }

        if fits_one_decode_window(buffer.len()) {
            decode_phrase(ctx, create_state(ctx)?, buffer, false, config, &text_tx).await?;
            return Ok(());
        }

        let ctx = Arc::clone(ctx);
        let language = config.language.clone();
        let is_multilingual = ctx.is_multilingual();
        let prompt = config.prompt.clone().filter(|p| !p.is_empty());
        let phrase_silence_ms = self.phrase_silence_ms;
        let vad = config.vad;
        let text = tokio::task::spawn_blocking(move || {
            let windows = batch_windows(&buffer, phrase_silence_ms, vad);
            let mut state = create_state(&ctx)?;
            decode_batch_windows(
                &mut state,
                &i16_to_f32(&buffer),
                &windows,
                &language,
                is_multilingual,
                prompt.as_deref(),
            )
        })
        .await??;
        if !text.is_empty() {
            text_tx.send(text).await.ok();
        }
        Ok(())
    }

    /// Cut a recording chunk into phrases of at most ~28 s with the same
    /// splitter as silence-mode dictation.
    ///
    /// `segmentation` is not consulted: it picks how dictation streams, and
    /// `"none"` would hand whisper.cpp a whole chunk (up to ten minutes) in one
    /// `full()` call, where it carries text across its 30 s windows even with
    /// `no_context(true)`: the repetition loop of #55. `phrase_silence_ms` is
    /// capped ([`super::recording_phrase_silence_ms`]), so every join between
    /// the chunk's speech regions ends a phrase. A long batch dictation is cut
    /// the same way ([`batch_windows`]). `cfg` is the track's open profile
    /// ([`TranscriptionConfig::open_vad`]).
    fn recording_phrases(
        &self,
        samples: &[i16],
        cfg: VadConfig,
    ) -> Vec<super::phrase_split::Phrase> {
        capped_phrases(samples, self.phrase_silence_ms, None, cfg)
    }
}

/// Whether a dictation buffer holds speech worth decoding: the whole-buffer
/// gate on `vad` with the close profile (one user near the mic) and no
/// minimum length. With [`Vad::Rms`] it is the 0.1.31 test: not empty, and
/// an RMS of at least 0.003.
fn holds_speech(samples: &[i16], vad: Vad) -> bool {
    speech_gate(samples, SAMPLE_RATE as u32, 0, &VadConfig::close(vad))
        .reason
        .is_none()
}

/// The splitter behind [`LocalWhisperBackend::recording_phrases`] and
/// [`batch_windows`], for the configured `phrase_silence_ms` (capped by
/// [`super::recording_phrase_silence_ms`]), fed the whole buffer in one call
/// or, with `feed_len`, that many samples at a time. Both give the same
/// phrases. A free function, so a blocking task can run it.
///
/// Speech is decided by `cfg`, an open profile: these are cut points (a
/// recording track may carry voices at any level), so a quiet voice must
/// not be dropped.
fn capped_phrases(
    samples: &[i16],
    phrase_silence_ms: u64,
    feed_len: Option<usize>,
    cfg: VadConfig,
) -> Vec<super::phrase_split::Phrase> {
    let mut splitter = PhraseSplitter::new(
        SAMPLE_RATE,
        cfg,
        super::recording_phrase_silence_ms(phrase_silence_ms),
    );
    let mut phrases = match feed_len {
        None => splitter.feed(samples),
        Some(len) => samples
            .chunks(len)
            .flat_map(|chunk| splitter.feed(chunk))
            .collect(),
    };
    phrases.extend(splitter.flush());
    phrases
}

/// Whether a batch dictation of `samples` samples fits one decode window
/// ([`MAX_DECODE_WINDOW_SECS`]) and so decodes in a single `full()` call
/// over the whole buffer. A longer one is cut into windows
/// ([`batch_windows`]).
fn fits_one_decode_window(samples: usize) -> bool {
    samples <= MAX_DECODE_WINDOW_SECS * SAMPLE_RATE
}

/// The windows a batch dictation past one decode window is cut into at
/// pauses (or at 28 s in continuous speech), about
/// [`MAX_DECODE_WINDOW_SECS`] at most ([`dictation_windows`]), one `full()`
/// call each ([`decode_batch_windows`]).
///
/// The splitter is fed a second at a time: fed the whole buffer in one call,
/// it costs time with the square of its length, since each frame it drops
/// moves the rest of the buffer. It still runs in the blocking task with
/// the decode, not on the async thread.
fn batch_windows(samples: &[i16], phrase_silence_ms: u64, vad: Vad) -> Vec<DecodeWindow> {
    let phrases = capped_phrases(
        samples,
        phrase_silence_ms,
        Some(SAMPLE_RATE),
        VadConfig::open(vad),
    );
    let windows = dictation_windows(&phrases, SAMPLE_RATE);
    debug!(
        "local whisper: {} phrase(s) in {} decode window(s) (whisper full() calls) \
         for a {:.1}s dictation",
        phrases.len(),
        windows.len(),
        samples.len() as f64 / SAMPLE_RATE as f64
    );
    windows
}

/// Convert i16 PCM samples to f32 in the range [-1.0, 1.0].
#[cfg(any(feature = "local-whisper", test))]
fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples
        .iter()
        .map(|&s| s as f32 / i16::MAX as f32)
        .collect()
}

/// Lowercase the first character of `text` if it is an uppercase letter.
///
/// Applied to continuation phrases (those starting at a forced mid-speech
/// cut): whisper capitalizes each decode independently, so a continuation's
/// leading capital is spurious mid-sentence. Char-safe for UTF-8;
/// `char::to_lowercase` may expand to multiple chars.
#[cfg(any(feature = "local-whisper", test))]
fn decapitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        // The standalone pronoun "I" (and contractions: "I'm", "I'll") is
        // correctly capitalized mid-sentence; lowercasing it is visibly wrong.
        Some('I') if !chars.clone().next().is_some_and(|c| c.is_alphabetic()) => text.to_string(),
        Some(c) if c.is_alphabetic() && c.is_uppercase() => {
            let mut out: String = c.to_lowercase().collect();
            out.push_str(chars.as_str());
            out
        }
        _ => text.to_string(),
    }
}

/// Create a decoder state for a stream (or a single batch call).
///
/// Creating a state allocates all of whisper.cpp's KV and compute buffers —
/// on GPU backends that is a full device buffer build — so it must happen
/// once per stream and be reused across decodes, never once per phrase.
/// Reuse carries no decoder context between phrases: every decode sets
/// `set_no_context(true)` (see [`run_whisper_inference`]).
fn create_state(ctx: &whisper_rs::WhisperContext) -> anyhow::Result<whisper_rs::WhisperState> {
    ctx.create_state()
        .map_err(|e| anyhow::anyhow!("failed to create whisper state: {e}"))
}

/// Decode a single phrase and send its trimmed text over `text_tx`.
///
/// Each phrase is decoded exactly once with only the static config prompt.
/// `continuation` marks a phrase starting at a forced mid-speech cut; its
/// spurious leading capital is lowercased before sending.
/// The stream's reusable `state` is taken by value (it has to move into the
/// blocking task) and handed back on return; if the decode fails or the task
/// panics, that state is discarded and a fresh one is created to keep the
/// stream going.
async fn decode_phrase(
    ctx: &Arc<whisper_rs::WhisperContext>,
    mut state: whisper_rs::WhisperState,
    mut phrase: Vec<i16>,
    continuation: bool,
    config: &TranscriptionConfig,
    text_tx: &mpsc::Sender<String>,
) -> anyhow::Result<whisper_rs::WhisperState> {
    debug!(
        "decoding phrase of {:.2}s",
        phrase.len() as f64 / SAMPLE_RATE as f64
    );
    // whisper.cpp silently produces nothing for <1 s buffers; pad with silence.
    if phrase.len() < MIN_DECODE_SAMPLES {
        phrase.resize(MIN_DECODE_SAMPLES, 0);
    }

    let samples_f32 = i16_to_f32(&phrase);
    let lang = config.language.clone();
    let is_multilingual = ctx.is_multilingual();
    let prompt = config.prompt.clone().filter(|p| !p.is_empty());

    match tokio::task::spawn_blocking(move || {
        let result = run_whisper_inference(
            &mut state,
            &samples_f32,
            &lang,
            is_multilingual,
            prompt.as_deref(),
        );
        (state, result)
    })
    .await
    {
        Ok((state, Ok(text))) => {
            let text = text.trim();
            if !text.is_empty() {
                let text = if continuation {
                    decapitalize_first(text)
                } else {
                    text.to_string()
                };
                debug!("phrase produced: {:?}", text);
                text_tx.send(text).await.ok();
            }
            Ok(state)
        }
        Ok((stale, Err(e))) => {
            warn!("whisper phrase inference failed: {e}");
            // Free the failed state before allocating its replacement, so
            // recovery never needs two states resident at once.
            drop(stale);
            create_state(ctx)
        }
        Err(e) => {
            warn!("whisper phrase task panicked: {e}");
            create_state(ctx)
        }
    }
}

/// Run whisper inference on an audio buffer.
///
/// - `state`: a reusable decoder state (see [`create_state`]). Reuse carries
///   no context between calls: `set_no_context(true)` below decodes every
///   buffer independently.
/// - `prompt`: the static vocabulary/context hint from the user's config.
///   Never prior transcription output — feeding decoded text back as the
///   prompt caused the repetition loops of issue #55.
fn run_whisper_inference(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    language: &str,
    is_multilingual: bool,
    prompt: Option<&str>,
) -> anyhow::Result<String> {
    state
        .full(full_params(language, is_multilingual, prompt), audio)
        .map_err(|e| anyhow::anyhow!("whisper inference failed: {e}"))?;

    let mut text = String::new();
    for segment in state.as_iter() {
        text.push_str(&format!("{}", segment));
    }

    Ok(text.trim().to_string())
}

fn decoder_language(language: &str, multilingual: bool) -> Option<&str> {
    let language = language.trim();
    if language.is_empty() {
        return None;
    }
    if language.eq_ignore_ascii_case("auto") {
        return if multilingual { None } else { Some("en") };
    }
    Some(language)
}

/// Decode parameters shared by every whisper call, dictation and recordings
/// alike, so the invariants below cannot drift between the two.
fn full_params<'a>(
    language: &'a str,
    is_multilingual: bool,
    prompt: Option<&str>,
) -> whisper_rs::FullParams<'a, 'a> {
    use whisper_rs::{FullParams, SamplingStrategy};

    // `best_of: 1` is load-bearing beyond decode quality: it keeps whisper.cpp's
    // decoder count at 1, which is the only reason the library never takes the
    // path where it frees the state itself and hands back an error. Raising it,
    // or switching to beam search, needs the error arms above to stop dropping
    // the returned state.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    params.set_language(decoder_language(language, is_multilingual));

    // Static vocabulary/context hint (config `prompt` + `vocabulary`).
    if let Some(prompt) = prompt {
        if !prompt.is_empty() {
            params.set_initial_prompt(prompt);
        }
    }
    // Decode every buffer independently: carrying decoder context across
    // buffers amplified repetition/invented text (issue #55). The static
    // initial_prompt above is still applied.
    // Reusing one state across decodes depends on this staying true: it is what
    // drops the previous decode's text context (see `create_state`).
    params.set_no_context(true);
    // Fall back to a higher temperature sooner when the decoder gets stuck in
    // low-entropy (repetitive) output. whisper.cpp default is 2.4.
    params.set_entropy_thold(2.6);
    // Suppress non-speech tokens (e.g. "(silence)", "♪") on pause-heavy audio.
    params.set_suppress_nst(true);

    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
        .min(8);
    params.set_n_threads(n_threads);
    params
}

/// One whisper segment of a decoded buffer, timed relative to that buffer.
struct RawSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
    no_speech_prob: f64,
    avg_logprob: Option<f64>,
}

/// Decode a recording window and return whisper's own segments with their
/// timing and confidence. Dictation's parameters ([`full_params`]), with one
/// change: `n_max_text_ctx` 0.
///
/// A window fits one 30 s mel window, but `full()` can still take a second
/// pass: it seeks to the last timestamp it decoded and runs on over the
/// rest, and that pass is conditioned on the first pass's text
/// (`no_context` clears the carried text only when the call starts). That
/// is the carried context of the #55 repetition loop. With `n_max_text_ctx`
/// 0 no pass takes any prompt; in this whisper.cpp that includes the
/// initial prompt, so recordings send local whisper none (see
/// `crate::transcription::factory::recording_transcription_config`).
fn run_whisper_segments(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    language: &str,
    is_multilingual: bool,
    prompt: Option<&str>,
) -> anyhow::Result<Vec<RawSegment>> {
    let mut params = full_params(language, is_multilingual, prompt);
    params.set_n_max_text_ctx(0);
    state
        .full(params, audio)
        .map_err(|e| anyhow::anyhow!("whisper inference failed: {e}"))?;

    let mut out = Vec::new();
    for segment in state.as_iter() {
        // Mean log-probability of the text tokens; special tokens
        // (`[_BEG_]`, `<|en|>`, ...) carry no information about the speech.
        let mut logprob_sum = 0.0f64;
        let mut tokens = 0usize;
        for i in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(i) else {
                continue;
            };
            let is_special = token
                .to_str_lossy()
                .map(|t| t.starts_with("[_") || t.starts_with("<|"))
                .unwrap_or(true);
            if is_special {
                continue;
            }
            logprob_sum += f64::from(token.token_probability().max(1e-10)).ln();
            tokens += 1;
        }
        // t0/t1 are centiseconds.
        out.push(RawSegment {
            start_ms: segment.start_timestamp().max(0) as u64 * 10,
            end_ms: segment.end_timestamp().max(0) as u64 * 10,
            text: segment
                .to_str_lossy()
                .map(|t| t.into_owned())
                .unwrap_or_default(),
            no_speech_prob: f64::from(segment.no_speech_probability()),
            avg_logprob: (tokens > 0).then(|| logprob_sum / tokens as f64),
        });
    }
    Ok(out)
}

/// Whisper's segments for one decode window, placed on the chunk: blank and
/// no-speech segments dropped, times clamped into the window (its decoded
/// buffer may have been padded) and moved by its start, and the first kept
/// segment of a window that starts at a forced cut lowercased, since it
/// continues a sentence.
fn place_window_segments(raw: Vec<RawSegment>, window: &DecodeWindow) -> Vec<Segment> {
    let window_start_ms = samples_to_ms(window.range.start as u64);
    let window_len_ms = samples_to_ms(window.range.len() as u64);
    let mut out = Vec::new();
    for seg in raw {
        let text = seg.text.trim();
        if text.is_empty() || is_no_speech(Some(seg.no_speech_prob), seg.avg_logprob) {
            continue;
        }
        let text = if window.continuation && out.is_empty() {
            decapitalize_first(text)
        } else {
            text.to_string()
        };
        let start = seg.start_ms.min(window_len_ms);
        let end = seg.end_ms.clamp(start, window_len_ms);
        out.push(Segment {
            start_ms: window_start_ms + start,
            end_ms: window_start_ms + end,
            text,
            speaker: None,
            words: None,
        });
    }
    out
}

/// Decode a long batch dictation window by window on one reused state, one
/// `full()` call per [`DecodeWindow`], with dictation's parameters (the
/// static prompt included), and join the windows' text
/// ([`join_window_texts`]).
///
/// One `full()` call over the whole buffer conditions each 30 s mel window
/// on the text decoded in the one before (`no_context` clears it only when
/// the call starts), and on long dictations that text loops (issue #208).
/// A window fits one mel window; a second pass over its tail can still take
/// the first pass's text, since `n_max_text_ctx` 0 would drop the prompt too
/// (see [`run_whisper_segments`]).
/// Any window failing fails the whole call, so the daemon keeps the audio
/// rather than typing a transcript with a hole in it.
fn decode_batch_windows(
    state: &mut whisper_rs::WhisperState,
    samples: &[f32],
    windows: &[DecodeWindow],
    language: &str,
    is_multilingual: bool,
    prompt: Option<&str>,
) -> anyhow::Result<String> {
    decode_each_window(samples, windows, prompt, |buf| {
        run_whisper_inference(state, buf, language, is_multilingual, prompt)
    })
}

/// The loop of [`decode_batch_windows`], with the decode passed in so a test
/// can stand in for whisper: each window's samples, padded to
/// [`MIN_DECODE_SAMPLES`], go to `decode` in order, the first error ends the
/// call, and the texts are joined ([`join_window_texts`]).
fn decode_each_window(
    samples: &[f32],
    windows: &[DecodeWindow],
    prompt: Option<&str>,
    mut decode: impl FnMut(&[f32]) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let mut texts: Vec<String> = Vec::with_capacity(windows.len());
    for window in windows {
        let mut buf = samples[window.range.clone()].to_vec();
        // whisper.cpp silently produces nothing for <1 s buffers; pad with silence.
        if buf.len() < MIN_DECODE_SAMPLES {
            buf.resize(MIN_DECODE_SAMPLES, 0.0);
        }
        let text = decode(&buf)?;
        debug!(
            "batch window at {:.2}s ({:.1}s): {text:?}",
            window.range.start as f64 / SAMPLE_RATE as f64,
            window.range.len() as f64 / SAMPLE_RATE as f64
        );
        texts.push(text);
    }
    Ok(join_window_texts(windows, texts, prompt))
}

/// Fewest words a window needs before [`join_window_texts`] drops it as a
/// prompt echo: the word-run threshold of `prompt_echo::is_prompt_echo`
/// (6), whose substring check also flags shorter text. A short window the
/// prompt contains, such as a closing "Claude Code." after a vocabulary
/// prompt, is more likely real speech; dropping it here would lose it
/// silently, where the daemon's check keeps it inside the joined text.
/// Words are counted as that crate counts them: runs of alphanumeric
/// characters.
const MIN_ECHO_WINDOW_WORDS: usize = 6;

/// Join a batch dictation's window texts, in order, with one space. A blank
/// window adds nothing, a window starting at a forced cut is lowercased
/// ([`decapitalize_first`]), since it continues a sentence, and a window of
/// at least [`MIN_ECHO_WINDOW_WORDS`] words that echoes the prompt is
/// dropped: the daemon's own echo check sees only the joined text, where
/// one window's echo would pass.
fn join_window_texts(windows: &[DecodeWindow], texts: Vec<String>, prompt: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(texts.len());
    for (window, text) in windows.iter().zip(texts) {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let words = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .count();
        if words >= MIN_ECHO_WINDOW_WORDS
            && prompt.is_some_and(|prompt| is_prompt_echo(text, prompt))
        {
            warn!(
                "discarding likely prompt-echo window at {:.2}s ({} chars)",
                window.range.start as f64 / SAMPLE_RATE as f64,
                text.len()
            );
            continue;
        }
        parts.push(if window.continuation {
            decapitalize_first(text)
        } else {
            text.to_string()
        });
    }
    parts.join(" ")
}

/// Decode a recording chunk window by window on one reused state, one
/// `full()` call per [`DecodeWindow`].
///
/// Windows group the phrases of the same [`PhraseSplitter`] as streaming
/// dictation, at most ~28 s each, so each fits one 30 s mel window and a
/// call never decodes on into a second one. Passes inside a window take no
/// carried text ([`run_whisper_segments`]). Whisper's own segments are
/// kept, placed by [`place_window_segments`].
fn decode_recording_windows(
    ctx: &whisper_rs::WhisperContext,
    samples: &[i16],
    windows: &[DecodeWindow],
    language: &str,
    prompt: Option<&str>,
) -> anyhow::Result<Vec<Segment>> {
    let mut state = create_state(ctx)?;
    let mut segments = Vec::new();
    let mut failures = 0usize;
    let total = windows.len();

    for window in windows {
        let window_start_ms = samples_to_ms(window.range.start as u64);
        let mut buf = samples[window.range.clone()].to_vec();
        // whisper.cpp silently produces nothing for <1 s buffers; pad with silence.
        if buf.len() < MIN_DECODE_SAMPLES {
            buf.resize(MIN_DECODE_SAMPLES, 0);
        }
        match run_whisper_segments(
            &mut state,
            &i16_to_f32(&buf),
            language,
            ctx.is_multilingual(),
            prompt,
        ) {
            Ok(raw) => segments.extend(place_window_segments(raw, window)),
            Err(e) => {
                warn!("whisper recording window at {window_start_ms} ms failed: {e}");
                failures += 1;
                // Same recovery as streaming: free the failed state first.
                drop(state);
                state = create_state(ctx)?;
            }
        }
    }

    if total > 0 && failures == total {
        anyhow::bail!("whisper failed to decode every window of the chunk");
    }
    Ok(segments)
}

#[async_trait]
impl TranscriptionBackend for LocalWhisperBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        let ctx = self.ctx.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "whisper model not loaded from {}. Run 'whisrs setup' to download a model.",
                self.model_path
            )
        })?;

        // Decode WAV to i16 samples, then convert to f32.
        let cursor = std::io::Cursor::new(audio);
        let reader = hound::WavReader::new(cursor)?;
        let samples_i16: Vec<i16> = reader.into_samples::<i16>().collect::<Result<_, _>>()?;
        let mut samples_f32 = vec![0.0f32; samples_i16.len()];
        whisper_rs::convert_integer_to_float_audio(&samples_i16, &mut samples_f32)
            .map_err(|e| anyhow::anyhow!("failed to convert audio: {e}"))?;

        let ctx = Arc::clone(ctx);
        let language = config.language.clone();
        let is_multilingual = ctx.is_multilingual();
        let prompt = config.prompt.clone();

        // Up to one decode window, a single `full()` call over the whole
        // buffer.
        if fits_one_decode_window(samples_i16.len()) {
            return tokio::task::spawn_blocking(move || {
                let mut state = create_state(&ctx)?;
                run_whisper_inference(
                    &mut state,
                    &samples_f32,
                    &language,
                    is_multilingual,
                    prompt.as_deref(),
                )
            })
            .await?;
        }

        // Longer, cut into windows that each fit one 30 s mel window
        // (issue #208), in the blocking task: see `batch_windows`.
        let phrase_silence_ms = self.phrase_silence_ms;
        let vad = config.vad;
        tokio::task::spawn_blocking(move || {
            let windows = batch_windows(&samples_i16, phrase_silence_ms, vad);
            let mut state = create_state(&ctx)?;
            decode_batch_windows(
                &mut state,
                &samples_f32,
                &windows,
                &language,
                is_multilingual,
                prompt.as_deref(),
            )
        })
        .await?
    }

    async fn transcribe_stream(
        &self,
        audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let ctx = self
            .ctx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("whisper model not loaded from {}", self.model_path))?;

        match self.segmentation {
            SegmentationMode::Silence => self.stream_phrases(ctx, audio_rx, text_tx, config).await,
            SegmentationMode::Window => self.stream_windows(ctx, audio_rx, text_tx, config).await,
            SegmentationMode::Unsegmented => {
                self.stream_unsegmented(ctx, audio_rx, text_tx, config)
                    .await
            }
        }
    }

    // Unsegmented mode opts out so the daemon records the whole session and calls
    // `transcribe` once on stop, instead of typing phrases as they end.
    fn supports_streaming(&self) -> bool {
        self.segmentation != SegmentationMode::Unsegmented
    }

    // `run_whisper_inference` hands the prompt to `set_initial_prompt`, so
    // whisper.cpp really does decode conditioned on it.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
    }

    // One trimmed phrase, or a window's new words, per send.
    fn stream_text(&self) -> StreamText {
        StreamText::Phrases
    }

    // Native timestamps from whisper's segments, decoded a window of phrases
    // at a time (see `decode_recording_windows` for why never a whole chunk at
    // once, and `recording_phrases` for why `segmentation` does not apply).
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript> {
        let ctx = self.ctx.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "whisper model not loaded from {}. Run 'whisrs setup' to download a model.",
                self.model_path
            )
        })?;

        let samples = decode_wav_samples(audio)?;
        // Minutes of audio per chunk: run the detector off the async worker.
        let phrases =
            crate::audio::block_off_runtime(|| self.recording_phrases(&samples, config.open_vad()));
        let windows = decode_windows(&samples, &phrases, SAMPLE_RATE);
        debug!(
            "local whisper: {} phrase(s) in {} decode window(s) (whisper full() calls) \
             for a {:.1}s recording chunk",
            phrases.len(),
            windows.len(),
            samples.len() as f64 / SAMPLE_RATE as f64
        );
        drop(phrases);

        let ctx = Arc::clone(ctx);
        let language = config.language.clone();
        let prompt = config.prompt.clone().filter(|p| !p.is_empty());
        let segments = tokio::task::spawn_blocking(move || {
            decode_recording_windows(&ctx, &samples, &windows, &language, prompt.as_deref())
        })
        .await??;
        Ok(Transcript { segments })
    }

    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        Some(LOCAL_CHUNK_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_error() {
        let backend = LocalWhisperBackend::new("/nonexistent/model.bin".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "base.en".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let err = backend.transcribe(&[1, 2, 3], &config).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not available")
                || msg.contains("not loaded")
                || msg.contains("not found"),
            "unexpected error: {msg}"
        );
    }
    #[test]
    fn decoder_language_resolution() {
        assert_eq!(decoder_language("auto", true), None);
        assert_eq!(decoder_language("auto", false), Some("en"));
        assert_eq!(decoder_language("AUTO", true), None);
        assert_eq!(decoder_language("AUTO", false), Some("en"));
        assert_eq!(decoder_language("", true), None);
        assert_eq!(decoder_language("", false), None);
        assert_eq!(decoder_language(" de ", true), Some("de"));
        assert_eq!(decoder_language(" de ", false), Some("de"));
    }

    #[test]
    fn i16_to_f32_conversion() {
        let samples = vec![0i16, i16::MAX, i16::MIN];
        let f32_samples = i16_to_f32(&samples);
        assert_eq!(f32_samples[0], 0.0);
        assert!((f32_samples[1] - 1.0).abs() < 0.001);
        assert!((f32_samples[2] + 1.0).abs() < 0.001);
    }

    #[test]
    fn decapitalize_first_lowercases_ascii_uppercase() {
        assert_eq!(decapitalize_first("Hello world"), "hello world");
    }

    #[test]
    fn decapitalize_first_leaves_lowercase_unchanged() {
        assert_eq!(decapitalize_first("hello World"), "hello World");
    }

    #[test]
    fn decapitalize_first_leaves_digit_and_punct_unchanged() {
        assert_eq!(decapitalize_first("42 things"), "42 things");
        assert_eq!(decapitalize_first("...And then"), "...And then");
    }

    #[test]
    fn decapitalize_first_handles_multibyte_uppercase() {
        assert_eq!(decapitalize_first("École normale"), "école normale");
    }

    #[test]
    fn decapitalize_first_handles_empty_string() {
        assert_eq!(decapitalize_first(""), "");
    }

    #[test]
    fn decapitalize_first_keeps_pronoun_i() {
        assert_eq!(decapitalize_first("I think so"), "I think so");
        assert_eq!(decapitalize_first("I'm here"), "I'm here");
        assert_eq!(decapitalize_first("I"), "I");
    }

    #[test]
    fn decapitalize_first_still_lowercases_i_words() {
        assert_eq!(decapitalize_first("It works"), "it works");
        assert_eq!(decapitalize_first("Instead of that"), "instead of that");
    }

    #[test]
    fn segmentation_mode_parsing() {
        assert_eq!(
            SegmentationMode::parse("silence"),
            SegmentationMode::Silence
        );
        assert_eq!(SegmentationMode::parse("Window"), SegmentationMode::Window);
        assert_eq!(
            SegmentationMode::parse(" window "),
            SegmentationMode::Window
        );
        assert_eq!(
            SegmentationMode::parse("none"),
            SegmentationMode::Unsegmented
        );
        assert_eq!(
            SegmentationMode::parse(" NONE "),
            SegmentationMode::Unsegmented
        );
        // Unknown values fall back to the default.
        assert_eq!(SegmentationMode::parse("bogus"), SegmentationMode::Silence);
        assert_eq!(SegmentationMode::default(), SegmentationMode::Silence);
    }

    /// Recordings decode in short phrases on every segmentation mode, `"none"`
    /// included: 70 s of unbroken speech (no pause to cut at) still comes out
    /// as phrases no longer than the splitter's hard cap.
    #[test]
    fn recording_phrases_stay_short_whatever_the_segmentation() {
        let speech: Vec<i16> = (0..70 * SAMPLE_RATE)
            .map(|i| if i % 2 == 0 { 8000 } else { -8000 })
            .collect();
        for mode in ["none", "silence", "window"] {
            let backend = LocalWhisperBackend::new("/nonexistent/ggml.bin".to_string())
                .with_segmentation(mode, 400);
            let phrases = backend.recording_phrases(&speech, VadConfig::open(Vad::Rms));
            assert!(phrases.len() >= 3, "{mode}: {} phrase(s)", phrases.len());
            for phrase in &phrases {
                assert!(
                    phrase.samples.len() <= 28 * SAMPLE_RATE,
                    "{mode}: a {:.1} s phrase",
                    phrase.samples.len() as f64 / SAMPLE_RATE as f64
                );
            }
            let covered: usize = phrases.iter().map(|p| p.samples.len()).sum();
            assert!(covered >= 69 * SAMPLE_RATE, "{mode}: speech was dropped");
        }
    }

    fn raw(start_ms: u64, end_ms: u64, text: &str, no_speech_prob: f64) -> RawSegment {
        RawSegment {
            start_ms,
            end_ms,
            text: text.to_string(),
            no_speech_prob,
            avg_logprob: Some(-1.5),
        }
    }

    /// A window's segments land on the chunk: moved by the window's start,
    /// clamped into it, blank and no-speech ones dropped, and a forced-cut
    /// window's first kept line lowercased (only that one).
    #[test]
    fn window_segments_are_placed_on_the_chunk() {
        let window = DecodeWindow {
            range: 16_000..16_000 + 20 * SAMPLE_RATE,
            continuation: true,
            phrases: 3,
        };
        let placed = place_window_segments(
            vec![
                raw(0, 900, " ", 0.0),
                raw(900, 1_800, " Thanks for watching.", 0.9),
                raw(2_000, 6_500, " And then We shipped it.", 0.1),
                raw(7_000, 9_000, " Then the tests.", 0.1),
                // Ends in the zero padding past the window.
                raw(19_000, 30_000, " Done.", 0.1),
            ],
            &window,
        );
        let lines: Vec<(u64, u64, &str)> = placed
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (3_000, 7_500, "and then We shipped it."),
                (8_000, 10_000, "Then the tests."),
                (20_000, 21_000, "Done."),
            ]
        );
        // A window that starts fresh keeps its capital.
        let fresh = DecodeWindow {
            continuation: false,
            ..window
        };
        let placed = place_window_segments(vec![raw(0, 500, " Yes.", 0.0)], &fresh);
        assert_eq!(placed[0].text, "Yes.");
    }

    /// Two speech regions a second of silence apart, as a recording chunk
    /// joins them, are two phrases however long `phrase_silence_ms` is.
    #[cfg(feature = "recording")]
    #[test]
    fn recording_phrases_break_at_every_join() {
        let region: Vec<i16> = (0..16 * SAMPLE_RATE / 10)
            .map(|i| if i % 8 < 4 { 8000 } else { -8000 })
            .collect();
        let join = crate::recording::speech::JOIN_GAP;
        let chunk: Vec<i16> = region
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0, join))
            .chain(region.iter().copied())
            .collect();
        for silence_ms in [400, 900, 1000, 5000] {
            let backend = LocalWhisperBackend::new("/nonexistent/ggml.bin".to_string())
                .with_segmentation("silence", silence_ms);
            let phrases = backend.recording_phrases(&chunk, VadConfig::open(Vad::Rms));
            assert_eq!(phrases.len(), 2, "phrase_silence_ms = {silence_ms}");
            assert!(phrases[1].start_sample as usize >= region.len() + join / 2);
        }
        assert!(
            super::super::RECORDING_PHRASE_SILENCE_MAX_MS < (join * 1000 / SAMPLE_RATE) as u64,
            "the cap must stay under the join"
        );
    }

    /// A batch dictation that fits one decode window keeps the single
    /// `full()` call; past that it is cut into windows of at most 28 s,
    /// every phrase in exactly one, in order (issue #208).
    #[test]
    fn long_batch_dictations_decode_a_window_at_a_time() {
        // 2.5 s turns of speech, each followed by 1.5 s of room noise.
        let mut audio: Vec<i16> = Vec::new();
        while audio.len() < 70 * SAMPLE_RATE {
            audio.extend((0..5 * SAMPLE_RATE / 2).map(|i| if i % 8 < 4 { 8000 } else { -8000 }));
            audio.extend((0..3 * SAMPLE_RATE / 2).map(|i| [3, -2, 1, -3][i % 4]));
        }
        let cap = MAX_DECODE_WINDOW_SECS * SAMPLE_RATE;
        assert!(fits_one_decode_window(10 * SAMPLE_RATE));
        assert!(fits_one_decode_window(cap));
        assert!(!fits_one_decode_window(cap + 1));
        assert!(!batch_windows(&audio[..cap + 1], 400, Vad::Rms).is_empty());

        let windows = batch_windows(&audio, 400, Vad::Rms);
        let phrases = capped_phrases(&audio, 400, Some(SAMPLE_RATE), VadConfig::open(Vad::Rms));
        assert!(windows.len() >= 3, "{windows:?}");
        assert_eq!(
            windows.iter().map(|w| w.phrases).sum::<usize>(),
            phrases.len()
        );
        let mut next = 0;
        for window in &windows {
            assert!(window.range.len() <= cap, "{window:?}");
            for phrase in &phrases[next..next + window.phrases] {
                let start = phrase.start_sample as usize;
                assert!(window.range.start <= start);
                assert!(start + phrase.samples.len() <= window.range.end);
            }
            next += window.phrases;
        }
    }

    /// `samples` of a 2 kHz square wave, loud enough to be speech.
    fn voice(samples: usize) -> impl Iterator<Item = i16> {
        (0..samples).map(|i| if i % 8 < 4 { 8000 } else { -8000 })
    }

    /// `samples` of room noise: quiet, never exact zeros.
    fn room_noise(samples: usize) -> impl Iterator<Item = i16> {
        (0..samples).map(|i| [3, -2, 1, -3][i % 4])
    }

    /// A long batch dictation is cut where a recording is:
    /// `phrase_silence_ms` is capped at 900 ms, so a 1 s pause ends a
    /// phrase however long it is set.
    #[test]
    fn batch_windows_cap_phrase_silence() {
        // 12 turns of 2 s of speech, each followed by 1 s of room noise.
        let pause = SAMPLE_RATE;
        let mut audio: Vec<i16> = Vec::new();
        for _ in 0..12 {
            audio.extend(voice(2 * SAMPLE_RATE));
            audio.extend(room_noise(pause));
        }
        assert!(!fits_one_decode_window(audio.len()));
        for silence_ms in [400, 900, 1000, 2000, 5000] {
            let windows = batch_windows(&audio, silence_ms, Vad::Rms);
            assert_eq!(
                windows.iter().map(|w| w.phrases).sum::<usize>(),
                12,
                "phrase_silence_ms = {silence_ms}: {windows:?}"
            );
        }
        // Uncapped, 2 s of silence would run the turns together.
        let mut uncapped = PhraseSplitter::new(SAMPLE_RATE, VadConfig::open(Vad::Rms), 2000);
        let mut phrases = uncapped.feed(&audio);
        phrases.extend(uncapped.flush());
        assert!(phrases.len() < 12, "{} phrase(s)", phrases.len());
        assert!(
            super::super::RECORDING_PHRASE_SILENCE_MAX_MS < (pause * 1000 / SAMPLE_RATE) as u64,
            "the cap must stay under the pause"
        );
    }

    /// The batch path feeds the splitter a second at a time, where a
    /// recording feeds it a whole chunk in one call: the phrases are the
    /// same, forced cuts and the tail flushed at the end included.
    #[test]
    fn batch_phrases_match_a_single_feed() {
        // 35 s without a pause (forced cuts), turns, then speech that stops
        // inside a frame.
        let mut audio: Vec<i16> = voice(35 * SAMPLE_RATE).collect();
        for _ in 0..6 {
            audio.extend(room_noise(3 * SAMPLE_RATE / 2));
            audio.extend(voice(5 * SAMPLE_RATE / 2));
        }
        audio.extend(room_noise(SAMPLE_RATE));
        audio.extend(voice(SAMPLE_RATE + 777));

        let whole = capped_phrases(&audio, 400, None, VadConfig::open(Vad::Rms));
        assert!(whole.iter().any(|p| p.continuation), "no forced cut");
        assert_eq!(
            capped_phrases(&audio, 400, Some(SAMPLE_RATE), VadConfig::open(Vad::Rms)),
            whole
        );
    }

    /// Each window reaches the decoder on its own, a short one padded with
    /// silence to `MIN_DECODE_SAMPLES`, and a window that fails fails the
    /// whole call, so the daemon keeps the audio rather than type a
    /// transcript with a hole in it.
    #[test]
    fn batch_windows_decode_one_at_a_time_and_fail_together() {
        let samples: Vec<f32> = (0..5 * SAMPLE_RATE)
            .map(|i| 0.1 + (i % 7) as f32 / 10.0)
            .collect();
        let window = |from: usize, to: usize| DecodeWindow {
            range: from..to,
            continuation: false,
            phrases: 1,
        };
        let half = SAMPLE_RATE / 2;
        let windows = [
            window(0, half),
            window(2 * half, 5 * half),
            window(6 * half, 9 * half),
        ];

        let mut seen: Vec<Vec<f32>> = Vec::new();
        let text = decode_each_window(&samples, &windows, None, |buf| {
            seen.push(buf.to_vec());
            Ok(format!("Window {}.", seen.len()))
        })
        .expect("every window decodes");
        assert_eq!(text, "Window 1. Window 2. Window 3.");
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].len(), MIN_DECODE_SAMPLES);
        assert_eq!(seen[0][..half], samples[..half]);
        assert!(seen[0][half..].iter().all(|&s| s == 0.0));
        assert_eq!(seen[1], samples[windows[1].range.clone()]);
        assert_eq!(seen[2], samples[windows[2].range.clone()]);

        let mut calls = 0;
        let err = decode_each_window(&samples, &windows, None, |_| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("window 2 failed");
            }
            Ok("Fine.".to_string())
        })
        .expect_err("a failed window fails the call");
        assert!(err.to_string().contains("window 2 failed"), "{err}");
        assert_eq!(calls, 2, "no decode after the failed window");
    }

    /// A long dictation's windows join into one transcript: blank windows add
    /// nothing, a window that starts at a forced cut continues the sentence,
    /// and a window that echoes the prompt is dropped, unless it is too short
    /// to tell from a phrase in the prompt's vocabulary.
    #[test]
    fn batch_window_texts_join_into_one_transcript() {
        let window = |start_secs: usize, continuation: bool| DecodeWindow {
            range: start_secs * SAMPLE_RATE..(start_secs + 20) * SAMPLE_RATE,
            continuation,
            phrases: 1,
        };
        let windows = [
            window(0, false),
            window(20, true),
            window(40, false),
            window(60, false),
            window(80, true),
            window(100, false),
            window(120, false),
        ];
        // The prompt of scripts/check-issue208.sh, typo included.
        let prompt = "make sure to spell out words like 'slash', 'question mark', etc, \
                      as pancuation marks, like '!', '/', etc, rather than actual text. \
                      whisrs, nvim, hyprland, claude code";
        const REGURGITATED: &str = "Make sure to spell out words like 'slash', \
                                    'question mark', etc, as pancuation marks, like '!', \
                                    '/', etc, rather than actual text.";
        const VOCABULARY: &str = "Whisrs, nvim, Hyprland, Claude Code.";
        // Both echo the prompt by the crate's measure; only the long one goes.
        assert!(is_prompt_echo(REGURGITATED, prompt));
        assert!(is_prompt_echo(VOCABULARY, prompt));
        let texts = || {
            [
                "Open the config.",
                "Then save it.",
                "  ",
                "Spell out words like 'slash', 'question mark'.",
                "I think so.",
                REGURGITATED,
                VOCABULARY,
            ]
            .map(String::from)
            .to_vec()
        };

        assert_eq!(
            join_window_texts(&windows, texts(), Some(prompt)),
            "Open the config. then save it. I think so. Whisrs, nvim, Hyprland, Claude Code."
        );
        let kept = format!(
            "Open the config. then save it. \
             Spell out words like 'slash', 'question mark'. I think so. {REGURGITATED} {VOCABULARY}"
        );
        assert_eq!(join_window_texts(&windows, texts(), None), kept);
        assert_eq!(join_window_texts(&windows, texts(), Some("")), kept);
        assert_eq!(join_window_texts(&[], Vec::new(), Some(prompt)), "");
    }

    #[test]
    fn no_segmentation_disables_streaming() {
        // A missing model only logs a warning, which is all this test needs:
        // `supports_streaming` must not depend on the model being loaded.
        let backend = |mode: &str| {
            LocalWhisperBackend::new("/nonexistent/ggml.bin".to_string())
                .with_segmentation(mode, 400)
        };
        assert!(!backend("none").supports_streaming());
        assert!(backend("silence").supports_streaming());
        assert!(backend("window").supports_streaming());
        assert!(backend("bogus").supports_streaming());
    }
}
