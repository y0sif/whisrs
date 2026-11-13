//! Text-to-speech backends: trait definition and implementations.
//!
//! The synthesis stage of the "read selection aloud" feature lives behind a
//! small [`TtsBackend`] trait (mirroring [`crate::transcription::TranscriptionBackend`]).
//!
//! Cloud and local-server backends are covered today:
//! - `groq` / `openai` / `tts-sidecar` share the OpenAI `/v1/audio/speech`
//!   request shape via [`openai_compat::OpenAiCompatTts`]. The `tts-sidecar`
//!   backend points at any local OpenAI-compatible server (Kokoro, Supertonic,
//!   etc.) and needs no API key. `openai` stops working on 2027-01-06, when
//!   OpenAI shuts down its speech models (#220).
//! - `deepgram` uses Deepgram Aura-2 via [`deepgram_aura::DeepgramAuraTts`].
//!
//! Roadmap: native in-process local backends (Supertonic via ONNX/`ort`,
//! Piper, Kokoro) are future work. Until then, those models are reachable
//! today through a local server behind the `tts-sidecar` backend.

pub mod deepgram_aura;
pub mod groq;
pub mod openai_compat;

use async_trait::async_trait;
use tracing::debug;

use crate::config::catalog;
use crate::{TtsConfig, WhisrsError};

/// Default endpoint for a local OpenAI-compatible TTS sidecar (e.g. Kokoro-FastAPI).
pub const DEFAULT_SIDECAR_URL: &str = "http://127.0.0.1:8880/v1/audio/speech";

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
/// Kokoro's id on OpenRouter, where `kokoro` names no model. Its voices are
/// Kokoro's own, so the sidecar's default voice works there too.
const OPENROUTER_DEFAULT_MODEL: &str = "hexgrad/kokoro-82m";

/// The model `backend` uses while `[tts] model` is unset (the config
/// editor names it). `url` is `[tts] url`, which only `tts-sidecar` reads:
/// on OpenRouter its default is Kokoro's id there.
pub fn default_model(backend: &str, url: Option<&str>) -> Option<&'static str> {
    match backend {
        "groq" => Some(GROQ_DEFAULT_MODEL),
        "openai" => Some(OPENAI_DEFAULT_MODEL),
        "tts-sidecar" | "openai-compat" => Some(sidecar_default_model(url)),
        "deepgram" => Some(deepgram_aura::DEFAULT_MODEL),
        _ => None,
    }
}

/// The `tts-sidecar` model while `[tts] model` is unset, for its `url`.
fn sidecar_default_model(url: Option<&str>) -> &'static str {
    if url.is_some_and(catalog::is_openrouter) {
        OPENROUTER_DEFAULT_MODEL
    } else {
        SIDECAR_DEFAULT_MODEL
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
/// instead of waiting for the whole clip. A raw PCM answer is handed over
/// as WAV too, with a header put in front of it
/// (`HttpAudioStream::wrapping_pcm`).
#[async_trait]
pub trait TtsBackend: Send + Sync {
    /// Start synthesizing `text` into speech.
    ///
    /// Returns once the response headers arrive. Request failures, non-2xx
    /// statuses and empty input are reported here, before any audio; the
    /// body, as WAV, is then read chunk by chunk from the returned stream.
    async fn synthesize(&self, text: &str) -> Result<Box<dyn TtsAudioStream>, WhisrsError>;
}

/// A synthesized WAV body, read incrementally as the backend produces it.
///
/// Chunks are split at arbitrary byte offsets (not at sample or header
/// boundaries); concatenated, they form the complete WAV file. For a raw PCM
/// answer that file starts with the header whisrs put in front of it.
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
    /// Puts a WAV header in front of a raw PCM body; `None` hands the body
    /// over as it is.
    pcm: Option<PcmToWav>,
}

impl HttpAudioStream {
    pub(crate) fn new(response: reqwest::Response, label: &'static str) -> Self {
        Self {
            response,
            label,
            pcm: None,
        }
    }

    /// Like [`Self::new`], for an answer that may be raw PCM
    /// (`reads_as_pcm`). Such a body gets a WAV header put in front of it,
    /// unless it starts with `RIFF` (a WAV after all), so playback, which
    /// reads WAV, plays it.
    pub(crate) fn wrapping_pcm(
        response: reqwest::Response,
        label: &'static str,
        response_format: &str,
    ) -> Self {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let pcm = reads_as_pcm(response_format, content_type).then(|| {
            let (rate, channels) = pcm_format(content_type);
            debug!(
                "TTS answer may be raw PCM (content type {content_type:?}): \
                 read as {rate} Hz, {channels} ch, 16-bit"
            );
            PcmToWav::new(rate, channels)
        });
        Self {
            response,
            label,
            pcm,
        }
    }
}

#[async_trait]
impl TtsAudioStream for HttpAudioStream {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, WhisrsError> {
        loop {
            let chunk = self.response.chunk().await.map_err(|e| {
                WhisrsError::Transcription(format!("{} read body failed: {e}", self.label))
            })?;
            let Some(pcm) = self.pcm.as_mut() else {
                return Ok(chunk.map(|bytes| bytes.to_vec()));
            };
            match chunk {
                Some(bytes) => {
                    // Nothing comes out while the first 4 bytes are still
                    // being gathered: read on.
                    if let Some(out) = pcm.push(&bytes) {
                        return Ok(Some(out));
                    }
                }
                None => return Ok(pcm.finish()),
            }
        }
    }
}

/// The sample rate of a raw PCM answer whose Content-Type does not name one
/// (OpenRouter's Kokoro and ElevenLabs voices send 24 kHz).
const PCM_DEFAULT_RATE: u32 = 24_000;

/// The media type of `content_type`, without its parameters.
fn mime_of(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or_default().trim()
}

/// Whether `content_type` is raw PCM: `audio/pcm` or `audio/L16`, in any
/// case, with or without parameters. `audio/L16` is read little-endian as
/// `audio/pcm` is, as Gemini's voices send it (RFC 2586's is big-endian).
fn is_pcm_type(content_type: &str) -> bool {
    let mime = mime_of(content_type);
    mime.eq_ignore_ascii_case("audio/pcm") || mime.eq_ignore_ascii_case("audio/l16")
}

/// Whether an answer of `content_type` to a request for `response_format`
/// is read as raw PCM: sent as PCM, or asked for as `pcm` and sent with no
/// type, or as `application/octet-stream`, which says nothing. Any other
/// type (`audio/mpeg`, `audio/wav`, `audio/ogg`) is what it says, whatever
/// was asked, and is handed over as it is.
fn reads_as_pcm(response_format: &str, content_type: &str) -> bool {
    let mime = mime_of(content_type);
    let untyped = mime.is_empty() || mime.eq_ignore_ascii_case("application/octet-stream");
    is_pcm_type(content_type) || (response_format.trim().eq_ignore_ascii_case("pcm") && untyped)
}

/// The sample rate and channel count of a raw PCM body: the Content-Type's
/// `rate=` and `channels=` parameters (`audio/pcm;rate=44100;channels=1`),
/// else 24 kHz mono.
fn pcm_format(content_type: &str) -> (u32, u16) {
    let mut rate = PCM_DEFAULT_RATE;
    let mut channels = 1;
    for param in content_type.split(';').skip(1) {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "rate" => {
                if let Some(v) = value.parse::<u32>().ok().filter(|&v| v > 0) {
                    rate = v;
                }
            }
            "channels" => {
                if let Some(v) = value.parse::<u8>().ok().filter(|&v| v > 0) {
                    channels = u16::from(v);
                }
            }
            _ => {}
        }
    }
    (rate, channels)
}

/// Puts a WAV header in front of a raw PCM body (16-bit little-endian
/// samples, no header), chunk by chunk. A body that starts with `RIFF` is a
/// WAV already and is handed over as it is.
struct PcmToWav {
    rate: u32,
    channels: u16,
    /// The body's first bytes, held until 4 are seen to tell `RIFF` from
    /// samples (a first chunk can be shorter).
    held: Vec<u8>,
    /// Whether the start of the body has been handed over.
    started: bool,
}

impl PcmToWav {
    fn new(rate: u32, channels: u16) -> Self {
        Self {
            rate,
            channels,
            held: Vec::new(),
            started: false,
        }
    }

    /// The bytes to hand over for `chunk`: `None` while the start is still
    /// being gathered.
    fn push(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if self.started {
            return Some(chunk.to_vec());
        }
        self.held.extend_from_slice(chunk);
        (self.held.len() >= 4).then(|| self.start())
    }

    /// The bytes still held once the body has ended: a body shorter than 4
    /// bytes. `None` when nothing is left, so an empty body stays empty.
    fn finish(&mut self) -> Option<Vec<u8>> {
        (!self.started && !self.held.is_empty()).then(|| self.start())
    }

    /// Hand over the held start of the body, with a header unless it is a
    /// WAV already.
    fn start(&mut self) -> Vec<u8> {
        self.started = true;
        let held = std::mem::take(&mut self.held);
        if held.starts_with(b"RIFF") {
            return held;
        }
        let mut out = streaming_wav_header(self.rate, self.channels);
        out.extend_from_slice(&held);
        out
    }
}

/// A 44-byte header for 16-bit PCM of unknown length. The RIFF and `data`
/// sizes are the 0xFFFFFFFF sentinel a streamed WAV carries, which playback
/// reads to the end of the body (`audio::playback`).
fn streaming_wav_header(rate: u32, channels: u16) -> Vec<u8> {
    const BITS: u16 = 16;
    const PCM: u16 = 1;
    let block_align = channels * (BITS / 8);
    let byte_rate = rate.saturating_mul(u32::from(block_align));
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&PCM.to_le_bytes());
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&BITS.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header
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
    let model_or = |default: &str| set_or(config.model.as_deref(), default);
    let voice_or = |default: &str| set_or(config.voice.as_deref(), default);

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
        "tts-sidecar" | "openai-compat" => Ok(Box::new(sidecar_backend(config, api_key))),
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

/// The `tts-sidecar` backend: `[tts] url`, else a local server on the
/// default port.
fn sidecar_backend(config: &TtsConfig, api_key: Option<String>) -> openai_compat::OpenAiCompatTts {
    let base_url = set_or(config.url.as_deref(), DEFAULT_SIDECAR_URL);
    // OpenRouter answers `wav` with a 400 (it sends `mp3` or `pcm` only), so
    // it is asked for raw PCM, which gets a WAV header before playback.
    let mut response_format = config.response_format.clone();
    if catalog::is_openrouter(&base_url) && response_format.trim().eq_ignore_ascii_case("wav") {
        debug!("OpenRouter sends no WAV: asking {base_url} for pcm instead");
        response_format = "pcm".to_string();
    }
    let model = set_or(
        config.model.as_deref(),
        sidecar_default_model(Some(&base_url)),
    );
    // `stream: true` asks the sidecar for a chunked body. Pocket TTS OpenAI
    // wrappers default to returning the whole clip at once; Kokoro-FastAPI
    // already streams by default.
    openai_compat::OpenAiCompatTts::new(
        base_url,
        api_key, // optional: sidecars usually need none
        model,
        set_or(config.voice.as_deref(), SIDECAR_DEFAULT_VOICE),
        response_format,
    )
    .with_stream_flag()
}

/// `value` when it is set and not blank, trimmed; else `default`.
fn set_or(value: Option<&str>, default: &str) -> String {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
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

    /// Start a `200 OK` WAV response with a chunked body.
    pub(crate) async fn write_chunked_head(stream: &mut TcpStream) {
        write_chunked_head_as(stream, "audio/wav").await;
    }

    /// Start a `200 OK` response of `content_type` with a chunked body; an
    /// empty `content_type` sends no Content-Type at all.
    pub(crate) async fn write_chunked_head_as(stream: &mut TcpStream, content_type: &str) {
        let typed = if content_type.is_empty() {
            String::new()
        } else {
            format!("content-type: {content_type}\r\n")
        };
        let head = format!("HTTP/1.1 200 OK\r\n{typed}transfer-encoding: chunked\r\n\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
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

    /// On OpenRouter the sidecar's default model is Kokoro's id there; the
    /// URL changes nothing for the other backends.
    #[test]
    fn default_model_follows_an_openrouter_url() {
        let openrouter = Some(catalog::OPENROUTER_TTS_URL);
        assert_eq!(
            default_model("tts-sidecar", openrouter),
            Some("hexgrad/kokoro-82m")
        );
        assert_eq!(
            default_model("openai-compat", openrouter),
            Some("hexgrad/kokoro-82m")
        );
        assert_eq!(default_model("tts-sidecar", None), Some("kokoro"));
        assert_eq!(
            default_model("tts-sidecar", Some(DEFAULT_SIDECAR_URL)),
            Some("kokoro")
        );
        assert_eq!(default_model("groq", openrouter), Some(GROQ_DEFAULT_MODEL));
        assert_eq!(
            default_model("deepgram", openrouter),
            Some(deepgram_aura::DEFAULT_MODEL)
        );
        assert_eq!(default_model("bogus", openrouter), None);
    }

    /// Tiny deterministic PRNG for chunk splits, as in `audio::playback`'s
    /// tests.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    /// Split `bytes` into random-size chunks (1..=max bytes).
    fn random_chunks(bytes: &[u8], seed: u64, max: usize) -> Vec<&[u8]> {
        let mut rng = Lcg(seed);
        let mut out = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let n = (1 + rng.next() as usize % max).min(bytes.len() - off);
            out.push(&bytes[off..off + n]);
            off += n;
        }
        out
    }

    /// Feed `chunks` through a [`PcmToWav`] as [`HttpAudioStream`] does, and
    /// join what comes out.
    fn wrap(rate: u32, channels: u16, chunks: &[&[u8]]) -> Vec<u8> {
        let mut pcm = PcmToWav::new(rate, channels);
        let mut out = Vec::new();
        for chunk in chunks {
            if let Some(bytes) = pcm.push(chunk) {
                out.extend_from_slice(&bytes);
            }
        }
        if let Some(bytes) = pcm.finish() {
            out.extend_from_slice(&bytes);
        }
        assert!(pcm.finish().is_none(), "an ended body stays ended");
        out
    }

    /// `n` 16-bit samples, and the same as little-endian bytes.
    fn pcm_samples(n: usize) -> (Vec<i16>, Vec<u8>) {
        let samples: Vec<i16> = (0..n)
            .map(|i| ((i as i32 * 977) % 65_536 - 32_768) as i16)
            .collect();
        let bytes = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        (samples, bytes)
    }

    /// However the body is split, first chunks of 1 to 3 bytes included,
    /// the header comes once, in front, and the samples follow untouched.
    #[test]
    fn wrapped_pcm_is_the_same_at_any_chunk_split() {
        let (_, body) = pcm_samples(300);
        let mut whole = streaming_wav_header(24_000, 1);
        whole.extend_from_slice(&body);
        assert_eq!(whole.len(), 44 + body.len());
        for seed in 0..100 {
            for max in [1, 2, 3, 5, 64] {
                assert_eq!(
                    wrap(24_000, 1, &random_chunks(&body, seed, max)),
                    whole,
                    "seed {seed}, chunks up to {max} bytes"
                );
            }
        }
        // A body shorter than 4 bytes is wrapped once it ends; an empty
        // one stays empty.
        let mut short = streaming_wav_header(24_000, 1);
        short.extend_from_slice(&[1, 2]);
        assert_eq!(wrap(24_000, 1, &[&[1], &[2]]), short);
        assert!(wrap(24_000, 1, &[]).is_empty());
        assert!(wrap(24_000, 1, &[&[], &[]]).is_empty());
    }

    /// The wrapped body decodes to the same samples at the rate the
    /// Content-Type names.
    #[test]
    fn wrapped_pcm_decodes_at_the_content_type_rate() {
        for (content_type, want_rate) in [
            ("audio/pcm;rate=24000;channels=1", 24_000),
            ("audio/pcm;rate=44100;channels=1", 44_100),
            ("Audio/PCM; Rate=44100; Channels=\"1\"", 44_100),
        ] {
            assert!(is_pcm_type(content_type), "{content_type}");
            let (rate, channels) = pcm_format(content_type);
            assert_eq!((rate, channels), (want_rate, 1), "{content_type}");
            let (samples, body) = pcm_samples(1_000);
            let wav = wrap(rate, channels, &random_chunks(&body, 7, 50));
            let decoded = crate::audio::playback::decode_wav(&wav).unwrap();
            assert_eq!(decoded.sample_rate, want_rate, "{content_type}");
            assert_eq!(decoded.channels, 1);
            let want: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_768.0).collect();
            assert_eq!(decoded.samples, want, "{content_type}");
        }

        // Two channels make two-sample frames.
        let (_, body) = pcm_samples(1_000);
        let wav = wrap(16_000, 2, &[&body]);
        let decoded = crate::audio::playback::decode_wav(&wav).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels), (16_000, 2));
        assert_eq!(decoded.frames(), 500);
    }

    /// A Content-Type that names no rate or channels, or names them wrong,
    /// is 24 kHz mono; only `audio/pcm` and `audio/L16` are raw PCM.
    #[test]
    fn pcm_format_defaults_to_24_khz_mono() {
        for content_type in [
            "audio/pcm",
            "audio/pcm;",
            "",
            "audio/pcm;rate=0;channels=0",
            "audio/pcm;rate=fast;channels=-1",
            "audio/pcm;rate",
            "audio/pcm;channels=300",
        ] {
            assert_eq!(pcm_format(content_type), (24_000, 1), "{content_type:?}");
        }
        assert_eq!(pcm_format("audio/pcm;channels=2"), (24_000, 2));
        assert!(is_pcm_type("AUDIO/PCM"));
        assert!(is_pcm_type(" audio/pcm ;rate=24000"));
        assert!(is_pcm_type("audio/L16;codec=pcm;rate=24000"));
        assert_eq!(pcm_format("audio/L16;codec=pcm;rate=24000"), (24_000, 1));
        for content_type in [
            "audio/wav",
            "audio/mpeg",
            "",
            "audio/pcmx",
            "text/audio/pcm",
        ] {
            assert!(!is_pcm_type(content_type), "{content_type:?}");
        }
    }

    /// A body is read as PCM when its type says so, or when pcm was asked
    /// for and its type says nothing (none, or `application/octet-stream`).
    /// A type that names another format is what it says, whatever was
    /// asked: an MP3 is never given a WAV header.
    #[test]
    fn only_a_pcm_or_untyped_answer_reads_as_pcm() {
        for format in ["pcm", "wav", "mp3", ""] {
            for content_type in ["audio/pcm", "AUDIO/PCM;rate=44100", "audio/l16;rate=24000"] {
                assert!(
                    reads_as_pcm(format, content_type),
                    "{format} {content_type}"
                );
            }
            for content_type in [
                "audio/mpeg",
                "audio/wav",
                "audio/x-wav",
                "audio/ogg; codecs=opus",
                "audio/flac",
                "audio/aac",
                "text/plain",
            ] {
                assert!(
                    !reads_as_pcm(format, content_type),
                    "{format} {content_type}"
                );
            }
        }
        for content_type in [
            "",
            "  ",
            "application/octet-stream",
            "Application/Octet-Stream; x=1",
        ] {
            assert!(reads_as_pcm("pcm", content_type), "{content_type:?}");
            assert!(reads_as_pcm(" PCM ", content_type), "{content_type:?}");
            assert!(!reads_as_pcm("wav", content_type), "{content_type:?}");
            assert!(!reads_as_pcm("mp3", content_type), "{content_type:?}");
        }
    }

    /// A body that starts with `RIFF` is a WAV already: it passes through
    /// untouched, however it is split.
    #[test]
    fn a_riff_body_passes_through_when_pcm_was_asked() {
        let (_, body) = pcm_samples(200);
        let mut wav = streaming_wav_header(22_050, 1);
        wav.extend_from_slice(&body);
        for seed in 0..50 {
            assert_eq!(wrap(24_000, 1, &random_chunks(&wav, seed, 3)), wav);
        }
    }

    /// An OpenRouter sidecar asks for `pcm` (OpenRouter refuses `wav`) and
    /// Kokoro's id there, and its raw answer comes out as a WAV at the rate
    /// the Content-Type names.
    #[tokio::test]
    async fn openrouter_sidecar_asks_for_pcm_and_plays_the_answer() {
        use test_server::*;

        let (samples, pcm) = pcm_samples(600);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, body) = accept_request(&listener).await;
            write_chunked_head_as(&mut stream, "audio/pcm;rate=44100;channels=1").await;
            for chunk in random_chunks(&pcm, 3, 7) {
                write_chunk(&mut stream, chunk).await;
            }
            finish_chunked(&mut stream).await;
            body
        });

        let config = TtsConfig {
            url: Some(catalog::OPENROUTER_TTS_URL.to_string()),
            ..cfg("tts-sidecar")
        };
        let backend = sidecar_backend(&config, Some("k".to_string()))
            .posting_to(format!("http://{addr}/api/v1/audio/speech"));
        let mut audio = match backend.synthesize("hi").await {
            Ok(audio) => audio,
            Err(e) => panic!("expected a 200 response: {e}"),
        };
        let mut wav = Vec::new();
        while let Some(chunk) = audio.next_chunk().await.unwrap() {
            wav.extend_from_slice(&chunk);
        }

        let body: serde_json::Value = serde_json::from_str(&server.await.unwrap()).unwrap();
        assert_eq!(body["response_format"], "pcm", "{body}");
        assert_eq!(body["model"], "hexgrad/kokoro-82m", "{body}");
        assert_eq!(body["voice"], SIDECAR_DEFAULT_VOICE, "{body}");
        assert_eq!(body["stream"], true, "{body}");

        assert_eq!(&wav[..4], b"RIFF");
        let decoded = crate::audio::playback::decode_wav(&wav).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels), (44_100, 1));
        assert_eq!(decoded.samples.len(), samples.len());
    }

    /// Only `wav` is swapped on OpenRouter: another format, a model of the
    /// user's own, and every other host are asked for as configured.
    #[test]
    fn only_wav_on_openrouter_becomes_pcm() {
        let body = |url: &str, model: Option<&str>, format: &str| {
            let config = TtsConfig {
                url: Some(url.to_string()),
                model: model.map(str::to_string),
                response_format: format.to_string(),
                ..cfg("tts-sidecar")
            };
            sidecar_backend(&config, None).wire_body("hi")
        };
        let openrouter = catalog::OPENROUTER_TTS_URL;
        let b = body(openrouter, Some("openai/gpt-4o-mini-tts"), "mp3");
        assert_eq!(b["response_format"], "mp3");
        assert_eq!(b["model"], "openai/gpt-4o-mini-tts");
        assert_eq!(body(openrouter, None, "WAV")["response_format"], "pcm");
        let local = body(DEFAULT_SIDECAR_URL, None, "wav");
        assert_eq!(local["response_format"], "wav");
        assert_eq!(local["model"], "kokoro");
    }
}
