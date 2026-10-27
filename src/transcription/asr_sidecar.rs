//! Generic HTTP ASR sidecar transcription backend.
//!
//! This backend keeps the Rust daemon independent from Python/PyTorch by
//! sending WAV audio to a local HTTP sidecar.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    secs_to_ms, segments_by_phrase, Segment, StreamText, Transcript, TranscriptionBackend,
    TranscriptionConfig,
};

/// Keep a guardrail so a runaway recording does not create an unbounded
/// multipart request.
const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

/// Generic HTTP ASR sidecar transcription backend.
pub struct AsrSidecarBackend {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    /// Set once the sidecar answered without `segments`, so later recording
    /// chunks go straight to the phrase fallback.
    no_segments: AtomicBool,
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
            no_segments: AtomicBool::new(false),
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
    /// POST `audio` to the sidecar and return the raw success body.
    async fn post(&self, audio: &[u8], config: &TranscriptionConfig) -> anyhow::Result<String> {
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
            let message = match serde_json::from_str::<AsrSidecarErrorResponse>(&body) {
                Ok(err_resp) => format!("ASR sidecar error ({code}): {}", err_resp.message()),
                Err(_) => format!("ASR sidecar error ({code}): {body}"),
            };
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
}

fn sidecar_error(err: reqwest::Error) -> anyhow::Error {
    super::retry::final_if_crashed(err, "the ASR sidecar")
}

/// Longest chunk sent to the sidecar in one request.
const SIDECAR_CHUNK_SECS: u32 = 600;

#[derive(Debug, Deserialize)]
struct SidecarSegmentsResponse {
    #[serde(default)]
    segments: Option<Vec<SidecarSegment>>,
}

#[derive(Debug, Deserialize)]
struct SidecarSegment {
    start: f64,
    end: f64,
    #[serde(default)]
    text: String,
    /// Diarizing sidecars label speakers with a string or a number.
    #[serde(default)]
    speaker: Option<serde_json::Value>,
}

/// Parse the optional `segments: [{start, end, text, speaker?}]` (seconds) of
/// a sidecar response.
///
/// `Ok(None)` means the sidecar sent no `segments` key at all and the caller
/// should fall back to phrases; `Ok(Some(..))` with no segments is a sidecar
/// that segments but heard nothing.
pub fn parse_sidecar_segments(body: &str) -> anyhow::Result<Option<Transcript>> {
    let parsed: SidecarSegmentsResponse = serde_json::from_str(body)?;
    Ok(parsed.segments.map(|segments| Transcript {
        segments: segments
            .into_iter()
            .filter(|s| !s.text.trim().is_empty())
            .map(|s| {
                let start_ms = secs_to_ms(s.start);
                Segment {
                    start_ms,
                    end_ms: secs_to_ms(s.end).max(start_ms),
                    text: s.text.trim().to_string(),
                    speaker: s.speaker.and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s),
                        serde_json::Value::Null => None,
                        other => Some(other.to_string()),
                    }),
                    words: None,
                }
            })
            .collect(),
    }))
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
    error: Option<String>,
    detail: Option<serde_json::Value>,
}

impl AsrSidecarErrorResponse {
    fn message(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        match &self.detail {
            Some(serde_json::Value::String(detail)) => detail.clone(),
            Some(detail) => detail.to_string(),
            None => "unknown sidecar error".to_string(),
        }
    }
}

#[async_trait]
impl TranscriptionBackend for AsrSidecarBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        let body = self.post(audio, config).await?;
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

    // Native segments when the sidecar returns them; otherwise the phrase
    // fallback, and the whole-chunk request is not repeated for later chunks.
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript> {
        if !self.no_segments.load(Ordering::Relaxed) {
            let body = self.post(audio, config).await?;
            if let Some(transcript) = parse_sidecar_segments(&body)? {
                return Ok(transcript);
            }
            debug!("ASR sidecar returned no segments; transcribing phrase by phrase");
            self.no_segments.store(true, Ordering::Relaxed);
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
        is_network_unavailable, is_transport_error, retry_transient, retry_with_backoff,
        RATE_LIMIT_BUDGET,
    };
    use crate::transcription::test_server::{
        accept_request, crash_after_request, serve_once, serve_responses,
    };
    use tokio::net::TcpListener;

    fn test_config() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
        }
    }

    #[tokio::test]
    async fn transcribe_sends_bearer_authorization_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        let backend = AsrSidecarBackend::new(
            format!("http://{addr}/transcribe"),
            Some("sk-test-key".to_string()),
        );
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        // reqwest lowercases header names on the wire, so compare lowercased.
        let head = server.await.unwrap().head.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer sk-test-key"),
            "request head is missing the bearer token: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_sends_no_authorization_header_without_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let head = server.await.unwrap().head.to_ascii_lowercase();
        assert!(
            !head.contains("authorization:"),
            "unauthenticated request must not carry an authorization header: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_posts_to_the_trimmed_path() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        // A trailing slash makes some OpenAI-compatible servers 307-redirect,
        // which aborts the multipart POST; the request must go out trimmed.
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe/"), None);
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let head = server.await.unwrap().head;
        assert!(
            head.starts_with("POST /transcribe HTTP/1.1\r\n"),
            "expected the trailing slash to be trimmed off the request line: {head}"
        );
    }

    #[test]
    fn effective_url_leaves_a_plain_url_alone() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_trims_one_trailing_slash() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe/"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_trims_multiple_trailing_slashes() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe///"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_keeps_original_when_trimming_would_empty_it() {
        // Trimming "/" to "" passes the empty-URL guard (which checks the
        // configured value) and then dies inside reqwest with an opaque
        // relative-URL error. Keep what the user configured instead.
        assert_eq!(effective_url("/"), "/");
        assert_eq!(effective_url("///"), "///");
    }

    #[test]
    fn effective_url_leaves_a_query_alone() {
        // The trailing slash belongs to the query value, not the path, so
        // trimming it would post a different `x=` than the user configured.
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe?x=1/"),
            "http://127.0.0.1:8765/transcribe?x=1/"
        );
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe/?x=1"),
            "http://127.0.0.1:8765/transcribe/?x=1"
        );
    }

    #[test]
    fn effective_url_leaves_a_fragment_alone() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe#frag/"),
            "http://127.0.0.1:8765/transcribe#frag/"
        );
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = AsrSidecarBackend::new("http://127.0.0.1:8765/transcribe".to_string(), None);
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_missing_url() {
        let backend = AsrSidecarBackend::new(String::new(), None);
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
        };
        let err = backend.transcribe(&[1, 2, 3], &config).await.unwrap_err();
        assert!(err.to_string().contains("sidecar URL"));
    }

    #[test]
    fn empty_api_key_is_normalized_to_none() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some(String::new()),
        );
        assert!(backend.api_key.is_none());
    }

    #[test]
    fn api_key_is_stored_when_present() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("sk-test-key".to_string()),
        );
        assert_eq!(backend.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn whitespace_only_api_key_is_normalized_to_none() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("   ".to_string()),
        );
        assert!(backend.api_key.is_none());
    }

    #[test]
    fn api_key_whitespace_is_trimmed() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("  sk-test-key  ".to_string()),
        );
        assert_eq!(backend.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn parse_asr_sidecar_response() {
        let body = r#"{"text": "Hello world"}"#;
        let parsed: AsrSidecarResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.text, "Hello world");
    }

    #[test]
    fn parse_asr_sidecar_error() {
        let body = r#"{"error": "model failed to load"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "model failed to load");
    }

    #[test]
    fn parse_fastapi_error_detail() {
        let body = r#"{"detail": "request asked for wrong model"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "request asked for wrong model");
    }

    #[test]
    fn parse_sidecar_with_segments() {
        let body = r#"{"text": "hi there. yes", "segments": [
            {"start": 0.5, "end": 1.25, "text": " hi there.", "speaker": "SPEAKER_00"},
            {"start": 2.0, "end": 2.4, "text": "yes", "speaker": 1},
            {"start": 3.0, "end": 3.1, "text": "  "}
        ]}"#;
        let t = parse_sidecar_segments(body)
            .unwrap()
            .expect("segments present");
        assert_eq!(t.segments.len(), 2);
        assert_eq!((t.segments[0].start_ms, t.segments[0].end_ms), (500, 1250));
        assert_eq!(t.segments[0].text, "hi there.");
        assert_eq!(t.segments[0].speaker.as_deref(), Some("SPEAKER_00"));
        assert_eq!(t.segments[1].speaker.as_deref(), Some("1"));
    }

    #[test]
    fn parse_sidecar_without_segments_asks_for_the_fallback() {
        assert!(parse_sidecar_segments(r#"{"text": "hello"}"#)
            .unwrap()
            .is_none());
        let empty = parse_sidecar_segments(r#"{"text": "", "segments": []}"#).unwrap();
        assert_eq!(empty, Some(Transcript::default()));
    }

    #[tokio::test]
    async fn transcribe_segments_uses_sidecar_segments() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            r#"{"text": "ok", "segments": [{"start": 1.0, "end": 2.0, "text": "ok"}]}"#,
        ));

        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let t = backend
            .transcribe_segments(&wav, &test_config())
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.segments[0].start_ms, 1000);
        assert!(!backend.no_segments.load(Ordering::Relaxed));
    }
    /// A Cloudflare origin timeout (524) on a recording chunk is retried on the
    /// server-error schedule, here with short stand-ins for its waits.
    #[tokio::test]
    async fn a_524_is_retried_until_the_chunk_goes_through() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_responses(
            listener,
            vec![
                (524, "error code: 524"),
                (
                    200,
                    r#"{"text": "ok", "segments": [{"start": 1.0, "end": 2.0, "text": "ok"}]}"#,
                ),
            ],
        ));
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let quick = [std::time::Duration::from_millis(1); 3];
        let config = test_config();
        let t = retry_transient(RATE_LIMIT_BUDGET, &quick, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap();
        assert_eq!(t.segments[0].text, "ok");
        assert_eq!(server.await.unwrap().len(), 2);
    }
    /// A 500 from the sidecar (an exception in its own code) is not
    /// retried; a 502 from a proxy in front of it is.
    #[tokio::test]
    async fn a_500_is_final_but_a_502_is_retried() {
        let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
        let config = test_config();
        let quick = [std::time::Duration::from_millis(1); 3];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_responses(
            listener,
            vec![(500, r#"{"detail": "CUDA out of memory"}"#)],
        ));
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let err = retry_transient(RATE_LIMIT_BUDGET, &quick, || {
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

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_responses(
            listener,
            vec![
                (502, "Bad Gateway"),
                (
                    200,
                    r#"{"text": "ok", "segments": [{"start": 1.0, "end": 2.0, "text": "ok"}]}"#,
                ),
            ],
        ));
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let t = retry_transient(RATE_LIMIT_BUDGET, &quick, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap();
        assert_eq!(t.segments[0].text, "ok");
        assert_eq!(server.await.unwrap().len(), 2);
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
