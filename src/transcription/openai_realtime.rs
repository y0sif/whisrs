//! OpenAI Realtime API transcription backend (true streaming via WebSocket).

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

use super::openai_realtime_protocol::{
    openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, OpenAiRealtimeProtocolEngine,
    RealtimeEngineConfig,
};
use super::{TranscriptionBackend, TranscriptionConfig};

/// OpenAI Realtime API transcription backend.
pub struct OpenAIRealtimeBackend {
    api_key: String,
}

impl OpenAIRealtimeBackend {
    /// Create a new OpenAI Realtime backend.
    pub fn new(api_key: String) -> Self {
        Self { api_key }
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

    fn engine_for_request(
        &self,
        request: &TranscriptionConfig,
    ) -> anyhow::Result<OpenAiRealtimeProtocolEngine> {
        Ok(OpenAiRealtimeProtocolEngine::new(RealtimeEngineConfig {
            url: "wss://api.openai.com/v1/realtime?intent=transcription".to_string(),
            endpoint_display: "wss://api.openai.com/v1/realtime".to_string(),
            auth_bearer: Some(self.resolve_api_key()?),
            host_header: Some("api.openai.com".to_string()),
            profile: OpenAiRealtimeProfile::OpenAi,
            turn_detection: openai_turn_detection_mode_for_model(&request.model),
            final_completion_timeout: None,
        }))
    }
}

#[async_trait]
impl TranscriptionBackend for OpenAIRealtimeBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        self.engine_for_request(config)?
            .transcribe(audio, config)
            .await
    }

    async fn transcribe_stream(
        &self,
        audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        self.engine_for_request(config)?
            .transcribe_stream(audio_rx, text_tx, config)
            .await
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}
