//! Groq Whisper API transcription backend.
//!
//! Sends WAV audio to Groq's `/openai/v1/audio/transcriptions` endpoint
//! and returns the transcribed text. Supports chunked pseudo-streaming
//! by splitting audio at silence boundaries and sending each chunk
//! independently.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio::capture::encode_wav;
use crate::audio::silence::is_silent;
use crate::audio::AudioChunk;

use super::dedup::DeduplicationTracker;
use super::{TranscriptionBackend, TranscriptionConfig};

/// Maximum file size accepted by the Groq API (25 MB).
const MAX_FILE_SIZE: usize = 25 * 1024 * 1024;

/// Groq API endpoint for audio transcription.
const GROQ_TRANSCRIPTION_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// Minimum chunk duration in samples (10 seconds at 16kHz) — Groq bills
/// at least 10 seconds per request.
const MIN_CHUNK_SAMPLES: usize = 16_000 * 10;

/// Sample rate used for audio capture.
const SAMPLE_RATE: u32 = 16_000;

/// Silence threshold for VAD-based chunk splitting (normalized RMS).
const SILENCE_THRESHOLD: f64 = 0.005;

/// Minimum silence duration (in samples) to trigger a chunk split.
/// 300ms at 16kHz.
const MIN_SILENCE_SAMPLES: usize = 16_000 * 300 / 1000;

/// Groq transcription backend.
pub struct GroqBackend {
    client: reqwest::Client,
    api_key: String,
}

impl GroqBackend {
    /// Create a new Groq backend with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }

    /// Send a WAV-encoded audio chunk to the Groq API and return the response.
    async fn send_chunk(
        &self,
        wav_data: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<GroqTranscriptionResponse> {
        let file_part = multipart::Part::bytes(wav_data.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone())
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word");

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }

        let response = self
            .client
            .post(GROQ_TRANSCRIPTION_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<GroqErrorResponse>(&body) {
                match status.as_u16() {
                    401 => anyhow::bail!("Groq API: invalid API key — {}", err_resp.error.message),
                    429 => anyhow::bail!("Groq API: rate limited — {}", err_resp.error.message),
                    _ => anyhow::bail!(
                        "Groq API error ({}): {}",
                        status.as_u16(),
                        err_resp.error.message
                    ),
                }
            }
            anyhow::bail!("Groq API error ({}): {}", status.as_u16(), body);
        }

        let parsed: GroqTranscriptionResponse = serde_json::from_str(&body)?;
        Ok(parsed)
    }
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

        debug!(
            "sending {} bytes to Groq API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone())
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word");

        // Only set language if not "auto" — letting the API auto-detect is the default.
        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }

        let response = self
            .client
            .post(GROQ_TRANSCRIPTION_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            // Try to parse error response for a better message.
            if let Ok(err_resp) = serde_json::from_str::<GroqErrorResponse>(&body) {
                match status.as_u16() {
                    401 => anyhow::bail!("Groq API: invalid API key — {}", err_resp.error.message),
                    429 => anyhow::bail!("Groq API: rate limited — {}", err_resp.error.message),
                    _ => anyhow::bail!(
                        "Groq API error ({}): {}",
                        status.as_u16(),
                        err_resp.error.message
                    ),
                }
            }
            anyhow::bail!("Groq API error ({}): {}", status.as_u16(), body);
        }

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

    async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let mut dedup = DeduplicationTracker::new();
        let mut buffer: Vec<i16> = Vec::new();
        let mut silence_count: usize = 0;

        while let Some(chunk) = audio_rx.recv().await {
            // Check if this chunk is silent.
            if is_silent(&chunk, SILENCE_THRESHOLD) {
                silence_count += chunk.len();
            } else {
                silence_count = 0;
            }

            buffer.extend_from_slice(&chunk);

            // Check if we should send this chunk:
            // - We have at least MIN_CHUNK_SAMPLES and hit a silence boundary.
            let has_enough = buffer.len() >= MIN_CHUNK_SAMPLES;
            let at_silence = silence_count >= MIN_SILENCE_SAMPLES;

            if has_enough && at_silence {
                let chunk_duration = buffer.len() as f64 / SAMPLE_RATE as f64;
                info!(
                    "groq stream: sending chunk of {:.1}s ({} samples)",
                    chunk_duration,
                    buffer.len()
                );

                let wav_data = encode_wav(&buffer)?;
                if wav_data.len() <= MAX_FILE_SIZE {
                    match self.send_chunk(&wav_data, config).await {
                        Ok(resp) => {
                            let text = if !resp.words.is_empty() {
                                let accepted = dedup.filter_words(&resp.words);
                                accepted
                                    .iter()
                                    .map(|w| w.word.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            } else {
                                dedup.filter_text(&resp.text)
                            };

                            if !text.is_empty() {
                                text_tx.send(text).await.ok();
                            }
                        }
                        Err(e) => {
                            warn!("groq stream: chunk transcription failed: {e}");
                        }
                    }

                    dedup.advance_offset(chunk_duration);
                } else {
                    warn!(
                        "groq stream: chunk too large ({} bytes), skipping",
                        wav_data.len()
                    );
                }

                buffer.clear();
                silence_count = 0;
            }
        }

        // Send any remaining audio.
        if !buffer.is_empty() {
            let chunk_duration = buffer.len() as f64 / SAMPLE_RATE as f64;
            info!(
                "groq stream: sending final chunk of {:.1}s ({} samples)",
                chunk_duration,
                buffer.len()
            );

            let wav_data = encode_wav(&buffer)?;
            if wav_data.len() <= MAX_FILE_SIZE {
                match self.send_chunk(&wav_data, config).await {
                    Ok(resp) => {
                        let text = if !resp.words.is_empty() {
                            let accepted = dedup.filter_words(&resp.words);
                            accepted
                                .iter()
                                .map(|w| w.word.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        } else {
                            dedup.filter_text(&resp.text)
                        };

                        if !text.is_empty() {
                            text_tx.send(text).await.ok();
                        }
                    }
                    Err(e) => {
                        warn!("groq stream: final chunk transcription failed: {e}");
                    }
                }
            }
        }

        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        false
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

    #[test]
    fn parse_error_response() {
        let body = r#"{"error": {"message": "Invalid API key", "type": "invalid_request_error"}}"#;
        let err: GroqErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(err.error.message, "Invalid API key");
        assert_eq!(
            err.error.error_type.as_deref(),
            Some("invalid_request_error")
        );
    }

    #[test]
    fn parse_response_with_extra_fields() {
        // The API may return extra fields we don't model — ensure we don't fail.
        let body = r#"{
            "task": "transcribe",
            "language": "english",
            "duration": 3.5,
            "text": "Test transcription",
            "words": [],
            "segments": [{"id": 0}]
        }"#;
        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "Test transcription");
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = GroqBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_oversized_audio() {
        let backend = GroqBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
        };
        let huge = vec![0u8; MAX_FILE_SIZE + 1];
        let err = backend.transcribe(&huge, &config).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }
}
