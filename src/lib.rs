//! whisrs — shared types for CLI and daemon communication.

pub mod audio;
pub mod config;
pub mod history;
pub mod hotkey;
pub mod input;
pub mod llm;
pub mod post_processing;
pub mod state;
pub mod transcription;
pub mod tray;
pub mod window;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// IPC protocol
// ---------------------------------------------------------------------------

/// Commands sent from the CLI to the daemon over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    Toggle,
    Cancel,
    Status,
    /// Retrieve recent transcription history.
    Log {
        #[serde(default = "default_log_limit")]
        limit: usize,
    },
    /// Clear all transcription history.
    #[serde(rename = "clear-history")]
    ClearHistory,
    /// Start command mode: copy selection → record voice instruction → LLM rewrite → paste.
    #[serde(rename = "command")]
    CommandMode,
}

fn default_log_limit() -> usize {
    20
}

/// Responses sent from the daemon back to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Response {
    Ok { state: State },
    Error { message: String },
    History { entries: Vec<history::HistoryEntry> },
}

/// Daemon state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Recording,
    Transcribing,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Idle => write!(f, "idle"),
            State::Recording => write!(f, "recording"),
            State::Transcribing => write!(f, "transcribing"),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Top-level configuration deserialized from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub deepgram: Option<DeepgramConfig>,
    #[serde(default)]
    pub groq: Option<GroqConfig>,
    #[serde(default)]
    pub openai: Option<OpenAiConfig>,
    #[serde(default, rename = "local-whisper", alias = "local")]
    pub local_whisper: Option<LocalWhisperConfig>,
    #[serde(default, rename = "local-vosk")]
    pub local_vosk: Option<LocalVoskConfig>,
    #[serde(default, rename = "local-parakeet")]
    pub local_parakeet: Option<LocalParakeetConfig>,
    /// LLM configuration for command mode (text rewriting).
    #[serde(default)]
    pub llm: Option<llm::LlmConfig>,
    /// Global hotkey configuration.
    #[serde(default)]
    pub hotkeys: Option<HotkeyConfig>,
}

/// Global hotkey configuration — key combos that trigger actions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HotkeyConfig {
    /// Hotkey to toggle recording (e.g. "Super+Shift+D").
    pub toggle: Option<String>,
    /// Hotkey to cancel recording (e.g. "Super+Shift+Escape").
    pub cancel: Option<String>,
    /// Hotkey to start command mode (e.g. "Super+Shift+C").
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_silence_timeout")]
    pub silence_timeout_ms: u64,
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Enable automatic filler word removal from transcriptions.
    #[serde(default)]
    pub remove_filler_words: bool,
    /// Custom filler words to remove. When empty, uses the built-in list.
    #[serde(default)]
    pub filler_words: Vec<String>,
    /// Enable audio feedback (tones on start/stop/done).
    #[serde(default)]
    pub audio_feedback: bool,
    /// Volume for audio feedback (0.0 to 1.0).
    #[serde(default = "default_audio_feedback_volume")]
    pub audio_feedback_volume: f32,
    /// Custom vocabulary — domain-specific terms, names, acronyms.
    /// Passed as a prompt hint to transcription backends to improve accuracy.
    #[serde(default)]
    pub vocabulary: Vec<String>,
    /// Enable system tray icon.
    #[serde(default = "default_true")]
    pub tray: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            language: default_language(),
            silence_timeout_ms: default_silence_timeout(),
            notify: true,
            remove_filler_words: false,
            filler_words: Vec::new(),
            audio_feedback: false,
            audio_feedback_volume: default_audio_feedback_volume(),
            vocabulary: Vec::new(),
            tray: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_device")]
    pub device: String,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: default_device(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepgramConfig {
    pub api_key: String,
    #[serde(default = "default_deepgram_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroqConfig {
    pub api_key: String,
    #[serde(default = "default_groq_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key: String,
    #[serde(default = "default_openai_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalWhisperConfig {
    pub model_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalVoskConfig {
    pub model_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalParakeetConfig {
    pub model_path: String,
}

fn default_backend() -> String {
    "groq".to_string()
}
fn default_language() -> String {
    "en".to_string()
}
fn default_silence_timeout() -> u64 {
    2000
}
fn default_true() -> bool {
    true
}
fn default_device() -> String {
    "default".to_string()
}
fn default_audio_feedback_volume() -> f32 {
    0.5
}
fn default_deepgram_model() -> String {
    "nova-3".to_string()
}
fn default_groq_model() -> String {
    "whisper-large-v3-turbo".to_string()
}
fn default_openai_model() -> String {
    "gpt-4o-mini-transcribe".to_string()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the path to the Unix domain socket used for CLI-daemon IPC.
///
/// Prefers `$XDG_RUNTIME_DIR/whisrs.sock`.
/// Falls back to `/tmp/whisrs-<uid>.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(runtime_dir) = dirs::runtime_dir() {
        runtime_dir.join("whisrs.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/whisrs-{uid}.sock"))
    }
}

/// Return the path to the configuration file.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("whisrs")
        .join("config.toml")
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

/// A warning about a configuration issue (non-fatal).
#[derive(Debug, Clone)]
pub struct ConfigWarning {
    pub message: String,
}

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Config {
    /// Validate the configuration and return a list of warnings.
    ///
    /// Returns `Err` for fatal issues (e.g., no backend configured).
    /// Returns `Ok(warnings)` with non-fatal warnings.
    pub fn validate(&self) -> Result<Vec<ConfigWarning>, WhisrsError> {
        let mut warnings = Vec::new();
        let backend = self.general.backend.as_str();

        match backend {
            "deepgram" | "deepgram-streaming" => {
                let has_config_key = self
                    .deepgram
                    .as_ref()
                    .map(|d| !d.api_key.is_empty())
                    .unwrap_or(false);
                let has_env_key = std::env::var("WHISRS_DEEPGRAM_API_KEY")
                    .map(|k| !k.is_empty())
                    .unwrap_or(false);
                if !has_config_key && !has_env_key {
                    return Err(WhisrsError::Config(
                        "Deepgram backend selected but no API key configured.\n\
                         Set WHISRS_DEEPGRAM_API_KEY or add [deepgram] api_key to config.toml.\n\
                         Run 'whisrs setup' to get started."
                            .to_string(),
                    ));
                }
            }
            "groq" => {
                let has_config_key = self
                    .groq
                    .as_ref()
                    .map(|g| !g.api_key.is_empty())
                    .unwrap_or(false);
                let has_env_key = std::env::var("WHISRS_GROQ_API_KEY")
                    .map(|k| !k.is_empty())
                    .unwrap_or(false);
                if !has_config_key && !has_env_key {
                    return Err(WhisrsError::Config(
                        "Groq backend selected but no API key configured.\n\
                         Set WHISRS_GROQ_API_KEY or add [groq] api_key to config.toml.\n\
                         Run 'whisrs setup' to get started."
                            .to_string(),
                    ));
                }
            }
            "openai" | "openai-realtime" => {
                let has_config_key = self
                    .openai
                    .as_ref()
                    .map(|o| !o.api_key.is_empty())
                    .unwrap_or(false);
                let has_env_key = std::env::var("WHISRS_OPENAI_API_KEY")
                    .map(|k| !k.is_empty())
                    .unwrap_or(false);
                if !has_config_key && !has_env_key {
                    return Err(WhisrsError::Config(
                        "OpenAI backend selected but no API key configured.\n\
                         Set WHISRS_OPENAI_API_KEY or add [openai] api_key to config.toml.\n\
                         Run 'whisrs setup' to get started."
                            .to_string(),
                    ));
                }
            }
            "local-whisper" | "local" => {
                let model_path = self
                    .local_whisper
                    .as_ref()
                    .map(|l| l.model_path.clone())
                    .unwrap_or_else(|| {
                        dirs::data_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                            .join("whisrs/models/ggml-base.en.bin")
                            .to_string_lossy()
                            .to_string()
                    });
                if !std::path::Path::new(&model_path).exists() {
                    warnings.push(ConfigWarning {
                        message: format!(
                            "Local whisper backend selected but model file not found: {model_path}\n\
                             Run 'whisrs setup' to download a model."
                        ),
                    });
                }
            }
            "local-vosk" => {
                let model_path = self
                    .local_vosk
                    .as_ref()
                    .map(|l| l.model_path.clone())
                    .unwrap_or_default();
                if model_path.is_empty() || !std::path::Path::new(&model_path).exists() {
                    warnings.push(ConfigWarning {
                        message: "Vosk backend selected but model directory not found.\n\
                             Run 'whisrs setup' to download a model."
                            .to_string(),
                    });
                }
            }
            "local-parakeet" => {
                let model_path = self
                    .local_parakeet
                    .as_ref()
                    .map(|l| l.model_path.clone())
                    .unwrap_or_default();
                if model_path.is_empty() || !std::path::Path::new(&model_path).exists() {
                    warnings.push(ConfigWarning {
                        message: "Parakeet backend selected but model directory not found.\n\
                             Run 'whisrs setup' to download a model."
                            .to_string(),
                    });
                }
            }
            other => {
                return Err(WhisrsError::Config(format!(
                    "Unknown backend '{other}'. Valid options: deepgram, deepgram-streaming, \
                     groq, openai, openai-realtime, local-whisper, local-vosk, local-parakeet"
                )));
            }
        }

        if self.general.silence_timeout_ms == 0 {
            warnings.push(ConfigWarning {
                message: "silence_timeout_ms is 0 — auto-stop is effectively disabled".to_string(),
            });
        }

        Ok(warnings)
    }

    /// Check if any transcription backend has an API key configured.
    pub fn has_any_backend_configured(&self) -> bool {
        let has_deepgram = self
            .deepgram
            .as_ref()
            .map(|d| !d.api_key.is_empty())
            .unwrap_or(false)
            || std::env::var("WHISRS_DEEPGRAM_API_KEY")
                .map(|k| !k.is_empty())
                .unwrap_or(false);

        let has_groq = self
            .groq
            .as_ref()
            .map(|g| !g.api_key.is_empty())
            .unwrap_or(false)
            || std::env::var("WHISRS_GROQ_API_KEY")
                .map(|k| !k.is_empty())
                .unwrap_or(false);

        let has_openai = self
            .openai
            .as_ref()
            .map(|o| !o.api_key.is_empty())
            .unwrap_or(false)
            || std::env::var("WHISRS_OPENAI_API_KEY")
                .map(|k| !k.is_empty())
                .unwrap_or(false);

        let has_local = self.local_whisper.is_some()
            || self.local_vosk.is_some()
            || self.local_parakeet.is_some();

        has_deepgram || has_groq || has_openai || has_local
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WhisrsError {
    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("transcription error: {0}")]
    Transcription(String),

    #[error("invalid state transition from {from} on {action}")]
    InvalidTransition { from: State, action: String },
}

// ---------------------------------------------------------------------------
// IPC wire helpers
// ---------------------------------------------------------------------------

/// Encode a message as a length-prefixed JSON frame (4-byte big-endian length + JSON bytes).
pub fn encode_message<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Read a length-prefixed JSON frame from an async reader.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    anyhow::ensure!(len <= 1024 * 1024, "message too large: {len} bytes");

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_serialization_roundtrip() {
        let cmd = Command::Toggle;
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Command::Toggle));
    }

    #[test]
    fn response_serialization_roundtrip() {
        let resp = Response::Ok {
            state: State::Recording,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Response::Ok {
                state: State::Recording
            }
        ));
    }

    #[test]
    fn command_json_format() {
        let cmd = Command::Toggle;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"toggle"}"#);
    }

    #[test]
    fn response_json_format() {
        let resp = Response::Ok { state: State::Idle };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"ok","state":"idle"}"#);

        let err = Response::Error {
            message: "no microphone found".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(
            json,
            r#"{"status":"error","message":"no microphone found"}"#
        );
    }

    #[test]
    fn state_display() {
        assert_eq!(State::Idle.to_string(), "idle");
        assert_eq!(State::Recording.to_string(), "recording");
        assert_eq!(State::Transcribing.to_string(), "transcribing");
    }

    #[test]
    fn socket_path_is_not_empty() {
        let path = socket_path();
        assert!(!path.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn encode_decode_roundtrip() {
        let cmd = Command::Status;
        let encoded = encode_message(&cmd).unwrap();

        let mut cursor = std::io::Cursor::new(encoded);
        let decoded: Command = read_message(&mut cursor).await.unwrap();
        assert!(matches!(decoded, Command::Status));
    }

    #[tokio::test]
    async fn ipc_client_server_roundtrip() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::{UnixListener, UnixStream};

        // Create a temporary socket path.
        let dir = std::env::temp_dir().join(format!("whisrs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");

        // Clean up any leftover socket.
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a server task that echoes back a response.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();

            let cmd: Command = read_message(&mut reader).await.unwrap();
            assert!(matches!(cmd, Command::Toggle));

            let response = Response::Ok {
                state: State::Recording,
            };
            let encoded = encode_message(&response).unwrap();
            writer.write_all(&encoded).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        // Client side: connect, send command, read response.
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        let cmd = Command::Toggle;
        let encoded = encode_message(&cmd).unwrap();
        writer.write_all(&encoded).await.unwrap();
        writer.shutdown().await.unwrap();

        let response: Response = read_message(&mut reader).await.unwrap();
        assert!(matches!(
            response,
            Response::Ok {
                state: State::Recording
            }
        ));

        server.await.unwrap();

        // Cleanup.
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn config_validate_unknown_backend() {
        let config = Config {
            general: GeneralConfig {
                backend: "nonexistent".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            llm: None,
            hotkeys: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Unknown backend"));
    }

    #[test]
    fn config_validate_groq_no_key() {
        // Clear env var in case it's set.
        std::env::remove_var("WHISRS_GROQ_API_KEY");
        let config = Config {
            general: GeneralConfig {
                backend: "groq".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            llm: None,
            hotkeys: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("no API key"));
    }

    #[test]
    fn config_validate_groq_with_key() {
        let config = Config {
            general: GeneralConfig {
                backend: "groq".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            deepgram: None,
            groq: Some(GroqConfig {
                api_key: "test-key".to_string(),
                model: "whisper-large-v3-turbo".to_string(),
            }),
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            llm: None,
            hotkeys: None,
        };
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn config_validate_zero_silence_timeout() {
        let config = Config {
            general: GeneralConfig {
                backend: "groq".to_string(),
                silence_timeout_ms: 0,
                ..Default::default()
            },
            audio: Default::default(),
            deepgram: None,
            groq: Some(GroqConfig {
                api_key: "test-key".to_string(),
                model: "whisper-large-v3-turbo".to_string(),
            }),
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            llm: None,
            hotkeys: None,
        };
        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("silence_timeout_ms")));
    }

    #[tokio::test]
    async fn ipc_error_response_roundtrip() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::{UnixListener, UnixStream};

        let dir = std::env::temp_dir().join(format!("whisrs-test-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();

            let _cmd: Command = read_message(&mut reader).await.unwrap();

            let response = Response::Error {
                message: "test error".to_string(),
            };
            let encoded = encode_message(&response).unwrap();
            writer.write_all(&encoded).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        let encoded = encode_message(&Command::Cancel).unwrap();
        writer.write_all(&encoded).await.unwrap();
        writer.shutdown().await.unwrap();

        let response: Response = read_message(&mut reader).await.unwrap();
        match response {
            Response::Error { message } => assert_eq!(message, "test error"),
            _ => panic!("expected error response"),
        }

        server.await.unwrap();

        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
