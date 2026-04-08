//! Transcription backends: trait definition and implementations.

pub mod dedup;
pub mod deepgram;
pub mod groq;
pub mod local_parakeet;
pub mod local_vosk;
#[cfg(feature = "local-whisper")]
pub mod local_whisper;
#[cfg(not(feature = "local-whisper"))]
pub mod local_whisper {
    //! Stub when local-whisper feature is disabled.
    pub struct LocalWhisperBackend;
    impl LocalWhisperBackend {
        pub fn new(_model_path: String) -> Self {
            Self
        }
    }
    #[async_trait::async_trait]
    impl super::TranscriptionBackend for LocalWhisperBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &super::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            anyhow::bail!("local-whisper feature not enabled — rebuild with: cargo build --features local-whisper")
        }
    }
}
pub mod openai_realtime;
pub mod openai_rest;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

/// Configuration for a transcription request.
#[derive(Debug, Clone)]
pub struct TranscriptionConfig {
    /// Language code (ISO 639-1), e.g. "en", or "auto" for auto-detection.
    pub language: String,
    /// Model identifier (backend-specific).
    pub model: String,
    /// Optional prompt hint for the transcription model (vocabulary, context).
    pub prompt: Option<String>,
}

/// Trait for transcription backends.
///
/// Each backend takes WAV-encoded audio bytes and returns the transcribed text.
/// Backends that support streaming override `transcribe_stream`.
#[async_trait]
pub trait TranscriptionBackend: Send + Sync {
    /// Transcribe a complete WAV-encoded audio buffer, returning the text.
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String>;

    /// Streaming transcription: receive audio chunks and send text incrementally.
    ///
    /// The default implementation collects all audio, encodes to WAV, and calls
    /// `transcribe()` — so non-streaming backends work without overriding this.
    async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        use crate::audio::capture::encode_wav;

        // Collect all audio chunks.
        let mut all_samples: Vec<i16> = Vec::new();
        while let Some(chunk) = audio_rx.recv().await {
            all_samples.extend_from_slice(&chunk);
        }

        if all_samples.is_empty() {
            return Ok(());
        }

        // Encode to WAV and use the non-streaming method.
        let wav_data = encode_wav(&all_samples)?;
        let text = self.transcribe(&wav_data, config).await?;

        if !text.is_empty() {
            text_tx.send(text).await.ok();
        }

        Ok(())
    }

    /// Whether this backend supports true/chunked streaming.
    ///
    /// When true, the daemon will use `transcribe_stream` during recording
    /// rather than waiting for recording to finish.
    fn supports_streaming(&self) -> bool {
        false
    }
}
