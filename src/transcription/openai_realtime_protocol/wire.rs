use serde::{Deserialize, Serialize};

use super::{clamp_prompt, TurnDetectionMode};

/// Client message: input_audio_buffer.append
#[derive(Debug, Serialize)]
pub struct AudioBufferAppend {
    #[serde(rename = "type")]
    msg_type: String,
    audio: String,
}

impl AudioBufferAppend {
    pub fn new(base64_audio: String) -> Self {
        Self {
            msg_type: "input_audio_buffer.append".to_string(),
            audio: base64_audio,
        }
    }
}

/// Client message: input_audio_buffer.commit
#[derive(Debug, Serialize)]
pub struct AudioBufferCommit {
    #[serde(rename = "type")]
    msg_type: String,
}

impl AudioBufferCommit {
    pub fn new() -> Self {
        Self {
            msg_type: "input_audio_buffer.commit".to_string(),
        }
    }
}

impl Default for AudioBufferCommit {
    fn default() -> Self {
        Self::new()
    }
}

/// Server message envelope for OpenAI-compatible realtime events.
#[derive(Debug, Deserialize)]
pub struct ServerMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub transcript: Option<String>,
    #[serde(default)]
    pub error: Option<ServerError>,
}

#[derive(Debug, Deserialize)]
pub struct ServerError {
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiSessionUpdate {
    #[serde(rename = "type")]
    msg_type: String,
    session: OpenAiSessionConfig,
}

#[derive(Debug, Serialize)]
struct OpenAiSessionConfig {
    #[serde(rename = "type")]
    session_type: String,
    audio: OpenAiSessionAudioConfig,
}

#[derive(Debug, Serialize)]
struct OpenAiSessionAudioConfig {
    input: OpenAiSessionAudioInputConfig,
}

#[derive(Debug, Serialize)]
struct OpenAiSessionAudioInputConfig {
    format: AudioInputFormatConfig,
    transcription: AudioTranscriptionConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_detection: Option<TurnDetectionConfig>,
}

#[derive(Debug, Serialize)]
struct AudioInputFormatConfig {
    #[serde(rename = "type")]
    format_type: String,
    rate: u32,
}

#[derive(Debug, Serialize)]
struct AudioTranscriptionConfig {
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

#[derive(Debug, Serialize)]
struct TurnDetectionConfig {
    #[serde(rename = "type")]
    detection_type: String,
    threshold: f32,
    prefix_padding_ms: u32,
    silence_duration_ms: u32,
}

impl TurnDetectionConfig {
    fn server_vad_default() -> Self {
        Self {
            detection_type: "server_vad".to_string(),
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
        }
    }
}

impl OpenAiSessionUpdate {
    pub(crate) fn new(
        model: &str,
        language: &str,
        prompt: Option<&str>,
        turn_detection: TurnDetectionMode,
    ) -> Self {
        let lang = if language == "auto" {
            String::new()
        } else {
            language.to_string()
        };

        // Manual-commit models intentionally omit both prompt and turn
        // detection, matching the current OpenAI-specific behavior.
        let prompt = match turn_detection {
            TurnDetectionMode::ServerVad => clamp_prompt(prompt),
            TurnDetectionMode::ManualCommit => None,
        };
        let turn_detection = match turn_detection {
            TurnDetectionMode::ServerVad => Some(TurnDetectionConfig::server_vad_default()),
            TurnDetectionMode::ManualCommit => None,
        };

        Self {
            msg_type: "session.update".to_string(),
            session: OpenAiSessionConfig {
                session_type: "transcription".to_string(),
                audio: OpenAiSessionAudioConfig {
                    input: OpenAiSessionAudioInputConfig {
                        format: AudioInputFormatConfig {
                            format_type: "audio/pcm".to_string(),
                            rate: 24_000,
                        },
                        transcription: AudioTranscriptionConfig {
                            model: model.to_string(),
                            language: lang,
                            prompt,
                        },
                        turn_detection,
                    },
                },
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct LemonadeSessionUpdate {
    #[serde(rename = "type")]
    msg_type: String,
    session: LemonadeSessionConfig,
}

#[derive(Debug, Serialize)]
struct LemonadeSessionConfig {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_detection: Option<LemonadeTurnDetectionConfig>,
}

#[derive(Debug, Serialize)]
struct LemonadeTurnDetectionConfig {
    #[serde(rename = "type")]
    detection_type: String,
}

impl LemonadeSessionUpdate {
    pub(crate) fn new(model: &str, turn_detection: TurnDetectionMode) -> Self {
        // Lemonade exposes a flatter session shape than OpenAI. We keep that
        // divergence here so the engine can stay protocol-agnostic.
        let turn_detection = match turn_detection {
            TurnDetectionMode::ServerVad => Some(LemonadeTurnDetectionConfig {
                detection_type: "server_vad".to_string(),
            }),
            TurnDetectionMode::ManualCommit => None,
        };

        Self {
            msg_type: "session.update".to_string(),
            session: LemonadeSessionConfig {
                model: model.to_string(),
                turn_detection,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_buffer_append_serialization() {
        let msg = AudioBufferAppend::new("AQID".to_string());
        let json = serde_json::to_value(&msg).unwrap();

        assert_eq!(json["type"], "input_audio_buffer.append");
        assert_eq!(json["audio"], "AQID");
    }

    #[test]
    fn audio_buffer_commit_serialization() {
        let msg = AudioBufferCommit::new();
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "input_audio_buffer.commit");
    }

    #[test]
    fn parse_delta_message() {
        let json = r#"{"type": "conversation.item.input_audio_transcription.delta", "item_id": "item_003", "delta": "Hello "}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.delta"
        );
        assert_eq!(msg.item_id.as_deref(), Some("item_003"));
        assert_eq!(msg.delta.as_deref(), Some("Hello "));
    }

    #[test]
    fn parse_completed_message() {
        let json = r#"{"type": "conversation.item.input_audio_transcription.completed", "item_id": "item_003", "transcript": "Hello world"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.completed"
        );
        assert_eq!(msg.item_id.as_deref(), Some("item_003"));
        assert_eq!(msg.transcript.as_deref(), Some("Hello world"));
    }

    #[test]
    fn parse_completed_message_without_item_id() {
        let json = r#"{"type": "conversation.item.input_audio_transcription.completed", "transcript": "Hello world"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.completed"
        );
        assert!(msg.item_id.is_none());
        assert_eq!(msg.transcript.as_deref(), Some("Hello world"));
    }

    #[test]
    fn parse_failed_message() {
        let json = r#"{"type": "conversation.item.input_audio_transcription.failed", "error": {"message": "decoder failed"}}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg.msg_type,
            "conversation.item.input_audio_transcription.failed"
        );
        assert_eq!(msg.error.unwrap().message, "decoder failed");
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
}
