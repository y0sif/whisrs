//! Vosk transcription backend for true streaming local speech recognition.
//!
//! Feature-gated behind `local-vosk`. Currently a stub — implementation planned.
//!
//! Vosk offers true streaming recognition with very small models (~40 MB),
//! making it ideal for low-end hardware. Trade-off: lower accuracy than whisper.

use async_trait::async_trait;

use super::{TranscriptionBackend, TranscriptionConfig};

/// Vosk-based local transcription backend.
pub struct VoskBackend {
    #[allow(dead_code)]
    model_path: String,
}

impl VoskBackend {
    pub fn new(model_path: String) -> Self {
        Self { model_path }
    }
}

#[async_trait]
impl TranscriptionBackend for VoskBackend {
    async fn transcribe(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        anyhow::bail!(
            "Vosk backend is not yet implemented. \
             Use the `local-whisper` backend instead, or check back in a future release."
        )
    }

    // Unreachable: `transcribe` bails before a request exists. `false` is
    // what the rule on the trait method yields — read the answer off the
    // request-building code, and there is none yet — and it is the safe
    // direction, since a stale `true` is the exact shape of #133. Whoever
    // implements Vosk answers it again off the real request.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vosk_stub_returns_error() {
        let backend = VoskBackend::new("/nonexistent".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "small-en-us".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
