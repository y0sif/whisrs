//! whisrs — shared types for CLI and daemon communication.

pub mod audio;
pub mod config;
pub mod history;
pub mod hotkey;
pub mod llm;
pub mod overlay;
pub mod state;
pub mod transcription;
pub mod tray;
pub mod tts;
pub mod window;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::transcription::openai_realtime_protocol::{OpenAiRealtimeProfile, TurnDetectionMode};

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
    pub input: InputConfig,
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
    #[serde(default, rename = "asr-sidecar", alias = "asr", alias = "vibevoice")]
    pub asr_sidecar: Option<AsrSidecarConfig>,
    #[serde(default, rename = "openai-compatible-realtime")]
    pub openai_compatible_realtime: Option<OpenAiCompatibleRealtimeConfig>,
    /// LLM configuration for command mode (text rewriting).
    #[serde(default)]
    pub llm: Option<llm::LlmConfig>,
    /// Text-to-speech configuration for read-selection-aloud.
    #[serde(default)]
    pub tts: Option<TtsConfig>,
    /// Global hotkey configuration.
    #[serde(default)]
    pub hotkeys: Option<HotkeyConfig>,
    /// Overlay appearance config (theme, dimensions, optional custom colors).
    #[serde(default)]
    pub overlay: Option<OverlayConfig>,
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
    /// Hotkey to read the selected text aloud (e.g. "Super+Shift+R").
    #[serde(alias = "read")]
    pub speak: Option<String>,
}

/// Visual configuration for the bottom recording overlay.
///
/// The shape is intentionally clamped tight (90–120 × 28–40) to keep the
/// gaussian-tapered bar layout legible. Themes pick the colors; if `colors`
/// is set, those override the theme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    /// Theme name: `"ember"` (default), `"carbon"`, `"cyan"`, or `"custom"`.
    /// Unknown values fall back to `"ember"` with a warning.
    #[serde(default = "default_overlay_theme")]
    pub theme: String,
    /// Pill width in pixels (clamped to 90..=120).
    #[serde(default = "default_overlay_width")]
    pub width: u32,
    /// Pill height in pixels (clamped to 28..=40).
    #[serde(default = "default_overlay_height")]
    pub height: u32,
    /// Custom color overrides; honored when `theme = "custom"`.
    /// Hex strings: `#RGB`, `#RRGGBB`, or `#RRGGBBAA`.
    #[serde(default)]
    pub colors: Option<OverlayColors>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayColors {
    pub background: Option<String>,
    pub ring: Option<String>,
    pub recording: Option<String>,
    pub transcribing: Option<String>,
    /// Override color for the read-aloud "speaking" bars.
    pub speaking: Option<String>,
    pub glow: Option<String>,
}

fn default_overlay_theme() -> String {
    "carbon".to_string()
}
fn default_overlay_width() -> u32 {
    100
}
fn default_overlay_height() -> u32 {
    40
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            theme: default_overlay_theme(),
            width: default_overlay_width(),
            height: default_overlay_height(),
            colors: None,
        }
    }
}

impl OverlayConfig {
    /// Width clamped to the supported range. Out-of-range values fall back
    /// silently to the nearest bound — we don't fail config load over UI.
    pub fn clamped_width(&self) -> u32 {
        self.width.clamp(90, 120)
    }
    pub fn clamped_height(&self) -> u32 {
        self.height.clamp(36, 48)
    }
}

/// Parse a hex color string into ARGB bytes `[A, R, G, B]` matching the
/// overlay renderer's color format. Accepts `#RGB`, `#RRGGBB`, `#RRGGBBAA`.
/// Returns `None` for malformed input so callers can fall back to a theme
/// default.
pub fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&s[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&s[2..3].repeat(2), 16).ok()?;
            (r, g, b, 255u8)
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            (r, g, b, 255u8)
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            let a = u8::from_str_radix(&s[6..8], 16).ok()?;
            (r, g, b, a)
        }
        _ => return None,
    };
    Some([a, r, g, b])
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
    /// Free-form prompt prepended to the vocabulary list before being sent to
    /// the transcription backend. Use this for sentence-style context (style,
    /// register, language hints) that doesn't fit a single-term vocabulary.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Enable system tray icon.
    #[serde(default = "default_true")]
    pub tray: bool,
    /// Enable bottom-screen recording overlay.
    #[serde(default)]
    pub overlay: bool,
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
            prompt: None,
            tray: true,
            overlay: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_device")]
    pub device: String,
}

/// Selects which keyboard-injection backend the daemon uses to type text.
///
/// On Wayland, the evdev/uinput backend emits raw keycodes that the
/// compositor reinterprets through the *active* XKB layout, so dictating
/// text that mixes scripts (e.g. Latin + Arabic, or any code-switching
/// between two keyboard layouts) gets garbled — characters absent from the
/// active layout cannot be produced. The Wayland virtual-keyboard backend
/// (`zwp_virtual_keyboard_v1`) ships its own keymap and types
/// layout-independently, fixing that class of bugs (see issue #44).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InjectorBackend {
    /// Use the Wayland virtual keyboard when the compositor supports
    /// `zwp_virtual_keyboard_v1`, otherwise fall back to evdev/uinput.
    #[default]
    Auto,
    /// Force the evdev/uinput backend (layout-dependent on Wayland).
    Uinput,
    /// Force `zwp_virtual_keyboard_v1` (errors at startup if unsupported).
    WaylandVk,
}

/// Keyboard injection (uinput) tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Delay between individual key events, in milliseconds. Raise this if
    /// characters are dropped by TUIs that read stdin in raw mode (e.g.
    /// Node/Ink-based apps like Claude Code).
    #[serde(default = "default_key_delay_ms")]
    pub key_delay_ms: u64,
    /// Keyboard-injection backend. `auto` (the recommended default) prefers
    /// the Wayland virtual keyboard when available and otherwise falls back
    /// to uinput. Set this to `wayland-vk` to fix garbled bilingual /
    /// code-switching dictation on Wayland (issue #44), where the uinput
    /// backend can only emit characters present in the active XKB layout.
    #[serde(default)]
    pub backend: InjectorBackend,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            key_delay_ms: default_key_delay_ms(),
            backend: InjectorBackend::default(),
        }
    }
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

/// Text-to-speech configuration for the read-selection-aloud feature.
///
/// Opt-in (`enabled` defaults to `false`). v1 uses the Groq TTS endpoint;
/// when `api_key` is absent the daemon falls back to the `[groq]` api_key /
/// `WHISRS_GROQ_API_KEY` env var (TTS runs on the same Groq account).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    /// Whether read-selection-aloud is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// TTS backend: `"groq"` (default), `"openai"`, `"tts-sidecar"`
    /// (alias `"openai-compat"`, for local Kokoro/Supertonic servers), or
    /// `"deepgram"` (Aura-2).
    #[serde(default = "default_tts_backend")]
    pub backend: String,
    /// TTS model identifier (backend-specific). When omitted, each backend
    /// applies its own sensible default (see [`crate::tts::create_backend`]),
    /// so switching `backend` works without also hand-editing the model.
    #[serde(default)]
    pub model: Option<String>,
    /// Voice name (backend-specific). When omitted, the backend's default
    /// voice is used.
    #[serde(default)]
    pub voice: Option<String>,
    /// Audio response format requested from the API (we decode WAV).
    #[serde(default = "default_tts_response_format")]
    pub response_format: String,
    /// Optional dedicated API key; falls back to the backend's key when absent.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Endpoint URL for the `tts-sidecar` backend (OpenAI-compatible
    /// `/v1/audio/speech`). Ignored by other backends.
    #[serde(default)]
    pub url: Option<String>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_tts_backend(),
            model: None,
            voice: None,
            response_format: default_tts_response_format(),
            api_key: None,
            url: None,
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsrSidecarConfig {
    #[serde(default = "default_asr_sidecar_url")]
    pub url: String,
    #[serde(default = "default_asr_sidecar_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatibleRealtimeConfig {
    pub url: String,
    #[serde(default = "default_openai_compatible_realtime_model")]
    pub model: String,
    #[serde(default = "default_openai_compatible_realtime_profile")]
    pub profile: String,
    #[serde(default = "default_openai_compatible_realtime_turn_detection")]
    pub turn_detection: String,
    #[serde(default)]
    pub api_key: Option<String>,
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
fn default_key_delay_ms() -> u64 {
    2
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
fn default_tts_backend() -> String {
    "groq".to_string()
}
fn default_tts_response_format() -> String {
    "wav".to_string()
}
fn default_asr_sidecar_url() -> String {
    "http://127.0.0.1:8765/transcribe".to_string()
}
fn default_asr_sidecar_model() -> String {
    "microsoft/VibeVoice-ASR-HF".to_string()
}
fn default_openai_compatible_realtime_model() -> String {
    "Whisper-Tiny".to_string()
}
fn default_openai_compatible_realtime_profile() -> String {
    "lemonade".to_string()
}
fn default_openai_compatible_realtime_turn_detection() -> String {
    "server-vad".to_string()
}

// ---------------------------------------------------------------------------
// Daemon control
// ---------------------------------------------------------------------------

/// Outcome of an attempt to restart the daemon via systemd.
///
/// The CLI and `whisrs config` both need to nudge the daemon after writing a
/// new `config.toml`. They want the same systemd detection logic but different
/// output formatting (e.g. ANSI colors only when stdout is a TTY), so this
/// helper returns a structured outcome instead of printing directly.
#[derive(Debug)]
pub enum RestartOutcome {
    /// `systemctl --user restart whisrs.service` succeeded.
    Restarted,
    /// No `whisrs.service` user unit is loaded — caller should show fallback hints.
    NoSystemdUnit,
    /// systemd is installed but the restart command failed (non-zero exit).
    Failed,
}

/// Restart the whisrs daemon via systemd if a user unit is loaded.
///
/// Returns [`RestartOutcome::NoSystemdUnit`] without running anything when the
/// unit isn't present; callers can fall back to printing manual instructions.
pub fn restart_daemon_via_systemd() -> RestartOutcome {
    if !has_systemd_unit() {
        return RestartOutcome::NoSystemdUnit;
    }
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "whisrs.service"])
        .status();
    match status {
        Ok(s) if s.success() => RestartOutcome::Restarted,
        _ => RestartOutcome::Failed,
    }
}

/// Returns `true` when `whisrs.service` is loaded as a user unit.
fn has_systemd_unit() -> bool {
    // `is-enabled` exits 0 for enabled/static/linked units and non-zero when
    // the unit isn't loaded. Falls back to `list-unit-files` for distros where
    // `is-enabled` may exit non-zero on static/linked units.
    let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "whisrs.service"])
        .output()
    else {
        return false;
    };
    if output.status.success() {
        return true;
    }
    let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "list-unit-files", "whisrs.service"])
        .output()
    else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).contains("whisrs.service")
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
            "asr-sidecar" | "asr" | "vibevoice" => {
                let url = self
                    .asr_sidecar
                    .as_ref()
                    .map(|v| v.url.trim())
                    .unwrap_or("");
                if url.is_empty() {
                    return Err(WhisrsError::Config(
                        "ASR sidecar backend selected but no sidecar URL configured.\n\
                         Add [asr-sidecar] url to config.toml."
                            .to_string(),
                    ));
                }
            }
            "openai-compatible-realtime" => {
                let config = self.openai_compatible_realtime.as_ref().ok_or_else(|| {
                    WhisrsError::Config(
                        "OpenAI-compatible realtime backend selected but no config section found.\n\
                         Add [openai-compatible-realtime] to config.toml."
                            .to_string(),
                    )
                })?;

                let url = config.url.trim();
                if url.is_empty() {
                    return Err(WhisrsError::Config(
                        "OpenAI-compatible realtime backend selected but no WebSocket URL configured.\n\
                         Add [openai-compatible-realtime] url to config.toml."
                            .to_string(),
                    ));
                }

                let parsed_url = reqwest::Url::parse(url).map_err(|e| {
                    WhisrsError::Config(format!("OpenAI-compatible realtime URL is invalid: {e}"))
                })?;
                match parsed_url.scheme() {
                    "ws" | "wss" => {}
                    scheme => {
                        return Err(WhisrsError::Config(format!(
                            "OpenAI-compatible realtime URL must use ws:// or wss://, got {scheme}://"
                        )));
                    }
                }

                if config.model.trim().is_empty() {
                    return Err(WhisrsError::Config(
                        "OpenAI-compatible realtime backend selected but model is empty.\n\
                         Set [openai-compatible-realtime] model in config.toml."
                            .to_string(),
                    ));
                }

                OpenAiRealtimeProfile::parse(config.profile.trim()).map_err(|e| {
                    WhisrsError::Config(format!(
                        "OpenAI-compatible realtime profile is invalid: {e}"
                    ))
                })?;
                if config.profile.trim() != "lemonade" {
                    return Err(WhisrsError::Config(
                        "OpenAI-compatible realtime backend currently supports only profile 'lemonade'."
                            .to_string(),
                    ));
                }

                TurnDetectionMode::parse(config.turn_detection.trim()).map_err(|e| {
                    WhisrsError::Config(format!(
                        "OpenAI-compatible realtime turn detection is invalid: {e}"
                    ))
                })?;
            }
            other => {
                return Err(WhisrsError::Config(format!(
                    "Unknown backend '{other}'. Valid options: deepgram, deepgram-streaming, \
                     groq, openai, openai-realtime, openai-compatible-realtime, \
                     local-whisper, local-vosk, local-parakeet, asr-sidecar"
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

        let has_asr_sidecar = self
            .asr_sidecar
            .as_ref()
            .map(|v| !v.url.trim().is_empty())
            .unwrap_or(false);

        let has_openai_compatible_realtime = self
            .openai_compatible_realtime
            .as_ref()
            .map(|v| !v.url.trim().is_empty())
            .unwrap_or(false);

        has_deepgram
            || has_groq
            || has_openai
            || has_local
            || has_asr_sidecar
            || has_openai_compatible_realtime
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
    fn config_tts_section_roundtrip() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [tts]
            enabled = true
            model = "canopylabs/orpheus-v1-english"
            voice = "autumn"
            response_format = "wav"
            "#,
        )
        .unwrap();

        let tts = config.tts.as_ref().expect("tts section parsed");
        assert!(tts.enabled);
        assert_eq!(tts.model.as_deref(), Some("canopylabs/orpheus-v1-english"));
        assert_eq!(tts.voice.as_deref(), Some("autumn"));
        assert_eq!(tts.response_format, "wav");
        assert!(tts.api_key.is_none());

        // Round-trips back out and parses again identically.
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert!(reparsed.tts.unwrap().enabled);
    }

    #[test]
    fn config_tts_backend_and_url_roundtrip() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [tts]
            enabled = true
            backend = "tts-sidecar"
            model = "kokoro"
            voice = "af_heart"
            url = "http://127.0.0.1:8880/v1/audio/speech"
            "#,
        )
        .unwrap();

        let tts = config.tts.as_ref().expect("tts section parsed");
        assert_eq!(tts.backend, "tts-sidecar");
        assert_eq!(
            tts.url.as_deref(),
            Some("http://127.0.0.1:8880/v1/audio/speech")
        );

        // Round-trips back out and parses again identically.
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        let tts = reparsed.tts.unwrap();
        assert_eq!(tts.backend, "tts-sidecar");
        assert_eq!(
            tts.url.as_deref(),
            Some("http://127.0.0.1:8880/v1/audio/speech")
        );
    }

    #[test]
    fn config_tts_backend_defaults_to_groq() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [tts]
            enabled = true
            "#,
        )
        .unwrap();
        assert_eq!(config.tts.unwrap().backend, "groq");
    }

    #[test]
    fn config_tts_defaults_when_minimal() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [tts]
            enabled = true
            "#,
        )
        .unwrap();

        let tts = config.tts.unwrap();
        assert!(tts.enabled);
        // model/voice are left unset in config — each backend supplies its own
        // default at build time (see tts::create_backend), so switching backend
        // doesn't require also overriding the model.
        assert!(tts.model.is_none());
        assert!(tts.voice.is_none());
        assert_eq!(tts.response_format, "wav");
    }

    #[test]
    fn config_without_tts_is_none() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"
            "#,
        )
        .unwrap();
        assert!(config.tts.is_none());
    }

    #[test]
    fn hotkey_speak_read_alias() {
        let hotkeys: HotkeyConfig = toml::from_str(r#"read = "Super+Shift+R""#).unwrap();
        assert_eq!(hotkeys.speak.as_deref(), Some("Super+Shift+R"));
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
    fn injector_backend_toml_roundtrip() {
        // Each variant serializes to its kebab-case string and parses back.
        for (variant, name) in [
            (InjectorBackend::Auto, "auto"),
            (InjectorBackend::Uinput, "uinput"),
            (InjectorBackend::WaylandVk, "wayland-vk"),
        ] {
            #[derive(Serialize, Deserialize)]
            struct Wrap {
                backend: InjectorBackend,
            }
            let toml_str = toml::to_string(&Wrap { backend: variant }).unwrap();
            assert_eq!(toml_str.trim(), format!("backend = \"{name}\""));
            let parsed: Wrap = toml::from_str(&format!("backend = \"{name}\"")).unwrap();
            assert_eq!(parsed.backend, variant);
        }
    }

    #[test]
    fn input_config_backend_defaults_to_auto_when_absent() {
        // An [input] table without a `backend` key must default to Auto
        // (back-compat for configs written before the field existed).
        let input: InputConfig = toml::from_str(
            r#"
            key_delay_ms = 5
            "#,
        )
        .unwrap();
        assert_eq!(input.backend, InjectorBackend::Auto);

        // And an explicit value is honoured.
        let input: InputConfig = toml::from_str(
            r#"
            backend = "wayland-vk"
            "#,
        )
        .unwrap();
        assert_eq!(input.backend, InjectorBackend::WaylandVk);
    }

    #[test]
    fn config_input_backend_back_compat() {
        // A full config whose [input] table omits `backend` parses with the
        // Auto default.
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "local-whisper"

            [audio]
            device = "default"

            [input]
            key_delay_ms = 8
            "#,
        )
        .unwrap();
        assert_eq!(config.input.backend, InjectorBackend::Auto);
    }

    #[test]
    fn config_validate_unknown_backend() {
        let config = Config {
            general: GeneralConfig {
                backend: "nonexistent".to_string(),
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
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            overlay: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Unknown backend"));
        assert!(err.to_string().contains("openai-compatible-realtime"));
    }

    #[test]
    fn config_defaults_overlay_off_for_old_configs() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "local-whisper"

            [audio]
            device = "default"
            "#,
        )
        .unwrap();

        assert!(!config.general.overlay);
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
            input: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: None,
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            overlay: None,
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
            input: Default::default(),
            deepgram: None,
            groq: Some(GroqConfig {
                api_key: "test-key".to_string(),
                model: "whisper-large-v3-turbo".to_string(),
            }),
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: None,
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            overlay: None,
        };
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn config_parse_asr_sidecar_defaults() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "asr-sidecar"

            [asr-sidecar]
            "#,
        )
        .unwrap();

        let asr_sidecar = config.asr_sidecar.unwrap();
        assert_eq!(asr_sidecar.url, "http://127.0.0.1:8765/transcribe");
        assert_eq!(asr_sidecar.model, "microsoft/VibeVoice-ASR-HF");
    }

    #[test]
    fn config_validate_asr_sidecar_with_url() {
        let config = Config {
            general: GeneralConfig {
                backend: "asr-sidecar".to_string(),
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
            asr_sidecar: Some(AsrSidecarConfig {
                url: "http://127.0.0.1:8765/transcribe".to_string(),
                model: "microsoft/VibeVoice-ASR-HF".to_string(),
            }),
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            overlay: None,
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_parse_vibevoice_alias() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "vibevoice"

            [vibevoice]
            url = "http://127.0.0.1:8765/transcribe"
            model = "microsoft/VibeVoice-ASR-HF"
            "#,
        )
        .unwrap();

        assert!(config.validate().is_ok());
        assert!(config.asr_sidecar.is_some());
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
            input: Default::default(),
            deepgram: None,
            groq: Some(GroqConfig {
                api_key: "test-key".to_string(),
                model: "whisper-large-v3-turbo".to_string(),
            }),
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: None,
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            overlay: None,
        };
        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("silence_timeout_ms")));
    }

    #[test]
    fn config_parse_openai_compatible_realtime_defaults() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "openai-compatible-realtime"

            [openai-compatible-realtime]
            url = "ws://localhost:1234/realtime"
            "#,
        )
        .unwrap();

        let realtime = config.openai_compatible_realtime.unwrap();
        assert_eq!(realtime.url, "ws://localhost:1234/realtime");
        assert_eq!(realtime.model, "Whisper-Tiny");
        assert_eq!(realtime.profile, "lemonade");
        assert_eq!(realtime.turn_detection, "server-vad");
        assert!(realtime.api_key.is_none());
    }

    #[test]
    fn config_validate_openai_compatible_realtime_with_valid_config() {
        let config = Config {
            general: GeneralConfig {
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
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "ws://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_openai_compatible_realtime_rejects_missing_url() {
        let config = Config {
            general: GeneralConfig {
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
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: " ".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("WebSocket URL"));
    }

    #[test]
    fn config_validate_openai_compatible_realtime_rejects_non_websocket_url() {
        let config = Config {
            general: GeneralConfig {
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
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "http://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("ws:// or wss://"));
    }

    #[test]
    fn config_validate_openai_compatible_realtime_rejects_unknown_profile() {
        let config = Config {
            general: GeneralConfig {
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
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "ws://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "bogus".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("profile is invalid"));
    }

    #[test]
    fn config_validate_openai_compatible_realtime_rejects_unsupported_turn_detection() {
        let config = Config {
            general: GeneralConfig {
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
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "ws://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "bogus".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("turn detection is invalid"));
    }

    #[test]
    fn has_any_backend_configured_counts_openai_compatible_realtime_url() {
        let config = Config {
            general: Default::default(),
            audio: Default::default(),
            input: Default::default(),
            deepgram: None,
            groq: None,
            openai: None,
            local_whisper: None,
            local_vosk: None,
            local_parakeet: None,
            asr_sidecar: None,
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "ws://localhost:1234/realtime".to_string(),
                model: "Whisper-Tiny".to_string(),
                profile: "lemonade".to_string(),
                turn_detection: "server-vad".to_string(),
                api_key: None,
            }),
            llm: None,
            hotkeys: None,
            overlay: None,
            tts: None,
        };

        assert!(config.has_any_backend_configured());
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
