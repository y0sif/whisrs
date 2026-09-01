//! Parakeet (NVIDIA) transcription backend for streaming local speech recognition.
//!
//! Feature-gated behind `local-parakeet`. Currently a stub — implementation planned.
//!
//! Parakeet offers 160ms chunk processing with end-of-utterance detection,
//! making it the lowest-latency local option. Native Rust crate, no C dependency.

use async_trait::async_trait;

use super::{TranscriptionBackend, TranscriptionConfig};

/// Parakeet-based local transcription backend.
pub struct ParakeetBackend {
    #[allow(dead_code)]
    model_path: String,
}

impl ParakeetBackend {
    pub fn new(model_path: String) -> Self {
        Self { model_path }
    }
}

#[async_trait]
impl TranscriptionBackend for ParakeetBackend {
    async fn transcribe(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        anyhow::bail!(
            "Parakeet backend is not yet implemented. \
             Use the `local-whisper` backend instead, or check back in a future release."
        )
    }

    // Unreachable: `transcribe` bails before a request exists. `false` is
    // what the rule on the trait method yields — read the answer off the
    // request-building code, and there is none yet — and it is the safe
    // direction, since a stale `true` is the exact shape of #133. Whoever
    // implements Parakeet answers it again off the real request.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parakeet_stub_returns_error() {
        let backend = ParakeetBackend::new("/nonexistent".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "eou-120m".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
