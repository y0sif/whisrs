//! OpenAI-compatible text-to-speech backend.
//!
//! Speaks the OpenAI `/v1/audio/speech` request shape (`{model, voice, input,
//! response_format}`) and returns the WAV body as a chunk stream; a raw PCM
//! body (`response_format = "pcm"`, as OpenRouter sends) gets a WAV header put
//! in front of it. Groq, OpenAI, OpenRouter and local servers (Kokoro,
//! Supertonic, etc.) all expose this interface, so a single backend covers
//! them by varying the base URL and whether an API key is sent.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::WhisrsError;

use super::{HttpAudioStream, TtsAudioStream, TtsBackend};

/// OpenAI-compatible text-to-speech backend.
pub struct OpenAiCompatTts {
    client: reqwest::Client,
    /// Full speech endpoint URL (e.g. `https://api.openai.com/v1/audio/speech`).
    base_url: String,
    /// Optional API key. When `None`, no `Authorization` header is sent —
    /// local sidecars usually need no auth.
    api_key: Option<String>,
    model: String,
    voice: String,
    response_format: String,
    /// Send `"stream": true` in the request body. Only local sidecars get it:
    /// OpenAI and Groq define no such field.
    stream: bool,
}

impl OpenAiCompatTts {
    /// Create a new OpenAI-compatible TTS backend.
    pub fn new(
        base_url: String,
        api_key: Option<String>,
        model: String,
        voice: String,
        response_format: String,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            api_key: api_key.filter(|k| !k.is_empty()),
            model,
            voice,
            response_format,
            stream: false,
        }
    }

    /// Ask the server for a streamed (chunked) body with `"stream": true`.
    ///
    /// For local OpenAI-compatible sidecars only: some (Pocket TTS wrappers)
    /// default to buffering the whole clip, which delays playback until the
    /// entire text is synthesized.
    pub fn with_stream_flag(mut self) -> Self {
        self.stream = true;
        self
    }

    /// Send the requests to `base_url` instead: a test's local server in
    /// place of the host the backend was built for.
    #[cfg(test)]
    pub(super) fn posting_to(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// The JSON request body for `text`, as it goes on the wire.
    #[cfg(test)]
    pub(super) fn wire_body(&self, text: &str) -> serde_json::Value {
        serde_json::to_value(self.request_body(text)).unwrap()
    }

    /// Build the JSON request body for the given input text.
    ///
    /// Exposed for unit testing the wire format without hitting the network.
    fn request_body<'a>(&'a self, text: &'a str) -> SpeechRequest<'a> {
        SpeechRequest {
            model: &self.model,
            voice: &self.voice,
            input: text,
            response_format: &self.response_format,
            stream: self.stream,
        }
    }
}

/// Request body for the OpenAI-compatible `/v1/audio/speech` endpoint.
#[derive(Debug, Serialize)]
struct SpeechRequest<'a> {
    model: &'a str,
    voice: &'a str,
    input: &'a str,
    response_format: &'a str,
    /// Omitted entirely when false, so OpenAI/Groq never see the field.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

/// An OpenAI-shaped error body, as OpenAI and OpenRouter send it.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: String,
}

/// What an error `body` says: the message of `{"error": {"message": "..."}}`,
/// else the body as it is.
fn error_message(body: String) -> String {
    serde_json::from_str::<ErrorResponse>(&body).map_or(body, |e| e.error.message)
}

#[async_trait]
impl TtsBackend for OpenAiCompatTts {
    async fn synthesize(&self, text: &str) -> Result<Box<dyn TtsAudioStream>, WhisrsError> {
        if text.trim().is_empty() {
            return Err(WhisrsError::Transcription(
                "cannot synthesize empty text".to_string(),
            ));
        }

        debug!(
            "sending {} chars to TTS at {} (model={}, voice={}, format={}, stream={})",
            text.len(),
            self.base_url,
            self.model,
            self.voice,
            self.response_format,
            self.stream
        );

        let mut request = self
            .client
            .post(&self.base_url)
            .json(&self.request_body(text));
        if let Some(key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }

        let response = request
            .send()
            .await
            .map_err(|e| WhisrsError::Transcription(format!("TTS request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(WhisrsError::Transcription(format!(
                "TTS error ({}): {}",
                status.as_u16(),
                error_message(body)
            )));
        }

        Ok(Box::new(HttpAudioStream::wrapping_pcm(
            response,
            "TTS",
            &self.response_format,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_serializes_expected_shape() {
        let backend = OpenAiCompatTts::new(
            "https://api.openai.com/v1/audio/speech".to_string(),
            Some("test-key".to_string()),
            "tts-1".to_string(),
            "alloy".to_string(),
            "wav".to_string(),
        );
        let json = serde_json::to_value(backend.request_body("hello world")).unwrap();
        assert_eq!(json["model"], "tts-1");
        assert_eq!(json["voice"], "alloy");
        assert_eq!(json["input"], "hello world");
        assert_eq!(json["response_format"], "wav");
    }

    #[test]
    fn stream_flag_is_omitted_unless_requested() {
        // OpenAI and Groq define no `stream` field: it must not be sent at all.
        let plain = OpenAiCompatTts::new(
            "https://api.openai.com/v1/audio/speech".to_string(),
            Some("test-key".to_string()),
            "tts-1".to_string(),
            "alloy".to_string(),
            "wav".to_string(),
        );
        let json = serde_json::to_value(plain.request_body("hi")).unwrap();
        assert!(
            json.get("stream").is_none(),
            "unexpected stream field: {json}"
        );

        let sidecar = OpenAiCompatTts::new(
            "http://127.0.0.1:8880/v1/audio/speech".to_string(),
            None,
            "kokoro".to_string(),
            "af_heart".to_string(),
            "wav".to_string(),
        )
        .with_stream_flag();
        let json = serde_json::to_value(sidecar.request_body("hi")).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["response_format"], "wav");
    }

    #[test]
    fn empty_api_key_is_normalized_to_none() {
        // Sidecars are configured without a key; an empty string must not
        // become a bogus "Authorization: Bearer " header.
        let backend = OpenAiCompatTts::new(
            "http://127.0.0.1:8880/v1/audio/speech".to_string(),
            Some(String::new()),
            "kokoro".to_string(),
            "af_heart".to_string(),
            "wav".to_string(),
        );
        assert!(backend.api_key.is_none());
    }

    #[tokio::test]
    async fn synthesize_rejects_empty_text() {
        let backend = OpenAiCompatTts::new(
            "https://api.groq.com/openai/v1/audio/speech".to_string(),
            Some("test-key".to_string()),
            "canopylabs/orpheus-v1-english".to_string(),
            "autumn".to_string(),
            "wav".to_string(),
        );
        let err = backend
            .synthesize("   ")
            .await
            .err()
            .expect("empty text must fail");
        assert!(err.to_string().contains("empty text"));
    }

    fn local_backend(addr: std::net::SocketAddr) -> OpenAiCompatTts {
        OpenAiCompatTts::new(
            format!("http://{addr}/v1/audio/speech"),
            None,
            "kokoro".to_string(),
            "af_heart".to_string(),
            "wav".to_string(),
        )
        .with_stream_flag()
    }

    /// The body is handed over chunk by chunk as it arrives: the server
    /// withholds everything after its first chunk until the test has
    /// received that chunk, so a backend that buffered the whole body would
    /// deadlock (and hit the timeout) instead of passing.
    #[tokio::test]
    async fn synthesize_yields_chunks_before_body_ends() {
        use crate::tts::test_server::*;
        use std::time::Duration;
        use tokio::time::timeout;

        const LIMIT: Duration = Duration::from_secs(5);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut stream, _body) = accept_request(&listener).await;
            write_chunked_head(&mut stream).await;
            write_chunk(&mut stream, b"RIFF-first").await;
            release_rx
                .await
                .expect("test released the rest of the body");
            for part in [&b"-second"[..], b"-third", b"-fourth"] {
                tokio::time::sleep(Duration::from_millis(10)).await;
                write_chunk(&mut stream, part).await;
            }
            finish_chunked(&mut stream).await;
        });

        let backend = local_backend(addr);
        let mut audio = match timeout(LIMIT, backend.synthesize("hello"))
            .await
            .expect("headers arrive before the body ends")
        {
            Ok(audio) => audio,
            Err(e) => panic!("expected a 200 response: {e}"),
        };

        let first = timeout(LIMIT, audio.next_chunk())
            .await
            .expect("first chunk arrives before the body ends")
            .unwrap()
            .expect("body has a first chunk");
        assert_eq!(first, b"RIFF-first");
        release_tx.send(()).unwrap();

        let mut all = first;
        while let Some(chunk) = timeout(LIMIT, audio.next_chunk()).await.unwrap().unwrap() {
            all.extend_from_slice(&chunk);
        }
        assert_eq!(all, b"RIFF-first-second-third-fourth");
        // Ended streams stay ended.
        assert!(audio.next_chunk().await.unwrap().is_none());
        server.await.unwrap();
    }

    /// A non-2xx status is reported by `synthesize` itself, before any audio,
    /// with the server's body in the message.
    #[tokio::test]
    async fn synthesize_returns_http_error_before_audio() {
        use crate::tts::test_server::accept_request;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _body) = accept_request(&listener).await;
            let body = "model not loaded";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });

        let err = local_backend(addr)
            .synthesize("hello")
            .await
            .err()
            .expect("503 must fail synthesize");
        assert_eq!(
            err.to_string(),
            "transcription error: TTS error (503): model not loaded"
        );
        server.await.unwrap();
    }

    /// The message of an OpenAI-shaped error body, as OpenRouter sends it;
    /// any other body is shown as it is.
    #[test]
    fn error_message_reads_the_nested_message() {
        let openrouter = r#"{"error":{"message":"Invalid option: expected one of \"mp3\"|\"pcm\"","code":400},"user_id":"u"}"#;
        assert_eq!(
            error_message(openrouter.to_string()),
            r#"Invalid option: expected one of "mp3"|"pcm""#
        );
        for body in [
            r#"{"error":"model not loaded"}"#,
            r#"{"error":{"code":400}}"#,
            "model not loaded",
            "",
        ] {
            assert_eq!(error_message(body.to_string()), body);
        }
    }

    /// The error `synthesize` fails with carries OpenRouter's message.
    #[tokio::test]
    async fn synthesize_names_an_openrouter_error_message() {
        use crate::tts::test_server::accept_request;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _body) = accept_request(&listener).await;
            let body = r#"{"error":{"message":"Invalid option: expected one of \"mp3\"|\"pcm\"","code":400},"user_id":"u"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });

        let err = local_backend(addr)
            .synthesize("hello")
            .await
            .err()
            .expect("400 must fail synthesize");
        assert_eq!(
            err.to_string(),
            r#"transcription error: TTS error (400): Invalid option: expected one of "mp3"|"pcm""#
        );
        server.await.unwrap();
    }

    /// Serve one `content_type` answer of `body` (in 3-byte chunks) to a
    /// backend asking for `format`, and return what the stream hands over.
    async fn answer(format: &str, content_type: &'static str, body: Vec<u8>) -> Vec<u8> {
        use crate::tts::test_server::*;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _body) = accept_request(&listener).await;
            write_chunked_head_as(&mut stream, content_type).await;
            for chunk in body.chunks(3) {
                write_chunk(&mut stream, chunk).await;
            }
            finish_chunked(&mut stream).await;
        });
        let backend = OpenAiCompatTts::new(
            format!("http://{addr}/v1/audio/speech"),
            None,
            "kokoro".to_string(),
            "af_heart".to_string(),
            format.to_string(),
        );
        let mut audio = match backend.synthesize("hello").await {
            Ok(audio) => audio,
            Err(e) => panic!("expected a 200 response: {e}"),
        };
        let mut out = Vec::new();
        while let Some(chunk) = audio.next_chunk().await.unwrap() {
            out.extend_from_slice(&chunk);
        }
        server.await.unwrap();
        out
    }

    /// An `audio/pcm` answer is wrapped even when `wav` was asked for; a
    /// RIFF body is handed over untouched, whatever was asked or sent; and
    /// a WAV answer to a WAV request is as it always was.
    #[tokio::test]
    async fn raw_pcm_is_wrapped_and_wav_passes_through() {
        let pcm: Vec<u8> = (0..400u16).flat_map(|i| (i * 97).to_le_bytes()).collect();

        let out = answer("wav", "audio/pcm;rate=44100;channels=1", pcm.clone()).await;
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[44..], &pcm[..]);
        let decoded = crate::audio::playback::decode_wav(&out).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels), (44_100, 1));
        assert_eq!(decoded.samples.len(), 400);

        // Asked for pcm, sent with no rate: 24 kHz mono.
        let out = answer("pcm", "application/octet-stream", pcm.clone()).await;
        let decoded = crate::audio::playback::decode_wav(&out).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels), (24_000, 1));

        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&pcm);
        assert_eq!(answer("pcm", "audio/pcm", wav.clone()).await, wav);
        assert_eq!(answer("wav", "audio/wav", wav.clone()).await, wav);
        // Not RIFF, but nothing said it is PCM: as it is.
        assert_eq!(answer("wav", "audio/wav", pcm.clone()).await, pcm);
    }
}
