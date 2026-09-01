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

/// Resolution rules for the ASR sidecar key, with the environment passed in.
///
/// Env vars are process-global, so the resolution itself is kept pure and
/// [`asr_sidecar_backend`] is the only thing that reads the environment.
/// `WHISRS_ASR_SIDECAR_API_KEY` wins over `[asr-sidecar] api_key`; both sides
/// are trimmed and a blank value on either side counts as absent.
fn resolve_asr_sidecar_api_key_from(env_key: Option<&str>, config: &Config) -> Option<String> {
    if let Some(key) = env_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    config
        .asr_sidecar
        .as_ref()
        .and_then(|s| s.api_key.as_deref())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Build the ASR sidecar backend described by `config`.
///
/// This is the only place the sidecar URL and the resolved API key are joined
/// into a backend — [`create_backend`] just wraps the result in an `Arc`. It
/// returns the concrete type so a test can assert that both actually made it
/// in; a refactor that dropped the key here would otherwise show up nowhere
/// but as a 401 against the user's sidecar.
fn asr_sidecar_backend(config: &Config) -> AsrSidecarBackend {
    asr_sidecar_backend_from(
        std::env::var("WHISRS_ASR_SIDECAR_API_KEY").ok().as_deref(),
        config,
    )
}

/// [`asr_sidecar_backend`] with the environment passed in, so a stray
/// `WHISRS_ASR_SIDECAR_API_KEY` in the shell running the tests cannot change
/// what they assert.
fn asr_sidecar_backend_from(env_key: Option<&str>, config: &Config) -> AsrSidecarBackend {
    let url = config
        .asr_sidecar
        .as_ref()
        .map(|v| v.url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8765/transcribe".to_string());
    let api_key = resolve_asr_sidecar_api_key_from(env_key, config);
    if api_key.is_some() {
        info!("ASR sidecar API key configured");
    }
    info!("using ASR sidecar transcription backend ({url})");
    AsrSidecarBackend::new(url, api_key)
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
            let local_whisper = local_whisper_settings(config);
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
        "asr-sidecar" | "asr" | "vibevoice" => Arc::new(asr_sidecar_backend(config)),
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

/// The `[local-whisper]` settings [`create_backend`] runs on, including the
/// fallback for a fully absent section.
///
/// Serde defaults only apply when the section exists, so the absent case needs
/// its own answer. It is `LocalWhisperConfig::default`, never a literal here:
/// `Config::validate` checks the same path for existence, and a separate copy
/// would let it warn about one model file while the daemon loads another. Split
/// out so `local_whisper_fallback_is_the_shared_default` can pin that, because
/// the copy this replaced was unpinned. Same trap as `get_model_for_backend`
/// below.
fn local_whisper_settings(config: &Config) -> LocalWhisperConfig {
    config.local_whisper.clone().unwrap_or_default()
}

pub(crate) fn get_model_for_backend(config: &Config) -> String {
    match config.general.backend.as_str() {
        // Not a local literal: `Config::deepgram_model` is the same function
        // `Config::validate`'s keyterm gate inspects, so the model warned about
        // at load is the model that goes on the wire. A separate copy here was
        // unpinned — flipping it to "nova-2" left all 498 tests green, which
        // would have meant `validate` staying silent while every keyterm
        // request 400'd.
        "deepgram" | "deepgram-streaming" => config.deepgram_model(),
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

    /// A bare config carrying only the `[asr-sidecar]` section under test.
    fn config_with_sidecar_key(api_key: Option<&str>) -> Config {
        Config {
            general: Default::default(),
            audio: Default::default(),
            input: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: Some(whisrs::AsrSidecarConfig {
                url: "http://127.0.0.1:8765/transcribe".to_string(),
                model: "microsoft/VibeVoice-ASR-HF".to_string(),
                api_key: api_key.map(str::to_string),
            }),
            openai_compatible_realtime: None,
            llm: None,
            hotkeys: None,
            hooks: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        }
    }

    #[test]
    fn asr_sidecar_env_key_wins_over_config() {
        let config = config_with_sidecar_key(Some("config-key"));
        assert_eq!(
            resolve_asr_sidecar_api_key_from(Some("env-key"), &config).as_deref(),
            Some("env-key")
        );
    }

    #[test]
    fn asr_sidecar_config_key_used_when_env_is_absent() {
        let config = config_with_sidecar_key(Some("config-key"));
        assert_eq!(
            resolve_asr_sidecar_api_key_from(None, &config).as_deref(),
            Some("config-key")
        );
    }

    #[test]
    fn asr_sidecar_blank_env_key_falls_through_to_config() {
        let config = config_with_sidecar_key(Some("config-key"));
        assert_eq!(
            resolve_asr_sidecar_api_key_from(Some("   "), &config).as_deref(),
            Some("config-key")
        );
    }

    #[test]
    fn asr_sidecar_blank_config_key_resolves_to_none() {
        let config = config_with_sidecar_key(Some("   "));
        assert_eq!(resolve_asr_sidecar_api_key_from(Some(""), &config), None);
        assert_eq!(resolve_asr_sidecar_api_key_from(None, &config), None);
    }

    #[test]
    fn asr_sidecar_keys_are_trimmed_on_both_paths() {
        let config = config_with_sidecar_key(Some("  config-key\n"));
        assert_eq!(
            resolve_asr_sidecar_api_key_from(Some("  env-key\n"), &config).as_deref(),
            Some("env-key")
        );
        assert_eq!(
            resolve_asr_sidecar_api_key_from(None, &config).as_deref(),
            Some("config-key")
        );
    }

    #[test]
    fn asr_sidecar_missing_section_resolves_to_none() {
        let mut config = config_with_sidecar_key(None);
        config.asr_sidecar = None;
        assert_eq!(resolve_asr_sidecar_api_key_from(None, &config), None);
    }

    /// Assert the constructed backend, not the resolver. `asr_sidecar_backend`
    /// is the single join between a resolved key and the thing that sends it,
    /// so a refactor that dropped the key there would leave every
    /// `resolve_*` test above green and only fail against a real sidecar.
    #[test]
    fn asr_sidecar_backend_carries_the_configured_url_and_key() {
        let mut config = config_with_sidecar_key(Some("sk-config-key"));
        config.asr_sidecar.as_mut().unwrap().url = "http://sidecar.local:9000/asr".to_string();

        let backend = asr_sidecar_backend_from(None, &config);

        assert_eq!(backend.url(), "http://sidecar.local:9000/asr");
        assert_eq!(backend.api_key(), Some("sk-config-key"));
    }

    #[test]
    fn asr_sidecar_backend_carries_the_env_key() {
        let config = config_with_sidecar_key(None);

        let backend = asr_sidecar_backend_from(Some("sk-env-key"), &config);

        assert_eq!(backend.api_key(), Some("sk-env-key"));
    }

    #[test]
    fn asr_sidecar_backend_is_unauthenticated_without_a_key() {
        let config = config_with_sidecar_key(None);

        let backend = asr_sidecar_backend_from(None, &config);

        assert_eq!(backend.api_key(), None);
    }

    /// `[asr-sidecar]` is optional, so selecting the backend with no section at
    /// all must still point at the address the contrib sidecar recipes bind to.
    #[test]
    fn asr_sidecar_backend_falls_back_to_the_default_url() {
        let mut config = config_with_sidecar_key(None);
        config.asr_sidecar = None;

        let backend = asr_sidecar_backend_from(None, &config);

        assert_eq!(backend.url(), "http://127.0.0.1:8765/transcribe");
        assert_eq!(backend.api_key(), None);
    }

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
            hooks: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };

        assert_eq!(get_model_for_backend(&config), "Whisper-Tiny");
    }

    #[test]
    fn deepgram_model_on_the_wire_is_the_one_validate_gates_on() {
        // `[deepgram]` is optional, and with the section absent this factory
        // used to fall back to its own `"nova-3"` literal while
        // `Config::validate`'s keyterm gate consulted
        // `Config::deepgram_model`. Two independent strings, neither pinned:
        // flipping the one here to "nova-2" left the whole suite green, and
        // would have meant `validate` staying silent about a vocabulary while
        // every request 400'd on an unsupported `keyterm` parameter.
        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.deepgram = None;

        for backend in ["deepgram", "deepgram-streaming"] {
            config.general.backend = backend.to_string();
            assert_eq!(
                get_model_for_backend(&config),
                config.deepgram_model(),
                "{backend}: the wire model and the model validate inspects disagree"
            );
        }
    }

    #[test]
    fn local_whisper_fallback_is_the_shared_default() {
        // Same trap as the test above, on the other local literal. With
        // `[local-whisper]` absent this factory used to build its own copy of
        // the model path while `Config::validate` computed the path it checks
        // for existence separately. Replacing this fallback with a literal
        // left the whole suite green, so pin it: the absent section, a section
        // that omits `model_path`, and the path `validate` resolves must all
        // be the same string, or the daemon loads a model the startup warning
        // never mentioned.
        let mut absent: Config = toml::from_str("").expect("empty config uses defaults");
        absent.local_whisper = None;

        let bare: Config = toml::from_str("[local-whisper]\nsegmentation = \"silence\"\n")
            .expect("a [local-whisper] section may omit model_path");

        assert_eq!(
            local_whisper_settings(&absent).model_path,
            whisrs::default_whisper_model_path(),
            "absent [local-whisper] resolves to a path validate does not check"
        );
        assert_eq!(
            local_whisper_settings(&absent).model_path,
            local_whisper_settings(&bare).model_path,
            "an absent section and a section without model_path load different models"
        );
    }
}
