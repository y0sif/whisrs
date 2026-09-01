//! Transcription backends: trait definition and implementations.

pub mod asr_sidecar;
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
        pub fn with_segmentation(self, _mode: &str, _phrase_silence_ms: u64) -> Self {
            self
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

        // Unreachable: `transcribe` bails before a request exists. `false`
        // is what the rule above the trait method yields — read the answer off
        // the request-building code, and this build has none — and it is the
        // safe direction, since a stale `true` is the exact shape of #133.
        // The real backend answers `true` off its own `set_initial_prompt`
        // call; the divergence is deliberate and unobservable.
        fn sends_prompt(&self, _config: &super::TranscriptionConfig) -> bool {
            false
        }
    }
}
pub mod openai_compatible_realtime;
pub mod openai_realtime;
pub mod openai_realtime_protocol;
pub mod openai_rest;
pub mod phrase_split;

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
    /// Backend-specific keyword-biasing terms — Deepgram keyterm prompting.
    ///
    /// Carried alongside `prompt` rather than on the backend struct so both
    /// forms of "bias the model toward these words" travel the same channel:
    /// a path that deliberately sends no prompt (command mode, where the
    /// recording is a spoken instruction rather than content) sends no
    /// keyterms either. Backends with no equivalent parameter ignore these —
    /// they receive the same terms folded into `prompt`.
    pub keyterms: Vec<String>,
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
        use crate::audio::wav::encode_wav;

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

    /// Whether this backend actually transmits [`TranscriptionConfig::prompt`]
    /// to the provider for *this* request.
    ///
    /// Gates the prompt-echo filter in the batch pipeline (issue #133).
    /// `config.prompt` is built for *every* backend from `[general] prompt` +
    /// `[general] vocabulary`, so a backend that silently drops it would
    /// otherwise have genuine speech that happens to resemble the user's own
    /// vocabulary discarded as an "echo" of a prompt that never went on the
    /// wire.
    ///
    /// Required rather than defaulted on purpose: a default of `true` is
    /// exactly what let `openai-compatible-realtime` inherit the wrong answer
    /// in the first cut of the #133 fix. Answer it from the request-building
    /// code, not from what the backend "should" do.
    ///
    /// Takes the request because for the realtime profiles the answer is not a
    /// property of the backend at all — the model is. `openai-realtime` derives
    /// its turn-detection mode from `config.model`, and the manual-commit arm
    /// of `OpenAiSessionUpdate::new` drops the prompt, so one backend struct
    /// sends it for one model and not for another. Backends whose answer is
    /// fixed ignore the argument.
    fn sends_prompt(&self, config: &TranscriptionConfig) -> bool;
}
