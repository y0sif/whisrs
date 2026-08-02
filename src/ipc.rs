//! IPC protocol: CLI↔daemon commands, responses, socket path, and wire framing.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::history;

// ---------------------------------------------------------------------------
// IPC protocol
// ---------------------------------------------------------------------------

/// Commands sent from the CLI to the daemon over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    /// Toggle recording. `language` overrides `general.language` for this
    /// session only; `None` uses the configured default.
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
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
    /// Start command mode: copy selection → record voice instruction → LLM rewrite → inject.
    #[serde(rename = "command")]
    CommandMode,
    /// Toggle a named custom LLM command (see `[[llm_commands]]`): dictate →
    /// LLM applies the configured instruction to the transcribed text →
    /// types the result at the cursor. A second press on the same command
    /// (or any other) stops recording, same as `toggle`.
    #[serde(rename = "llm-command")]
    LlmCommand {
        name: String,
    },
    /// Reprogram a named LLM command (see `[[llm_commands]]` `set_hotkey`):
    /// the currently selected text becomes the command's new instruction,
    /// saved to config and applied live. No recording, LLM call, or typing.
    #[serde(rename = "set-llm-instruction")]
    SetLlmInstruction {
        name: String,
    },
    /// Read the selected text aloud via TTS. A repeat `Speak` (or `Cancel`)
    /// stops any in-progress playback.
    #[serde(alias = "read")]
    Speak,
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
    /// Read-aloud: synthesizing speech from the selection (no audio yet).
    Synthesizing,
    /// Read-aloud: playing the synthesized speech.
    Speaking,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Idle => write!(f, "idle"),
            State::Recording => write!(f, "recording"),
            State::Transcribing => write!(f, "transcribing"),
            State::Synthesizing => write!(f, "synthesizing"),
            State::Speaking => write!(f, "speaking"),
        }
    }
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

// ---------------------------------------------------------------------------
// Language override validation
// ---------------------------------------------------------------------------

/// Validate and normalize a per-session language override (`toggle -l`).
///
/// Accepts, case-insensitively:
/// - `auto` — let the backend detect the language
/// - `multi` — Deepgram's multilingual mode (what `auto` maps to there)
/// - BCP-47-style tags whose primary subtag is a 2–3 letter ISO 639 code,
///   with optional region/script subtags: `en`, `en-US`, `pt-BR`, `zh-Hans`
///
/// The rule is deliberately structural rather than a whitelist: backends
/// accept different sets (Deepgram takes region tags and `multi`;
/// whisper.cpp takes ISO 639-1 plus a few three-letter codes like `yue`),
/// so only clearly-invalid input (empty, `english`, `123`) is rejected and
/// the backend interprets the rest. This validates the `-l` override only
/// — `general.language` from the config file is not validated here.
///
/// Returns the normalized tag: primary subtag lowercased (whisper.cpp
/// requires lowercase codes), two-letter region subtags uppercased,
/// `_` separators replaced with `-`.
pub fn validate_language_override(lang: &str) -> Result<String, String> {
    let trimmed = lang.trim();
    let invalid = || {
        format!(
            "invalid language '{trimmed}': use an ISO 639 code like 'en', \
             optionally with a region ('en-US'), or 'auto'"
        )
    };

    if trimmed.eq_ignore_ascii_case("auto") || trimmed.eq_ignore_ascii_case("multi") {
        return Ok(trimmed.to_ascii_lowercase());
    }

    let mut subtags = trimmed.split(['-', '_']);
    let primary = subtags.next().unwrap_or("");
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(invalid());
    }

    let mut normalized = primary.to_ascii_lowercase();
    for subtag in subtags {
        if !(2..=8).contains(&subtag.len()) || !subtag.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid());
        }
        normalized.push('-');
        if subtag.len() == 2 {
            normalized.push_str(&subtag.to_ascii_uppercase());
        } else {
            normalized.push_str(subtag);
        }
    }
    Ok(normalized)
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
        let cmd = Command::Toggle { language: None };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Command::Toggle { language: None }));
    }

    #[test]
    fn toggle_language_serialization_roundtrip() {
        let cmd = Command::Toggle {
            language: Some("pl".to_string()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"toggle","language":"pl"}"#);
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Command::Toggle { language: Some(l) } if l == "pl"));
    }

    #[test]
    fn language_override_accepts_iso_codes() {
        assert_eq!(validate_language_override("en").unwrap(), "en");
        assert_eq!(validate_language_override("PL").unwrap(), "pl");
        assert_eq!(validate_language_override("auto").unwrap(), "auto");
    }

    #[test]
    fn language_override_accepts_region_tags() {
        assert_eq!(validate_language_override("en-US").unwrap(), "en-US");
        assert_eq!(validate_language_override("pt-br").unwrap(), "pt-BR");
        assert_eq!(validate_language_override("en_US").unwrap(), "en-US");
    }

    #[test]
    fn language_override_accepts_backend_specific_codes() {
        // Deepgram's multilingual mode, whisper.cpp's three-letter codes,
        // and script subtags pass through structurally.
        assert_eq!(validate_language_override("multi").unwrap(), "multi");
        assert_eq!(validate_language_override("yue").unwrap(), "yue");
        assert_eq!(validate_language_override("zh-Hans").unwrap(), "zh-Hans");
    }

    #[test]
    fn language_override_rejects_clearly_invalid() {
        assert!(validate_language_override("").is_err());
        assert!(validate_language_override("english").is_err());
        assert!(validate_language_override("123").is_err());
        assert!(validate_language_override("e").is_err());
        assert!(validate_language_override("en-").is_err());
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
        let cmd = Command::Toggle { language: None };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"toggle"}"#);
    }

    #[test]
    fn speak_command_serializes_lowercase() {
        let cmd = Command::Speak;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"speak"}"#);
    }

    #[test]
    fn speak_command_roundtrip() {
        let parsed: Command = serde_json::from_str(r#"{"cmd":"speak"}"#).unwrap();
        assert!(matches!(parsed, Command::Speak));
    }

    #[test]
    fn speak_command_read_alias() {
        let parsed: Command = serde_json::from_str(r#"{"cmd":"read"}"#).unwrap();
        assert!(matches!(parsed, Command::Speak));
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
        assert_eq!(State::Synthesizing.to_string(), "synthesizing");
        assert_eq!(State::Speaking.to_string(), "speaking");
    }

    #[test]
    fn state_serde_wire_format() {
        // serde rename_all = "lowercase" governs the IPC wire form.
        assert_eq!(
            serde_json::to_string(&State::Synthesizing).unwrap(),
            r#""synthesizing""#
        );
        assert_eq!(
            serde_json::to_string(&State::Speaking).unwrap(),
            r#""speaking""#
        );
        let parsed: State = serde_json::from_str(r#""speaking""#).unwrap();
        assert_eq!(parsed, State::Speaking);
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
            assert!(matches!(cmd, Command::Toggle { language: None }));

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

        let cmd = Command::Toggle { language: None };
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

    #[test]
    fn llm_command_serialization_roundtrip() {
        let cmd = Command::LlmCommand {
            name: "translate-de".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"llm-command","name":"translate-de"}"#);
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Command::LlmCommand { name } if name == "translate-de"));
    }

    #[test]
    fn set_llm_instruction_serialization_roundtrip() {
        let cmd = Command::SetLlmInstruction {
            name: "translate-de".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            json,
            r#"{"cmd":"set-llm-instruction","name":"translate-de"}"#
        );
        let parsed: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Command::SetLlmInstruction { name } if name == "translate-de"));
    }
}
