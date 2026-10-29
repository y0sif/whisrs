//! Text-to-speech backends: trait definition and implementations.
//!
//! The synthesis stage of the "read selection aloud" feature lives behind a
//! small [`TtsBackend`] trait (mirroring [`crate::transcription::TranscriptionBackend`]).
//!
//! Cloud and local-server backends are covered today:
//! - `groq` / `openai` / `tts-sidecar` share the OpenAI `/v1/audio/speech`
//!   request shape via [`openai_compat::OpenAiCompatTts`]. The `tts-sidecar`
//!   backend points at any local OpenAI-compatible server (Kokoro, Supertonic,
//!   etc.) and needs no API key.
//! - `deepgram` uses Deepgram Aura-2 via [`deepgram_aura::DeepgramAuraTts`].
//!
//! Roadmap: native in-process local backends (Supertonic via ONNX/`ort`,
//! Piper, Kokoro) are future work. Until then, those models are reachable
//! today through a local server behind the `tts-sidecar` backend.

pub mod deepgram_aura;
pub mod groq;
pub mod openai_compat;

use async_trait::async_trait;

use crate::{TtsConfig, WhisrsError};

/// Default endpoint for a local OpenAI-compatible TTS sidecar (e.g. Kokoro-FastAPI).
const DEFAULT_SIDECAR_URL: &str = "http://127.0.0.1:8880/v1/audio/speech";

/// OpenAI's text-to-speech endpoint.
const OPENAI_SPEECH_URL: &str = "https://api.openai.com/v1/audio/speech";

// Per-backend default model/voice, applied when `[tts] model`/`voice` are unset
// so a user can switch `backend` without also overriding the model. The Groq
// default (orpheus) is meaningless to OpenAI/Deepgram, which is why the default
// is resolved here per backend rather than baked into the config.
const GROQ_DEFAULT_MODEL: &str = "canopylabs/orpheus-v1-english";
const GROQ_DEFAULT_VOICE: &str = "autumn";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini-tts";
const OPENAI_DEFAULT_VOICE: &str = "alloy";
const SIDECAR_DEFAULT_MODEL: &str = "kokoro";
const SIDECAR_DEFAULT_VOICE: &str = "af_heart";

/// The model `backend` uses while `[tts] model` is unset (the config
/// editor names it).
pub fn default_model(backend: &str) -> Option<&'static str> {
    match backend {
        "groq" => Some(GROQ_DEFAULT_MODEL),
        "openai" => Some(OPENAI_DEFAULT_MODEL),
        "tts-sidecar" | "openai-compat" => Some(SIDECAR_DEFAULT_MODEL),
        "deepgram" => Some(deepgram_aura::DEFAULT_MODEL),
        _ => None,
    }
}

/// The voice `backend` uses while `[tts] voice` is unset; `None` for
/// deepgram, whose voice is part of the model id.
pub fn default_voice(backend: &str) -> Option<&'static str> {
    match backend {
        "groq" => Some(GROQ_DEFAULT_VOICE),
        "openai" => Some(OPENAI_DEFAULT_VOICE),
        "tts-sidecar" | "openai-compat" => Some(SIDECAR_DEFAULT_VOICE),
        _ => None,
    }
}

/// Trait for text-to-speech backends.
///
/// Each backend takes input text and returns the synthesized speech as a
/// [`TtsAudioStream`] of WAV bytes, so playback
/// ([`crate::audio::playback::play_wav_stream`]) can start on the first chunk
/// instead of waiting for the whole clip.
#[async_trait]
pub trait TtsBackend: Send + Sync {
    /// Start synthesizing `text` into speech.
    ///
    /// Returns once the response headers arrive. Request failures, non-2xx
    /// statuses and empty input are reported here, before any audio; the
    /// WAV body is then read chunk by chunk from the returned stream.
    async fn synthesize(&self, text: &str) -> Result<Box<dyn TtsAudioStream>, WhisrsError>;
}

/// A synthesized WAV body, read incrementally as the backend produces it.
///
/// Chunks are split at arbitrary byte offsets (not at sample or header
/// boundaries); concatenated, they form the complete WAV file.
#[async_trait]
pub trait TtsAudioStream: Send {
    /// The next chunk of the body, or `Ok(None)` once the body has ended.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, WhisrsError>;
}

/// [`TtsAudioStream`] over an HTTP response body.
pub(crate) struct HttpAudioStream {
    response: reqwest::Response,
    /// Error prefix for a failed body read (e.g. `"TTS"`, `"Deepgram TTS"`).
    label: &'static str,
}

impl HttpAudioStream {
    pub(crate) fn new(response: reqwest::Response, label: &'static str) -> Self {
        Self { response, label }
    }
}

#[async_trait]
impl TtsAudioStream for HttpAudioStream {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, WhisrsError> {
        match self.response.chunk().await {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(WhisrsError::Transcription(format!(
                "{} read body failed: {e}",
                self.label
            ))),
        }
    }
}

/// Build the configured TTS backend.
///
/// `api_key` should already be resolved for the configured backend (see the
/// daemon's `resolve_tts_api_key`). Cloud backends (`groq`, `openai`,
/// `deepgram`) require a key; the `tts-sidecar` backend treats it as optional.
pub fn create_backend(
    config: &TtsConfig,
    api_key: Option<String>,
) -> Result<Box<dyn TtsBackend>, WhisrsError> {
    let api_key = api_key.filter(|k| !k.is_empty());

    // Require a key for the cloud backends; the sidecar can run keyless.
    let require_key = |key: Option<String>| -> Result<String, WhisrsError> {
        key.ok_or_else(|| {
            WhisrsError::Config(
                "TTS is enabled but no API key is configured.\n\
                 Add an api_key to [tts], or configure the backend's key \
                 (e.g. [groq] api_key / WHISRS_GROQ_API_KEY)."
                    .to_string(),
            )
        })
    };

    // Resolve the effective model/voice: the configured value when present and
    // non-blank, otherwise the selected backend's default.
    let model_or = |default: &str| -> String {
        config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    let voice_or = |default: &str| -> String {
        config
            .voice
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };

    match config.backend.as_str() {
        "groq" => Ok(Box::new(openai_compat::OpenAiCompatTts::new(
            groq::GROQ_SPEECH_URL.to_string(),
            Some(require_key(api_key)?),
            model_or(GROQ_DEFAULT_MODEL),
            voice_or(GROQ_DEFAULT_VOICE),
            config.response_format.clone(),
        ))),
        "openai" => Ok(Box::new(openai_compat::OpenAiCompatTts::new(
            OPENAI_SPEECH_URL.to_string(),
            Some(require_key(api_key)?),
            model_or(OPENAI_DEFAULT_MODEL),
            voice_or(OPENAI_DEFAULT_VOICE),
            config.response_format.clone(),
        ))),
        "tts-sidecar" | "openai-compat" => {
            let base_url = config
                .url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_SIDECAR_URL)
                .to_string();
            // `stream: true` asks the sidecar for a chunked body. Pocket TTS
            // OpenAI wrappers default to returning the whole clip at once;
            // Kokoro-FastAPI already streams by default.
            Ok(Box::new(
                openai_compat::OpenAiCompatTts::new(
                    base_url,
                    api_key, // optional — sidecars usually need none
                    model_or(SIDECAR_DEFAULT_MODEL),
                    voice_or(SIDECAR_DEFAULT_VOICE),
                    config.response_format.clone(),
                )
                .with_stream_flag(),
            ))
        }
        // `[tts] voice` is intentionally not passed here: Aura encodes the
        // voice in the model id (e.g. `aura-2-thalia-en`), so the model alone
        // selects the voice.
        "deepgram" => Ok(Box::new(deepgram_aura::DeepgramAuraTts::new(
            require_key(api_key)?,
            model_or(deepgram_aura::DEFAULT_MODEL),
        ))),
        other => Err(WhisrsError::Config(format!(
            "Unknown TTS backend '{other}'. Valid options: groq, openai, tts-sidecar, deepgram"
        ))),
    }
}

/// Hand-rolled one-connection HTTP server pieces for exercising the backends
/// against real sockets (mirrors `serve_once` in the asr-sidecar tests).
#[cfg(test)]
pub(crate) mod test_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Accept one connection and read the whole request. Returns the open
    /// connection (for the test to answer on) and the request body.
    pub(crate) async fn accept_request(listener: &TcpListener) -> (TcpStream, String) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before sending a request head");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let content_length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .expect("a JSON request body carries content-length");
        // Drain the body before replying, or the reply surfaces as a reset.
        while buf.len() < head_end + content_length {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[head_end..]).into_owned();
        (stream, body)
    }

    /// Start a `200 OK` response with a chunked body.
    pub(crate) async fn write_chunked_head(stream: &mut TcpStream) {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: audio/wav\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
    }

    /// Write and flush one body chunk in chunked transfer encoding.
    pub(crate) async fn write_chunk(stream: &mut TcpStream, data: &[u8]) {
        stream
            .write_all(format!("{:x}\r\n", data.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(data).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();
    }

    /// Terminate a chunked body.
    pub(crate) async fn finish_chunked(stream: &mut TcpStream) {
        stream.write_all(b"0\r\n\r\n").await.unwrap();
        stream.flush().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(backend: &str) -> TtsConfig {
        TtsConfig {
            enabled: true,
            backend: backend.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn create_groq_backend_requires_key() {
        assert!(create_backend(&cfg("groq"), Some("k".to_string())).is_ok());
        assert!(create_backend(&cfg("groq"), None).is_err());
    }

    #[test]
    fn create_openai_backend_requires_key() {
        assert!(create_backend(&cfg("openai"), Some("k".to_string())).is_ok());
        assert!(create_backend(&cfg("openai"), None).is_err());
    }

    #[test]
    fn create_deepgram_backend_requires_key() {
        assert!(create_backend(&cfg("deepgram"), Some("k".to_string())).is_ok());
        assert!(create_backend(&cfg("deepgram"), None).is_err());
    }

    #[test]
    fn create_sidecar_backend_allows_no_key() {
        // Sidecar must build with no key; alias must also work.
        assert!(create_backend(&cfg("tts-sidecar"), None).is_ok());
        assert!(create_backend(&cfg("openai-compat"), None).is_ok());
    }

    #[test]
    fn create_unknown_backend_errors() {
        // `Box<dyn TtsBackend>` isn't Debug, so match instead of unwrap_err().
        match create_backend(&cfg("bogus"), Some("k".to_string())) {
            Err(e) => assert!(e.to_string().contains("Unknown TTS backend")),
            Ok(_) => panic!("expected an error for an unknown backend"),
        }
    }

    /// The sidecar branch of `create_backend` must put `"stream": true` on the
    /// wire; checked through a real request so the builder call can't be lost.
    #[tokio::test]
    async fn sidecar_backend_sends_stream_flag() {
        use test_server::*;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, body) = accept_request(&listener).await;
            write_chunked_head(&mut stream).await;
            write_chunk(&mut stream, b"RIFF").await;
            finish_chunked(&mut stream).await;
            body
        });

        let config = TtsConfig {
            url: Some(format!("http://{addr}/v1/audio/speech")),
            ..cfg("tts-sidecar")
        };
        let Ok(backend) = create_backend(&config, None) else {
            panic!("sidecar backend must build");
        };
        let mut audio = match backend.synthesize("hi").await {
            Ok(audio) => audio,
            Err(e) => panic!("expected a 200 response: {e}"),
        };
        while audio.next_chunk().await.unwrap().is_some() {}

        let body: serde_json::Value = serde_json::from_str(&server.await.unwrap()).unwrap();
        assert_eq!(body["stream"], true, "sidecar request body: {body}");
    }
}
