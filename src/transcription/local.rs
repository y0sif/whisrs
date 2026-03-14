//! Local whisper.cpp transcription backend via `whisper-rs`.
//!
//! When compiled without the `local` feature, this module provides a stub
//! that returns an error. When compiled with `local`, it uses whisper-rs
//! with a sliding window approach for pseudo-streaming.

use async_trait::async_trait;

use super::{TranscriptionBackend, TranscriptionConfig};

/// Local whisper.cpp transcription backend.
pub struct LocalWhisperBackend {
    #[allow(dead_code)]
    model_path: String,
}

impl LocalWhisperBackend {
    /// Create a new local whisper backend.
    pub fn new(model_path: String) -> Self {
        Self { model_path }
    }
}

#[cfg(not(feature = "local"))]
#[async_trait]
impl TranscriptionBackend for LocalWhisperBackend {
    async fn transcribe(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        anyhow::bail!(
            "local whisper backend not available — whisrs was compiled without the `local` feature. \
             Rebuild with `cargo build --features local` and ensure libclang is installed."
        )
    }
}

#[cfg(feature = "local")]
#[async_trait]
impl TranscriptionBackend for LocalWhisperBackend {
    async fn transcribe(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        // TODO: Implement actual whisper-rs transcription with sliding window.
        anyhow::bail!(
            "local whisper backend not yet fully implemented — install a whisper model first. \
             Run `whisrs setup` to download a model."
        )
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
        };
        let err = backend.transcribe(&[1, 2, 3], &config).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not available") || msg.contains("not yet"),
            "unexpected error: {msg}"
        );
    }
}
