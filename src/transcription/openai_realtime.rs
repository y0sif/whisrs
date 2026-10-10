//! OpenAI Realtime API transcription backend (true streaming via WebSocket).

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

use super::openai_realtime_protocol::openai_model_supports_prompt;
use super::openai_realtime_protocol::{
    openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, OpenAiRealtimeProtocolEngine,
    RealtimeEngineConfig,
};
use super::{TranscriptionBackend, TranscriptionConfig};

/// OpenAI Realtime API transcription backend.
pub struct OpenAIRealtimeBackend {
    api_key: String,
    /// `[openai] languages`, trimmed, with blank codes dropped.
    languages: Vec<String>,
    /// `[general] language` as the daemon was started with it. See
    /// [`OpenAIRealtimeBackend::languages_for_request`].
    default_language: String,
}

impl OpenAIRealtimeBackend {
    /// Create a new OpenAI Realtime backend.
    pub fn new(api_key: String) -> Self {
        Self::with_languages(api_key, Vec::new(), String::new())
    }

    /// Create a backend that sends `[openai] languages` as expected-language
    /// hints on the models that take them.
    ///
    /// `default_language` must be the `[general] language` the daemon resolves
    /// each session against, so a per-session `-l` can be told apart from it.
    /// Codes are trimmed and blanks dropped here, so a stray `""` in the
    /// config never reaches the wire (`Config::validate` warns about it).
    pub fn with_languages(
        api_key: String,
        languages: Vec<String>,
        default_language: String,
    ) -> Self {
        let languages = languages
            .iter()
            .map(|code| code.trim())
            .filter(|code| !code.is_empty())
            .map(str::to_string)
            .collect();
        Self {
            api_key,
            languages,
            default_language,
        }
    }

    /// The `[openai] languages` list this request carries to the wire.
    ///
    /// Precedence is `-l` > `[openai] languages` > `[general] language` >
    /// nothing for "auto", and the request only carries the resolved language
    /// (`resolve_language` in the daemon). So a request whose language differs
    /// from `[general] language` had a per-session override, and gets no list:
    /// `OpenAiSessionUpdate::new` then sends `[request.language]`, or no hint
    /// for "auto". A `-l` equal to `[general] language` is indistinguishable
    /// from no override and keeps the list. Command mode always transcribes on
    /// `[general] language`, so it keeps the list too.
    ///
    /// The comparison cannot go stale: the daemon builds this backend once
    /// from the same `Config` it resolves sessions against, and a config
    /// change takes a daemon restart.
    ///
    /// Test seam as well: the factory that joins the list and the default
    /// language lives in the `whisrsd` binary crate, so `pub(crate)` here is not
    /// visible to the test that proves that wiring. Hidden from the docs to
    /// keep it off the supported surface.
    #[doc(hidden)]
    pub fn languages_for_request(&self, request: &TranscriptionConfig) -> &[String] {
        if request.language == self.default_language {
            &self.languages
        } else {
            &[]
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
            languages: self.languages_for_request(request).to_vec(),
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

    // gpt-realtime-whisper is promptless even though the newer manual-commit
    // models accept prompt. Keep this gate aligned with the wire serializer.
    fn sends_prompt(&self, config: &TranscriptionConfig) -> bool {
        openai_model_supports_prompt(&config.model)
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
    /// backend, three models, two answers, and the `false` case is the one
    /// `whisrs setup` and `get_model_for_backend` both default to.
    #[test]
    fn sends_prompt_follows_the_model() {
        let backend = OpenAIRealtimeBackend::new(String::new());

        assert!(
            !backend.sends_prompt(&request_for_model("gpt-realtime-whisper")),
            "gpt-realtime-whisper gets `prompt = None` in the session.update"
        );
        assert!(
            backend.sends_prompt(&request_for_model("gpt-4o-transcribe")),
            "server-VAD models carry the clamped prompt in the session.update"
        );
        assert!(
            backend.sends_prompt(&request_for_model("gpt-live-transcribe")),
            "gpt-live-transcribe accepts a prompt despite manual commit"
        );
    }

    /// The `languages` field of the session.update this backend sends for a
    /// gpt-live-transcribe session resolved to `language`, built the way
    /// `engine_for_request` and the engine build it.
    fn wire_languages(backend: &OpenAIRealtimeBackend, language: &str) -> serde_json::Value {
        let mut request = request_for_model("gpt-live-transcribe");
        request.language = language.to_string();
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                &request.model,
                &request.language,
                backend.languages_for_request(&request),
                request.prompt.as_deref(),
                openai_turn_detection_mode_for_model(&request.model),
            )
            .unwrap();
        json["session"]["audio"]["input"]["transcription"]["languages"].clone()
    }

    /// `-l` > `[openai] languages` > `[general] language` > nothing for "auto".
    #[test]
    fn language_hint_precedence() {
        let ru_en = vec!["ru".to_string(), "en".to_string()];

        // No override: the request carries `[general] language`, so the
        // configured list wins over it.
        let listed =
            OpenAIRealtimeBackend::with_languages(String::new(), ru_en.clone(), "ru".into());
        assert_eq!(
            wire_languages(&listed, "ru"),
            serde_json::json!(["ru", "en"])
        );

        // A per-session `-l pl` wins over the list.
        assert_eq!(wire_languages(&listed, "pl"), serde_json::json!(["pl"]));

        // `-l auto` over a list on a fixed `[general] language`: no hint.
        assert!(wire_languages(&listed, "auto").is_null());

        // No list: `[general] language` goes out as a one-item list.
        let unlisted =
            OpenAIRealtimeBackend::with_languages(String::new(), Vec::new(), "ru".into());
        assert_eq!(wire_languages(&unlisted, "ru"), serde_json::json!(["ru"]));

        // No list and "auto": no hint at all.
        let auto = OpenAIRealtimeBackend::with_languages(String::new(), Vec::new(), "auto".into());
        assert!(wire_languages(&auto, "auto").is_null());

        // The list still applies on "auto", which is the common setup for
        // mixed-language dictation.
        let auto_listed =
            OpenAIRealtimeBackend::with_languages(String::new(), ru_en, "auto".into());
        assert_eq!(
            wire_languages(&auto_listed, "auto"),
            serde_json::json!(["ru", "en"])
        );
    }

    #[test]
    fn blank_language_codes_never_reach_the_wire() {
        let backend = OpenAIRealtimeBackend::with_languages(
            String::new(),
            vec![" ru ".into(), String::new(), "  ".into(), "en".into()],
            "auto".into(),
        );
        assert_eq!(
            wire_languages(&backend, "auto"),
            serde_json::json!(["ru", "en"])
        );

        // A list of nothing but blanks is no list: `[general] language` applies.
        let blanks =
            OpenAIRealtimeBackend::with_languages(String::new(), vec![" ".into()], "ru".into());
        assert_eq!(wire_languages(&blanks, "ru"), serde_json::json!(["ru"]));
    }
}
