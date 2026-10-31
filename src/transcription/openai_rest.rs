//! OpenAI REST API transcription backend (non-streaming).
//!
//! Sends WAV audio (FLAC for whisper-1 recording chunks) to OpenAI's
//! `/v1/audio/transcriptions` endpoint via multipart HTTP POST. Same pattern
//! as the Groq backend.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use super::flac::{recording_upload, UploadFormat};
use super::verbose_json::parse_verbose_segments;
use super::{
    segments_by_phrase, StreamText, Transcript, TranscriptionBackend, TranscriptionConfig,
    UPLOAD_CHUNK_SECS,
};

/// Maximum file size accepted by the OpenAI API (25 MB).
const MAX_FILE_SIZE: usize = 25 * 1024 * 1024;

/// OpenAI API endpoint for audio transcription.
const OPENAI_TRANSCRIPTION_URL: &str = "https://api.openai.com/v1/audio/transcriptions";

/// OpenAI REST transcription backend.
pub struct OpenAIRestBackend {
    client: reqwest::Client,
    api_key: String,
    /// [`OPENAI_TRANSCRIPTION_URL`]; tests point it at a local server.
    url: String,
}

impl OpenAIRestBackend {
    /// Create a new OpenAI REST backend with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            url: OPENAI_TRANSCRIPTION_URL.to_string(),
        }
    }

    /// Resolve the API key from the struct field or environment variable.
    fn resolve_api_key(&self) -> anyhow::Result<String> {
        if !self.api_key.is_empty() {
            return Ok(self.api_key.clone());
        }
        std::env::var("WHISRS_OPENAI_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "no OpenAI API key configured — set WHISRS_OPENAI_API_KEY or add [openai] to config.toml"
            )
        })
    }
}

impl OpenAIRestBackend {
    /// POST one transcription request with `audio` in `format` and return
    /// the raw success body.
    async fn post(
        &self,
        audio: &[u8],
        format: UploadFormat,
        config: &TranscriptionConfig,
        response_format: &str,
        granularities: &[&str],
    ) -> anyhow::Result<String> {
        let api_key = self.resolve_api_key()?;

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name(format.file_name)
            .mime_str(format.mime)?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone())
            .text("response_format", response_format.to_string());
        for granularity in granularities {
            form = form.text("timestamp_granularities[]", granularity.to_string());
        }

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }
        if let Some(prompt) = &config.prompt {
            form = form.text("prompt", prompt.clone());
        }

        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {api_key}"))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();
        let retry_after = super::retry_after_header(response.headers());
        let body = response.text().await?;

        if !status.is_success() {
            let code = status.as_u16();
            let message = match serde_json::from_str::<OpenAIErrorResponse>(&body) {
                Ok(err_resp) => match code {
                    401 => format!("OpenAI API: invalid API key: {}", err_resp.error.message),
                    429 => format!("OpenAI API: rate limited: {}", err_resp.error.message),
                    _ => format!("OpenAI API error ({code}): {}", err_resp.error.message),
                },
                Err(_) => format!("OpenAI API error ({code}): {body}"),
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

/// Whether `model` returns segment timestamps.
///
/// Only the Whisper models accept `verbose_json` with
/// `timestamp_granularities[]`. The gpt-4o family (`gpt-4o-transcribe`,
/// `gpt-4o-mini-transcribe`, `gpt-transcribe`, and the `-diarize` variant
/// until #175 wires its diarized format) answer plain `json`, so recordings on
/// those go through the phrase fallback.
pub fn model_has_timestamps(model: &str) -> bool {
    model.starts_with("whisper")
}

/// Response from the OpenAI transcription API (verbose_json format).
#[derive(Debug, Deserialize)]
struct OpenAITranscriptionResponse {
    text: String,
}

/// Error response from the OpenAI API.
#[derive(Debug, Deserialize)]
struct OpenAIErrorResponse {
    error: OpenAIErrorDetail,
}

#[derive(Debug, Deserialize)]
struct OpenAIErrorDetail {
    message: String,
}

#[async_trait]
impl TranscriptionBackend for OpenAIRestBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        check_audio_size(audio)?;

        debug!(
            "sending {} bytes to OpenAI API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        // whisper-1 supports verbose_json; gpt-4o-transcribe models only support json/text.
        let response_format = if config.model.starts_with("whisper") {
            "verbose_json"
        } else {
            "json"
        };
        let body = self
            .post(audio, UploadFormat::WAV, config, response_format, &[])
            .await?;

        let parsed: OpenAITranscriptionResponse = serde_json::from_str(&body)?;
        let text = parsed.text.trim().to_string();

        if text.is_empty() {
            warn!("OpenAI returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe) since this
    // backend does not support streaming.

    // `transcribe` puts the prompt on the wire as the `prompt` multipart field.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
    }

    fn stream_text(&self) -> StreamText {
        StreamText::Phrases
    }

    // whisper-1: verbose_json segments, the chunk sent as FLAC. Everything
    // else: one WAV request per phrase via the shared fallback, with the same
    // prompt on each.
    async fn transcribe_segments(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<Transcript> {
        if !model_has_timestamps(&config.model) {
            check_audio_size(audio)?;
            return segments_by_phrase(self, audio, config).await;
        }
        let upload = recording_upload(audio).await;
        check_audio_size(&upload.bytes)?;
        debug!(
            "sending {} bytes ({}, WAV was {}) to OpenAI API for segments (model={})",
            upload.bytes.len(),
            upload.format.mime,
            audio.len(),
            config.model
        );
        let body = self
            .post(
                &upload.bytes,
                upload.format,
                config,
                "verbose_json",
                &["segment", "word"],
            )
            .await?;
        parse_verbose_segments(&body)
    }

    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        Some(UPLOAD_CHUNK_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::retry::{retry_transient, RATE_LIMIT_BUDGET};
    use crate::transcription::test_server::{serve_once, serve_responses};

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = OpenAIRestBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "gpt-4o-mini-transcribe".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_oversized_audio() {
        let backend = OpenAIRestBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "gpt-4o-mini-transcribe".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let huge = vec![0u8; MAX_FILE_SIZE + 1];
        let err = backend.transcribe(&huge, &config).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn parse_openai_response() {
        let body = r#"{"text": "Hello world"}"#;
        let parsed: OpenAITranscriptionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.text, "Hello world");
    }

    #[test]
    fn parse_openai_error() {
        let body = r#"{"error": {"message": "Invalid API key"}}"#;
        let parsed: OpenAIErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.error.message, "Invalid API key");
    }

    #[test]
    fn only_whisper_models_have_timestamps() {
        assert!(model_has_timestamps("whisper-1"));
        for model in [
            "gpt-4o-transcribe",
            "gpt-4o-mini-transcribe",
            "gpt-transcribe",
            "gpt-4o-transcribe-diarize",
        ] {
            assert!(!model_has_timestamps(model), "{model}");
        }
    }

    #[tokio::test]
    async fn segments_reject_empty_audio_before_any_request() {
        let backend = OpenAIRestBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "whisper-1".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let err = backend.transcribe_segments(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    /// A backend that posts to the local server on `listener`.
    fn local_backend(listener: &tokio::net::TcpListener) -> OpenAIRestBackend {
        OpenAIRestBackend {
            url: format!("http://{}/transcriptions", listener.local_addr().unwrap()),
            ..OpenAIRestBackend::new("test-key".to_string())
        }
    }

    fn config_for(model: &str) -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: model.to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        }
    }

    // FLAC needs the encoder, which comes with the recording feature.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn whisper_recording_chunks_upload_as_flac() {
        let wav = crate::audio::wav::encode_wav(&[300i16; 16_000]).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_once(
            listener,
            r#"{"text": "hi", "segments": [{"start": 0.0, "end": 1.0, "text": "hi"}]}"#,
        ));
        let t = backend
            .transcribe_segments(&wav, &config_for("whisper-1"))
            .await
            .unwrap();
        assert_eq!(t.segments.len(), 1);
        let request = server.await.unwrap();
        let (headers, data) = request.file_part();
        assert!(headers.contains("filename=\"audio.flac\""), "{headers}");
        assert!(headers.contains("Content-Type: audio/flac"), "{headers}");
        assert!(data.starts_with(b"fLaC"));
    }

    #[tokio::test]
    async fn whisper_dictation_uploads_as_wav() {
        let wav = crate::audio::wav::encode_wav(&[300i16; 16_000]).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_once(listener, r#"{"text": "hi"}"#));
        let text = backend
            .transcribe(&wav, &config_for("whisper-1"))
            .await
            .unwrap();
        assert_eq!(text, "hi");
        let request = server.await.unwrap();
        let (headers, data) = request.file_part();
        assert!(headers.contains("filename=\"audio.wav\""), "{headers}");
        assert!(headers.contains("Content-Type: audio/wav"), "{headers}");
        assert!(data.starts_with(&wav));
    }

    #[tokio::test]
    async fn phrase_fallback_keeps_wav() {
        // One phrase between silences: exactly one request.
        let mut samples = vec![0i16; 16_000];
        samples.extend((0..16_000).map(|i| ((i as f64 * 0.17).sin() * 6000.0) as i16));
        samples.extend(vec![0i16; 16_000]);
        let wav = crate::audio::wav::encode_wav(&samples).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_once(listener, r#"{"text": "hi"}"#));
        let t = backend
            .transcribe_segments(&wav, &config_for("gpt-4o-mini-transcribe"))
            .await
            .unwrap();
        assert_eq!(t.segments.len(), 1);
        let request = server.await.unwrap();
        let (headers, data) = request.file_part();
        assert!(headers.contains("filename=\"audio.wav\""), "{headers}");
        assert!(data.starts_with(b"RIFF"));
    }
    /// A Cloudflare origin timeout (524) on a recording chunk is retried on the
    /// server-error schedule, here with short stand-ins for its waits.
    #[tokio::test]
    async fn a_524_is_retried_until_the_chunk_goes_through() {
        let config = config_for("whisper-1");
        let wav = crate::audio::wav::encode_wav(&[300i16; 16_000]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = local_backend(&listener);
        let server = tokio::spawn(serve_responses(
            listener,
            vec![
                (524, "error code: 524"),
                (
                    200,
                    r#"{"text": "hi", "segments": [{"start": 0.0, "end": 1.0, "text": "hi"}]}"#,
                ),
            ],
        ));
        let quick = [std::time::Duration::from_millis(1); 3];
        let t = retry_transient(RATE_LIMIT_BUDGET, &quick, || {
            backend.transcribe_segments(&wav, &config)
        })
        .await
        .unwrap();
        assert_eq!(t.segments[0].text, "hi");
        assert_eq!(server.await.unwrap().len(), 2);
    }
}
