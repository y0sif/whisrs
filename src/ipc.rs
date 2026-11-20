//! IPC protocol: CLI↔daemon commands, responses, socket path, and wire framing.

use serde::{Deserialize, Serialize};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{AgentAction, AgentInfo};
use crate::history;
use crate::recording::{RecordingAction, RecordingOptions, RecordingPhase, RecordingStatus};

// ---------------------------------------------------------------------------
// IPC protocol
// ---------------------------------------------------------------------------

/// Commands sent from the CLI to the daemon over the Unix socket. One is
/// built per request and dropped once handled, so the size of the
/// `Recording` variant does not matter.
#[allow(clippy::large_enum_variant)]
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
    /// What the daemon is and does ([`DaemonInfo`]). Newer than `status`, so
    /// a client falls back to `status` when an older daemon hangs up on it.
    ///
    /// Never waits on a lock, so it is safe to poll: with the dictation lock
    /// held, `state` is the last one published and `busy` is set
    /// ([`DaemonInfo::busy`]).
    Info,
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
    /// Speak the given text via TTS (`whisrs speak --text`). A variant of its
    /// own, not a field on `Speak`: a daemon that predates it must answer
    /// with an error, never read the selection instead. Stops a read-aloud in
    /// progress first.
    #[serde(rename = "speak-text")]
    SpeakText {
        text: String,
    },
    /// Agent mode (`[[agents]]`). `name` is required by every action but
    /// `list`; `text` is the typed instruction of `send`; `new` on `send`
    /// or `toggle` sends the turn to a fresh session (the old one is
    /// forgotten first); `reset` only forgets the session.
    Agent {
        action: AgentAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        new: bool,
    },
    /// A harness hook event, forwarded by `whisrs agent-hook` (the payload is
    /// the hook's stdin JSON, as the harness wrote it).
    #[serde(rename = "agent-event")]
    AgentEvent {
        payload: serde_json::Value,
    },
    /// The recording feature: record mic + system audio, then transcribe.
    /// `title` names a new recording (`start`/`toggle`); `id` selects an
    /// existing one (`retry`/`copy`/`name`). `options` (flat on the wire:
    /// `language`, `mic_language`, `system_language`, `others`,
    /// `shared_mic`, `num_speakers`, `participants`) set the languages and
    /// speaker settings of a new recording or a retry and the label of the
    /// system track; a request without them gets the config defaults.
    /// `name` takes `speaker` and `name` (blank clears it); `summarize`
    /// takes only `id`.
    Recording {
        action: RecordingAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        options: RecordingOptions,
    },
    /// Transcribe audio a failed batch dictation saved for recovery
    /// (`whisrs transcribe-recovery`): `file` is a file in the recovery
    /// folder, by its bare name (no `/`, looked up there) or its absolute
    /// path, `None` the newest one; `language` overrides `general.language`,
    /// as `toggle`'s does. Answered with `Transcript`; the file is removed
    /// once the backend answered.
    #[serde(rename = "transcribe-recovery")]
    TranscribeRecovery {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Switch the config profile (`config::profile`): a profile's name,
    /// `base`, or `None` for the next one. The daemon checks it loads, writes
    /// the active name and restarts on it; refused unless everything is idle.
    #[serde(rename = "switch-profile")]
    SwitchProfile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

fn default_log_limit() -> usize {
    20
}

/// Responses sent from the daemon back to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Response {
    Ok {
        state: State,
    },
    Error {
        message: String,
    },
    History {
        entries: Vec<history::HistoryEntry>,
    },
    /// Recording status after a `recording` command.
    Recording {
        recording: RecordingStatus,
    },
    /// Agent rows after an `agent` command: every agent for `list`, the one
    /// agent for `send`, `reset` and `stop`.
    Agents {
        agents: Vec<AgentInfo>,
    },
    /// The answer to `info`.
    Info {
        info: DaemonInfo,
    },
    /// The answer to `switch-profile`: the profile switched to (`base` for
    /// the base config). `switching` when the daemon restarts on it now,
    /// false when it was already on it. `skipped`: the profiles `next`
    /// passed over on the way, which do not load.
    Profile {
        active: String,
        #[serde(default)]
        switching: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<String>,
    },
    /// The answer to `transcribe-recovery`: the transcript (empty when
    /// nothing was transcribed), the recovery file it came from, whether
    /// the text was copied to the clipboard (never when empty) and whether
    /// the file is gone (`false`: still there, the removal failed).
    Transcript {
        text: String,
        file: String,
        #[serde(default)]
        copied: bool,
        #[serde(default)]
        removed: bool,
    },
}

/// What `info` answers. Every field has a default, so a field added later is
/// additive: an answer without it still parses, and a client that does not
/// know it ignores it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonInfo {
    /// The whisrsd version.
    pub version: String,
    /// The dictation state, as `status` answers it. `None` when a later
    /// daemon names a state this client does not know: the rest of the
    /// answer still parses.
    #[serde(deserialize_with = "known_variant")]
    pub state: Option<State>,
    /// The recording feature's phase: always idle in a build without it.
    /// `None` for a phase this client does not know, as `state`.
    #[serde(deserialize_with = "known_variant")]
    pub recording: Option<RecordingPhase>,
    /// The config profile this whisrsd runs on: `None` is base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Why the active profile did not load, as the line clients show
    /// (`config::profile::not_loaded`): the daemon runs on base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_error: Option<String>,
    /// `[general] backend` as configured.
    pub backend: String,
    /// The cargo features this whisrsd was built with ([`built_features`]).
    pub features: Vec<String>,
    /// When this whisrsd started, in Unix milliseconds: a client that sees
    /// it change knows the daemon restarted.
    pub started_at: u64,
    /// Something `info` does not wait for was mid-change when asked, and
    /// the daemon counts as busy: the dictation lock was held (a dictation
    /// starting, a command running; `state` is then the last one published,
    /// which can lag), or the agent runtime's lock, or a recording start or
    /// stop was under way.
    pub busy: bool,
    /// An agent turn runs, or is on its way to start: a restart stops it.
    pub agent_turn: bool,
}

/// A field of [`DaemonInfo`] that an enum fills: its variant, or `None` for
/// one this client does not know. A later daemon's new state or phase must
/// not fail the whole answer, which `info` exists to keep additive.
fn known_variant<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Every cargo feature of whisrs and whether this build has it, in
/// `Cargo.toml` order.
const FEATURES: [(&str, bool); 9] = [
    ("local-whisper", cfg!(feature = "local-whisper")),
    ("tray", cfg!(feature = "tray")),
    ("overlay", cfg!(feature = "overlay")),
    ("hooks", cfg!(feature = "hooks")),
    ("recording", cfg!(feature = "recording")),
    ("mcp", cfg!(feature = "mcp")),
    ("vulkan", cfg!(feature = "vulkan")),
    ("cuda", cfg!(feature = "cuda")),
    ("hipblas", cfg!(feature = "hipblas")),
];

/// The cargo features this build has, for [`DaemonInfo::features`].
pub fn built_features() -> Vec<String> {
    FEATURES
        .iter()
        .filter(|(_, built)| *built)
        .map(|(name, _)| name.to_string())
        .collect()
}

impl Command {
    /// The command for the daemon's log: the new text-carrying commands are
    /// summarized, so an agent's reply or a hook payload is not logged whole.
    pub fn log_summary(&self) -> String {
        match self {
            Command::SpeakText { text } => {
                format!("SpeakText {{ {} chars }}", text.chars().count())
            }
            Command::Agent {
                action,
                name,
                text,
                new,
            } => format!(
                "Agent {{ action: {action:?}, name: {name:?}, text: {}, new: {new} }}",
                text.as_ref()
                    .map(|t| format!("{} chars", t.chars().count()))
                    .unwrap_or_else(|| "none".to_string())
            ),
            Command::AgentEvent { payload } => format!(
                "AgentEvent {{ {} }}",
                payload
                    .get("hook_event_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            ),
            other => format!("{other:?}"),
        }
    }
}

/// Daemon state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
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

/// The largest frame either side reads; a longer length prefix is refused.
const MAX_MESSAGE_LEN: usize = 1024 * 1024;

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

    anyhow::ensure!(len <= MAX_MESSAGE_LEN, "message too large: {len} bytes");

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

// ---------------------------------------------------------------------------
// Blocking client
// ---------------------------------------------------------------------------

/// Why [`request_blocking`] has no answer.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Nothing answers at the socket: whisrsd is not running.
    #[error("whisrsd is not running ({0})")]
    NotRunning(io::Error),
    /// The daemon hung up without answering. A daemon older than the command
    /// does that with one it cannot parse.
    #[error("whisrsd closed the connection without answering")]
    Closed,
    /// No answer within the timeout.
    #[error("whisrsd did not answer in time")]
    TimedOut,
    /// The daemon answered with an error.
    #[error("{0}")]
    Daemon(String),
    /// The socket failed otherwise, or the answer was not a response.
    #[error("IPC error: {0}")]
    Io(io::Error),
}

impl RequestError {
    fn from_io(e: io::Error) -> Self {
        match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => Self::TimedOut,
            _ => Self::Io(e),
        }
    }
}

/// Send `cmd` to the daemon and wait for its answer on a blocking socket, for
/// a caller without a tokio runtime. Same framing as [`encode_message`] and
/// [`read_message`]. `timeout` bounds each read and write; `None` waits as
/// long as the daemon takes.
///
/// The connect is not bounded: a daemon that stopped accepting, with its
/// listen backlog full, blocks it for good. A caller that must stay
/// responsive (the TUI) calls this from a thread of its own, never from the
/// loop that draws and reads keys.
pub fn request_blocking(
    cmd: &Command,
    timeout: Option<Duration>,
) -> Result<Response, RequestError> {
    request_blocking_at(&socket_path(), cmd, timeout)
}

/// [`request_blocking`] against an explicit socket (testable).
pub fn request_blocking_at(
    path: &Path,
    cmd: &Command,
    timeout: Option<Duration>,
) -> Result<Response, RequestError> {
    use std::io::Write as _;

    let mut stream =
        std::os::unix::net::UnixStream::connect(path).map_err(RequestError::NotRunning)?;
    stream.set_read_timeout(timeout).map_err(RequestError::Io)?;
    stream
        .set_write_timeout(timeout)
        .map_err(RequestError::Io)?;
    let frame = encode_message(cmd).map_err(|e| RequestError::Io(io::Error::other(e)))?;
    stream.write_all(&frame).map_err(RequestError::from_io)?;
    // As the async client does: nothing more is coming.
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(RequestError::from_io)?;
    read_response_blocking(&mut stream)
}

/// Read one response frame. A connection closed before its length prefix is
/// [`RequestError::Closed`].
fn read_response_blocking(reader: &mut impl io::Read) -> Result<Response, RequestError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|e| match e.kind() {
            ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset => RequestError::Closed,
            _ => RequestError::from_io(e),
        })?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(RequestError::Io(io::Error::new(
            ErrorKind::InvalidData,
            format!("message too large: {len} bytes"),
        )));
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(RequestError::from_io)?;
    serde_json::from_slice(&body).map_err(|e| RequestError::Io(e.into()))
}

/// What [`request_info_blocking`] learned about the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum InfoAnswer {
    /// A daemon that knows `info`.
    Info(DaemonInfo),
    /// A daemon older than `info`: it hung up on it and answered `status`,
    /// so only its state is known.
    Older(State),
}

impl InfoAnswer {
    /// The dictation state, which every daemon reports: `None` only for a
    /// state this client does not know ([`DaemonInfo::state`]).
    pub fn state(&self) -> Option<State> {
        match self {
            Self::Info(info) => info.state,
            Self::Older(state) => Some(*state),
        }
    }
}

/// Ask the daemon for its dictation state with `status`, which every daemon
/// knows. The TUI polls with this once a daemon turns out older than `info`:
/// such a daemon logs an error for each `info` it cannot parse.
pub fn request_status_blocking(timeout: Option<Duration>) -> Result<State, RequestError> {
    request_status_blocking_at(&socket_path(), timeout)
}

/// [`request_status_blocking`] against an explicit socket (testable).
pub fn request_status_blocking_at(
    path: &Path,
    timeout: Option<Duration>,
) -> Result<State, RequestError> {
    match request_blocking_at(path, &Command::Status, timeout)? {
        Response::Ok { state } => Ok(state),
        Response::Error { message } => Err(RequestError::Daemon(message)),
        _ => Err(RequestError::Io(io::Error::new(
            ErrorKind::InvalidData,
            "unexpected answer to status",
        ))),
    }
}

/// Ask the daemon for its [`DaemonInfo`]. A daemon older than `info` closes
/// the connection without answering; it is then asked `status`, which every
/// daemon knows.
pub fn request_info_blocking(timeout: Option<Duration>) -> Result<InfoAnswer, RequestError> {
    request_info_blocking_at(&socket_path(), timeout)
}

/// [`request_info_blocking`] against an explicit socket (testable).
pub fn request_info_blocking_at(
    path: &Path,
    timeout: Option<Duration>,
) -> Result<InfoAnswer, RequestError> {
    let response = match request_blocking_at(path, &Command::Info, timeout) {
        Err(RequestError::Closed) => request_blocking_at(path, &Command::Status, timeout)?,
        answer => answer?,
    };
    match response {
        Response::Info { info } => Ok(InfoAnswer::Info(info)),
        Response::Ok { state } => Ok(InfoAnswer::Older(state)),
        Response::Error { message } => Err(RequestError::Daemon(message)),
        _ => Err(RequestError::Io(io::Error::new(
            ErrorKind::InvalidData,
            "unexpected answer to info",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `-l` rule: a 2 to 3 letter ISO 639 code, with optional region or
    /// script subtags, or `auto`; normalized, or refused when clearly
    /// invalid.
    #[test]
    fn language_override_normalizes_or_refuses() {
        for (input, expected) in [
            ("en", Some("en")),
            ("PL", Some("pl")),
            ("auto", Some("auto")),
            ("en-US", Some("en-US")),
            ("pt-br", Some("pt-BR")),
            ("en_US", Some("en-US")),
            // Deepgram's multilingual mode, whisper.cpp's three-letter codes,
            // and script subtags pass through structurally.
            ("multi", Some("multi")),
            ("yue", Some("yue")),
            ("zh-Hans", Some("zh-Hans")),
            ("", None),
            ("english", None),
            ("123", None),
            ("e", None),
            ("en-", None),
        ] {
            assert_eq!(
                validate_language_override(input).ok().as_deref(),
                expected,
                "{input:?}"
            );
        }
    }

    /// A state has one name, on the wire (serde) and on screen (`Display`).
    #[test]
    fn state_names_match_on_the_wire_and_on_screen() {
        for (state, name) in [
            (State::Idle, "idle"),
            (State::Recording, "recording"),
            (State::Transcribing, "transcribing"),
            (State::Synthesizing, "synthesizing"),
            (State::Speaking, "speaking"),
        ] {
            assert_eq!(state.to_string(), name, "{name}: Display");
            let wire = format!("\"{name}\"");
            let json = serde_json::to_string(&state).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(json, wire, "{name}: serialized");
            let parsed: State =
                serde_json::from_str(&wire).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(parsed, state, "{name}: parsed");
        }
    }

    /// Every command's JSON, both ways: the command serializes to exactly
    /// this, and this parses back to the command. Old and new builds must
    /// keep talking, so no name or field here changes.
    #[test]
    fn every_command_keeps_its_wire_shape() {
        let recording =
            |action, title: Option<&str>, id: Option<&str>, options| Command::Recording {
                action,
                title: title.map(str::to_string),
                id: id.map(str::to_string),
                options,
            };
        let agent = |action, name: Option<&str>, text: Option<&str>, new| Command::Agent {
            action,
            name: name.map(str::to_string),
            text: text.map(str::to_string),
            new,
        };
        let standup = Some("2026-10-27_1402_standup");
        // The options travel flat beside `action`, and each one is optional.
        let lesson = RecordingOptions {
            language: Some("de".to_string()),
            system_language: Some("auto".to_string()),
            others: Some("Video".to_string()),
            shared_mic: Some(true),
            num_speakers: Some(3),
            participants: Some(vec!["Sarah".to_string(), "Omar".to_string()]),
            ..Default::default()
        };
        let retry = RecordingOptions {
            mic_language: Some("en".to_string()),
            shared_mic: Some(false),
            num_speakers: Some(0),
            participants: Some(Vec::new()),
            ..Default::default()
        };
        let rename = RecordingOptions {
            speaker: Some("Speaker 2".to_string()),
            name: Some("Sarah".to_string()),
            ..Default::default()
        };
        // `exact` goes on the wire only when set.
        let exact = RecordingOptions {
            exact: true,
            ..Default::default()
        };
        let rows: Vec<(&str, Command, &str)> = vec![
            (
                "toggle",
                Command::Toggle { language: None },
                r#"{"cmd":"toggle"}"#,
            ),
            (
                "toggle -l",
                Command::Toggle {
                    language: Some("pl".to_string()),
                },
                r#"{"cmd":"toggle","language":"pl"}"#,
            ),
            ("cancel", Command::Cancel, r#"{"cmd":"cancel"}"#),
            ("status", Command::Status, r#"{"cmd":"status"}"#),
            ("info", Command::Info, r#"{"cmd":"info"}"#),
            (
                "log",
                Command::Log { limit: 5 },
                r#"{"cmd":"log","limit":5}"#,
            ),
            (
                "clear-history",
                Command::ClearHistory,
                r#"{"cmd":"clear-history"}"#,
            ),
            ("command", Command::CommandMode, r#"{"cmd":"command"}"#),
            ("speak", Command::Speak, r#"{"cmd":"speak"}"#),
            // Its own command: it never parses as `speak`, which a daemon
            // would answer by reading the selection.
            (
                "speak-text",
                Command::SpeakText {
                    text: "hello".to_string(),
                },
                r#"{"cmd":"speak-text","text":"hello"}"#,
            ),
            (
                "llm-command",
                Command::LlmCommand {
                    name: "translate-de".to_string(),
                },
                r#"{"cmd":"llm-command","name":"translate-de"}"#,
            ),
            (
                "set-llm-instruction",
                Command::SetLlmInstruction {
                    name: "translate-de".to_string(),
                },
                r#"{"cmd":"set-llm-instruction","name":"translate-de"}"#,
            ),
            (
                "recording start, title",
                recording(
                    RecordingAction::Start,
                    Some("x"),
                    None,
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"start","title":"x"}"#,
            ),
            (
                "recording toggle",
                recording(
                    RecordingAction::Toggle,
                    None,
                    None,
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"toggle"}"#,
            ),
            (
                "recording retry, id",
                recording(
                    RecordingAction::Retry,
                    None,
                    Some("last"),
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"retry","id":"last"}"#,
            ),
            (
                "recording start, options",
                recording(RecordingAction::Start, Some("Lesson 50"), None, lesson),
                r#"{"cmd":"recording","action":"start","title":"Lesson 50","language":"de","system_language":"auto","others":"Video","shared_mic":true,"num_speakers":3,"participants":["Sarah","Omar"]}"#,
            ),
            (
                "recording retry, options",
                recording(RecordingAction::Retry, None, Some("last"), retry),
                r#"{"cmd":"recording","action":"retry","id":"last","mic_language":"en","shared_mic":false,"num_speakers":0,"participants":[]}"#,
            ),
            // `name` carries the speaker and its new name beside the id.
            (
                "recording name",
                recording(RecordingAction::Name, None, Some("last"), rename),
                r#"{"cmd":"recording","action":"name","id":"last","speaker":"Speaker 2","name":"Sarah"}"#,
            ),
            (
                "recording summarize",
                recording(
                    RecordingAction::Summarize,
                    None,
                    Some("last"),
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"summarize","id":"last"}"#,
            ),
            (
                "recording copy",
                recording(
                    RecordingAction::Copy,
                    None,
                    standup,
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"copy","id":"2026-10-27_1402_standup"}"#,
            ),
            (
                "recording copy, exact",
                recording(RecordingAction::Copy, None, standup, exact),
                r#"{"cmd":"recording","action":"copy","id":"2026-10-27_1402_standup","exact":true}"#,
            ),
            (
                "recording delete",
                recording(
                    RecordingAction::Delete,
                    None,
                    standup,
                    RecordingOptions::default(),
                ),
                r#"{"cmd":"recording","action":"delete","id":"2026-10-27_1402_standup"}"#,
            ),
            (
                "agent list",
                agent(AgentAction::List, None, None, false),
                r#"{"cmd":"agent","action":"list"}"#,
            ),
            (
                "agent send, new",
                agent(
                    AgentAction::Send,
                    Some("jarvis"),
                    Some("run the tests"),
                    true,
                ),
                r#"{"cmd":"agent","action":"send","name":"jarvis","text":"run the tests","new":true}"#,
            ),
            // A fresh-session voice turn.
            (
                "agent toggle, new",
                agent(AgentAction::Toggle, Some("jarvis"), None, true),
                r#"{"cmd":"agent","action":"toggle","name":"jarvis","new":true}"#,
            ),
            (
                "agent toggle",
                agent(AgentAction::Toggle, Some("jarvis"), None, false),
                r#"{"cmd":"agent","action":"toggle","name":"jarvis"}"#,
            ),
            (
                "agent reset",
                agent(AgentAction::Reset, Some("jarvis"), None, false),
                r#"{"cmd":"agent","action":"reset","name":"jarvis"}"#,
            ),
            (
                "agent stop",
                agent(AgentAction::Stop, Some("jarvis"), None, false),
                r#"{"cmd":"agent","action":"stop","name":"jarvis"}"#,
            ),
            // `transcribe-recovery` leaves out what it was not given.
            (
                "transcribe-recovery, newest",
                Command::TranscribeRecovery {
                    file: None,
                    language: None,
                },
                r#"{"cmd":"transcribe-recovery"}"#,
            ),
            (
                "transcribe-recovery, named",
                Command::TranscribeRecovery {
                    file: Some("/abs/path".to_string()),
                    language: Some("de".to_string()),
                },
                r#"{"cmd":"transcribe-recovery","file":"/abs/path","language":"de"}"#,
            ),
            // `switch-profile` names the profile, or leaves it out for the
            // next one.
            (
                "switch-profile, named",
                Command::SwitchProfile {
                    name: Some("offline".to_string()),
                },
                r#"{"cmd":"switch-profile","name":"offline"}"#,
            ),
            (
                "switch-profile, next",
                Command::SwitchProfile { name: None },
                r#"{"cmd":"switch-profile"}"#,
            ),
        ];
        for (row, cmd, json) in rows {
            let written = serde_json::to_string(&cmd).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(written, json, "{row}: serialized");
            let parsed: Command =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(format!("{parsed:?}"), format!("{cmd:?}"), "{row}: parsed");
        }

        // Read, never written: the `read` alias, and a field the options do
        // not know, ignored as an older daemon ignores `exact`.
        for (row, json, cmd) in [
            ("read alias", r#"{"cmd":"read"}"#, Command::Speak),
            (
                "recording, unknown field ignored",
                r#"{"cmd":"recording","action":"copy","id":"x","later":1}"#,
                recording(
                    RecordingAction::Copy,
                    None,
                    Some("x"),
                    RecordingOptions::default(),
                ),
            ),
        ] {
            let parsed: Command =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(format!("{parsed:?}"), format!("{cmd:?}"), "{row}: parsed");
        }

        // A speak-text without its text is malformed, not a read-aloud.
        assert!(
            serde_json::from_str::<Command>(r#"{"cmd":"speak-text"}"#).is_err(),
            "speak-text without its text"
        );
    }

    #[test]
    fn socket_path_is_not_empty() {
        let path = socket_path();
        assert!(!path.as_os_str().is_empty());
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

    #[test]
    fn recording_response_roundtrip() {
        let status = RecordingStatus {
            phase: crate::recording::RecordingPhase::Recording,
            id: Some("2026-10-02_1400_standup".to_string()),
            title: Some("standup".to_string()),
            started_at: Some(chrono::Local::now()),
            dir: Some(PathBuf::from("/tmp/m")),
            queued: vec!["2026-10-01_0900".to_string()],
            waiting: vec!["2026-09-30_1500".to_string()],
            summarizing: true,
        };
        let resp = Response::Recording {
            recording: status.clone(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.starts_with(r#"{"status":"recording","recording":{"phase":"recording""#));
        let parsed: Response = serde_json::from_str(&json).unwrap();
        match parsed {
            Response::Recording { recording } => assert_eq!(recording, status),
            other => panic!("expected recording response, got {other:?}"),
        }
        let idle: Response =
            serde_json::from_str(r#"{"status":"recording","recording":{"phase":"idle"}}"#).unwrap();
        assert!(
            matches!(idle, Response::Recording { recording } if recording == RecordingStatus::default())
        );
    }

    #[test]
    fn agent_event_carries_the_payload_verbatim() {
        let json = r#"{"cmd":"agent-event","payload":{"session_id":"s1","hook_event_name":"Stop","last_assistant_message":"done"}}"#;
        let parsed: Command = serde_json::from_str(json).unwrap();
        let Command::AgentEvent { payload } = &parsed else {
            panic!("not an agent event: {parsed:?}");
        };
        assert_eq!(payload["last_assistant_message"], "done");
        let back: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
        // The log line names the event, not the message.
        assert_eq!(parsed.log_summary(), "AgentEvent { Stop }");
        let speak = Command::SpeakText {
            text: "secret reply".to_string(),
        };
        assert_eq!(speak.log_summary(), "SpeakText { 12 chars }");
    }

    fn sample_info() -> DaemonInfo {
        DaemonInfo {
            version: "0.2.0".to_string(),
            state: Some(State::Recording),
            recording: Some(RecordingPhase::Processing),
            profile: None,
            profile_error: None,
            backend: "deepgram-streaming".to_string(),
            features: vec!["tray".to_string(), "recording".to_string()],
            started_at: 1_793_000_000_000,
            busy: true,
            agent_turn: false,
        }
    }

    /// Every response's JSON, both ways, as for the commands.
    #[test]
    fn every_response_keeps_its_wire_shape() {
        use crate::agent::AgentPhase;
        use chrono::TimeZone;
        // chrono writes a history timestamp in the local zone ("Z" in UTC);
        // the fields around it are the wire shape.
        let at = chrono::Local
            .with_ymd_and_hms(2026, 10, 27, 14, 2, 0)
            .unwrap();
        let history_json = format!(
            r#"{{"status":"history","entries":[{{"timestamp":{},"text":"hello there","backend":"groq","language":"en","duration_secs":2.5}}]}}"#,
            serde_json::to_string(&at).unwrap()
        );
        let rows: Vec<(&str, Response, &str)> = vec![
            (
                "ok idle",
                Response::Ok { state: State::Idle },
                r#"{"status":"ok","state":"idle"}"#,
            ),
            (
                "ok recording",
                Response::Ok {
                    state: State::Recording,
                },
                r#"{"status":"ok","state":"recording"}"#,
            ),
            (
                "error",
                Response::Error {
                    message: "no microphone found".to_string(),
                },
                r#"{"status":"error","message":"no microphone found"}"#,
            ),
            (
                "history",
                Response::History {
                    entries: vec![history::HistoryEntry {
                        timestamp: at,
                        text: "hello there".to_string(),
                        backend: "groq".to_string(),
                        language: "en".to_string(),
                        duration_secs: 2.5,
                    }],
                },
                history_json.as_str(),
            ),
            // The answer to `transcribe-recovery` is a response of its own.
            (
                "transcript",
                Response::Transcript {
                    text: "hello there".to_string(),
                    file: "/c/recovery_a.wav".to_string(),
                    copied: true,
                    removed: false,
                },
                r#"{"status":"transcript","text":"hello there","file":"/c/recovery_a.wav","copied":true,"removed":false}"#,
            ),
            (
                "agents",
                Response::Agents {
                    agents: vec![AgentInfo {
                        name: "jarvis".to_string(),
                        harness: "claude".to_string(),
                        phase: AgentPhase::Working,
                        session: Some("cc7a37cf".to_string()),
                        bg_id: Some("cc7a37cf".to_string()),
                    }],
                },
                r#"{"status":"agents","agents":[{"name":"jarvis","harness":"claude","phase":"working","session":"cc7a37cf","bg_id":"cc7a37cf"}]}"#,
            ),
            // On base, `info` sends no profile, so its answer reads as before.
            (
                "info",
                Response::Info {
                    info: sample_info(),
                },
                r#"{"status":"info","info":{"version":"0.2.0","state":"recording","recording":"processing","backend":"deepgram-streaming","features":["tray","recording"],"started_at":1793000000000,"busy":true,"agent_turn":false}}"#,
            ),
            (
                "info, profile",
                Response::Info {
                    info: DaemonInfo {
                        profile: Some("offline".to_string()),
                        ..sample_info()
                    },
                },
                r#"{"status":"info","info":{"version":"0.2.0","state":"recording","recording":"processing","profile":"offline","backend":"deepgram-streaming","features":["tray","recording"],"started_at":1793000000000,"busy":true,"agent_turn":false}}"#,
            ),
            (
                "profile",
                Response::Profile {
                    active: "offline".to_string(),
                    switching: true,
                    skipped: Vec::new(),
                },
                r#"{"status":"profile","active":"offline","switching":true}"#,
            ),
            (
                "profile, skipped",
                Response::Profile {
                    active: "clean".to_string(),
                    switching: true,
                    skipped: vec!["broken".to_string()],
                },
                r#"{"status":"profile","active":"clean","switching":true,"skipped":["broken"]}"#,
            ),
        ];
        for (row, resp, json) in rows {
            let written = serde_json::to_string(&resp).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(written, json, "{row}: serialized");
            let parsed: Response =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(format!("{parsed:?}"), format!("{resp:?}"), "{row}: parsed");
        }

        // Read with what is left out at its default.
        let why = "profile offline did not load, using base: profiles/offline.toml does not exist";
        for (row, json, resp) in [
            // An answer without the flags claims neither a copy nor a removal.
            (
                "transcript, no flags",
                r#"{"status":"transcript","text":"","file":"/c/recovery_a.wav"}"#.to_string(),
                Response::Transcript {
                    text: String::new(),
                    file: "/c/recovery_a.wav".to_string(),
                    copied: false,
                    removed: false,
                },
            ),
            (
                "profile, defaults",
                r#"{"status":"profile","active":"base"}"#.to_string(),
                Response::Profile {
                    active: "base".to_string(),
                    switching: false,
                    skipped: Vec::new(),
                },
            ),
            // Why the profile did not load, with no profile named.
            (
                "info, profile_error only",
                format!(r#"{{"status":"info","info":{{"profile_error":"{why}"}}}}"#),
                Response::Info {
                    info: DaemonInfo {
                        profile_error: Some(why.to_string()),
                        ..Default::default()
                    },
                },
            ),
        ] {
            let parsed: Response =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(format!("{parsed:?}"), format!("{resp:?}"), "{row}: parsed");
        }
    }

    /// Fields come and go additively: a missing one takes its default, and
    /// one this client does not know (a later daemon's) is ignored.
    #[test]
    fn info_fields_are_additive() {
        let empty: Response = serde_json::from_str(r#"{"status":"info","info":{}}"#).unwrap();
        assert!(matches!(empty, Response::Info { info } if info == DaemonInfo::default()));

        let later: Response =
            serde_json::from_str(r#"{"status":"info","info":{"state":"speaking","uptime_ms":12}}"#)
                .unwrap();
        let Response::Info { info } = later else {
            panic!("not an info response: {later:?}");
        };
        assert_eq!(info.state, Some(State::Speaking));
        assert_eq!(info.recording, None);
    }

    /// A state or phase this client does not know (a later daemon's) is
    /// `None`, and the rest of the answer still parses: `info` stays
    /// additive for its enum fields too.
    #[test]
    fn an_unknown_state_or_phase_is_none() {
        let later: Response = serde_json::from_str(
            r#"{"status":"info","info":{"version":"9.0.0","state":"dancing","recording":"paused","backend":"groq"}}"#,
        )
        .unwrap();
        let Response::Info { info } = later else {
            panic!("not an info response: {later:?}");
        };
        assert_eq!(info.version, "9.0.0");
        assert_eq!(info.state, None);
        assert_eq!(info.recording, None);
        assert_eq!(info.backend, "groq");
        assert_eq!(InfoAnswer::Info(info).state(), None);

        let null: DaemonInfo = serde_json::from_str(r#"{"state":null,"recording":7}"#).unwrap();
        assert_eq!((null.state, null.recording), (None, None));
    }

    /// The feature list names every feature in `Cargo.toml`, so one added
    /// there cannot go missing from `info`.
    #[test]
    fn built_features_cover_every_cargo_feature() {
        let manifest = include_str!("../Cargo.toml");
        let section = manifest
            .split("\n[features]\n")
            .nth(1)
            .expect("Cargo.toml has a [features] section")
            .split("\n[")
            .next()
            .unwrap();
        let declared: Vec<&str> = section
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once(" = ").map(|(name, _)| name))
            .filter(|name| *name != "default")
            .collect();
        let listed: Vec<&str> = FEATURES.iter().map(|(name, _)| *name).collect();
        assert_eq!(listed, declared);
        assert_eq!(
            built_features().contains(&"recording".to_string()),
            cfg!(feature = "recording")
        );
    }

    /// A stand-in daemon on a socket in a temp dir. Each connection reads one
    /// frame and is answered with the next of `answers`, or closed without
    /// an answer for `None`, as a daemon that cannot parse the command does.
    /// The thread returns the `cmd` of every frame it read.
    fn fake_daemon(
        answers: Vec<Option<Response>>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        std::thread::JoinHandle<Vec<String>>,
    ) {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let daemon = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
                let cmd: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen.push(cmd["cmd"].as_str().unwrap_or_default().to_string());
                if let Some(answer) = answer {
                    stream.write_all(&encode_message(&answer).unwrap()).unwrap();
                }
            }
            seen
        });
        (dir, path, daemon)
    }

    const TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

    #[test]
    fn info_round_trips_over_the_socket() {
        let (_dir, path, daemon) = fake_daemon(vec![Some(Response::Info {
            info: sample_info(),
        })]);
        let answer = request_info_blocking_at(&path, TIMEOUT).unwrap();
        assert_eq!(answer, InfoAnswer::Info(sample_info()));
        assert_eq!(answer.state(), Some(State::Recording));
        assert_eq!(daemon.join().unwrap(), vec!["info"]);
    }

    /// A daemon older than `info` hangs up on it; the client then asks
    /// `status` and reports the state alone.
    #[test]
    fn an_older_daemon_is_asked_status_instead() {
        let (_dir, path, daemon) = fake_daemon(vec![
            None,
            Some(Response::Ok {
                state: State::Transcribing,
            }),
        ]);
        let answer = request_info_blocking_at(&path, TIMEOUT).unwrap();
        assert_eq!(answer, InfoAnswer::Older(State::Transcribing));
        assert_eq!(answer.state(), Some(State::Transcribing));
        assert_eq!(daemon.join().unwrap(), vec!["info", "status"]);
    }

    /// `whisrs status` asks `status` alone, never `info`: a daemon older
    /// than `info` would log an error for each one.
    #[test]
    fn status_asks_status_only() {
        let (_dir, path, daemon) = fake_daemon(vec![Some(Response::Ok {
            state: State::Recording,
        })]);
        assert_eq!(
            request_status_blocking_at(&path, TIMEOUT).unwrap(),
            State::Recording
        );
        assert_eq!(daemon.join().unwrap(), vec!["status"]);

        let (_dir, path, daemon) = fake_daemon(vec![Some(Response::Error {
            message: "busy".to_string(),
        })]);
        assert!(matches!(
            request_status_blocking_at(&path, TIMEOUT),
            Err(RequestError::Daemon(message)) if message == "busy"
        ));
        daemon.join().unwrap();
    }

    /// The hang-up is told apart from the other failures, which are what the
    /// fallback must not hide.
    #[test]
    fn request_errors_say_what_happened() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("whisrs.sock");
        assert!(matches!(
            request_blocking_at(&absent, &Command::Status, TIMEOUT),
            Err(RequestError::NotRunning(_))
        ));
        assert!(matches!(
            request_info_blocking_at(&absent, TIMEOUT),
            Err(RequestError::NotRunning(_))
        ));

        let (_dir, path, daemon) = fake_daemon(vec![None]);
        assert!(matches!(
            request_blocking_at(&path, &Command::Status, TIMEOUT),
            Err(RequestError::Closed)
        ));
        daemon.join().unwrap();

        let (_dir, path, daemon) = fake_daemon(vec![Some(Response::Error {
            message: "no microphone found".to_string(),
        })]);
        assert!(matches!(
            request_info_blocking_at(&path, TIMEOUT),
            Err(RequestError::Daemon(message)) if message == "no microphone found"
        ));
        daemon.join().unwrap();
    }

    #[test]
    fn a_daemon_that_never_answers_times_out() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = Vec::new();
            stream.read_to_end(&mut frame).unwrap();
            // Holds the connection open, unanswered, until the client gave up.
            let _ = done_rx.recv();
        });
        let result = request_blocking_at(&path, &Command::Info, Some(Duration::from_millis(200)));
        done_tx.send(()).unwrap();
        daemon.join().unwrap();
        assert!(matches!(result, Err(RequestError::TimedOut)), "{result:?}");
    }
}
