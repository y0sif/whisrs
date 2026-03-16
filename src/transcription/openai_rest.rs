//! OpenAI REST API transcription backend (non-streaming).
//!
//! Sends WAV audio to OpenAI's `/v1/audio/transcriptions` endpoint via
//! multipart HTTP POST. Same pattern as the Groq backend.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use super::{TranscriptionBackend, TranscriptionConfig};

/// Maximum file size accepted by the OpenAI API (25 MB).
const MAX_FILE_SIZE: usize = 25 * 1024 * 1024;

/// OpenAI API endpoint for audio transcription.
const OPENAI_TRANSCRIPTION_URL: &str = "https://api.openai.com/v1/audio/transcriptions";

/// OpenAI REST transcription backend.
pub struct OpenAIRestBackend {
    client: reqwest::Client,
    api_key: String,
}

impl OpenAIRestBackend {
    /// Create a new OpenAI REST backend with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
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

        let api_key = self.resolve_api_key()?;

        debug!(
            "sending {} bytes to OpenAI API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        // whisper-1 supports verbose_json; gpt-4o-transcribe models only support json/text.
        let response_format = if config.model.starts_with("whisper") {
            "verbose_json"
        } else {
            "json"
        };

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone())
            .text("response_format", response_format.to_string());

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }

        let response = self
            .client
            .post(OPENAI_TRANSCRIPTION_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .multipart(form)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<OpenAIErrorResponse>(&body) {
                match status.as_u16() {
                    401 => {
                        anyhow::bail!("OpenAI API: invalid API key — {}", err_resp.error.message)
                    }
                    429 => {
                        anyhow::bail!("OpenAI API: rate limited — {}", err_resp.error.message)
                    }
                    _ => anyhow::bail!(
                        "OpenAI API error ({}): {}",
                        status.as_u16(),
                        err_resp.error.message
                    ),
                }
            }
            anyhow::bail!("OpenAI API error ({}): {}", status.as_u16(), body);
        }

        let parsed: OpenAITranscriptionResponse = serde_json::from_str(&body)?;
        let text = parsed.text.trim().to_string();

        if text.is_empty() {
            warn!("OpenAI returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe) since this
    // backend does not support streaming.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = OpenAIRestBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "gpt-4o-mini-transcribe".to_string(),
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
}
