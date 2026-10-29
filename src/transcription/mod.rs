//! Transcription backends: trait definition and implementations.

pub mod asr_sidecar;
pub mod deepgram;
pub mod groq;
pub mod local_parakeet;
pub mod local_vosk;
#[cfg(feature = "local-whisper")]
pub mod local_whisper;
#[cfg(not(feature = "local-whisper"))]
pub mod local_whisper {
    //! Stub when local-whisper feature is disabled.
    pub struct LocalWhisperBackend;
    impl LocalWhisperBackend {
        pub fn new(_model_path: String) -> Self {
            Self
        }
        pub fn with_flash_attn(_model_path: String, _flash_attn: bool) -> Self {
            Self
        }
        pub fn with_segmentation(self, _mode: &str, _phrase_silence_ms: u64) -> Self {
            self
        }
    }
    #[async_trait::async_trait]
    impl super::TranscriptionBackend for LocalWhisperBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &super::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            anyhow::bail!("local-whisper feature not enabled; rebuild with: cargo build --features local-whisper")
        }

        // Unreachable: `transcribe` bails before a request exists. `false`
        // is what the rule above the trait method yields — read the answer off
        // the request-building code, and this build has none — and it is the
        // safe direction, since a stale `true` is the exact shape of #133.
        // The real backend answers `true` off its own `set_initial_prompt`
        // call; the divergence is deliberate and unobservable.
        fn sends_prompt(&self, _config: &super::TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> super::StreamText {
            super::StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &super::TranscriptionConfig,
        ) -> anyhow::Result<super::Transcript> {
            anyhow::bail!("local-whisper feature not enabled; rebuild with: cargo build --features local-whisper")
        }

        // Matches the real backend so chunking does not depend on the build.
        fn max_segment_chunk_secs(&self, _config: &super::TranscriptionConfig) -> Option<u32> {
            Some(super::LOCAL_CHUNK_SECS)
        }
    }
}
pub mod factory;
mod flac;
pub mod openai_compatible_realtime;
pub mod openai_realtime;
pub mod openai_realtime_protocol;
pub mod openai_rest;
pub mod phrase_split;
pub mod retry;
#[cfg(test)]
pub(crate) mod test_server;
pub mod verbose_json;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

pub use crate::recording::{Segment, Word};

/// A timed transcript of one audio chunk, as returned by
/// [`TranscriptionBackend::transcribe_segments`]. Times are milliseconds
/// relative to the start of the chunk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub segments: Vec<Segment>,
}

/// Chunk length for the 25 MB upload cap of Groq and OpenAI: 16 kHz mono i16
/// WAV is 32 KB/s, so 10 minutes is ~19 MB with headroom.
pub const UPLOAD_CHUNK_SECS: u32 = 600;

/// Chunk length for local decoding. whisper.cpp still decodes a chunk in
/// windows of at most ~28 s; the cap only bounds how much audio sits in
/// memory.
pub const LOCAL_CHUNK_SECS: u32 = 600;

/// Sample rate of the WAV handed to [`TranscriptionBackend::transcribe_segments`].
const SEGMENT_SAMPLE_RATE: usize = 16_000;

/// Silence that ends a phrase in [`segments_by_phrase`]. Matches the
/// local-whisper default so both phrase paths cut alike.
const FALLBACK_PHRASE_SILENCE_MS: u64 = 400;

/// The longest silence a recording phrase may hold. A recording chunk joins its
/// speech regions with a second of digital silence
/// ([`crate::recording::speech::JOIN_GAP`]), so a longer `phrase_silence_ms`
/// would decode two regions, possibly minutes apart, as one phrase.
pub const RECORDING_PHRASE_SILENCE_MAX_MS: u64 = 900;

/// `[local-whisper] phrase_silence_ms` as recordings apply it: capped at
/// [`RECORDING_PHRASE_SILENCE_MAX_MS`].
pub fn recording_phrase_silence_ms(configured: u64) -> u64 {
    configured.min(RECORDING_PHRASE_SILENCE_MAX_MS)
}

/// A transient HTTP failure worth retrying later: 429 (rate limited), 503
/// (overloaded), or a server or gateway error (500, 502, 504, and
/// Cloudflare's 520 to 524, such as a 524 origin timeout on a long upload).
/// Cloud backends return it inside their `anyhow::Error` so a long job such
/// as a recording can back off and resume ([`retry::retry_rate_limited`]),
/// while dictation, which does not retry, shows the same message as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryableHttpError {
    pub status: u16,
    /// The server's `Retry-After` (or `retry-after-ms`), when it sent one.
    pub retry_after: Option<std::time::Duration>,
    pub message: String,
}

impl std::fmt::Display for RetryableHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RetryableHttpError {}

impl RetryableHttpError {
    /// Rate limited or overloaded (429, 503): waited out within the budget.
    /// The other retryable statuses are server errors, retried a few times
    /// only ([`retry::SERVER_ERROR_BACKOFF`]).
    pub fn is_rate_limit(&self) -> bool {
        matches!(self.status, 429 | 503)
    }
}

/// Whether an HTTP status is a transient failure ([`RetryableHttpError`]).
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 520..=524)
}

/// An error for a failed request: a [`RetryableHttpError`] for a transient
/// status ([`is_retryable_status`]), a plain error otherwise. `message` is
/// what the user sees either way.
pub(crate) fn http_error(
    status: u16,
    retry_after: Option<std::time::Duration>,
    message: String,
) -> anyhow::Error {
    if is_retryable_status(status) {
        RetryableHttpError {
            status,
            retry_after,
            message,
        }
        .into()
    } else {
        anyhow::anyhow!(message)
    }
}

/// The wait a response asks for: `retry-after-ms` (OpenAI), else
/// `Retry-After` as seconds or an HTTP date.
pub(crate) fn retry_after_header(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        if ms.is_finite() && ms >= 0.0 {
            return Some(std::time::Duration::from_millis(ms as u64));
        }
    }
    get("retry-after").and_then(|v| parse_retry_after(v, chrono::Utc::now()))
}

/// Parse a `Retry-After` value: delay seconds (fractions allowed) or an
/// HTTP date, relative to `now`. A date in the past means "now".
pub(crate) fn parse_retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0)
            .then(|| std::time::Duration::from_secs_f64(secs.min(86_400.0)));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        at.with_timezone(&chrono::Utc)
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default(),
    )
}

/// Convert seconds (as reported by every cloud API) to whole milliseconds.
pub(crate) fn secs_to_ms(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        (secs * 1000.0).round() as u64
    } else {
        0
    }
}

/// Convert a sample index at [`SEGMENT_SAMPLE_RATE`] to milliseconds.
pub(crate) fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / SEGMENT_SAMPLE_RATE as u64
}

/// Decode a 16-bit PCM WAV buffer into samples.
pub(crate) fn decode_wav_samples(audio: &[u8]) -> anyhow::Result<Vec<i16>> {
    let reader = hound::WavReader::new(std::io::Cursor::new(audio))?;
    Ok(reader.into_samples::<i16>().collect::<Result<_, _>>()?)
}

/// Phrase fallback for backends that return no timestamps.
///
/// Splits the chunk at pauses with [`phrase_split::PhraseSplitter`], sends each
/// phrase through the backend's plain [`TranscriptionBackend::transcribe`], and
/// returns one segment per non-empty phrase, timed by where the phrase sits in
/// the chunk. Not a trait default on purpose: each backend that relies on it
/// calls it explicitly from its own `transcribe_segments`.
///
/// One request per phrase, unlike local whisper's decode windows
/// ([`phrase_split::decode_windows`]): a text-only backend returns no inner
/// timing, so a phrase's place is all that orders its line against the other
/// channel, and the models behind it (Parakeet, Moonshine) do not pad every
/// call to 30 s the way whisper does, so grouping would buy little.
///
/// Each phrase request waits out rate limits on its own
/// ([`retry::retry_rate_limited`], [`retry::RATE_LIMIT_BUDGET`] per phrase),
/// so a 429 on one phrase does not resend the phrases already done. A phrase
/// still limited when its budget runs out fails the chunk with a
/// [`retry::RateLimitExhausted`], which the caller's own retry layer does
/// not retry again.
pub async fn segments_by_phrase<B>(
    backend: &B,
    audio: &[u8],
    config: &TranscriptionConfig,
) -> anyhow::Result<Transcript>
where
    B: TranscriptionBackend + ?Sized,
{
    use crate::audio::wav::encode_wav;

    let samples = decode_wav_samples(audio)?;
    let mut splitter = phrase_split::PhraseSplitter::new(
        SEGMENT_SAMPLE_RATE,
        audio_silence_gate::SILENCE_RMS_THRESHOLD,
        FALLBACK_PHRASE_SILENCE_MS,
    );
    let mut phrases = splitter.feed(&samples);
    phrases.extend(splitter.flush());

    let mut segments = Vec::new();
    for phrase in phrases {
        let wav = encode_wav(&phrase.samples)?;
        let text = retry::retry_rate_limited(retry::RATE_LIMIT_BUDGET, || {
            backend.transcribe(&wav, config)
        })
        .await?;
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        segments.push(Segment {
            start_ms: samples_to_ms(phrase.start_sample),
            end_ms: samples_to_ms(phrase.start_sample + phrase.samples.len() as u64),
            text: text.to_string(),
            speaker: None,
            words: None,
        });
    }
    Ok(Transcript { segments })
}

/// Joins `prompt` and `vocabulary` with `". "`, prompt first. Blank prompts
/// are treated as absent; returns `None` only when both inputs are empty so
/// backends skip the hint entirely rather than receiving an empty string.
pub fn transcription_prompt(prompt: Option<&str>, vocabulary: &[String]) -> Option<String> {
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

/// Configuration for a transcription request.
#[derive(Debug, Clone)]
pub struct TranscriptionConfig {
    /// Language code (ISO 639-1), e.g. "en", or "auto" for auto-detection.
    pub language: String,
    /// Model identifier (backend-specific).
    pub model: String,
    /// Optional prompt hint for the transcription model (vocabulary, context).
    pub prompt: Option<String>,
    /// Backend-specific keyword-biasing terms — Deepgram keyterm prompting.
    ///
    /// Carried alongside `prompt` rather than on the backend struct so both
    /// forms of "bias the model toward these words" travel the same channel:
    /// a path that deliberately sends no prompt (command mode, where the
    /// recording is a spoken instruction rather than content) sends no
    /// keyterms either. Backends with no equivalent parameter ignore these —
    /// they receive the same terms folded into `prompt`.
    pub keyterms: Vec<String>,
    /// Ask the backend to tell the voices apart. Only a recording track
    /// whose voices are separated by the backend sets it, and only Deepgram
    /// REST acts on it (`diarize_model`, in
    /// [`TranscriptionBackend::transcribe_segments`]); dictation never
    /// does.
    pub diarize: bool,
}

/// How the pieces of text a backend sends on `text_tx` in
/// [`TranscriptionBackend::transcribe_stream`] join up.
///
/// The daemon's typing batcher coalesces pieces that arrive close together
/// (several queue up while typing waits on a held modifier), and only the
/// backend knows whether two of them need a space between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamText {
    /// Whole phrases, usually trimmed (Deepgram streaming, Groq, local
    /// whisper, Lemonade's completed utterances): the daemon puts a space
    /// between two of them unless one already has it at the boundary or the
    /// next one starts with closing punctuation.
    Phrases,
    /// Fragments that carry their own spacing and may split a word (OpenAI
    /// realtime append deltas: "Deep", "gram"): appended verbatim.
    Fragments,
}

/// Trait for transcription backends.
///
/// Each backend takes WAV-encoded audio bytes and returns the transcribed text.
/// Backends that support streaming override `transcribe_stream`.
#[async_trait]
pub trait TranscriptionBackend: Send + Sync {
    /// Transcribe a complete WAV-encoded audio buffer, returning the text.
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String>;

    /// Streaming transcription: receive audio chunks and send text incrementally.
    ///
    /// The default implementation collects all audio, encodes to WAV, and calls
    /// `transcribe()` — so non-streaming backends work without overriding this.
    async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        use crate::audio::wav::encode_wav;

        // Collect all audio chunks.
        let mut all_samples: Vec<i16> = Vec::new();
        while let Some(chunk) = audio_rx.recv().await {
            all_samples.extend_from_slice(&chunk);
        }

        if all_samples.is_empty() {
            return Ok(());
        }

        // Encode to WAV and use the non-streaming method.
        let wav_data = encode_wav(&all_samples)?;
        let text = self.transcribe(&wav_data, config).await?;

        if !text.is_empty() {
            text_tx.send(text).await.ok();
        }

        Ok(())
    }

    /// Whether this backend supports true/chunked streaming.
    ///
    /// When true, the daemon will use `transcribe_stream` during recording
    /// rather than waiting for recording to finish.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Whether this backend actually transmits [`TranscriptionConfig::prompt`]
    /// to the provider for *this* request.
    ///
    /// Gates the prompt-echo filter in the batch pipeline (issue #133).
    /// `config.prompt` is built for *every* backend from `[general] prompt` +
    /// `[general] vocabulary`, so a backend that silently drops it would
    /// otherwise have genuine speech that happens to resemble the user's own
    /// vocabulary discarded as an "echo" of a prompt that never went on the
    /// wire.
    ///
    /// Required rather than defaulted on purpose: a default of `true` is
    /// exactly what let `openai-compatible-realtime` inherit the wrong answer
    /// in the first cut of the #133 fix. Answer it from the request-building
    /// code, not from what the backend "should" do.
    ///
    /// Takes the request because for the realtime profiles the answer is not a
    /// property of the backend at all: the model is. `openai-realtime` asks
    /// `openai_model_supports_prompt(config.model)`, and
    /// `OpenAiSessionUpdate::new` drops the prompt for gpt-realtime-whisper,
    /// so one backend struct sends it for one model and not for another.
    /// Backends whose answer is fixed ignore the argument.
    fn sends_prompt(&self, config: &TranscriptionConfig) -> bool;

    /// How the text [`Self::transcribe_stream`] sends joins up.
    ///
    /// Required, like `sends_prompt`: a default of `Phrases` would put spaces
    /// inside the words a fragment backend splits, and treating everything as
    /// `Fragments` is what glued queued phrases together ("itcopies") before
    /// this existed. A backend on the default `transcribe_stream` sends one
    /// text, so its answer never matters: it says `Phrases`.
    fn stream_text(&self) -> StreamText;

    /// Transcribe one chunk of a recording track into timed segments.
    ///
    /// `audio` is a 16 kHz mono i16 WAV of at most
    /// [`Self::max_segment_chunk_secs`]; segment times are relative to its
    /// start. Backends with native timestamps parse them; the rest call
    /// [`segments_by_phrase`] explicitly. Live-only backends return an error
    /// naming the batch backends to use instead.
    ///
    /// Required, like `sends_prompt`: a default would let a new backend ship
    /// recording support nobody decided on.
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript>;

    /// Longest chunk, in seconds, [`Self::transcribe_segments`] accepts, or
    /// `None` to send a whole track at once.
    fn max_segment_chunk_secs(&self, config: &TranscriptionConfig) -> Option<u32>;
}

/// Error for a backend that only transcribes live audio.
pub fn live_only_error(backend: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the {backend} backend only transcribes live audio and cannot transcribe a \
         recording. Set [recording] backend to a batch backend: groq, openai, deepgram, \
         local-whisper or asr-sidecar"
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn retry_after_seconds_and_dates() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        let secs = std::time::Duration::from_secs;
        assert_eq!(parse_retry_after("30", now), Some(secs(30)));
        assert_eq!(
            parse_retry_after(" 1.5 ", now),
            Some(std::time::Duration::from_millis(1500))
        );
        assert_eq!(
            parse_retry_after("Fri, 02 Oct 2026 12:01:00 GMT", now),
            Some(secs(60))
        );
        // A date already past means retry now.
        assert_eq!(
            parse_retry_after("Fri, 02 Oct 2026 11:00:00 GMT", now),
            Some(secs(0))
        );
        assert_eq!(parse_retry_after("-3", now), None);
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn transient_statuses_are_retryable() {
        let err = http_error(429, None, "rate limited".into());
        assert!(err.downcast_ref::<RetryableHttpError>().is_some());
        assert_eq!(err.to_string(), "rate limited");
        for status in [429, 500, 502, 503, 504, 520, 521, 522, 523, 524] {
            let err = http_error(status, None, "x".into());
            let retryable = err.downcast_ref::<RetryableHttpError>();
            assert!(retryable.is_some(), "{status}");
            assert_eq!(
                retryable.unwrap().is_rate_limit(),
                matches!(status, 429 | 503),
                "{status}"
            );
        }
        for status in [400, 401, 403, 404, 413, 422, 501, 505, 525, 526] {
            assert!(
                http_error(status, None, "x".into())
                    .downcast_ref::<RetryableHttpError>()
                    .is_none(),
                "{status}"
            );
        }
    }
    use super::*;

    /// Canned `transcribe` that reports what it was sent, for the fallback.
    struct EchoLength;

    #[async_trait]
    impl TranscriptionBackend for EchoLength {
        async fn transcribe(
            &self,
            audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> anyhow::Result<String> {
            let samples = decode_wav_samples(audio)?;
            Ok(format!("{} samples", samples.len()))
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            audio: &[u8],
            config: &TranscriptionConfig,
        ) -> anyhow::Result<Transcript> {
            segments_by_phrase(self, audio, config).await
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    fn cfg() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".into(),
            model: "m".into(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
        }
    }

    /// 1 s of a 440 Hz tone at moderate level.
    fn tone(secs: f64) -> Vec<i16> {
        let n = (secs * SEGMENT_SAMPLE_RATE as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / SEGMENT_SAMPLE_RATE as f64;
                ((t * 440.0 * std::f64::consts::TAU).sin() * 6000.0) as i16
            })
            .collect()
    }

    #[tokio::test]
    async fn phrase_fallback_times_segments_by_phrase_position() {
        let mut audio = vec![0i16; 32_000]; // 2 s silence
        audio.extend(tone(1.0));
        audio.extend(vec![0i16; 32_000]);
        audio.extend(tone(1.5));
        audio.extend(vec![0i16; 16_000]);
        let wav = crate::audio::wav::encode_wav(&audio).unwrap();

        let t = EchoLength.transcribe_segments(&wav, &cfg()).await.unwrap();
        assert_eq!(t.segments.len(), 2, "{t:?}");
        // Onset minus one 100 ms pad frame.
        assert_eq!(t.segments[0].start_ms, 1900);
        assert_eq!(t.segments[1].start_ms, 4900);
        assert!(t.segments[0].end_ms > 3000 && t.segments[0].end_ms <= 3200);
        assert!(t.segments[1].end_ms > 6500 && t.segments[1].end_ms <= 6700);
        assert!(t.segments.iter().all(|s| s.text.ends_with("samples")));
    }

    /// Phrases a recorded pause apart (room noise, not digital silence)
    /// still go out one request each, each with its own place.
    #[tokio::test]
    async fn phrase_fallback_sends_each_phrase_alone() {
        let room = |secs: f64| -> Vec<i16> {
            let n = (secs * SEGMENT_SAMPLE_RATE as f64) as usize;
            (0..n).map(|i| [3, -2, 1, -3][i % 4]).collect()
        };
        let mut audio = room(1.0);
        audio.extend(tone(1.0));
        audio.extend(room(1.5));
        audio.extend(tone(1.5));
        audio.extend(room(1.0));
        let wav = crate::audio::wav::encode_wav(&audio).unwrap();

        let t = EchoLength.transcribe_segments(&wav, &cfg()).await.unwrap();
        let starts: Vec<u64> = t.segments.iter().map(|s| s.start_ms).collect();
        assert_eq!(starts, [900, 3400], "{t:?}");
    }

    #[tokio::test]
    async fn phrase_fallback_on_silence_is_empty() {
        let wav = crate::audio::wav::encode_wav(&vec![0i16; 48_000]).unwrap();
        let t = EchoLength.transcribe_segments(&wav, &cfg()).await.unwrap();
        assert!(t.segments.is_empty());
    }

    /// Phrase backend whose calls listed in `limited` answer HTTP 429.
    struct FlakyPhrases {
        calls: std::sync::atomic::AtomicUsize,
        limited: Vec<usize>,
    }

    #[async_trait]
    impl TranscriptionBackend for FlakyPhrases {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> anyhow::Result<String> {
            let call = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.limited.contains(&call) || self.limited.contains(&usize::MAX) {
                return Err(RetryableHttpError {
                    status: 429,
                    retry_after: Some(std::time::Duration::from_secs(5)),
                    message: "slow down".into(),
                }
                .into());
            }
            Ok(format!("call {call}"))
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            audio: &[u8],
            config: &TranscriptionConfig,
        ) -> anyhow::Result<Transcript> {
            segments_by_phrase(self, audio, config).await
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    fn two_phrases() -> Vec<u8> {
        let mut audio = vec![0i16; 16_000];
        audio.extend(tone(1.0));
        audio.extend(vec![0i16; 32_000]);
        audio.extend(tone(1.0));
        audio.extend(vec![0i16; 16_000]);
        crate::audio::wav::encode_wav(&audio).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_phrase_is_retried_alone() {
        // The second phrase's first request is rate limited.
        let backend = FlakyPhrases {
            calls: Default::default(),
            limited: vec![1],
        };
        let t = backend
            .transcribe_segments(&two_phrases(), &cfg())
            .await
            .unwrap();
        // Phrase one once, phrase two twice: the first phrase is not resent.
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        let texts: Vec<_> = t.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["call 0", "call 2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_phrase_fails_without_a_retryable_error() {
        let backend = FlakyPhrases {
            calls: Default::default(),
            limited: vec![usize::MAX],
        };
        let started = tokio::time::Instant::now();
        let err = backend
            .transcribe_segments(&two_phrases(), &cfg())
            .await
            .unwrap_err();
        // One phrase's budget, then the chunk fails.
        assert!(started.elapsed() <= retry::RATE_LIMIT_BUDGET);
        assert!(err.downcast_ref::<retry::RateLimitExhausted>().is_some());
        assert!(err
            .chain()
            .all(|c| c.downcast_ref::<RetryableHttpError>().is_none()));
    }

    #[test]
    fn transcription_prompt_joins_prompt_and_vocabulary() {
        let vocab = vec!["Hyprland".to_string(), "whisrs".to_string()];
        assert_eq!(
            transcription_prompt(Some(" Notes. "), &vocab).as_deref(),
            Some("Notes.. Hyprland, whisrs")
        );
        assert_eq!(transcription_prompt(Some("  "), &[]), None);
        assert_eq!(
            transcription_prompt(None, &vocab).as_deref(),
            Some("Hyprland, whisrs")
        );
    }

    #[test]
    fn secs_to_ms_rounds_and_clamps() {
        assert_eq!(secs_to_ms(1.2345), 1235);
        assert_eq!(secs_to_ms(-1.0), 0);
        assert_eq!(secs_to_ms(f64::NAN), 0);
    }
}
