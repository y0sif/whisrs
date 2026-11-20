//! Groq Whisper API transcription backend.
//!
//! Sends WAV audio (FLAC for recording chunks) to Groq's
//! `/openai/v1/audio/transcriptions` endpoint and returns the transcribed
//! text. Dictation sends the whole buffer in one request once it stops.

use asr_dedup::Word as AsrWord;
use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use super::flac::{recording_upload, UploadFormat};
use super::verbose_json::parse_verbose_segments;
use super::{StreamText, Transcript, TranscriptionBackend, TranscriptionConfig, UPLOAD_CHUNK_SECS};

/// Maximum file size accepted by the Groq API (25 MB).
const MAX_FILE_SIZE: usize = 25 * 1024 * 1024;

/// Groq API endpoint for audio transcription.
const GROQ_TRANSCRIPTION_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// Groq transcription backend.
pub struct GroqBackend {
    client: reqwest::Client,
    api_key: String,
    /// [`GROQ_TRANSCRIPTION_URL`]; tests point it at a local server.
    url: String,
}

impl GroqBackend {
    /// Create a new Groq backend with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            url: GROQ_TRANSCRIPTION_URL.to_string(),
        }
    }

    /// POST `audio`, in `format`, as a `verbose_json` request with the given
    /// `timestamp_granularities[]` and return the raw success body.
    async fn post(
        &self,
        audio: &[u8],
        format: UploadFormat,
        config: &TranscriptionConfig,
        granularities: &[&str],
    ) -> anyhow::Result<String> {
        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(build_form(audio, format, config, granularities)?)
            .send()
            .await?;

        let status = response.status();
        let retry_after = super::retry_after_header(response.headers());
        let body = response.text().await?;

        if !status.is_success() {
            let code = status.as_u16();
            // Try to parse error response for a better message.
            let message = match serde_json::from_str::<GroqErrorResponse>(&body) {
                Ok(err_resp) => match code {
                    401 => format!("Groq API: invalid API key: {}", err_resp.error.message),
                    429 => format!("Groq API: rate limited: {}", err_resp.error.message),
                    _ => format!("Groq API error ({code}): {}", err_resp.error.message),
                },
                Err(_) => format!("Groq API error ({code}): {body}"),
            };
            return Err(super::http_error(code, retry_after, message));
        }
        Ok(body)
    }
}

/// Reject audio the API would refuse before uploading it.
fn check_audio_size(audio: &[u8]) -> anyhow::Result<()> {
    if audio.len() > MAX_FILE_SIZE {
        anyhow::bail!(
            "audio file too large ({} bytes, max {} bytes / 25MB)",
            audio.len(),
            MAX_FILE_SIZE
        );
    }
    if audio.is_empty() {
        anyhow::bail!("cannot transcribe empty audio");
    }
    Ok(())
}

/// The multipart form for one transcription request.
///
/// Dictation sends WAV and asks for `word` timestamps only; recordings send
/// FLAC and add `segment`.
fn build_form(
    audio: &[u8],
    format: UploadFormat,
    config: &TranscriptionConfig,
    granularities: &[&str],
) -> anyhow::Result<multipart::Form> {
    let file_part = multipart::Part::bytes(audio.to_vec())
        .file_name(format.file_name)
        .mime_str(format.mime)?;

    let mut form = multipart::Form::new()
        .part("file", file_part)
        .text("model", config.model.clone())
        .text("response_format", "verbose_json");
    for granularity in granularities {
        form = form.text("timestamp_granularities[]", granularity.to_string());
    }

    // Only set language if not "auto" — letting the API auto-detect is the default.
    if config.language != "auto" {
        form = form.text("language", config.language.clone());
    }
    if let Some(prompt) = &config.prompt {
        form = form.text("prompt", prompt.clone());
    }
    Ok(form)
}

/// The verbose JSON response from Groq's transcription API.
#[derive(Debug, Deserialize)]
pub struct GroqTranscriptionResponse {
    /// The transcribed text.
    pub text: String,
    /// Word-level segments (when requested with timestamp_granularities).
    #[serde(default)]
    pub words: Vec<GroqWord>,
}

/// A single word with timestamps from the Groq API.
#[derive(Debug, Clone, Deserialize)]
pub struct GroqWord {
    pub word: String,
    pub start: f64,
    pub end: f64,
}

impl From<&GroqWord> for AsrWord {
    fn from(w: &GroqWord) -> Self {
        Self {
            text: w.word.clone(),
            start_secs: w.start,
            end_secs: w.end,
        }
    }
}

/// Error response from the Groq API.
#[derive(Debug, Deserialize)]
pub struct GroqErrorResponse {
    pub error: GroqErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct GroqErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: Option<String>,
}

#[async_trait]
impl TranscriptionBackend for GroqBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        check_audio_size(audio)?;

        debug!(
            "sending {} bytes to Groq API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let body = self
            .post(audio, UploadFormat::WAV, config, &["word"])
            .await?;
        let parsed: GroqTranscriptionResponse = serde_json::from_str(&body)?;

        if !parsed.words.is_empty() {
            debug!("received {} words from Groq", parsed.words.len());
        }

        let text = parsed.text.trim().to_string();
        if text.is_empty() {
            warn!("Groq returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe) since this
    // backend does not support streaming.
    fn supports_streaming(&self) -> bool {
        false
    }

    // Whisper's `prompt` multipart field, set by `build_form` for every
    // request.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
    }

    fn stream_text(&self) -> StreamText {
        StreamText::Phrases
    }

    // Native timestamps: verbose_json segments, parsed by
    // `verbose_json::parse_verbose_segments` (no-speech heuristic included).
    // The chunk goes up as FLAC.
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript> {
        let upload = recording_upload(audio).await;
        check_audio_size(&upload.bytes)?;
        debug!(
            "sending {} bytes ({}, WAV was {}) to Groq API for segments (model={})",
            upload.bytes.len(),
            upload.format.mime,
            audio.len(),
            config.model
        );
        let body = self
            .post(&upload.bytes, upload.format, config, &["segment", "word"])
            .await?;
        parse_verbose_segments(&body)
    }

    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        Some(UPLOAD_CHUNK_SECS)
    }
}

/// Parse a Groq verbose JSON response body into a `GroqTranscriptionResponse`.
///
/// Exposed for unit testing.
pub fn parse_response(body: &str) -> anyhow::Result<GroqTranscriptionResponse> {
    let parsed: GroqTranscriptionResponse = serde_json::from_str(body)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::encode_wav;
    use crate::transcription::retry::{retry_transient, RateLimitExhausted, RATE_LIMIT_BUDGET};
    use crate::transcription::test_server::{serve_once, serve_responses};
    use crate::transcription::RetryableHttpError;
    use std::time::Duration;

    #[test]
    fn parse_verbose_json_response() {
        let body = r#"{
            "text": "Hello, how are you?",
            "words": [
                {"word": "Hello,", "start": 0.0, "end": 0.5},
                {"word": "how", "start": 0.6, "end": 0.8},
                {"word": "are", "start": 0.9, "end": 1.0},
                {"word": "you?", "start": 1.1, "end": 1.4}
            ]
        }"#;

        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "Hello, how are you?");
        assert_eq!(resp.words.len(), 4);
        assert_eq!(resp.words[0].word, "Hello,");
        assert!((resp.words[0].start - 0.0).abs() < f64::EPSILON);
        assert!((resp.words[0].end - 0.5).abs() < f64::EPSILON);
        assert_eq!(resp.words[3].word, "you?");
    }

    #[test]
    fn parse_response_without_words() {
        let body = r#"{"text": "Some text here"}"#;
        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "Some text here");
        assert!(resp.words.is_empty());
    }

    #[test]
    fn parse_empty_text_response() {
        let body = r#"{"text": "", "words": []}"#;
        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "");
        assert!(resp.words.is_empty());
    }

    #[tokio::test]
    async fn transcribe_rejects_audio_the_api_would_refuse() {
        let backend = GroqBackend::new("test-key".to_string());
        let (_, config) = recording_request();
        for (name, audio, want) in [
            ("empty", vec![], "empty audio"),
            ("over 25 MB", vec![0u8; MAX_FILE_SIZE + 1], "too large"),
        ] {
            let Err(err) = backend.transcribe(&audio, &config).await else {
                panic!("{name}: accepted");
            };
            assert!(err.to_string().contains(want), "{name}: {err}");
        }
    }

    #[test]
    fn groq_chunks_fit_the_upload_cap() {
        let backend = GroqBackend::new(String::new());
        let (_, config) = recording_request();
        let secs = backend.max_segment_chunk_secs(&config).unwrap() as usize;
        // 16 kHz mono i16 plus a 44-byte header.
        assert!(secs * 32_000 + 44 <= MAX_FILE_SIZE);
    }

    /// A backend that posts to the local server on `listener`.
    fn local_backend(listener: &tokio::net::TcpListener) -> GroqBackend {
        GroqBackend {
            url: format!("http://{}/transcriptions", listener.local_addr().unwrap()),
            ..GroqBackend::new("test-key".to_string())
        }
    }

    // FLAC needs the encoder, which comes with the recording feature.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn recording_chunks_upload_as_flac() {
        let (wav, config) = recording_request();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_once(
            listener,
            r#"{"text": "hi", "segments": [{"start": 0.0, "end": 1.0, "text": "hi"}]}"#,
        ));
        let t = backend.transcribe_segments(&wav, &config).await.unwrap();
        assert_eq!(t.segments.len(), 1);
        let request = server.await.unwrap();
        let (headers, data) = request.file_part();
        assert!(headers.contains("filename=\"audio.flac\""), "{headers}");
        assert!(headers.contains("Content-Type: audio/flac"), "{headers}");
        assert!(data.starts_with(b"fLaC"));
    }

    #[tokio::test]
    async fn dictation_uploads_as_wav() {
        // Language auto and a prompt, so the form's no-language and prompt
        // branches run too.
        let config = TranscriptionConfig {
            language: "auto".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            prompt: Some("whisrs".to_string()),
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let wav = encode_wav(&[300i16; 16_000]).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        // The real verbose_json answer carries fields the parse ignores.
        let server = tokio::spawn(serve_once(
            listener,
            r#"{"task": "transcribe", "language": "english", "duration": 3.5,
                "text": "hi", "words": [], "segments": [{"id": 0}]}"#,
        ));
        assert_eq!(backend.transcribe(&wav, &config).await.unwrap(), "hi");
        let request = server.await.unwrap();
        let (headers, data) = request.file_part();
        assert!(headers.contains("filename=\"audio.wav\""), "{headers}");
        assert!(headers.contains("Content-Type: audio/wav"), "{headers}");
        assert!(data.starts_with(&wav));
    }

    /// Short stand-ins for the server-error waits, so these tests talk to a
    /// real socket without sleeping 50 s.
    const QUICK: [Duration; 3] = [Duration::from_millis(1); 3];
    const CLOUDFLARE_524: &str = "error code: 524";
    const SEGMENTS: &str =
        r#"{"text": "hi", "segments": [{"start": 0.0, "end": 1.0, "text": "hi"}]}"#;

    fn recording_request() -> (Vec<u8>, TranscriptionConfig) {
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        (encode_wav(&[300i16; 16_000]).unwrap(), config)
    }

    /// A Cloudflare origin timeout (524) on a recording chunk is retried, and
    /// the chunk goes through on the next try.
    #[tokio::test]
    async fn a_524_is_retried_until_the_chunk_goes_through() {
        let (wav, config) = recording_request();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_responses(
            listener,
            vec![(524, CLOUDFLARE_524), (200, SEGMENTS)],
        ));
        let t = retry_transient(RATE_LIMIT_BUDGET, &QUICK, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap();
        assert_eq!(t.segments[0].text, "hi");
        assert_eq!(server.await.unwrap().len(), 2);
    }

    /// A 524 on every try ends after the bounded retries in the error the
    /// server gave, as a final error the recording reports.
    #[tokio::test]
    async fn a_lasting_524_gives_up_after_the_bounded_retries() {
        let (wav, config) = recording_request();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_responses(listener, vec![(524, CLOUDFLARE_524); 4]));
        let err = retry_transient(RATE_LIMIT_BUDGET, &QUICK, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        // Bounded, so a client that stops early fails here instead of hanging.
        let served = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the server waited for four requests")
            .unwrap();
        assert_eq!(served.len(), 4, "one try and three retries");
        let exhausted = err.downcast_ref::<RateLimitExhausted>().unwrap();
        assert_eq!(exhausted.status, 524);
        assert!(
            err.to_string()
                .ends_with("Groq API error (524): error code: 524"),
            "{err}"
        );
        assert!(err.downcast_ref::<RetryableHttpError>().is_none());
    }

    #[tokio::test]
    async fn a_400_is_not_retried() {
        let (wav, config) = recording_request();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_responses(
            listener,
            vec![(
                400,
                r#"{"error": {"message": "could not decode the audio", "type": "invalid_request_error"}}"#,
            )],
        ));
        let err = retry_transient(RATE_LIMIT_BUDGET, &QUICK, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap_err();
        assert_eq!(server.await.unwrap().len(), 1);
        assert_eq!(
            err.to_string(),
            "Groq API error (400): could not decode the audio"
        );
    }
}
