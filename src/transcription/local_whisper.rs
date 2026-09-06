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
//! Both streaming modes create one decoder state per stream and reuse it
//! across decodes; see [`create_state`].

use std::sync::Arc;

use asr_dedup::TextDedup;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::phrase_split::PhraseSplitter;
use super::{TranscriptionBackend, TranscriptionConfig};
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

/// Silence threshold — must match or be below the daemon's auto-stop threshold
/// (0.003) so we never skip audio that auto-stop considers speech.
const SILENCE_THRESHOLD: f64 = 0.003;

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
}

impl SegmentationMode {
    /// Parse a config string; unknown values warn and fall back to `Silence`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "silence" => Self::Silence,
            "window" => Self::Window,
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
        let ctx = match Self::load_model(&model_path) {
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
    /// `mode` is `"silence"` (default) or `"window"`; `phrase_silence_ms` is
    /// how much continuous silence ends a phrase in silence mode.
    pub fn with_segmentation(mut self, mode: &str, phrase_silence_ms: u64) -> Self {
        self.segmentation = SegmentationMode::parse(mode);
        self.phrase_silence_ms = phrase_silence_ms.max(1);
        self
    }

    fn load_model(path: &str) -> anyhow::Result<whisper_rs::WhisperContext> {
        if !std::path::Path::new(path).exists() {
            anyhow::bail!("model file not found: {path}. Run 'whisrs setup' to download a model.");
        }

        let params = whisper_rs::WhisperContextParameters::default();

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
        let mut splitter =
            PhraseSplitter::new(SAMPLE_RATE, SILENCE_THRESHOLD, self.phrase_silence_ms);
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

                // Skip silent windows.
                if !audio_silence_gate::is_silent(&window, SILENCE_THRESHOLD) {
                    let samples_f32 = i16_to_f32(&window);
                    let lang = config.language.clone();
                    let prompt = prompt.clone();
                    let mut decode_state = state;

                    match tokio::task::spawn_blocking(move || {
                        let result = run_whisper_inference(
                            &mut decode_state,
                            &samples_f32,
                            &lang,
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

            if !remaining.is_empty() && !audio_silence_gate::is_silent(remaining, SILENCE_THRESHOLD)
            {
                let samples_f32 = i16_to_f32(remaining);
                let lang = config.language.clone();
                let prompt = prompt.clone();
                // Final decode of the stream — the state is consumed.
                let mut decode_state = state;

                match tokio::task::spawn_blocking(move || {
                    let result = run_whisper_inference(
                        &mut decode_state,
                        &samples_f32,
                        &lang,
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
    let prompt = config.prompt.clone().filter(|p| !p.is_empty());

    match tokio::task::spawn_blocking(move || {
        let result = run_whisper_inference(&mut state, &samples_f32, &lang, prompt.as_deref());
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
    prompt: Option<&str>,
) -> anyhow::Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy};

    // `best_of: 1` is load-bearing beyond decode quality: it keeps whisper.cpp's
    // decoder count at 1, which is the only reason the library never takes the
    // path where it frees the state itself and hands back an error. Raising it,
    // or switching to beam search, needs the error arms above to stop dropping
    // the returned state.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    if language != "auto" {
        params.set_language(Some(language));
    }

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

    state
        .full(params, audio)
        .map_err(|e| anyhow::anyhow!("whisper inference failed: {e}"))?;

    let mut text = String::new();
    for segment in state.as_iter() {
        text.push_str(&format!("{}", segment));
    }

    Ok(text.trim().to_string())
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
        let prompt = config.prompt.clone();

        tokio::task::spawn_blocking(move || {
            let mut state = create_state(&ctx)?;
            run_whisper_inference(&mut state, &samples_f32, &language, prompt.as_deref())
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
        }
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    // `run_whisper_inference` hands the prompt to `set_initial_prompt`, so
    // whisper.cpp really does decode conditioned on it.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
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
        // Unknown values fall back to the default.
        assert_eq!(SegmentationMode::parse("bogus"), SegmentationMode::Silence);
        assert_eq!(SegmentationMode::default(), SegmentationMode::Silence);
    }
}
