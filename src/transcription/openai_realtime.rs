//! OpenAI Realtime API transcription backend (true streaming via WebSocket).

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

use super::openai_realtime_protocol::{
    openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, OpenAiRealtimeProtocolEngine,
    RealtimeEngineConfig, TurnDetectionMode,
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

    // Per request, not per backend: the model picks the turn-detection mode,
    // and the mode decides whether a prompt goes on the wire at all. Derived
    // from the same `openai_turn_detection_mode_for_model` call
    // `engine_for_request` hands the engine, so this cannot drift from what is
    // actually sent — `OpenAiSessionUpdate::new` sets `prompt = None` on the
    // `ManualCommit` arm and `clamp_prompt`s it on the `ServerVad` one.
    //
    // Manual commit is not an exotic corner: `whisrs setup` writes `[openai]
    // model = "gpt-realtime-whisper"` when this backend is chosen and
    // `get_model_for_backend` falls back to the same string, so the default
    // openai-realtime install is the promptless case. Answering `true` there
    // would reproduce #133 one backend over — real speech shaped like the
    // user's own `[general] vocabulary` discarded as an echo of a prompt
    // OpenAI never saw.
    fn sends_prompt(&self, config: &TranscriptionConfig) -> bool {
        !matches!(
            openai_turn_detection_mode_for_model(&config.model),
            TurnDetectionMode::ManualCommit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_for_model(model: &str) -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: model.to_string(),
            prompt: Some("Hyprland, whisrs".to_string()),
            keyterms: Vec::new(),
        }
    }

    /// The answer has to follow the model, not the backend struct (#133). One
    /// backend, two models, two answers — and the `false` case is the one
    /// `whisrs setup` and `get_model_for_backend` both default to.
    #[test]
    fn sends_prompt_follows_the_model() {
        let backend = OpenAIRealtimeBackend::new(String::new());

        assert!(
            !backend.sends_prompt(&request_for_model("gpt-realtime-whisper")),
            "manual-commit models get `prompt = None` in the session.update"
        );
        assert!(
            backend.sends_prompt(&request_for_model("gpt-4o-transcribe")),
            "server-VAD models carry the clamped prompt in the session.update"
        );
    }
}
