use std::sync::Arc;

use tracing::{info, warn};

use whisrs::transcription::asr_sidecar::AsrSidecarBackend;
use whisrs::transcription::deepgram::{DeepgramRestBackend, DeepgramStreamingBackend};
use whisrs::transcription::groq::GroqBackend;
use whisrs::transcription::local_parakeet::ParakeetBackend;
use whisrs::transcription::local_vosk::VoskBackend;
use whisrs::transcription::local_whisper::LocalWhisperBackend;
use whisrs::transcription::openai_compatible_realtime::OpenAiCompatibleRealtimeBackend;
use whisrs::transcription::openai_realtime::OpenAIRealtimeBackend;
use whisrs::transcription::openai_rest::OpenAIRestBackend;
use whisrs::transcription::TranscriptionBackend;
use whisrs::{Config, LocalWhisperConfig};

fn resolve_groq_api_key(config: &Config) -> Option<String> {
    if let Ok(key) = std::env::var("WHISRS_GROQ_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    config.groq.as_ref().map(|g| g.api_key.clone())
}

/// Resolve the API key used for TTS.
///
/// The dedicated `[tts] api_key` always wins. Otherwise the key is resolved
/// per the configured `[tts] backend`, falling back to that provider's
/// transcription key (env var or `config.toml`):
/// - `groq`                       → Groq key (`[groq]` / `WHISRS_GROQ_API_KEY`)
/// - `openai`                     → OpenAI key (`[openai]` / `WHISRS_OPENAI_API_KEY`)
/// - `deepgram`                   → Deepgram key (`[deepgram]` / `WHISRS_DEEPGRAM_API_KEY`)
/// - `tts-sidecar` / `openai-compat` → no key (local servers usually need none)
pub(crate) fn resolve_tts_api_key(config: &Config) -> Option<String> {
    if let Some(tts) = &config.tts {
        if let Some(key) = tts.api_key.as_ref().filter(|k| !k.is_empty()) {
            return Some(key.clone());
        }
        match tts.backend.as_str() {
            "openai" => return resolve_openai_api_key(config),
            "deepgram" => return resolve_deepgram_api_key(config),
            "tts-sidecar" | "openai-compat" => return None,
            _ => {}
        }
    }
    resolve_groq_api_key(config)
}

fn resolve_openai_api_key(config: &Config) -> Option<String> {
    if let Ok(key) = std::env::var("WHISRS_OPENAI_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    config.openai.as_ref().map(|o| o.api_key.clone())
}

fn resolve_deepgram_api_key(config: &Config) -> Option<String> {
    if let Ok(key) = std::env::var("WHISRS_DEEPGRAM_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    config.deepgram.as_ref().map(|d| d.api_key.clone())
}

fn sanitize_ws_endpoint_for_log(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return "<invalid ws endpoint>".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

pub(crate) fn create_backend(config: &Config) -> Arc<dyn TranscriptionBackend> {
    match config.general.backend.as_str() {
        "deepgram" => {
            let api_key = resolve_deepgram_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no Deepgram API key configured");
            }
            info!("using Deepgram REST transcription backend");
            Arc::new(DeepgramRestBackend::new(api_key))
        }
        "deepgram-streaming" => {
            let api_key = resolve_deepgram_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no Deepgram API key configured");
            }
            info!("using Deepgram streaming transcription backend");
            Arc::new(DeepgramStreamingBackend::new(api_key))
        }
        "groq" => {
            let api_key = resolve_groq_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no Groq API key configured");
            }
            info!("using Groq transcription backend");
            Arc::new(GroqBackend::new(api_key))
        }
        "openai-realtime" => {
            let api_key = resolve_openai_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no OpenAI API key configured");
            }
            info!("using OpenAI Realtime transcription backend");
            Arc::new(OpenAIRealtimeBackend::new(api_key))
        }
        "openai" => {
            let api_key = resolve_openai_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no OpenAI API key configured");
            }
            info!("using OpenAI REST transcription backend");
            Arc::new(OpenAIRestBackend::new(api_key))
        }
        "local-whisper" | "local" => {
            // Serde defaults only apply when the [local-whisper] section
            // exists; `LocalWhisperConfig::new` supplies the same defaults
            // for a fully absent section.
            let local_whisper = config.local_whisper.clone().unwrap_or_else(|| {
                LocalWhisperConfig::new(
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/ggml-base.en.bin")
                        .to_string_lossy()
                        .to_string(),
                )
            });
            info!(
                "using local whisper transcription backend \
                 (model: {}, segmentation: {})",
                local_whisper.model_path, local_whisper.segmentation
            );
            Arc::new(
                LocalWhisperBackend::new(local_whisper.model_path).with_segmentation(
                    &local_whisper.segmentation,
                    local_whisper.phrase_silence_ms,
                ),
            )
        }
        "local-vosk" => {
            let model_path = config
                .local_vosk
                .as_ref()
                .map(|l| l.model_path.clone())
                .unwrap_or_else(|| {
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/vosk-model-small-en-us-0.15")
                        .to_string_lossy()
                        .to_string()
                });
            info!("using Vosk transcription backend (model: {model_path})");
            Arc::new(VoskBackend::new(model_path))
        }
        "local-parakeet" => {
            let model_path = config
                .local_parakeet
                .as_ref()
                .map(|l| l.model_path.clone())
                .unwrap_or_else(|| {
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/parakeet-eou-120m")
                        .to_string_lossy()
                        .to_string()
                });
            info!("using Parakeet transcription backend (model: {model_path})");
            Arc::new(ParakeetBackend::new(model_path))
        }
        "asr-sidecar" | "asr" | "vibevoice" => {
            let url = config
                .asr_sidecar
                .as_ref()
                .map(|v| v.url.clone())
                .unwrap_or_else(|| "http://127.0.0.1:8765/transcribe".to_string());
            info!("using ASR sidecar transcription backend ({url})");
            Arc::new(AsrSidecarBackend::new(url))
        }
        "openai-compatible-realtime" => {
            let realtime = config.openai_compatible_realtime.as_ref().cloned();
            let Some(realtime) = realtime else {
                warn!(
                    "openai-compatible-realtime backend selected but config section is missing; falling back to groq"
                );
                let api_key = resolve_groq_api_key(config).unwrap_or_default();
                return Arc::new(GroqBackend::new(api_key));
            };

            let endpoint_display = sanitize_ws_endpoint_for_log(&realtime.url);
            info!(
                "using OpenAI-compatible realtime transcription backend ({endpoint_display}, profile={}, turn_detection={})",
                realtime.profile, realtime.turn_detection
            );

            match OpenAiCompatibleRealtimeBackend::new(
                realtime.url,
                realtime.model,
                realtime.profile,
                realtime.turn_detection,
                realtime.api_key,
            ) {
                Ok(backend) => Arc::new(backend),
                Err(e) => {
                    warn!(
                        "failed to initialize OpenAI-compatible realtime backend for {endpoint_display}: {e}; falling back to groq"
                    );
                    let api_key = resolve_groq_api_key(config).unwrap_or_default();
                    Arc::new(GroqBackend::new(api_key))
                }
            }
        }
        other => {
            warn!("unknown backend '{other}', falling back to groq");
            let api_key = resolve_groq_api_key(config).unwrap_or_default();
            Arc::new(GroqBackend::new(api_key))
        }
    }
}

pub(crate) fn get_model_for_backend(config: &Config) -> String {
    match config.general.backend.as_str() {
        "deepgram" | "deepgram-streaming" => config
            .deepgram
            .as_ref()
            .map(|d| d.model.clone())
            .unwrap_or_else(|| "nova-3".to_string()),
        "groq" => config
            .groq
            .as_ref()
            .map(|g| g.model.clone())
            .unwrap_or_else(|| "whisper-large-v3-turbo".to_string()),
        "openai-realtime" => config
            .openai
            .as_ref()
            .map(|o| o.model.clone())
            .unwrap_or_else(|| "gpt-realtime-whisper".to_string()),
        "openai" => config
            .openai
            .as_ref()
            .map(|o| o.model.clone())
            .unwrap_or_else(|| "gpt-4o-mini-transcribe".to_string()),
        "openai-compatible-realtime" => config
            .openai_compatible_realtime
            .as_ref()
            .map(|v| v.model.clone())
            .unwrap_or_else(|| "Whisper-Tiny".to_string()),
        "local-whisper" | "local" => "base.en".to_string(),
        "local-vosk" => "small-en-us".to_string(),
        "local-parakeet" => "eou-120m".to_string(),
        "asr-sidecar" | "asr" | "vibevoice" => config
            .asr_sidecar
            .as_ref()
            .map(|v| v.model.clone())
            .unwrap_or_else(|| "microsoft/VibeVoice-ASR-HF".to_string()),
        _ => "whisper-large-v3-turbo".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_ws_endpoint_for_log_strips_credentials_and_query() {
        assert_eq!(
            sanitize_ws_endpoint_for_log("ws://user:secret@localhost:1234/realtime?token=abc"),
            "ws://localhost:1234/realtime"
        );
    }

    #[test]
    fn get_model_for_openai_compatible_realtime_backend() {
        let config = Config {
            general: whisrs::GeneralConfig {
                backend: "openai-compatible-realtime".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            input: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: None,
            openai_compatible_realtime: Some(whisrs::OpenAiCompatibleRealtimeConfig {
                url: "ws://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };

        assert_eq!(get_model_for_backend(&config), "Whisper-Tiny");
    }
}
