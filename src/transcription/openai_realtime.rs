//! OpenAI Realtime API transcription backend (true streaming via WebSocket).
//!
//! Connects to `wss://api.openai.com/v1/realtime` and streams base64-encoded
//! 24kHz PCM audio. Receives incremental transcription deltas.

use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

use crate::audio::AudioChunk;

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
        std::env::var("WHISRS_OPENAI_API_KEY")
            .map_err(|_| anyhow::anyhow!("no OpenAI API key configured — set WHISRS_OPENAI_API_KEY or add [openai] to config.toml"))
    }
}

// ---------------------------------------------------------------------------
// WebSocket message types (manually defined per task requirements)
// ---------------------------------------------------------------------------

/// Client message: session.update
#[derive(Debug, Serialize)]
struct SessionUpdate {
    #[serde(rename = "type")]
    msg_type: String,
    session: SessionConfig,
}

#[derive(Debug, Serialize)]
struct SessionConfig {
    input_audio_transcription: AudioTranscriptionConfig,
    turn_detection: TurnDetectionConfig,
}

#[derive(Debug, Serialize)]
struct AudioTranscriptionConfig {
    model: String,
}

#[derive(Debug, Serialize)]
struct TurnDetectionConfig {
    #[serde(rename = "type")]
    detection_type: String,
}

/// Client message: input_audio_buffer.append
#[derive(Debug, Serialize)]
struct AudioBufferAppend {
    #[serde(rename = "type")]
    msg_type: String,
    audio: String,
}

/// Server message envelope — we parse the `type` field first, then
/// deserialize the specific variant.
#[derive(Debug, Deserialize)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    /// Transcription delta text.
    #[serde(default)]
    delta: Option<String>,
    /// Completed transcript text.
    #[serde(default)]
    transcript: Option<String>,
    /// Error details.
    #[serde(default)]
    error: Option<ServerError>,
}

#[derive(Debug, Deserialize)]
struct ServerError {
    #[serde(default)]
    message: String,
}

impl SessionUpdate {
    fn new(model: &str) -> Self {
        Self {
            msg_type: "session.update".to_string(),
            session: SessionConfig {
                input_audio_transcription: AudioTranscriptionConfig {
                    model: model.to_string(),
                },
                turn_detection: TurnDetectionConfig {
                    detection_type: "server_vad".to_string(),
                },
            },
        }
    }
}

impl AudioBufferAppend {
    fn new(base64_audio: String) -> Self {
        Self {
            msg_type: "input_audio_buffer.append".to_string(),
            audio: base64_audio,
        }
    }
}

/// Resample 16kHz i16 samples to 24kHz i16 samples using linear interpolation.
fn resample_16k_to_24k(samples: &[i16]) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }

    let ratio = 24_000.0 / 16_000.0; // 1.5
    let output_len = (samples.len() as f64 * ratio).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_pos = i as f64 / ratio;
        let src_idx = src_pos as usize;
        let frac = src_pos - src_idx as f64;

        let sample = if src_idx + 1 < samples.len() {
            let a = samples[src_idx] as f64;
            let b = samples[src_idx + 1] as f64;
            (a + frac * (b - a)) as i16
        } else if src_idx < samples.len() {
            samples[src_idx]
        } else {
            0
        };

        output.push(sample);
    }

    output
}

/// Encode i16 PCM samples to base64.
fn encode_pcm_base64(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

#[async_trait]
impl TranscriptionBackend for OpenAIRealtimeBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        // For non-streaming use, we set up the full WebSocket pipeline with a
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

        // Collect all text pieces.
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
        let api_key = self.resolve_api_key()?;
        let model = &config.model;
        let url = format!("wss://api.openai.com/v1/realtime?model={model}");

        info!("connecting to OpenAI Realtime API: {url}");

        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("OpenAI-Beta", "realtime=v1")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13")
            .header("Host", "api.openai.com")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .body(())?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        info!("connected to OpenAI Realtime API");

        // Send session configuration.
        let session_update = SessionUpdate::new(model);
        let session_json = serde_json::to_string(&session_update)?;
        ws_sink
            .send(tungstenite::Message::Text(session_json.into()))
            .await?;
        debug!("sent session.update");

        // Spawn a task to send audio.
        let send_task = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                // Resample 16kHz to 24kHz for the Realtime API.
                let resampled = resample_16k_to_24k(&chunk);
                let b64 = encode_pcm_base64(&resampled);
                let msg = AudioBufferAppend::new(b64);
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        error!("failed to serialize audio buffer append: {e}");
                        continue;
                    }
                };
                if ws_sink
                    .send(tungstenite::Message::Text(json.into()))
                    .await
                    .is_err()
                {
                    warn!("WebSocket send failed — connection may be closed");
                    break;
                }
            }

            // Close the WebSocket gracefully.
            ws_sink.send(tungstenite::Message::Close(None)).await.ok();
        });

        // Receive transcription events.
        while let Some(msg_result) = ws_source.next().await {
            match msg_result {
                Ok(tungstenite::Message::Text(text)) => {
                    match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(server_msg) => match server_msg.msg_type.as_str() {
                            "conversation.item.input_audio_transcription.delta" => {
                                if let Some(delta) = server_msg.delta {
                                    if !delta.is_empty() {
                                        debug!("realtime delta: {delta}");
                                        text_tx.send(delta).await.ok();
                                    }
                                }
                            }
                            "conversation.item.input_audio_transcription.completed" => {
                                if let Some(transcript) = server_msg.transcript {
                                    debug!("realtime completed: {transcript}");
                                    // The completed event contains the full text;
                                    // deltas already sent it incrementally, so we
                                    // don't re-send here.
                                }
                            }
                            "error" => {
                                let err_msg = server_msg
                                    .error
                                    .map(|e| e.message)
                                    .unwrap_or_else(|| "unknown error".to_string());
                                error!("OpenAI Realtime error: {err_msg}");
                            }
                            "session.created" | "session.updated" => {
                                debug!("session event: {}", server_msg.msg_type);
                            }
                            other => {
                                debug!("unhandled server message type: {other}");
                            }
                        },
                        Err(e) => {
                            debug!("failed to parse server message: {e}");
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) => {
                    info!("WebSocket closed by server");
                    break;
                }
                Err(e) => {
                    warn!("WebSocket receive error: {e}");
                    break;
                }
                _ => {}
            }
        }

        send_task.await.ok();
        info!("OpenAI Realtime stream finished");

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
    fn session_update_serialization() {
        let msg = SessionUpdate::new("gpt-4o-mini-transcribe");
        let json = serde_json::to_value(&msg).unwrap();

        assert_eq!(json["type"], "session.update");
        assert_eq!(
            json["session"]["input_audio_transcription"]["model"],
            "gpt-4o-mini-transcribe"
        );
        assert_eq!(json["session"]["turn_detection"]["type"], "server_vad");
    }

    #[test]
    fn audio_buffer_append_serialization() {
        let msg = AudioBufferAppend::new("AQID".to_string());
        let json = serde_json::to_value(&msg).unwrap();

        assert_eq!(json["type"], "input_audio_buffer.append");
        assert_eq!(json["audio"], "AQID");
    }

    #[test]
    fn parse_delta_message() {
        let json =
            r#"{"type": "conversation.item.input_audio_transcription.delta", "delta": "Hello "}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.delta"
        );
        assert_eq!(msg.delta.as_deref(), Some("Hello "));
    }

    #[test]
    fn parse_completed_message() {
        let json = r#"{"type": "conversation.item.input_audio_transcription.completed", "transcript": "Hello world"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.completed"
        );
        assert_eq!(msg.transcript.as_deref(), Some("Hello world"));
    }

    #[test]
    fn parse_error_message() {
        let json = r#"{"type": "error", "error": {"message": "Invalid API key"}}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "error");
        assert_eq!(msg.error.unwrap().message, "Invalid API key");
    }

    #[test]
    fn parse_session_created() {
        let json = r#"{"type": "session.created"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "session.created");
    }

    #[test]
    fn resample_empty() {
        let result = resample_16k_to_24k(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn resample_ratio() {
        // 16000 samples at 16kHz = 1 second.
        // At 24kHz, 1 second = 24000 samples.
        let input: Vec<i16> = vec![100; 16000];
        let output = resample_16k_to_24k(&input);
        // Allow some rounding tolerance.
        assert!(
            (output.len() as i64 - 24000).abs() <= 2,
            "expected ~24000, got {}",
            output.len()
        );
    }

    #[test]
    fn encode_pcm_base64_roundtrip() {
        let samples: Vec<i16> = vec![1, 2, 3, -1];
        let encoded = encode_pcm_base64(&samples);

        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let decoded_samples: Vec<i16> = decoded_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(decoded_samples, samples);
    }
}
