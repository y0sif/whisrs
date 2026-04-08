//! Deepgram transcription backend (REST + streaming WebSocket).
//!
//! Supports two modes:
//! - **REST**: POST WAV audio to `/v1/listen` (non-streaming, simple).
//! - **Streaming**: WebSocket to `wss://api.deepgram.com/v1/listen` with raw
//!   PCM binary frames and real-time transcription results.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

use crate::audio::AudioChunk;

use super::{TranscriptionBackend, TranscriptionConfig};

/// Deepgram REST API endpoint for pre-recorded audio.
const DEEPGRAM_REST_URL: &str = "https://api.deepgram.com/v1/listen";

/// Deepgram WebSocket endpoint for live streaming.
const DEEPGRAM_WS_URL: &str = "wss://api.deepgram.com/v1/listen";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve the API key from the struct field or `WHISRS_DEEPGRAM_API_KEY`.
fn resolve_api_key(api_key: &str) -> anyhow::Result<String> {
    if !api_key.is_empty() {
        return Ok(api_key.to_string());
    }
    std::env::var("WHISRS_DEEPGRAM_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "no Deepgram API key configured — set WHISRS_DEEPGRAM_API_KEY or add [deepgram] to config.toml"
        )
    })
}

/// Map whisrs language codes to Deepgram's `language` query parameter.
/// "auto" maps to "multi" (Deepgram's auto-detect / code-switching mode).
fn map_language(language: &str) -> &str {
    if language == "auto" {
        "multi"
    } else {
        language
    }
}

/// Build common query parameters for Deepgram requests.
fn build_query_params<'a>(
    config: &'a TranscriptionConfig,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![
        ("model", config.model.as_str()),
        ("language", map_language(&config.language)),
        ("smart_format", "true"),
    ];
    params.extend_from_slice(extra);
    params
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level response from Deepgram's pre-recorded API.
#[derive(Debug, Deserialize)]
struct DeepgramResponse {
    results: DeepgramResults,
}

#[derive(Debug, Deserialize)]
struct DeepgramResults {
    channels: Vec<DeepgramChannel>,
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    transcript: String,
}

/// Error response from the Deepgram API.
#[derive(Debug, Deserialize)]
struct DeepgramErrorResponse {
    #[serde(default)]
    err_msg: String,
    #[serde(default)]
    err_code: String,
}

/// A streaming result message from the Deepgram WebSocket.
#[derive(Debug, Deserialize)]
struct StreamingResult {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    channel: Option<StreamingChannel>,
}

#[derive(Debug, Deserialize)]
struct StreamingChannel {
    alternatives: Vec<DeepgramAlternative>,
}

// ===========================================================================
// REST backend (non-streaming)
// ===========================================================================

/// Deepgram REST transcription backend.
///
/// Sends the full WAV file to `/v1/listen` and returns the complete transcript.
pub struct DeepgramRestBackend {
    client: reqwest::Client,
    api_key: String,
}

impl DeepgramRestBackend {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }
}

#[async_trait]
impl TranscriptionBackend for DeepgramRestBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        let api_key = resolve_api_key(&self.api_key)?;

        debug!(
            "sending {} bytes to Deepgram REST API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let params = build_query_params(config, &[]);

        let response = self
            .client
            .post(DEEPGRAM_REST_URL)
            .header("Authorization", format!("Token {api_key}"))
            .header("Content-Type", "audio/wav")
            .query(&params)
            .body(audio.to_vec())
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<DeepgramErrorResponse>(&body) {
                match status.as_u16() {
                    401 | 403 => {
                        anyhow::bail!("Deepgram API: invalid API key — {}", err_resp.err_msg)
                    }
                    429 => {
                        anyhow::bail!("Deepgram API: rate limited — {}", err_resp.err_msg)
                    }
                    _ => anyhow::bail!(
                        "Deepgram API error ({} {}): {}",
                        status.as_u16(),
                        err_resp.err_code,
                        err_resp.err_msg
                    ),
                }
            }
            anyhow::bail!("Deepgram API error ({}): {}", status.as_u16(), body);
        }

        let parsed: DeepgramResponse = serde_json::from_str(&body)?;
        let text = parsed
            .results
            .channels
            .first()
            .and_then(|ch| ch.alternatives.first())
            .map(|alt| alt.transcript.trim().to_string())
            .unwrap_or_default();

        if text.is_empty() {
            warn!("Deepgram returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe) since this
    // backend does not support streaming.
}

// ===========================================================================
// Streaming backend (WebSocket)
// ===========================================================================

/// Deepgram streaming transcription backend.
///
/// Opens a WebSocket to Deepgram, sends raw PCM audio as binary frames,
/// and receives incremental transcription results. Only emits `is_final`
/// results to avoid duplicates.
pub struct DeepgramStreamingBackend {
    api_key: String,
}

impl DeepgramStreamingBackend {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[async_trait]
impl TranscriptionBackend for DeepgramStreamingBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        // For non-streaming use, set up the full WebSocket pipeline with a
        // single audio chunk, then collect the result.
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(16);
        let (text_tx, mut text_rx) = mpsc::channel::<String>(16);

        // Decode WAV to get raw samples.
        let cursor = std::io::Cursor::new(audio);
        let reader = hound::WavReader::new(cursor)?;
        let samples: Vec<i16> = reader.into_samples::<i16>().collect::<Result<_, _>>()?;

        // Send all audio as one chunk, then close.
        audio_tx.send(samples).await.ok();
        drop(audio_tx);

        let config_clone = config.clone();
        let stream_result = self.transcribe_stream(audio_rx, text_tx, &config_clone);

        let collector = async {
            let mut full_text = String::new();
            while let Some(text) = text_rx.recv().await {
                if !full_text.is_empty() {
                    full_text.push(' ');
                }
                full_text.push_str(&text);
            }
            full_text
        };

        let (stream_res, text) = tokio::join!(stream_result, collector);
        stream_res?;

        Ok(text)
    }

    async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let api_key = resolve_api_key(&self.api_key)?;

        let params = build_query_params(
            config,
            &[
                ("encoding", "linear16"),
                ("sample_rate", "16000"),
                ("channels", "1"),
                ("interim_results", "false"),
            ],
        );

        // Build the WebSocket URL with query parameters.
        let query_string: String = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{DEEPGRAM_WS_URL}?{query_string}");

        info!("connecting to Deepgram streaming API");

        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Token {api_key}"))
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13")
            .header("Host", "api.deepgram.com")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .body(())?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        info!("connected to Deepgram streaming API");

        // Spawn a task to send audio as raw PCM binary frames.
        let send_task = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                // Convert i16 samples to little-endian bytes.
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();

                if ws_sink
                    .send(tungstenite::Message::Binary(bytes.into()))
                    .await
                    .is_err()
                {
                    error!("Deepgram WebSocket send failed — connection may be closed");
                    break;
                }
            }

            // Signal end of audio stream.
            debug!("sending CloseStream to Deepgram");
            let close_msg = r#"{"type":"CloseStream"}"#;
            ws_sink
                .send(tungstenite::Message::Text(close_msg.into()))
                .await
                .ok();
        });

        // Receive transcription results.
        let timeout_duration = std::time::Duration::from_secs(15);
        while let Ok(Some(msg_result)) =
            tokio::time::timeout(timeout_duration, ws_source.next()).await
        {
            match msg_result {
                Ok(tungstenite::Message::Text(text)) => {
                    match serde_json::from_str::<StreamingResult>(&text) {
                        Ok(result) => match result.msg_type.as_str() {
                            "Results" => {
                                // Only emit final results to avoid duplicates.
                                if result.is_final {
                                    let transcript = result
                                        .channel
                                        .and_then(|ch| ch.alternatives.into_iter().next())
                                        .map(|alt| alt.transcript.trim().to_string())
                                        .unwrap_or_default();

                                    if !transcript.is_empty() {
                                        debug!("deepgram final: {transcript}");
                                        text_tx.send(transcript).await.ok();
                                    }
                                }
                            }
                            "Metadata" => {
                                debug!("deepgram metadata received");
                            }
                            "SpeechStarted" => {
                                debug!("deepgram speech started");
                            }
                            "UtteranceEnd" => {
                                debug!("deepgram utterance end");
                            }
                            other => {
                                debug!("unhandled Deepgram message type: {other}");
                            }
                        },
                        Err(e) => {
                            debug!("failed to parse Deepgram message: {e}");
                            debug!("raw message: {text}");
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) => {
                    info!("Deepgram WebSocket closed by server");
                    break;
                }
                Err(e) => {
                    error!("Deepgram WebSocket receive error: {e}");
                    break;
                }
                _ => {}
            }
        }

        send_task.await.ok();
        info!("Deepgram stream finished");

        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_language_auto_to_multi() {
        assert_eq!(map_language("auto"), "multi");
    }

    #[test]
    fn map_language_passthrough() {
        assert_eq!(map_language("en"), "en");
        assert_eq!(map_language("fr"), "fr");
        assert_eq!(map_language("ja"), "ja");
    }

    #[test]
    fn parse_rest_response() {
        let body = r#"{
            "metadata": {"request_id": "test"},
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "Hello world.",
                        "confidence": 0.98
                    }]
                }]
            }
        }"#;
        let parsed: DeepgramResponse = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed.results.channels[0].alternatives[0].transcript,
            "Hello world."
        );
    }

    #[test]
    fn parse_rest_response_empty_transcript() {
        let body = r#"{
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "",
                        "confidence": 0.0
                    }]
                }]
            }
        }"#;
        let parsed: DeepgramResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.results.channels[0].alternatives[0]
            .transcript
            .is_empty());
    }

    #[test]
    fn parse_error_response() {
        let body = r#"{"err_msg": "Invalid credentials", "err_code": "INVALID_AUTH"}"#;
        let parsed: DeepgramErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.err_msg, "Invalid credentials");
        assert_eq!(parsed.err_code, "INVALID_AUTH");
    }

    #[test]
    fn parse_streaming_result_final() {
        let body = r#"{
            "type": "Results",
            "channel_index": [0, 1],
            "duration": 1.5,
            "start": 0.0,
            "is_final": true,
            "speech_final": true,
            "channel": {
                "alternatives": [{
                    "transcript": "Hello world.",
                    "confidence": 0.98
                }]
            }
        }"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.msg_type, "Results");
        assert!(parsed.is_final);
        let transcript = &parsed.channel.unwrap().alternatives[0].transcript;
        assert_eq!(transcript, "Hello world.");
    }

    #[test]
    fn parse_streaming_result_interim() {
        let body = r#"{
            "type": "Results",
            "is_final": false,
            "channel": {
                "alternatives": [{"transcript": "Hel", "confidence": 0.5}]
            }
        }"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert!(!parsed.is_final);
    }

    #[test]
    fn parse_streaming_metadata() {
        let body = r#"{"type": "Metadata", "request_id": "abc123"}"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.msg_type, "Metadata");
    }

    #[tokio::test]
    async fn rest_rejects_empty_audio() {
        let backend = DeepgramRestBackend::new("test-key".to_string());
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "nova-3".to_string(),
            prompt: None,
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[test]
    fn build_query_params_includes_smart_format() {
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "nova-3".to_string(),
            prompt: None,
        };
        let params = build_query_params(&config, &[]);
        assert!(params
            .iter()
            .any(|(k, v)| *k == "smart_format" && *v == "true"));
        assert!(params.iter().any(|(k, v)| *k == "model" && *v == "nova-3"));
        assert!(params.iter().any(|(k, v)| *k == "language" && *v == "en"));
    }

    #[test]
    fn build_query_params_auto_language() {
        let config = TranscriptionConfig {
            language: "auto".to_string(),
            model: "nova-3".to_string(),
            prompt: None,
        };
        let params = build_query_params(&config, &[]);
        assert!(params
            .iter()
            .any(|(k, v)| *k == "language" && *v == "multi"));
    }
}
