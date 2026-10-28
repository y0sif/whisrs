//! Backend construction: turn a backend name plus the config into a
//! [`TranscriptionBackend`], and pick the model and prompt each request uses.
//!
//! Lives in the library rather than the daemon so recording processing
//! ([`crate::recording::process`]) can build a backend other than the dictation
//! one. The daemon's `factory` module wraps these for `[general] backend`.

use std::sync::Arc;

use tracing::{info, warn};

use super::asr_sidecar::AsrSidecarBackend;
use super::deepgram::{DeepgramRestBackend, DeepgramStreamingBackend};
use super::groq::GroqBackend;
use super::local_parakeet::ParakeetBackend;
use super::local_vosk::VoskBackend;
use super::local_whisper::LocalWhisperBackend;
use super::openai_compatible_realtime::OpenAiCompatibleRealtimeBackend;
use super::openai_realtime::OpenAIRealtimeBackend;
use super::openai_rest::OpenAIRestBackend;
#[cfg(feature = "recording")]
use super::transcription_prompt;
use super::TranscriptionBackend;
#[cfg(feature = "recording")]
use super::TranscriptionConfig;
#[cfg(feature = "recording")]
use crate::RecordingConfig;
use crate::{Config, LocalWhisperConfig};

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
pub fn resolve_tts_api_key(config: &Config) -> Option<String> {
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
/// into a backend — [`create_backend_named`] just wraps the result in an `Arc`. It
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
    let url = asr_sidecar_url(config);
    let api_key = resolve_asr_sidecar_api_key_from(env_key, config);
    if api_key.is_some() {
        info!("ASR sidecar API key configured");
    }
    info!("using ASR sidecar transcription backend ({url})");
    AsrSidecarBackend::new(url, api_key)
}

/// The sidecar endpoint: `[asr-sidecar] url`, or the default when the
/// section is absent.
fn asr_sidecar_url(config: &Config) -> String {
    config
        .asr_sidecar
        .as_ref()
        .map(|v| v.url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8765/transcribe".to_string())
}

/// Build the OpenAI Realtime backend described by `config`.
///
/// Returns the concrete type for the same reason [`asr_sidecar_backend`] does:
/// `[openai] languages` and `[general] language` are joined into the backend
/// here and nowhere else, and a refactor that dropped either would otherwise
/// show up only as hints missing from the user's session.update.
fn openai_realtime_backend(config: &Config) -> OpenAIRealtimeBackend {
    let api_key = resolve_openai_api_key(config).unwrap_or_default();
    if api_key.is_empty() {
        warn!("no OpenAI API key configured");
    }
    info!("using OpenAI Realtime transcription backend");
    let languages = config
        .openai
        .as_ref()
        .map(|o| o.languages.clone())
        .unwrap_or_default();
    OpenAIRealtimeBackend::with_languages(api_key, languages, config.general.language.clone())
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

/// Build the backend named `name` (a `[general] backend` string, aliases
/// included). Unknown names fall back to Groq with a warning, which is the
/// dictation behavior; recordings resolve their name through
/// [`recording_backend_name`] first, which rejects unknown names instead.
pub fn create_backend_named(name: &str, config: &Config) -> Arc<dyn TranscriptionBackend> {
    match name {
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
        "openai-realtime" => Arc::new(openai_realtime_backend(config)),
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
                 (model: {}, segmentation: {}, flash_attn: {})",
                local_whisper.model_path, local_whisper.segmentation, local_whisper.flash_attn
            );
            Arc::new(
                LocalWhisperBackend::with_flash_attn(
                    local_whisper.model_path,
                    local_whisper.flash_attn,
                )
                .with_segmentation(&local_whisper.segmentation, local_whisper.phrase_silence_ms),
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

/// The `[local-whisper]` settings [`create_backend_named`] runs on, including the
/// fallback for a fully absent section.
///
/// Serde defaults only apply when the section exists, so the absent case needs
/// its own answer. It is `LocalWhisperConfig::default`, never a literal here:
/// `Config::validate` checks the same path for existence, and a separate copy
/// would let it warn about one model file while the daemon loads another. Split
/// out so `local_whisper_fallback_is_the_shared_default` can pin that, because
/// the copy this replaced was unpinned. Same trap as `model_for_backend`
/// below.
fn local_whisper_settings(config: &Config) -> LocalWhisperConfig {
    config.local_whisper.clone().unwrap_or_default()
}

/// The model a request to backend `name` carries.
pub fn model_for_backend(name: &str, config: &Config) -> String {
    match name {
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
        // Same pin as the deepgram arm above: `Config::validate`'s
        // inert-prompt gate decides whether the model takes a prompt from this
        // accessor, so a copy here could drift from what the wire carries.
        "openai-realtime" => config.openai_realtime_model(),
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

/// The canonical name of a backend: aliases resolve to the name they stand
/// for (`local` is `local-whisper`; `asr` and `vibevoice` are
/// `asr-sidecar`), every other name is returned as is.
pub fn canonical_backend_name(name: &str) -> &str {
    match name {
        "local" => "local-whisper",
        "asr" | "vibevoice" => "asr-sidecar",
        other => other,
    }
}

/// What decides backend `name`'s output beyond its [`TranscriptionConfig`]:
/// for local whisper the model file, the phrase cut (`phrase_silence_ms`,
/// as recordings cap it: [`super::recording_phrase_silence_ms`]) and
/// `flash_attn` (which can change the decoded text slightly); the
/// endpoint for the ASR sidecar. Empty for cloud backends, whose model is in
/// the request. Recordings key their chunk cache on it, so changing it
/// re-transcribes.
///
/// `[local-whisper] segmentation` is deliberately absent: it only picks how
/// dictation streams, and recordings always decode phrase by phrase whatever it
/// says (see `LocalWhisperBackend::transcribe_segments`).
pub fn backend_identity(name: &str, config: &Config) -> String {
    match canonical_backend_name(name) {
        "local-whisper" => {
            let settings = local_whisper_settings(config);
            format!(
                "model_path={}\nphrase_silence_ms={}\nflash_attn={}",
                settings.model_path,
                super::recording_phrase_silence_ms(settings.phrase_silence_ms),
                settings.flash_attn
            )
        }
        "asr-sidecar" => format!("url={}", asr_sidecar_url(config)),
        _ => String::new(),
    }
}

/// The backend recordings transcribe with: `[recording] backend`, else
/// `[general] backend`.
///
/// Recordings are transcribed after they end, so live-only backends
/// are mapped or refused here rather than failing per chunk:
/// `deepgram-streaming` becomes the `deepgram` REST backend (same key and
/// model), and the two OpenAI realtime backends are an error that names the
/// batch backends. Unknown names are an error too: unlike dictation, a
/// recording never silently falls back to Groq.
#[cfg(feature = "recording")]
pub fn recording_backend_name(config: &Config) -> anyhow::Result<String> {
    let recording = RecordingConfig::effective(config);
    let name = recording
        .backend
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(config.general.backend.as_str());
    match name {
        "deepgram-streaming" => Ok("deepgram".to_string()),
        "openai-realtime" | "openai-compatible-realtime" => Err(super::live_only_error(name)),
        "deepgram" | "groq" | "openai" | "local-whisper" | "local" | "asr-sidecar" | "asr"
        | "vibevoice" | "local-vosk" | "local-parakeet" => Ok(name.to_string()),
        other => anyhow::bail!(
            "unknown recording backend '{other}'. Use one of: groq, openai, deepgram, \
             local-whisper, asr-sidecar"
        ),
    }
}

/// The request config for one track of a recording on backend `name`:
/// `language` (the track's own, never `[general] language` by default: the
/// two tracks of a recording may differ), `[general]` vocabulary, and that
/// backend's model. Same shape as the dictation batch config, which uses
/// `[general] backend`'s model and also sends `[general] prompt`. A recording
/// does not: the prompt is an instruction for dictation ("Transcribe in the
/// language spoken", a style), and the vocabulary still goes as the hint,
/// keyterms or hotwords as before.
///
/// Local whisper gets no prompt at all: its recording windows decode with
/// `n_max_text_ctx` 0, so no pass is conditioned on earlier text, and in
/// this whisper.cpp that leaves the initial prompt unused too. Sending none
/// keeps the prompt-echo filter and the chunk cache honest about it.
#[cfg(feature = "recording")]
pub fn recording_transcription_config(
    name: &str,
    config: &Config,
    language: &str,
) -> TranscriptionConfig {
    let model = model_for_backend(name, config);
    let canonical = canonical_backend_name(name);
    let model = match canonical {
        "openai" => openai_recording_model(model),
        _ => model,
    };
    let prompt = match canonical {
        "local-whisper" => None,
        _ => transcription_prompt(None, &config.general.vocabulary),
    };
    TranscriptionConfig {
        language: language.to_string(),
        model,
        prompt,
        keyterms: config.general.vocabulary.clone(),
        diarize: false,
        vad: config.audio.vad(),
        soft_voices: true,
    }
}

/// The model a recording on backend `name` runs, for the log: the request's
/// model, except for local whisper, whose request carries a fixed name while
/// `[local-whisper] model_path` decides, so its file name. Nothing sent or
/// cached depends on this.
#[cfg(feature = "recording")]
pub fn recording_model_label(name: &str, config: &Config, request: &TranscriptionConfig) -> String {
    match canonical_backend_name(name) {
        "local-whisper" => {
            let path = local_whisper_settings(config).model_path;
            std::path::Path::new(&path)
                .file_name()
                .map_or_else(|| path.clone(), |f| f.to_string_lossy().into_owned())
        }
        _ => request.model.clone(),
    }
}

/// The model a recording on the `openai` REST backend sends, given
/// `[openai] model`.
///
/// `[openai] model` is shared with `openai-realtime`, and its realtime-only
/// models (gpt-live-transcribe, gpt-realtime-whisper) are refused by the file
/// endpoint, `/v1/audio/transcriptions`. Without this, someone dictating with
/// one of them could not run recordings on `openai` at all, since there is no
/// separate recording model to set. Those map to gpt-transcribe, the model
/// OpenAI points recorded audio at; every other model is sent as written.
#[cfg(feature = "recording")]
fn openai_recording_model(model: String) -> String {
    let realtime_only = ["gpt-live-transcribe", "gpt-realtime-whisper"];
    if realtime_only.iter().any(|m| model.eq_ignore_ascii_case(m)) {
        "gpt-transcribe".to_string()
    } else {
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::TranscriptionConfig;

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
            asr_sidecar: Some(crate::AsrSidecarConfig {
                url: "http://127.0.0.1:8765/transcribe".to_string(),
                model: "microsoft/VibeVoice-ASR-HF".to_string(),
                api_key: api_key.map(str::to_string),
            }),
            openai_compatible_realtime: None,
            llm: None,
            hotkeys: None,
            hooks: None,
            llm_commands: Vec::new(),
            agents: Vec::new(),
            agent_overlay: None,
            replacements: Default::default(),
            recording: None,
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
            general: crate::GeneralConfig {
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
            openai_compatible_realtime: Some(crate::OpenAiCompatibleRealtimeConfig {
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
            agents: Vec::new(),
            agent_overlay: None,
            replacements: Default::default(),
            recording: None,
            overlay: None,
            tts: None,
        };

        assert_eq!(
            model_for_backend(&config.general.backend, &config),
            "Whisper-Tiny"
        );
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
                model_for_backend(&config.general.backend, &config),
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
            crate::default_whisper_model_path(),
            "absent [local-whisper] resolves to a path validate does not check"
        );
        assert_eq!(
            local_whisper_settings(&absent).model_path,
            local_whisper_settings(&bare).model_path,
            "an absent section and a section without model_path load different models"
        );
    }

    /// The model the openai-realtime arm puts on the wire is the model
    /// `Config::openai_realtime_model` reports, on an absent `[openai]`
    /// section and on an explicit one.
    ///
    /// `Config::validate`'s inert-prompt warning reads that accessor to decide
    /// whether the model takes a prompt, which decides whether the
    /// session.update carries `[general] prompt` at all. Let the two answers
    /// drift apart and the daemon warns that the prompt is dropped while
    /// sending it, or stays silent while dropping it. Same pin as the deepgram
    /// arm above, for the same reason.
    ///
    /// What is pinned is agreement, not routing. Re-duplicating the literal
    /// keeps this green, since a copy holding the same string still answers
    /// the same; drift is what it catches, and drift is the failure that
    /// actually happened on the deepgram arm.
    #[test]
    fn openai_realtime_model_on_the_wire_is_the_one_the_prompt_gate_reads() {
        // No [openai] section at all: the fallback has to be the accessor's,
        // which is the case `whisrs setup` leaves behind for anyone who wrote
        // the config by hand.
        let absent: Config = toml::from_str("[general]\nbackend = \"openai-realtime\"\n")
            .expect("[openai] is optional");
        assert!(
            absent.openai.is_none(),
            "the fixture must exercise the fallback"
        );
        assert_eq!(
            model_for_backend(&absent.general.backend, &absent),
            absent.openai_realtime_model(),
            "the model the daemon sends and the model the inert-prompt gate reads have \
             diverged on the fallback"
        );

        // An explicit model: still the accessor, and still the configured
        // string rather than the plain-openai serde default.
        let explicit: Config = toml::from_str(
            "[general]\nbackend = \"openai-realtime\"\n\
             [openai]\napi_key = \"test-key\"\nmodel = \"gpt-4o-transcribe\"\n",
        )
        .expect("an [openai] section with a model parses");
        assert_eq!(
            model_for_backend(&explicit.general.backend, &explicit),
            "gpt-4o-transcribe"
        );
        assert_eq!(
            model_for_backend(&explicit.general.backend, &explicit),
            explicit.openai_realtime_model(),
            "the model the daemon sends and the model the inert-prompt gate reads have \
             diverged on an explicit [openai] model"
        );
    }

    /// `[openai] languages` and `[general] language` both reach the realtime
    /// backend. Asserted through `languages_for_request`, which is what the
    /// engine config is built from, so dropping either join in
    /// `openai_realtime_backend` turns this red: a lost list fails the first
    /// assert, and a lost or replaced default language fails one of the two.
    #[test]
    fn openai_realtime_backend_carries_languages_and_default_language() {
        let config: Config = toml::from_str(
            "[general]\nbackend = \"openai-realtime\"\nlanguage = \"ru\"\n\
             [openai]\napi_key = \"test-key\"\nmodel = \"gpt-live-transcribe\"\n\
             languages = [\"ru\", \" en \", \"\"]\n",
        )
        .expect("an [openai] section with languages parses");
        let request = |language: &str| TranscriptionConfig {
            language: language.to_string(),
            model: "gpt-live-transcribe".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        };

        let backend = openai_realtime_backend(&config);

        assert_eq!(
            backend.languages_for_request(&request("ru")),
            ["ru", "en"],
            "a session on [general] language must carry the trimmed [openai] languages"
        );
        assert!(
            backend.languages_for_request(&request("pl")).is_empty(),
            "a per-session -l must win over [openai] languages"
        );
    }

    /// The recording backend rules, in a build with the feature.
    #[cfg(feature = "recording")]
    mod recording {
        use super::*;

        /// How recordings treat one `[general] backend` name.
        struct RecordingCase {
            name: &'static str,
            /// What [`recording_backend_name`] resolves it to, `None` = refused.
            resolves_to: Option<&'static str>,
            /// The real backend the resolved name builds.
            backend: Box<dyn TranscriptionBackend>,
            /// Its declared chunk cap.
            chunk_secs: Option<u32>,
        }

        /// The names `Config::validate` routes, read off its unknown-backend
        /// error because `BACKEND_NAMES` is private to `config::types`. Aliases are
        /// not advertised there, so they are listed by hand below.
        fn routed_backend_names() -> Vec<String> {
            let config: Config =
                toml::from_str("[general]\nbackend = \"no-such-backend\"\n").unwrap();
            let msg = config.validate().unwrap_err().to_string();
            let primaries = msg
                .split("Valid options: ")
                .nth(1)
                .and_then(|rest| rest.split(". (").next())
                .expect("unknown-backend error lists the valid options");
            let stubs = msg
                .split(". (")
                .nth(1)
                .and_then(|rest| rest.split(" are also").next())
                .expect("unknown-backend error lists the stub backends");
            primaries
                .split(", ")
                .chain(stubs.split(" and "))
                .map(str::to_string)
                .collect()
        }

        /// Every backend name a user can select has a decided recording behavior,
        /// asked of the real impls: what the name resolves to, the chunk cap of the
        /// backend it builds, and that live-only backends are refused up front.
        /// Mirrors `config_inert_prompt_gate_agrees_with_every_sends_prompt_impl`.
        #[tokio::test]
        async fn every_backend_name_has_a_recording_behavior() {
            let sidecar = || {
                Box::new(AsrSidecarBackend::new(
                    "http://127.0.0.1:8765/transcribe".to_string(),
                    None,
                )) as Box<dyn TranscriptionBackend>
            };
            let whisper = || {
                Box::new(LocalWhisperBackend::new(String::new())) as Box<dyn TranscriptionBackend>
            };
            let cases = vec![
                RecordingCase {
                    name: "groq",
                    resolves_to: Some("groq"),
                    backend: Box::new(GroqBackend::new(String::new())),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "openai",
                    resolves_to: Some("openai"),
                    backend: Box::new(OpenAIRestBackend::new(String::new())),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "deepgram",
                    resolves_to: Some("deepgram"),
                    backend: Box::new(DeepgramRestBackend::new(String::new())),
                    chunk_secs: Some(3600),
                },
                // Mapped to REST; the streaming impl agrees if it is ever asked.
                RecordingCase {
                    name: "deepgram-streaming",
                    resolves_to: Some("deepgram"),
                    backend: Box::new(DeepgramStreamingBackend::new(String::new())),
                    chunk_secs: Some(3600),
                },
                RecordingCase {
                    name: "local-whisper",
                    resolves_to: Some("local-whisper"),
                    backend: whisper(),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "local",
                    resolves_to: Some("local"),
                    backend: whisper(),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "asr-sidecar",
                    resolves_to: Some("asr-sidecar"),
                    backend: sidecar(),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "asr",
                    resolves_to: Some("asr"),
                    backend: sidecar(),
                    chunk_secs: Some(600),
                },
                RecordingCase {
                    name: "vibevoice",
                    resolves_to: Some("vibevoice"),
                    backend: sidecar(),
                    chunk_secs: Some(600),
                },
                // Built the way the factory builds a GPT Live Transcribe setup
                // (`[openai] languages`): language hints do not make it a batch
                // backend.
                RecordingCase {
                    name: "openai-realtime",
                    resolves_to: None,
                    backend: Box::new(openai_realtime_backend(
                        &toml::from_str(
                            "[general]\nbackend = \"openai-realtime\"\n\
                         [openai]\napi_key = \"\"\nmodel = \"gpt-live-transcribe\"\n\
                         languages = [\"en\", \"de\"]\n",
                        )
                        .unwrap(),
                    )),
                    chunk_secs: None,
                },
                RecordingCase {
                    name: "openai-compatible-realtime",
                    resolves_to: None,
                    backend: Box::new(
                        OpenAiCompatibleRealtimeBackend::new(
                            "ws://localhost:1234/realtime".to_string(),
                            "Whisper-Tiny".to_string(),
                            "lemonade".to_string(),
                            "server-vad".to_string(),
                            None,
                        )
                        .unwrap(),
                    ),
                    chunk_secs: None,
                },
                // Stubs resolve (dictation accepts them too) and then fail at
                // transcription time with "not yet implemented".
                RecordingCase {
                    name: "local-vosk",
                    resolves_to: Some("local-vosk"),
                    backend: Box::new(VoskBackend::new(String::new())),
                    chunk_secs: None,
                },
                RecordingCase {
                    name: "local-parakeet",
                    resolves_to: Some("local-parakeet"),
                    backend: Box::new(ParakeetBackend::new(String::new())),
                    chunk_secs: None,
                },
            ];

            let request = TranscriptionConfig {
                language: "en".to_string(),
                model: String::new(),
                prompt: None,
                keyterms: Vec::new(),
                diarize: false,
                vad: Default::default(),
                soft_voices: true,
            };
            let wav = crate::audio::wav::encode_wav(&[0i16; 1600]).unwrap();
            for case in &cases {
                let mut config: Config = toml::from_str("").unwrap();
                config.general.backend = case.name.to_string();
                let resolved = recording_backend_name(&config);
                match case.resolves_to {
                    Some(expected) => assert_eq!(resolved.unwrap(), expected, "{}", case.name),
                    None => {
                        let err = resolved.expect_err(case.name).to_string();
                        assert!(err.contains("batch backend"), "{}: {err}", case.name);
                        // The impl refuses too, without touching the network.
                        let err = case
                            .backend
                            .transcribe_segments(&wav, &request)
                            .await
                            .expect_err(case.name);
                        assert!(err.to_string().contains("batch backend"), "{}", case.name);
                    }
                }
                assert_eq!(
                    case.backend.max_segment_chunk_secs(&request),
                    case.chunk_secs,
                    "{}",
                    case.name
                );
            }

            let routed = routed_backend_names();
            assert!(
                routed.iter().any(|n| n == "groq") && routed.iter().any(|n| n == "local-vosk"),
                "parsing the unknown-backend error went wrong: {routed:?}"
            );
            for name in routed {
                assert!(
                cases.iter().any(|c| c.name == name),
                "{name} is a backend validate() routes but has no recording behavior here. Add a \
                 case with its real impl."
            );
            }
        }

        #[test]
        fn recording_backend_override_and_refusals() {
            let mut config: Config = toml::from_str(
            "[general]\nbackend = \"openai-realtime\"\n[recording]\nbackend = \"deepgram-streaming\"\n",
        )
        .unwrap();
            // `[recording] backend` wins over a live-only dictation backend.
            assert_eq!(recording_backend_name(&config).unwrap(), "deepgram");

            config.recording.as_mut().unwrap().backend = Some("  ".into());
            assert!(
                recording_backend_name(&config).is_err(),
                "blank falls back to general"
            );

            config.recording.as_mut().unwrap().backend = Some("bogus".into());
            let err = recording_backend_name(&config).unwrap_err().to_string();
            assert!(
                err.contains("unknown recording backend"),
                "no silent groq fallback: {err}"
            );
        }

        /// A realtime-only `[openai] model` (what an openai-realtime dictation
        /// setup holds) still lets recordings run on `openai`: the recording request
        /// carries gpt-transcribe, while dictation keeps the configured model.
        #[test]
        fn recording_on_openai_maps_realtime_only_models_to_gpt_transcribe() {
            for (configured, sent) in [
                ("gpt-live-transcribe", "gpt-transcribe"),
                ("GPT-Live-Transcribe", "gpt-transcribe"),
                ("gpt-realtime-whisper", "gpt-transcribe"),
                ("gpt-transcribe", "gpt-transcribe"),
                ("gpt-4o-transcribe", "gpt-4o-transcribe"),
                ("whisper-1", "whisper-1"),
            ] {
                let config: Config = toml::from_str(&format!(
                    "[general]\nbackend = \"openai-realtime\"\n[recording]\nbackend = \"openai\"\n\
                 [openai]\napi_key = \"k\"\nmodel = \"{configured}\"\n\
                 languages = [\"ru\", \"en\"]\n"
                ))
                .unwrap();
                let name = recording_backend_name(&config).unwrap();
                assert_eq!(name, "openai");
                assert_eq!(
                    recording_transcription_config(&name, &config, "en").model,
                    sent,
                    "[openai] model = {configured}"
                );
                assert_eq!(
                    model_for_backend(&config.general.backend, &config),
                    configured,
                    "dictation keeps [openai] model as written"
                );
            }
        }

        #[test]
        fn the_recording_log_names_the_local_model_file() {
            let config: Config = toml::from_str(
                "[general]\nbackend = \"local-whisper\"\n\
             [local-whisper]\nmodel_path = \"/m/ggml-large-v3-turbo-q5_0.bin\"\n",
            )
            .unwrap();
            let request = recording_transcription_config("local-whisper", &config, "en");
            assert_eq!(
                recording_model_label("local-whisper", &config, &request),
                "ggml-large-v3-turbo-q5_0.bin"
            );
            // Its recording windows take no prompt, so none is sent.
            let with_vocabulary: Config = toml::from_str(
                "[general]\nbackend = \"local-whisper\"\nvocabulary = [\"whisrs\"]\n",
            )
            .unwrap();
            assert_eq!(
                recording_transcription_config("local", &with_vocabulary, "en").prompt,
                None
            );
            assert_eq!(
                recording_model_label("local", &config, &request),
                "ggml-large-v3-turbo-q5_0.bin"
            );
            // What the request carries (and the cache keys on) is unchanged.
            assert_eq!(request.model, "base.en");
            let groq: Config = toml::from_str("[general]\nbackend = \"groq\"\n").unwrap();
            let request = recording_transcription_config("groq", &groq, "en");
            assert_eq!(
                recording_model_label("groq", &groq, &request),
                request.model
            );
        }

        #[test]
        fn recording_request_uses_the_recording_backends_model() {
            let config: Config = toml::from_str(
                "[general]\nbackend = \"groq\"\nlanguage = \"de\"\nvocabulary = [\"whisrs\"]\n\
             prompt = \"Transcribe in the language spoken.\"\n\
             [recording]\nbackend = \"openai\"\n\
             [openai]\napi_key = \"k\"\nmodel = \"whisper-1\"\n",
            )
            .unwrap();
            let name = recording_backend_name(&config).unwrap();
            let request = recording_transcription_config(&name, &config, "fr");
            assert_eq!(request.model, "whisper-1");
            // The track's language, not `[general] language`.
            assert_eq!(request.language, "fr");
            // The vocabulary, without the dictation prompt.
            assert_eq!(request.prompt.as_deref(), Some("whisrs"));
            assert_eq!(request.keyterms, ["whisrs"]);

            let no_vocabulary: Config = toml::from_str(
                "[general]\nbackend = \"groq\"\nprompt = \"Transcribe in the language spoken.\"\n",
            )
            .unwrap();
            let request = recording_transcription_config("groq", &no_vocabulary, "en");
            assert_eq!(request.prompt, None, "nothing to send");
        }
    }
}
