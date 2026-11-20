//! Parakeet (NVIDIA) transcription backend for streaming local speech recognition.
//!
//! A stub in every build: `transcribe` bails with "not yet implemented".
//!
//! Parakeet offers 160ms chunk processing with end-of-utterance detection,
//! making it the lowest-latency local option. Native Rust crate, no C dependency.

use async_trait::async_trait;

use super::{StreamText, TranscriptionBackend, TranscriptionConfig};

/// Parakeet-based local transcription backend.
pub struct ParakeetBackend;

impl ParakeetBackend {
    pub fn new(_model_path: String) -> Self {
        Self
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
    // what the rule on the trait method yields (read the answer off the
    // request-building code, and there is none yet), and it is the safe
    // direction, since a stale `true` lets the prompt-echo filter discard
    // real speech. Whoever implements Parakeet answers it again off the real
    // request.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }

    fn stream_text(&self) -> StreamText {
        StreamText::Phrases
    }

    // Unreachable for the same reason as `transcribe`: there is no backend yet.
    async fn transcribe_segments(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<super::Transcript> {
        anyhow::bail!("Parakeet backend is not yet implemented")
    }

    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        None
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
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
