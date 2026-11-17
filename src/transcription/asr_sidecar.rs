//! Generic HTTP ASR sidecar transcription backend.
//!
//! This backend keeps the Rust daemon independent from Python/PyTorch by
//! sending WAV audio to a local HTTP sidecar.
//!
//! Dictation sends a plain request and reads `text`. A recording chunk asks
//! for timings as well ([`TimingForm`]), and falls back to transcribing
//! phrase by phrase when the server has none to give.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, info, warn};

use std::sync::atomic::{AtomicU8, Ordering};

use super::verbose_json::{split_sentences, VerboseResponse, MAX_SEGMENT_MS};
use super::{
    segments_by_phrase, StreamText, Transcript, TranscriptionBackend, TranscriptionConfig,
};

/// Keep a guardrail so a runaway recording does not create an unbounded
/// multipart request.
const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

/// Generic HTTP ASR sidecar transcription backend.
pub struct AsrSidecarBackend {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    /// How recording chunks ask for timings, learned from the answers.
    timings: TimingState,
}

/// The timing fields of a recording chunk's request, tried in this order: a
/// server that refuses one with an HTTP 400 or 422 is asked with the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimingForm {
    /// `response_format=verbose_json` and `timestamp_granularities[]` of
    /// `segment` and `word`. nemo-speech answers with `words` alone,
    /// OpenRouter with `segments` and, for most models, `words`; a server
    /// that knows neither ignores the fields (the bundled sidecars).
    Words,
    /// `response_format=verbose_json` alone, for a model that refuses the
    /// granularities (OpenRouter's voxtral).
    Verbose,
    /// No timing fields: the request dictation sends.
    Plain,
}

impl TimingForm {
    /// The form to ask with once this one was refused.
    fn next(self) -> Option<Self> {
        match self {
            Self::Words => Some(Self::Verbose),
            Self::Verbose => Some(Self::Plain),
            Self::Plain => None,
        }
    }

    /// How the logs name it.
    fn describe(self) -> &'static str {
        match self {
            Self::Words => "verbose_json with segment and word timestamps",
            Self::Verbose => "verbose_json without timestamp granularities",
            Self::Plain => "no timing fields",
        }
    }

    /// Its [`TimingState`] once it gave usable timings.
    fn settled(self) -> u8 {
        match self {
            Self::Words => TimingState::WORDS,
            Self::Verbose => TimingState::VERBOSE,
            Self::Plain => TimingState::PLAIN,
        }
    }
}

/// What the sidecar's answers taught the backend about timings, kept for its
/// life: the chunks of a recording share it, and so do later recordings
/// while they reuse the dictation backend.
///
/// It moves once, out of [`Self::UNSETTLED`], to what the first answer that
/// says something showed (`compare_exchange`), and never back, so two chunks
/// answered at once cannot undo each other. An answer that heard nothing
/// says nothing, and nor does one that is used but shows no timings
/// ([`shows_timings`]: one wordless segment). Once a form gave usable
/// timings, a later answer that cannot place its text, or a refusal of that
/// form, concerns that chunk alone: it goes phrase by phrase, or asks with
/// the next form, and the next chunk asks with the remembered one again.
struct TimingState(AtomicU8);

impl TimingState {
    /// Nothing answered yet: ask with [`TimingForm::Words`].
    const UNSETTLED: u8 = 0;
    /// [`TimingForm::Words`] gave usable timings.
    const WORDS: u8 = 1;
    /// [`TimingForm::Words`] was refused, [`TimingForm::Verbose`] gave
    /// usable timings.
    const VERBOSE: u8 = 2;
    /// Both verbose forms were refused, the plain request gave timings.
    const PLAIN: u8 = 3;
    /// The first answer had no usable timings: every chunk goes phrase by
    /// phrase.
    const PHRASES: u8 = 4;

    fn new() -> Self {
        Self(AtomicU8::new(Self::UNSETTLED))
    }

    /// The form a chunk's request starts with, or `None` once the sidecar
    /// fell back to phrases.
    fn first_form(&self) -> Option<TimingForm> {
        match self.0.load(Ordering::Relaxed) {
            Self::UNSETTLED | Self::WORDS => Some(TimingForm::Words),
            Self::VERBOSE => Some(TimingForm::Verbose),
            Self::PLAIN => Some(TimingForm::Plain),
            _ => None,
        }
    }

    /// Remember that `form` gave usable timings, unless an earlier answer
    /// already settled the state. True when that is news, so the caller logs
    /// it once.
    fn settle(&self, form: TimingForm) -> bool {
        self.advance(form.settled())
    }

    /// Fall back to phrases for good, unless a form already gave usable
    /// timings. True when it did fall back: false means only this chunk
    /// goes phrase by phrase.
    fn fall_back(&self) -> bool {
        self.advance(Self::PHRASES)
    }

    /// Move to `to` if nothing settled the state yet.
    fn advance(&self, to: u8) -> bool {
        self.0
            .compare_exchange(Self::UNSETTLED, to, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

impl AsrSidecarBackend {
    /// Create a new sidecar backend with the transcription URL and optional API key.
    pub fn new(url: String, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            api_key: api_key
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
            timings: TimingState::new(),
        }
    }

    /// The endpoint this backend posts to, exactly as configured.
    ///
    /// Test seam. The backend factory ([`super::factory`]) is the only place a
    /// URL and a key are joined into a backend, and its test proves that wiring
    /// through this accessor. Hidden from the docs to keep it off the supported
    /// surface.
    #[doc(hidden)]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The normalized API key, or `None` for an unauthenticated sidecar.
    /// Test seam; see [`AsrSidecarBackend::url`].
    #[doc(hidden)]
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

impl AsrSidecarBackend {
    /// POST `audio` to the sidecar with `timing`'s fields and return the raw
    /// success body. An HTTP 400 or 422 comes back as a [`BadRequest`].
    async fn post(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
        timing: TimingForm,
    ) -> anyhow::Result<String> {
        if audio.len() > MAX_FILE_SIZE {
            anyhow::bail!(
                "audio file too large ({} bytes, max {} bytes / 1GB)",
                audio.len(),
                MAX_FILE_SIZE
            );
        }

        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        if self.url.trim().is_empty() {
            anyhow::bail!("no ASR sidecar URL configured");
        }

        debug!(
            "sending {} bytes to ASR sidecar (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone());
        let granularities: &[&str] = match timing {
            TimingForm::Words => &["segment", "word"],
            TimingForm::Verbose | TimingForm::Plain => &[],
        };
        if timing != TimingForm::Plain {
            form = form.text("response_format", "verbose_json");
        }
        for granularity in granularities {
            form = form.text("timestamp_granularities[]", granularity.to_string());
        }

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }
        if let Some(prompt) = &config.prompt {
            form = form.text("hotwords", prompt.clone());
        }

        let url = effective_url(&self.url);
        if url != self.url {
            debug!("trimmed trailing slashes from ASR sidecar URL: {url}");
        }
        let mut request = self.client.post(url);
        if let Some(key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        // A sidecar that is not running refuses the connection, and a
        // network that went down times out or finds no route: a recording
        // waits those out. One that took the request and then closed the
        // connection crashed on it, which is final (`final_if_crashed`).
        let response = request
            .multipart(form)
            .send()
            .await
            .map_err(sidecar_error)?;
        let status = response.status();
        let retry_after = super::retry_after_header(response.headers());
        let body = response.text().await.map_err(sidecar_error)?;

        if !status.is_success() {
            let code = status.as_u16();
            let message = match serde_json::from_str::<AsrSidecarErrorResponse>(&body)
                .ok()
                .and_then(|err_resp| err_resp.message())
            {
                Some(message) => format!("ASR sidecar error ({code}): {message}"),
                None => format!("ASR sidecar error ({code}): {body}"),
            };
            // FastAPI answers a field it does not take with 422, others
            // with 400.
            if code == 400 || code == 422 {
                return Err(BadRequest(message).into());
            }
            // A local sidecar answers 500 for an uncaught exception, which a
            // retry rarely fixes and which would cost three more inference
            // runs. Gateway errors in front of it (502, 504, 520 to 524)
            // stay retryable.
            if code == 500 {
                return Err(anyhow::anyhow!(message));
            }
            return Err(super::http_error(code, retry_after, message));
        }
        Ok(body)
    }

    /// POST a recording chunk asking for timings with `timing`, stepping to
    /// the next form on each HTTP 400 or 422. Returns the form that was
    /// answered and its body; any other failure, or a refusal of the plain
    /// request, is the chunk's error.
    async fn post_for_timings(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
        mut timing: TimingForm,
    ) -> anyhow::Result<(TimingForm, String)> {
        loop {
            debug!("ASR sidecar recording request: {}", timing.describe());
            match self.post(audio, config, timing).await {
                Ok(body) => return Ok((timing, body)),
                Err(err) => match timing.next() {
                    Some(next) if err.is::<BadRequest>() => {
                        debug!(
                            "ASR sidecar refused {} ({err}); asking with {}",
                            timing.describe(),
                            next.describe()
                        );
                        timing = next;
                    }
                    _ => return Err(err),
                },
            }
        }
    }
}

/// An HTTP 400 or 422: the sidecar refused the request itself. A recording
/// chunk's request steps to a plainer [`TimingForm`] on it; anywhere else it
/// is the message alone, like any other refusal.
#[derive(Debug)]
struct BadRequest(String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRequest {}

fn sidecar_error(err: reqwest::Error) -> anyhow::Error {
    super::retry::final_if_crashed(err, "the ASR sidecar")
}

/// Longest chunk sent to the sidecar in one request.
const SIDECAR_CHUNK_SECS: u32 = 600;

/// Parse the timings of a sidecar response into sentence-sized segments.
///
/// `segments: [{start, end, text, speaker?}]` and `words: [{word, start,
/// end}]` (seconds) are read as `verbose_json` is
/// ([`super::verbose_json::parse_verbose_segments`]: words attached to their
/// segment, Whisper's no-speech heuristic, a diarizing sidecar's speaker
/// kept, `null` as a missing key, a word missing a time given one by its
/// neighbours), then
/// each segment with words is cut into sentences ([`split_sentences`]).
/// Words with no segment to attach them to, the key missing or its list
/// empty, make one segment (nemo-speech sends `words` alone).
///
/// `Ok(None)` means the body holds text and nothing to place it with
/// (neither a segment nor a timed word), or no text and no timing key, and
/// the caller should fall back to phrases. `Ok(Some(..))` with no segments
/// is a sidecar that heard nothing: blank text, or a missing one beside an
/// empty `segments` or `words`. An `Err` is a body that could not be read.
pub fn parse_sidecar_segments(body: &str) -> anyhow::Result<Option<Transcript>> {
    let reply = VerboseResponse::parse(body)?;
    let segments = if reply.has_segments() {
        reply.into_transcript().segments
    } else if let Some(segment) = reply.words_segment() {
        vec![segment]
    } else {
        let heard_nothing = match reply.text() {
            Some(text) => text.is_empty(),
            None => reply.has_timing_keys(),
        };
        return Ok(heard_nothing.then(Transcript::default));
    };
    Ok(Some(Transcript {
        segments: segments.into_iter().flat_map(split_sentences).collect(),
    }))
}

/// Why a timing request's answer cannot place its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unplaced {
    /// Text with neither a segment nor a timed word to place it with.
    NoTimings,
    /// A segment of this many milliseconds without word timings
    /// ([`coarse_segment_ms`]).
    Coarse(u64),
    /// A body whose timings could not be read.
    Unreadable,
}

impl Unplaced {
    /// How the logs say it.
    fn describe(self) -> String {
        match self {
            Self::NoTimings => "returned no segments or words".to_string(),
            Self::Coarse(ms) => format!(
                "returned a {} s segment without word timings, too coarse to place its text",
                ms / 1000
            ),
            Self::Unreadable => "returned timings it could not read".to_string(),
        }
    }
}

/// A timing request's answer as the chunk's segments, or why it cannot
/// place its text. A body that does not parse is [`Unplaced::Unreadable`]
/// rather than the chunk's error: the phrase fallback asks without timings.
fn place(body: &str, timing: TimingForm) -> Result<Transcript, Unplaced> {
    match parse_sidecar_segments(body) {
        Ok(Some(transcript)) => match coarse_segment_ms(&transcript) {
            None => Ok(transcript),
            Some(ms) => Err(Unplaced::Coarse(ms)),
        },
        Ok(None) => Err(Unplaced::NoTimings),
        Err(err) => {
            debug!(
                "ASR sidecar answer (asked for {}) could not be read for timings: {err}",
                timing.describe()
            );
            Err(Unplaced::Unreadable)
        }
    }
}

/// Whether a usable answer shows that the server times what it hears: a
/// timed word, or two segments or more. One wordless segment does not, as
/// a server that times only the whole clip (OpenRouter's
/// nemotron-3.5-transcribe) answers a short chunk with one that is usable
/// and a long one with one that is not ([`coarse_segment_ms`]): it is used
/// for its chunk and settles nothing, so a later long chunk still falls
/// back to phrases for good rather than pay for the whole-chunk request
/// every time.
fn shows_timings(transcript: &Transcript) -> bool {
    transcript.segments.len() > 1
        || transcript
            .segments
            .iter()
            .any(|s| s.words.as_deref().is_some_and(|words| !words.is_empty()))
}

/// The longest segment past [`MAX_SEGMENT_MS`] with neither word timings
/// nor a speaker, if any: a server that times only the whole clip
/// (OpenRouter's nemotron-3.5-transcribe answers one segment for it), which
/// would put minutes of text at one moment of the transcript. A diarizing
/// sidecar's turn carries a speaker, and is kept whatever its length.
fn coarse_segment_ms(transcript: &Transcript) -> Option<u64> {
    transcript
        .segments
        .iter()
        .filter(|s| s.speaker.is_none() && s.words.as_deref().unwrap_or_default().is_empty())
        .map(|s| s.end_ms.saturating_sub(s.start_ms))
        .filter(|&ms| ms > MAX_SEGMENT_MS)
        .max()
}

/// The URL actually posted to, with trailing path slashes removed.
///
/// Some OpenAI-compatible endpoints 307-redirect when the URL has a trailing
/// slash, which can downgrade http→https and cause reqwest to abort multipart
/// POSTs. Trimming makes both forms work without a redirect. A URL made only of
/// slashes would trim to the empty string, which reqwest rejects with an opaque
/// relative-URL error, so keep the original in that case and let the request
/// fail against what the user actually configured.
///
/// Only the *path* carries that noise. A trailing slash inside a query value or
/// a fragment is data the user typed, so a URL carrying `?` or `#` is posted
/// exactly as configured rather than silently rewritten.
fn effective_url(url: &str) -> &str {
    if url.contains('?') || url.contains('#') {
        return url;
    }
    let trimmed = url.trim_end_matches('/');
    if trimmed.is_empty() {
        url
    } else {
        trimmed
    }
}

/// Response from the ASR sidecar.
#[derive(Debug, Deserialize)]
pub struct AsrSidecarResponse {
    /// Plain text transcript. Sidecars may also return richer diarized output,
    /// but whisrs currently consumes the flattened text for typing.
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct AsrSidecarErrorResponse {
    /// `"..."`, or `{"message": "...", "code": 400}` as OpenRouter (and
    /// OpenAI) send it.
    error: Option<serde_json::Value>,
    detail: Option<serde_json::Value>,
}

impl AsrSidecarErrorResponse {
    /// What the body says, or `None` for an `error` of another shape (the
    /// caller shows the body as it is).
    fn message(&self) -> Option<String> {
        match &self.error {
            Some(serde_json::Value::String(error)) => return Some(error.clone()),
            Some(serde_json::Value::Object(error)) => {
                return error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
            }
            Some(_) => return None,
            None => {}
        }
        Some(match &self.detail {
            Some(serde_json::Value::String(detail)) => detail.clone(),
            Some(detail) => detail.to_string(),
            None => "unknown sidecar error".to_string(),
        })
    }
}

#[async_trait]
impl TranscriptionBackend for AsrSidecarBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        let body = self.post(audio, config, TimingForm::Plain).await?;
        let parsed: AsrSidecarResponse = serde_json::from_str(&body)?;
        let text = parsed.text.trim().to_string();

        if text.is_empty() {
            warn!("ASR sidecar returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe). Model-specific
    // streaming behavior belongs in the sidecar process.

    // The prompt does reach the sidecar, just under a different name: it is
    // sent as the `hotwords` multipart field.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
    }

    fn stream_text(&self) -> StreamText {
        StreamText::Phrases
    }

    // The whole chunk in one request asking for timings (`TimingForm`),
    // split into sentences, when the sidecar returns usable ones; otherwise
    // the phrase fallback (plain requests, as dictation sends). The first
    // answer that says something decides for later chunks (`TimingState`):
    // timings (`shows_timings`) keep the form, unusable ones stop the
    // whole-chunk request for good.
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript> {
        if let Some(first) = self.timings.first_form() {
            let (timing, body) = self.post_for_timings(audio, config, first).await?;
            match place(&body, timing) {
                // Heard nothing: says nothing about the server's timings.
                Ok(transcript) if transcript.segments.is_empty() => return Ok(transcript),
                Ok(transcript) => {
                    if shows_timings(&transcript) && self.timings.settle(timing) {
                        let why = match timing {
                            TimingForm::Words => "it answers with timings",
                            TimingForm::Verbose => {
                                "it refused timestamp granularities with HTTP 400 or 422"
                            }
                            TimingForm::Plain => "it refused verbose_json with HTTP 400 or 422",
                        };
                        info!(
                            "ASR sidecar: recording chunks ask for {}, since {why}",
                            timing.describe()
                        );
                    }
                    return Ok(transcript);
                }
                Err(why) => {
                    if self.timings.fall_back() {
                        info!(
                            "ASR sidecar {} (asked for {}); transcribing recording chunks \
                             phrase by phrase",
                            why.describe(),
                            timing.describe()
                        );
                    } else {
                        debug!(
                            "ASR sidecar {} (asked for {}); transcribing this chunk phrase \
                             by phrase",
                            why.describe(),
                            timing.describe()
                        );
                    }
                }
            }
        }
        segments_by_phrase(self, audio, config).await
    }

    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        Some(SIDECAR_CHUNK_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::retry::{
        is_network_unavailable, is_transport_error, retry_with_backoff, RATE_LIMIT_BUDGET,
    };
    use crate::transcription::test_server::{
        accept_request, crash_after_request, serve_responses, Request,
    };
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    fn test_config() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        }
    }

    /// A backend posting to `path` of a test server that answers
    /// `responses` in turn, and the server, which hands back the requests.
    async fn sidecar_at(
        path: &str,
        api_key: Option<&str>,
        responses: Vec<(u16, &'static str)>,
    ) -> (AsrSidecarBackend, JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_responses(listener, responses));
        let url = format!("http://{addr}{path}");
        (
            AsrSidecarBackend::new(url, api_key.map(str::to_string)),
            server,
        )
    }

    /// [`sidecar_at`] `/transcribe`, without a key.
    async fn sidecar(
        responses: Vec<(u16, &'static str)>,
    ) -> (AsrSidecarBackend, JoinHandle<Vec<Request>>) {
        sidecar_at("/transcribe", None, responses).await
    }

    /// `wav` transcribed as a recording chunk.
    async fn chunk(
        backend: &AsrSidecarBackend,
        wav: &[u8],
        config: &TranscriptionConfig,
    ) -> Transcript {
        backend.transcribe_segments(wav, config).await.unwrap()
    }

    #[tokio::test]
    async fn transcribe_sends_bearer_authorization_header() {
        let (backend, server) = sidecar_at(
            "/transcribe",
            Some("sk-test-key"),
            vec![(200, r#"{"text": "ok"}"#)],
        )
        .await;
        let text = backend.transcribe(&[1, 2, 3], &test_config()).await;
        assert_eq!(text.unwrap(), "ok");

        // reqwest lowercases header names on the wire, so compare lowercased.
        let head = server.await.unwrap()[0].head.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer sk-test-key"),
            "request head is missing the bearer token: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_sends_no_authorization_header_without_key() {
        let (backend, server) = sidecar(vec![(200, r#"{"text": "ok"}"#)]).await;
        let text = backend.transcribe(&[1, 2, 3], &test_config()).await;
        assert_eq!(text.unwrap(), "ok");

        let head = server.await.unwrap()[0].head.to_ascii_lowercase();
        assert!(
            !head.contains("authorization:"),
            "unauthenticated request must not carry an authorization header: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_posts_to_the_trimmed_path() {
        // A trailing slash makes some OpenAI-compatible servers 307-redirect,
        // which aborts the multipart POST; the request must go out trimmed.
        let (backend, server) =
            sidecar_at("/transcribe/", None, vec![(200, r#"{"text": "ok"}"#)]).await;
        let text = backend.transcribe(&[1, 2, 3], &test_config()).await;
        assert_eq!(text.unwrap(), "ok");

        let head = &server.await.unwrap()[0].head;
        assert!(
            head.starts_with("POST /transcribe HTTP/1.1\r\n"),
            "expected the trailing slash to be trimmed off the request line: {head}"
        );
    }

    /// Trailing slashes come off the path alone.
    #[test]
    fn effective_url_trims_only_the_paths_trailing_slashes() {
        let plain = "http://127.0.0.1:8765/transcribe";
        for (url, posted) in [
            (plain, plain),
            ("http://127.0.0.1:8765/transcribe/", plain),
            ("http://127.0.0.1:8765/transcribe///", plain),
            // Trimmed to "", it would pass the empty-URL guard (which checks
            // the configured value) and die inside reqwest with an opaque
            // relative-URL error.
            ("/", "/"),
            ("///", "///"),
            // A query or a fragment keeps the URL as configured: its last
            // slash can be the query value's, a different `x=` if trimmed.
            (
                "http://127.0.0.1:8765/transcribe?x=1/",
                "http://127.0.0.1:8765/transcribe?x=1/",
            ),
            (
                "http://127.0.0.1:8765/transcribe/?x=1",
                "http://127.0.0.1:8765/transcribe/?x=1",
            ),
            (
                "http://127.0.0.1:8765/transcribe#frag/",
                "http://127.0.0.1:8765/transcribe#frag/",
            ),
        ] {
            assert_eq!(effective_url(url), posted, "{url}");
        }
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = AsrSidecarBackend::new("http://127.0.0.1:8765/transcribe".to_string(), None);
        let err = backend.transcribe(&[], &test_config()).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_missing_url() {
        let backend = AsrSidecarBackend::new(String::new(), None);
        let err = backend.transcribe(&[1, 2, 3], &test_config()).await;
        assert!(err.unwrap_err().to_string().contains("sidecar URL"));
    }

    /// The key is trimmed, and a blank one is no key.
    #[test]
    fn api_key_is_trimmed_and_a_blank_one_is_none() {
        for (given, kept) in [
            ("", None),
            ("   ", None),
            ("  sk-test-key  ", Some("sk-test-key")),
        ] {
            let url = "http://127.0.0.1:8765/transcribe".to_string();
            let backend = AsrSidecarBackend::new(url, Some(given.to_string()));
            assert_eq!(backend.api_key.as_deref(), kept, "{given:?}");
        }
    }

    /// What an error body says: a string `error`, a FastAPI `detail`, or
    /// OpenRouter's message nested in an object. Another `error` shape
    /// leaves the message to the raw body.
    #[test]
    fn parse_sidecar_error_bodies() {
        let openrouter = r#"{"error":{"message":"The selected model does not support keyterms.","code":400},"user_id":"u"}"#;
        for (body, message) in [
            (
                r#"{"error": "model failed to load"}"#,
                Some("model failed to load"),
            ),
            (
                r#"{"detail": "request asked for wrong model"}"#,
                Some("request asked for wrong model"),
            ),
            (
                openrouter,
                Some("The selected model does not support keyterms."),
            ),
            (r#"{"error":{"code":400}}"#, None),
            (r#"{"error": 5}"#, None),
        ] {
            let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
            assert_eq!(parsed.message().as_deref(), message, "{body}");
        }
    }

    /// The error a request fails with names OpenRouter's message, not its
    /// JSON; a body that is no JSON is shown as it is.
    #[tokio::test]
    async fn an_openrouter_error_carries_its_message() {
        let (backend, server) = sidecar(vec![
            (
                400,
                r#"{"error":{"message":"The selected model does not support keyterms. Remove the field","code":400},"user_id":"u"}"#,
            ),
            (400, "Bad Request"),
        ])
        .await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let err = backend.transcribe(&wav, &test_config()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "ASR sidecar error (400): The selected model does not support keyterms. Remove the field"
        );
        let err = backend.transcribe(&wav, &test_config()).await.unwrap_err();
        assert_eq!(err.to_string(), "ASR sidecar error (400): Bad Request");
        server.await.unwrap();
    }

    /// A body with no segment and no timed word: blank text, or none beside
    /// an empty `segments` or `words`, is a server that heard nothing (an
    /// empty transcript); text, or no text and no timing key, asks for the
    /// fallback (`None`). `null` reads as a missing key.
    #[test]
    fn parse_sidecar_without_timings() {
        for (body, heard_nothing) in [
            (r#"{"text": "hello"}"#, false),
            // Empty timing keys place no text.
            (r#"{"text": "hello", "segments": [], "words": []}"#, false),
            (r#"{"text": "hello", "words": []}"#, false),
            ("{}", false),
            (r#"{"text": ""}"#, true),
            (r#"{"text": "  \n"}"#, true),
            (r#"{"text": "", "segments": []}"#, true),
            (r#"{"text": "", "words": []}"#, true),
            (r#"{"text": "", "segments": [], "words": []}"#, true),
            (
                r#"{"text": "hello", "segments": null, "words": null}"#,
                false,
            ),
            (r#"{"text": null}"#, false),
            (r#"{"text": "", "segments": null, "words": null}"#, true),
            (r#"{"text": null, "words": []}"#, true),
        ] {
            let parsed = parse_sidecar_segments(body).unwrap();
            assert_eq!(parsed, heard_nothing.then(Transcript::default), "{body}");
        }
    }

    /// `null` text reads as a missing key beside timings too: the segments'
    /// text, or the words', joined.
    #[test]
    fn parse_sidecar_reads_nulls_as_missing_keys() {
        let segmented = r#"{"text": null, "segments": [{"start": 0.5, "end": 1.0, "text": "hi"}]}"#;
        let t = parse_sidecar_segments(segmented).unwrap().unwrap();
        assert_eq!((t.segments[0].start_ms, t.segments[0].end_ms), (500, 1000));
        assert_eq!(t.segments[0].text, "hi");

        // Words with no text: theirs, joined.
        let words = r#"{"text": null, "words": [
            {"word": "hi", "start": 0.2, "end": 0.4},
            {"word": "there", "start": 0.4, "end": 0.7}]}"#;
        let t = parse_sidecar_segments(words).unwrap().unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!((t.segments[0].start_ms, t.segments[0].end_ms), (200, 700));
        assert_eq!(t.segments[0].text, "hi there");
    }

    /// WhisperX leaves a numeral unaligned: the word takes its times from
    /// its neighbours (the pause around it), so the words still match the
    /// text, and "1990." ends its sentence.
    #[test]
    fn parse_sidecar_times_unaligned_words_from_their_neighbours() {
        let body = r#"{"text": "It was 1990. Then we left.", "words": [
            {"word": "It", "start": 0.0, "end": 0.2, "score": 0.9},
            {"word": "was", "start": 0.2, "end": 0.4, "score": 0.9},
            {"word": "1990."},
            {"word": "Then", "start": 1.6, "end": 1.8, "score": 0.8},
            {"word": "we", "start": 1.8, "end": 1.9, "score": 0.9},
            {"word": "left.", "start": 1.9, "end": 2.2, "score": 0.9}]}"#;
        let t = parse_sidecar_segments(body).unwrap().unwrap();
        let lines: Vec<(u64, u64, &str)> = t
            .segments
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [(0, 1600, "It was 1990."), (1600, 2200, "Then we left.")]
        );
        let numeral = &t.segments[0].words.as_ref().unwrap()[2];
        assert_eq!((numeral.start_ms, numeral.end_ms), (400, 1600));
    }

    /// An unaligned "1990." in a words-only answer over 30 s is kept where
    /// the length cap cuts it: it has its words' times, so the words match
    /// the text's tokens, and the pieces take their text from those.
    #[test]
    fn an_unaligned_word_survives_the_length_cap() {
        // w0..w49, 500 ms each and 100 ms apart, but 800 ms after w19:
        // the cap's cut. "1990." is unaligned, between w48 and w49.
        let mut words: Vec<serde_json::Value> = (0..50u64)
            .map(|i| {
                let start_ms = i * 600 + if i < 20 { 0 } else { 700 };
                serde_json::json!({
                    "word": format!("w{i}"),
                    "start": start_ms as f64 / 1000.0,
                    "end": (start_ms + 500) as f64 / 1000.0,
                })
            })
            .collect();
        words.insert(49, serde_json::json!({"word": "1990."}));
        let text: Vec<&str> = words.iter().map(|w| w["word"].as_str().unwrap()).collect();
        let text = text.join(" ");
        let body = serde_json::json!({"text": text, "words": words}).to_string();

        let t = parse_sidecar_segments(&body).unwrap().unwrap();
        let spans: Vec<(u64, u64)> = t.segments.iter().map(|s| (s.start_ms, s.end_ms)).collect();
        assert_eq!(spans, [(0, 11_900), (12_700, 30_100), (30_100, 30_600)]);
        assert!(
            t.segments[1].text.ends_with(" w48 1990."),
            "{}",
            t.segments[1].text
        );
        let pieces: Vec<&str> = t.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(pieces.join(" "), text, "no word lost");
    }

    /// `"segments": []` beside words is read as words alone, not as a
    /// server that heard nothing. Groq and OpenAI's parse is unchanged.
    #[test]
    fn parse_sidecar_reads_words_beside_empty_segments() {
        let body = r#"{"text": "Hi there. Bye now", "segments": [], "words": [
            {"word": "Hi", "start": 0.0, "end": 0.3},
            {"word": "there.", "start": 0.3, "end": 0.8},
            {"word": "Bye", "start": 2.0, "end": 2.3},
            {"word": "now", "start": 2.3, "end": 2.6}]}"#;
        let t = parse_sidecar_segments(body).unwrap().unwrap();
        let lines: Vec<(u64, u64, &str)> = t
            .segments
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.as_str()))
            .collect();
        assert_eq!(lines, [(0, 800, "Hi there."), (2000, 2600, "Bye now")]);
        let verbose = crate::transcription::verbose_json::parse_verbose_segments(body).unwrap();
        assert!(verbose.segments.is_empty());
    }

    /// The state leaves UNSETTLED once, and never comes back.
    #[test]
    fn the_timing_state_moves_once() {
        let state = TimingState::new();
        assert!(state.settle(TimingForm::Verbose));
        assert!(
            !state.settle(TimingForm::Words),
            "not moved by a later chunk"
        );
        assert!(!state.fall_back(), "a later unusable answer is its chunk's");
        assert_eq!(state.first_form(), Some(TimingForm::Verbose));

        let state = TimingState::new();
        assert!(state.fall_back());
        assert!(!state.settle(TimingForm::Words));
        assert_eq!(state.first_form(), None);
    }

    /// The timing fields of a request, as `name=value`.
    fn timing_fields(request: &Request) -> Vec<String> {
        request
            .text_fields()
            .into_iter()
            .filter(|(name, _)| name == "response_format" || name == "timestamp_granularities[]")
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    const WORDS_FIELDS: [&str; 3] = [
        "response_format=verbose_json",
        "timestamp_granularities[]=segment",
        "timestamp_granularities[]=word",
    ];
    const VERBOSE_FIELDS: [&str; 1] = ["response_format=verbose_json"];

    /// The text fields dictation sends, and a phrase of the fallback.
    fn plain_fields() -> Vec<(String, String)> {
        vec![
            ("model".to_string(), "test-asr-model".to_string()),
            ("language".to_string(), "en".to_string()),
        ]
    }

    /// The RMS test, so the tone below is one phrase to the fallback.
    fn phrase_config() -> TranscriptionConfig {
        TranscriptionConfig {
            vad: audio_silence_gate::Vad::Rms,
            ..test_config()
        }
    }

    /// 1 s of silence, 1 s of a 440 Hz tone, 1 s of silence: one phrase.
    fn one_phrase_wav() -> Vec<u8> {
        let mut samples = vec![0i16; 16_000];
        samples.extend((0..16_000).map(|i| {
            let t = f64::from(i) / 16_000.0;
            ((t * 440.0 * std::f64::consts::TAU).sin() * 6000.0) as i16
        }));
        samples.extend(vec![0i16; 16_000]);
        crate::audio::wav::encode_wav(&samples).unwrap()
    }

    const SEGMENT_OK: &str =
        r#"{"text": "ok", "segments": [{"start": 1.0, "end": 2.0, "text": "ok"}]}"#;

    /// Two wordless segments: enough to show the server times what it hears.
    const TWO_SEGMENTS: &str = r#"{"text": "Hi. Bye.", "segments": [
        {"start": 0.5, "end": 1.0, "text": " Hi."},
        {"start": 1.5, "end": 2.0, "text": " Bye."}]}"#;

    /// One wordless segment over a 45 s clip: too coarse to place its text.
    const LONG_SEGMENT: &str = r#"{"text": "a long talk", "segments": [
        {"id": 0, "start": 0.0, "end": 45.0, "text": "a long talk"}]}"#;

    /// A 400 to the word timings (OpenRouter's voxtral), or a 422 (FastAPI
    /// refusing a field), asks again with verbose_json alone, and the next
    /// chunk starts there.
    #[tokio::test]
    async fn a_refused_timing_request_steps_down_and_the_form_is_kept() {
        let fastapi = r#"{"detail": [{"loc": ["body", "timestamp_granularities[]"],
            "msg": "extra fields not permitted", "type": "value_error.extra"}]}"#;
        for (status, body) in [
            (
                400,
                r#"{"error":{"message":"Provider returned 400","code":400}}"#,
            ),
            (422, fastapi),
        ] {
            let (backend, server) =
                sidecar(vec![(status, body), (200, TWO_SEGMENTS), (200, SEGMENT_OK)]).await;
            let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
            let first = chunk(&backend, &wav, &test_config()).await;
            let texts: Vec<&str> = first.segments.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(texts, ["Hi.", "Bye."], "{status}");
            let second = chunk(&backend, &wav, &test_config()).await;
            assert_eq!(second.segments[0].text, "ok", "{status}");

            let requests = server.await.unwrap();
            assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS, "{status}");
            assert_eq!(timing_fields(&requests[1]), VERBOSE_FIELDS, "{status}");
            assert_eq!(
                timing_fields(&requests[2]),
                VERBOSE_FIELDS,
                "{status} sticky"
            );
            assert_eq!(backend.timings.first_form(), Some(TimingForm::Verbose));
        }
    }

    /// Both verbose forms refused: the plain request goes out, and its
    /// text alone sends the chunk phrase by phrase, plain requests too.
    #[tokio::test]
    async fn refused_twice_the_plain_answer_falls_back_to_phrases() {
        let (backend, server) = sidecar(vec![
            (400, r#"{"detail": "unsupported response_format"}"#),
            (400, r#"{"detail": "unsupported response_format"}"#),
            (200, r#"{"text": "the whole chunk"}"#),
            (200, r#"{"text": "hi"}"#),
        ])
        .await;
        let t = chunk(&backend, &one_phrase_wav(), &phrase_config()).await;
        let texts: Vec<&str> = t.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["hi"]);

        let requests = server.await.unwrap();
        assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[1]), VERBOSE_FIELDS);
        assert_eq!(requests[2].text_fields(), plain_fields());
        assert_eq!(requests[3].text_fields(), plain_fields(), "the phrase");
        assert_eq!(backend.timings.first_form(), None);
    }

    /// A 400 to the plain request is the chunk's error, as it always was,
    /// and nothing is remembered.
    #[tokio::test]
    async fn a_400_to_the_plain_request_is_the_chunks_error() {
        let refused = r#"{"detail": "request asked for wrong model"}"#;
        let (backend, server) = sidecar(vec![(400, refused), (400, refused), (400, refused)]).await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let err = backend
            .transcribe_segments(&wav, &test_config())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ASR sidecar error (400): request asked for wrong model"
        );
        let requests = server.await.unwrap();
        assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[1]), VERBOSE_FIELDS);
        assert_eq!(requests[2].text_fields(), plain_fields());
        assert_eq!(
            backend.timings.0.load(Ordering::Relaxed),
            TimingState::UNSETTLED
        );
    }

    /// nemo-speech (Nemotron, Parakeet) answers `words` alone, punctuation
    /// on the words: one segment per sentence or pause, timed in the chunk.
    #[tokio::test]
    async fn a_words_only_answer_becomes_sentence_segments() {
        let (backend, server) = sidecar(vec![(
            200,
            r#"{"duration": 6.0, "language": "en", "task": "transcribe",
                "text": "Everything looks normal. Next we ship it on Friday okay",
                "words": [
                    {"word": "Everything", "start": 0.4, "end": 0.88, "confidence": 0.97},
                    {"word": "looks", "start": 0.88, "end": 1.2, "confidence": 0.99},
                    {"word": "normal.", "start": 1.2, "end": 1.76, "confidence": 0.98},
                    {"word": "Next", "start": 2.08, "end": 2.32, "confidence": 0.95},
                    {"word": "we", "start": 2.32, "end": 2.48, "confidence": 0.99},
                    {"word": "ship", "start": 2.48, "end": 2.8, "confidence": 0.99},
                    {"word": "it", "start": 2.8, "end": 2.88, "confidence": 0.99},
                    {"word": "on", "start": 2.88, "end": 3.04, "confidence": 0.99},
                    {"word": "Friday", "start": 3.04, "end": 3.6, "confidence": 0.96},
                    {"word": "okay", "start": 5.2, "end": 5.6, "confidence": 0.9}]}"#,
        )])
        .await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let t = chunk(&backend, &wav, &test_config()).await;
        server.await.unwrap();
        let lines: Vec<(u64, u64, &str, usize)> = t
            .segments
            .iter()
            .map(|s| {
                let words = s.words.as_ref().map_or(0, Vec::len);
                (s.start_ms, s.end_ms, s.text.as_str(), words)
            })
            .collect();
        assert_eq!(
            lines,
            [
                (400, 1760, "Everything looks normal.", 3),
                (2080, 3600, "Next we ship it on Friday", 6),
                (5200, 5600, "okay", 1),
            ]
        );
        assert_eq!(backend.timings.first_form(), Some(TimingForm::Words));
    }

    /// A first answer that cannot place its text sends the chunk phrase by
    /// phrase, and the next one too, without asking for timings again.
    #[tokio::test]
    async fn an_unplaced_first_answer_falls_back_to_phrases_for_good() {
        for body in [
            // One segment over a long clip and no words (OpenRouter's
            // nemotron-3.5-transcribe).
            LONG_SEGMENT,
            // Text alone: the bundled sidecars ignore the timing fields.
            r#"{"text": "the whole chunk"}"#,
            // Timings that do not parse (a time sent as a string) are no
            // usable timings, not the chunk's error.
            r#"{"text": "hi there", "segments": [
                {"start": "0.0", "end": 1.0, "text": "hi there"}]}"#,
        ] {
            let (backend, server) = sidecar(vec![
                (200, body),
                (200, r#"{"text": "hi"}"#),
                (200, r#"{"text": "hello"}"#),
            ])
            .await;
            let wav = one_phrase_wav();
            let first = chunk(&backend, &wav, &phrase_config()).await;
            let texts: Vec<&str> = first.segments.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(texts, ["hi"], "{body}");
            let second = chunk(&backend, &wav, &phrase_config()).await;
            assert_eq!(second.segments[0].text, "hello", "{body}");

            let requests = server.await.unwrap();
            assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS, "{body}");
            assert_eq!(requests[1].text_fields(), plain_fields(), "{body}");
            assert_eq!(requests[2].text_fields(), plain_fields(), "sticky: {body}");
            assert_eq!(backend.timings.first_form(), None, "{body}");
        }
    }

    /// A wordless segment of 30 s or less is used as returned, and so is a
    /// longer one with a speaker (a diarizing sidecar's turn). Alone in its
    /// answer, neither settles the form: a server that times only the
    /// whole clip answers a short chunk the same way.
    #[tokio::test]
    async fn a_short_or_diarized_segment_without_words_is_used() {
        let (backend, server) = sidecar(vec![
            (
                200,
                r#"{"text": "short", "segments": [
                    {"start": 0.0, "end": 30.0, "text": "short"}]}"#,
            ),
            (
                200,
                r#"{"text": "monologue", "segments": [
                    {"start": 0.0, "end": 45.0, "text": "monologue", "speaker": "SPEAKER_00"}]}"#,
            ),
        ])
        .await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let short = chunk(&backend, &wav, &test_config()).await;
        assert_eq!(
            (short.segments[0].start_ms, short.segments[0].end_ms),
            (0, 30_000)
        );
        let turn = chunk(&backend, &wav, &test_config()).await;
        assert_eq!(turn.segments[0].end_ms, 45_000);
        assert_eq!(turn.segments[0].speaker.as_deref(), Some("SPEAKER_00"));
        assert_eq!(server.await.unwrap().len(), 2);
        assert_eq!(
            backend.timings.0.load(Ordering::Relaxed),
            TimingState::UNSETTLED
        );
    }

    /// OpenRouter's nemotron-3.5-transcribe answers one wordless segment
    /// for the whole clip. A short chunk's is used and settles nothing, so
    /// a later long chunk's still falls back to phrases for good: the chunk
    /// after it skips the whole-chunk request.
    #[tokio::test]
    async fn after_a_short_wordless_segment_a_long_one_still_falls_back_for_good() {
        let (backend, server) = sidecar(vec![
            (200, SEGMENT_OK),
            (200, LONG_SEGMENT),
            (200, r#"{"text": "hi"}"#),
            (200, r#"{"text": "hello"}"#),
        ])
        .await;
        let wav = one_phrase_wav();
        let short = chunk(&backend, &wav, &phrase_config()).await;
        assert_eq!(short.segments[0].text, "ok");
        assert_eq!(
            backend.timings.0.load(Ordering::Relaxed),
            TimingState::UNSETTLED
        );
        let mut texts = Vec::new();
        for _ in 0..2 {
            let t = chunk(&backend, &wav, &phrase_config()).await;
            texts.push(t.segments[0].text.clone());
        }
        assert_eq!(texts, ["hi", "hello"]);

        let requests = server.await.unwrap();
        assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[1]), WORDS_FIELDS);
        assert_eq!(requests[2].text_fields(), plain_fields(), "the phrase");
        assert_eq!(
            requests[3].text_fields(),
            plain_fields(),
            "no timing request"
        );
        assert_eq!(backend.timings.first_form(), None);
    }

    /// A blank answer heard nothing, which says nothing about timings: an
    /// empty transcript, no phrase fallback, and the next chunk asks as
    /// before.
    #[tokio::test]
    async fn a_blank_answer_is_heard_nothing_and_settles_nothing() {
        let (backend, server) = sidecar(vec![
            (200, r#"{"text": " "}"#),
            (200, r#"{"text": "", "segments": [], "words": []}"#),
            (200, TWO_SEGMENTS),
        ])
        .await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        for _ in 0..2 {
            let t = chunk(&backend, &wav, &test_config()).await;
            assert_eq!(t, Transcript::default());
            assert_eq!(
                backend.timings.0.load(Ordering::Relaxed),
                TimingState::UNSETTLED
            );
        }
        let t = chunk(&backend, &wav, &test_config()).await;
        assert_eq!(t.segments[0].text, "Hi.");

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 3, "no phrase was sent");
        for request in &requests {
            assert_eq!(timing_fields(request), WORDS_FIELDS);
        }
        assert_eq!(
            backend.timings.0.load(Ordering::Relaxed),
            TimingState::WORDS
        );
    }

    /// Once a form gave usable timings, a later answer that cannot place
    /// its text (a long wordless segment, text alone) sends that chunk
    /// phrase by phrase, and the next chunk asks with the form again.
    #[tokio::test]
    async fn once_settled_an_unplaced_answer_falls_back_for_its_chunk_only() {
        let (backend, server) = sidecar(vec![
            (200, TWO_SEGMENTS),
            (200, LONG_SEGMENT),
            (200, r#"{"text": "hi"}"#),
            (200, r#"{"text": "the whole chunk"}"#),
            (200, r#"{"text": "hello"}"#),
            (200, SEGMENT_OK),
        ])
        .await;
        let wav = one_phrase_wav();
        let mut texts = Vec::new();
        for _ in 0..4 {
            let t = chunk(&backend, &wav, &phrase_config()).await;
            texts.push(t.segments[0].text.clone());
            assert_eq!(backend.timings.first_form(), Some(TimingForm::Words));
        }
        assert_eq!(texts, ["Hi.", "hi", "hello", "ok"]);

        let requests = server.await.unwrap();
        assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[1]), WORDS_FIELDS);
        assert_eq!(requests[2].text_fields(), plain_fields(), "the phrase");
        assert_eq!(timing_fields(&requests[3]), WORDS_FIELDS);
        assert_eq!(requests[4].text_fields(), plain_fields(), "the phrase");
        assert_eq!(timing_fields(&requests[5]), WORDS_FIELDS);
    }

    /// Once a form gave usable timings, a refusal of it steps down for that
    /// chunk alone: the next chunk asks with the remembered form again.
    #[tokio::test]
    async fn once_settled_a_refusal_steps_down_for_its_chunk_only() {
        let (backend, server) = sidecar(vec![
            (200, TWO_SEGMENTS),
            (
                400,
                r#"{"detail": "timestamp_granularities not supported"}"#,
            ),
            (200, TWO_SEGMENTS),
            (200, SEGMENT_OK),
        ])
        .await;
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        chunk(&backend, &wav, &test_config()).await;
        let refused = chunk(&backend, &wav, &test_config()).await;
        let texts: Vec<&str> = refused.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Hi.", "Bye."]);
        assert_eq!(backend.timings.first_form(), Some(TimingForm::Words));
        chunk(&backend, &wav, &test_config()).await;

        let requests = server.await.unwrap();
        assert_eq!(timing_fields(&requests[0]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[1]), WORDS_FIELDS);
        assert_eq!(timing_fields(&requests[2]), VERBOSE_FIELDS);
        assert_eq!(timing_fields(&requests[3]), WORDS_FIELDS, "not moved down");
    }

    /// A 500 from the sidecar (an exception in its own code) is not
    /// retried; a 502 from a proxy in front of it is.
    #[tokio::test]
    async fn a_500_is_final_but_a_502_is_retried() {
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let config = test_config();
        let quick = [std::time::Duration::from_millis(1); 3];

        // The 502 first, and quick network waits: a 500 retried by mistake
        // fails at once against the closed server instead of waiting it out.
        let (backend, server) = sidecar(vec![(502, "Bad Gateway"), (200, SEGMENT_OK)]).await;
        let t = retry_with_backoff(RATE_LIMIT_BUDGET, &quick, &quick, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap();
        assert_eq!(t.segments[0].text, "ok");
        assert_eq!(server.await.unwrap().len(), 2);

        let (backend, server) = sidecar(vec![(500, r#"{"detail": "CUDA out of memory"}"#)]).await;
        let err = retry_with_backoff(RATE_LIMIT_BUDGET, &quick, &quick, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        assert_eq!(server.await.unwrap().len(), 1);
        assert!(
            err.to_string().starts_with("ASR sidecar error (500)"),
            "{err}"
        );
        assert!(err
            .downcast_ref::<crate::transcription::RetryableHttpError>()
            .is_none());
    }

    /// A sidecar that is not running refuses the connection: retried on the
    /// network schedule, then waited out like no network. One that read the
    /// request and dropped the connection crashed on it: final at once, the
    /// chunk is not sent again, like the sidecar's HTTP 500.
    #[tokio::test]
    async fn a_sidecar_that_is_down_is_waited_out_but_a_crash_is_final() {
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let config = test_config();
        let quick = [std::time::Duration::from_millis(1); 4];

        let (_closed, addr) = crate::transcription::test_server::refused_addr();
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = retry_with_backoff(RATE_LIMIT_BUDGET, &[], &quick, || {
            calls.fetch_add(1, Ordering::Relaxed);
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        assert!(is_network_unavailable(&err), "{err:#}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            5,
            "tried once and retried 4 times"
        );

        // It takes the whole request, then the connection drops. A retry
        // would find the port closed and end as a network wait instead.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { crash_after_request(&listener).await });
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = retry_with_backoff(RATE_LIMIT_BUDGET, &[], &quick, || {
            calls.fetch_add(1, Ordering::Relaxed);
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        let request = server.await.unwrap();
        assert!(request.file_part().1.len() > wav.len(), "read whole");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "not sent again");
        assert!(!is_network_unavailable(&err), "{err:#}");
        assert!(!is_transport_error(&err), "{err:#}");
        assert!(
            err.to_string().starts_with(
                "the ASR sidecar closed the connection while handling the request, so it \
                 likely crashed on this audio; not sent again: "
            ),
            "{err:#}"
        );
    }

    /// A request that times out after it was sent (a hosted gateway behind a
    /// network that went quiet) is the network, not a crash: retried on the
    /// network schedule, then waited out.
    #[tokio::test]
    async fn a_sidecar_timeout_after_the_request_is_waited_out() {
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let config = test_config();
        let quick = [std::time::Duration::from_millis(1); 4];

        // Takes each request and never answers, holding the connection.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                held.push(accept_request(&listener).await.0);
            }
        });
        let backend = AsrSidecarBackend {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(200))
                .build()
                .unwrap(),
            ..AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None)
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = retry_with_backoff(RATE_LIMIT_BUDGET, &[], &quick, || {
            calls.fetch_add(1, Ordering::Relaxed);
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        server.abort();
        assert!(is_network_unavailable(&err), "{err:#}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            5,
            "tried once and retried 4 times"
        );
    }
}
