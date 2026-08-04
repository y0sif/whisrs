//! Configuration structs, defaults, and validation for `config.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::hotkey;
use crate::llm;
use crate::transcription::openai_realtime_protocol::{OpenAiRealtimeProfile, TurnDetectionMode};
use crate::WhisrsError;

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
    /// Named custom LLM commands, each with its own hotkey (see
    /// [`llm::LlmCommandConfig`]). Empty by default.
    #[serde(default)]
    pub llm_commands: Vec<llm::LlmCommandConfig>,
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
    /// Inject text by clipboard paste (Ctrl+V) instead of typing keystrokes.
    ///
    /// The uinput backend emits raw keycodes that the compositor decodes
    /// through the target window's *active* XKB layout. On compositors without
    /// the Wayland virtual-keyboard protocol (e.g. KWin), when that active
    /// layout isn't the one whisrs detected — most commonly with per-window
    /// layouts (KDE `SwitchMode=WinClass`) or a non-US keymap — the output is
    /// garbled (`z`↔`y`, mangled punctuation, dropped accents). Pasting sends
    /// the text through the clipboard, which is layout-independent and
    /// Unicode-complete, so it comes out verbatim.
    ///
    /// Trade-offs: briefly replaces the clipboard (restored right after) and
    /// the target app must support Ctrl+V. It covers batch (non-streaming)
    /// dictation and command-mode output (`whisrs command` injects its LLM
    /// result with a single injection call, so it honors this regardless of
    /// the configured backend). The streaming *dictation* path is the
    /// exception: streaming backends (including `local-whisper`, which always
    /// streams regardless of its `segmentation` mode) type incrementally as
    /// text arrives and ignore this setting. [`Config::validate`] warns when
    /// this is set alongside one of those backends.
    #[serde(default)]
    pub paste: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            key_delay_ms: default_key_delay_ms(),
            backend: InjectorBackend::default(),
            paste: false,
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
    /// Streaming segmentation strategy: `"silence"` (default) splits audio
    /// into phrases at natural pauses and decodes each exactly once;
    /// `"window"` is the legacy overlapping sliding window with text dedup.
    #[serde(default = "default_local_whisper_segmentation")]
    pub segmentation: String,
    /// Milliseconds of continuous silence that ends a phrase in `"silence"`
    /// segmentation mode.
    #[serde(default = "default_phrase_silence_ms")]
    pub phrase_silence_ms: u64,
}

impl LocalWhisperConfig {
    /// Config for `model_path` with default segmentation settings.
    pub fn new(model_path: String) -> Self {
        Self {
            model_path,
            segmentation: default_local_whisper_segmentation(),
            phrase_silence_ms: default_phrase_silence_ms(),
        }
    }
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
fn default_local_whisper_segmentation() -> String {
    "silence".to_string()
}
pub(crate) fn default_phrase_silence_ms() -> u64 {
    400
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

/// Keys in a parsed `config.toml` document that the configuration schema
/// does not know.
///
/// The known set is derived from serde itself: the parsed [`Config`] is
/// serialized back into a `toml::Table`, and the two tables are diffed
/// recursively. A key present in the document but absent from the
/// reserialization is a key the running binary silently dropped. This avoids
/// a hand-maintained field-name list, which would go stale, and avoids a new
/// dependency.
///
/// Returns `[]` when the document is not valid TOML or does not deserialize,
/// because those cases already produce their own error in [`crate::daemon::startup::load_config`].
pub fn unknown_config_keys(contents: &str) -> Vec<String> {
    let Ok(document) = contents.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Ok(config) = toml::from_str::<Config>(contents) else {
        return Vec::new();
    };
    let Ok(known) = toml::Value::try_from(&config) else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    diff_config_tables(
        &document,
        known.as_table().expect("Config serializes to a table"),
        "",
        &mut unknown,
    );
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Collect every leaf key of `table` (with `prefix`) into `out`.
fn collect_unknown_leaves(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        match value {
            toml::Value::Table(inner) => collect_unknown_leaves(inner, &format!("{path}."), out),
            _ => out.push(path),
        }
    }
}

/// Recursively compare a parsed document table against the reserialized
/// (schema-known) table, appending dotted paths of unknown keys to `out`.
fn diff_config_tables(
    document: &toml::Table,
    known: &toml::Table,
    prefix: &str,
    out: &mut Vec<String>,
) {
    for (key, value) in document {
        let path = format!("{prefix}{key}");
        match known.get(key) {
            None => match value {
                // A whole section the schema dropped (e.g. an Option section
                // whose only keys are unknown): report each leaf, so the user
                // learns which key inside it is the typo.
                toml::Value::Table(inner) => {
                    collect_unknown_leaves(inner, &format!("{path}."), out)
                }
                _ => out.push(path),
            },
            Some(toml::Value::Table(known_table)) => {
                if let toml::Value::Table(document_table) = value {
                    diff_config_tables(document_table, known_table, &format!("{path}."), out);
                }
            }
            Some(toml::Value::Array(known_array)) => {
                if let toml::Value::Array(document_array) = value {
                    for (index, item) in document_array.iter().enumerate() {
                        if let (
                            toml::Value::Table(document_table),
                            Some(toml::Value::Table(known_table)),
                        ) = (item, known_array.get(index))
                        {
                            diff_config_tables(
                                document_table,
                                known_table,
                                &format!("{path}[{index}]."),
                                out,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
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

        // Streaming backends (including local-whisper, which always streams
        // regardless of its `segmentation` mode) type dictated text
        // incrementally as it arrives and never go through the
        // paste-injection path, so `[input] paste` does not apply to
        // dictation with them. Command mode is unaffected whatever backend
        // transcribed the instruction: the instruction is never injected, and
        // the LLM result goes out in a single injection call through the same
        // wrapper the batch dictation path uses.
        if self.input.paste
            && matches!(
                backend,
                "deepgram-streaming"
                    | "openai-realtime"
                    | "openai-compatible-realtime"
                    | "local-whisper"
                    | "local"
            )
        {
            warnings.push(ConfigWarning {
                message: format!(
                    "[input] paste = true does not apply to dictation with backend = \
                     \"{backend}\": streaming backends (deepgram-streaming, openai-realtime, \
                     openai-compatible-realtime, local-whisper) type text incrementally as it \
                     arrives and never use the paste path. Command mode output is injected in \
                     one shot, so it still uses paste where that mode is configured. Switch to \
                     a non-streaming backend (deepgram, groq, openai, local-vosk, \
                     local-parakeet, asr-sidecar) to use paste injection for dictation too."
                ),
            });
        }

        if !self.llm_commands.is_empty() {
            if self.llm.is_none() {
                warnings.push(ConfigWarning {
                    message: "llm_commands configured but no [llm] section — add [llm] api_key \
                              (or set WHISRS_OPENAI_API_KEY / WHISRS_GROQ_API_KEY) or these \
                              hotkeys will fail at runtime"
                        .to_string(),
                });
            }

            // The llm-command path always transcribes the recorded instruction
            // with a single batch `transcribe()` call, even when the dictation
            // backend streams: deepgram-streaming and both realtime backends
            // push the whole WAV through their websocket in one shot. Not a
            // failure — the transcript still comes back — but none of the
            // streaming behavior the user configured applies. (local-whisper
            // is excluded: its `transcribe()` is a real batch path.)
            if matches!(
                backend,
                "deepgram-streaming" | "openai-realtime" | "openai-compatible-realtime"
            ) {
                warnings.push(ConfigWarning {
                    message: format!(
                        "llm_commands run one-shot with backend = \"{backend}\": the \
                         llm-command path pushes the whole recording through a single batch \
                         transcription call, so the streaming behavior this backend is \
                         configured for does not apply to these hotkeys. They still work — \
                         the transcript just arrives in one round trip after recording \
                         stops. Dictation (toggle) streams as configured."
                    ),
                });
            }

            let mut seen_names = std::collections::HashSet::new();
            for entry in &self.llm_commands {
                if entry.name.trim().is_empty() {
                    warnings.push(ConfigWarning {
                        message: "llm_commands entry has an empty name".to_string(),
                    });
                } else if !seen_names.insert(entry.name.clone()) {
                    warnings.push(ConfigWarning {
                        message: format!("llm_commands has a duplicate name: '{}'", entry.name),
                    });
                }
                if entry.hotkey.trim().is_empty() {
                    warnings.push(ConfigWarning {
                        message: format!("llm_commands '{}' has an empty hotkey", entry.name),
                    });
                } else if let Err(e) = hotkey::parse_hotkey(&entry.hotkey) {
                    warnings.push(ConfigWarning {
                        message: format!(
                            "llm_commands '{}' has an invalid hotkey '{}': {e}",
                            entry.name, entry.hotkey
                        ),
                    });
                }
                if let Some(set_hotkey) = &entry.set_hotkey {
                    if let Err(e) = hotkey::parse_hotkey(set_hotkey) {
                        warnings.push(ConfigWarning {
                            message: format!(
                                "llm_commands '{}' has an invalid set_hotkey '{}': {e}",
                                entry.name, set_hotkey
                            ),
                        });
                    } else if *set_hotkey == entry.hotkey {
                        warnings.push(ConfigWarning {
                            message: format!(
                                "llm_commands '{}' has set_hotkey equal to hotkey '{}' — one \
                                 press can't both run and reprogram",
                                entry.name, entry.hotkey
                            ),
                        });
                    }
                }
                if entry.instruction.trim().is_empty() {
                    warnings.push(ConfigWarning {
                        message: format!("llm_commands '{}' has an empty instruction", entry.name),
                    });
                }
            }
        }

        // Duplicate-binding detection across [hotkeys] and llm_commands. The
        // listener dispatches every action whose binding matches, with no
        // early exit, so a shared combo silently fires multiple commands and
        // the loser's error is never seen. Compare parsed bindings (sorted
        // modifier set + trigger key) rather than raw strings, so
        // "shift+super+t" collides with "Super+Shift+T".
        let mut binding_sources: Vec<(String, &str)> = Vec::new();
        if let Some(hotkeys) = &self.hotkeys {
            for (label, value) in [
                ("[hotkeys] toggle", &hotkeys.toggle),
                ("[hotkeys] cancel", &hotkeys.cancel),
                ("[hotkeys] command", &hotkeys.command),
                ("[hotkeys] speak", &hotkeys.speak),
            ] {
                if let Some(spec) = value {
                    binding_sources.push((label.to_string(), spec.as_str()));
                }
            }
        }
        for entry in &self.llm_commands {
            binding_sources.push((
                format!("llm_commands '{}' hotkey", entry.name),
                entry.hotkey.as_str(),
            ));
            if let Some(set_hotkey) = &entry.set_hotkey {
                // A set_hotkey textually equal to its own hotkey already got
                // the dedicated warning above; skip it here so the same
                // mistake is not reported twice. A collision with any third
                // binding is still reported through the hotkey itself.
                if *set_hotkey != entry.hotkey {
                    binding_sources.push((
                        format!("llm_commands '{}' set_hotkey", entry.name),
                        set_hotkey.as_str(),
                    ));
                }
            }
        }
        let mut seen_bindings: std::collections::HashMap<(Vec<u16>, u16), (String, String)> =
            std::collections::HashMap::new();
        for (source, spec) in binding_sources {
            // Unset or empty bindings never fire, so they must not collide
            // with each other; empty llm_commands hotkeys warn above.
            if spec.trim().is_empty() {
                continue;
            }
            // Invalid specs warn above (llm_commands) or are rejected by the
            // listener at startup; either way they never fire.
            let Ok(parsed) = hotkey::parse_hotkey(spec) else {
                continue;
            };
            let mut modifiers: Vec<u16> = parsed.modifiers.iter().map(|k| k.code()).collect();
            modifiers.sort_unstable();
            modifiers.dedup();
            let canonical = (modifiers, parsed.trigger.code());
            if let Some((first_source, first_spec)) = seen_bindings.get(&canonical) {
                warnings.push(ConfigWarning {
                    message: format!(
                        "duplicate hotkey binding: {first_source} ('{first_spec}') and \
                         {source} ('{spec}') use the same combo — one press fires both \
                         actions; rebind one of them"
                    ),
                });
            } else {
                seen_bindings.insert(canonical, (source, spec.to_string()));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_top_level_key_is_reported() {
        let unknown = unknown_config_keys("bogus = 1\n[general]\nbackend = \"groq\"\n");
        assert_eq!(unknown, vec!["bogus"]);
    }

    #[test]
    fn unknown_nested_key_reports_full_path() {
        // The live case from #99: `past` is a typo for `paste`.
        let unknown = unknown_config_keys("[input]\npast = true\n");
        assert_eq!(unknown, vec!["input.past"]);
    }

    #[test]
    fn unknown_key_in_option_section_is_reported() {
        let unknown = unknown_config_keys("[deepgram]\napi_key = \"k\"\nbogus = 2\n");
        assert_eq!(unknown, vec!["deepgram.bogus"]);
    }

    #[test]
    fn section_with_only_unknown_keys_reports_leaves() {
        let unknown = unknown_config_keys("[deepgram]\nbogus = 1\n");
        assert_eq!(unknown, vec!["deepgram.bogus"]);
    }

    #[test]
    fn unknown_key_in_llm_command_array_element_is_reported() {
        let unknown = unknown_config_keys(
            "[[llm_commands]]\nname = \"x\"\nhotkey = \"Super+T\"\ninstruction = \"y\"\nbogus = 1\n",
        );
        assert_eq!(unknown, vec!["llm_commands[0].bogus"]);
    }

    #[test]
    fn valid_config_reports_nothing() {
        let unknown = unknown_config_keys("[general]\nbackend = \"groq\"\n[input]\npaste = true\n");
        assert!(unknown.is_empty());
    }

    #[test]
    fn invalid_toml_reports_nothing() {
        let unknown = unknown_config_keys("not [valid toml");
        assert!(unknown.is_empty());
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
    fn input_config_paste_defaults_false_and_parses() {
        // Omitted → false (back-compat for configs written before the field).
        let input: InputConfig = toml::from_str("key_delay_ms = 5").unwrap();
        assert!(!input.paste);

        // Explicit value honoured.
        let input: InputConfig = toml::from_str("paste = true").unwrap();
        assert!(input.paste);
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
            overlay: None,
        };
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn config_validate_paste_with_non_streaming_backend_no_warning() {
        let config = Config {
            general: GeneralConfig {
                backend: "groq".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            input: InputConfig {
                paste: true,
                ..Default::default()
            },
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
            llm_commands: Vec::new(),
            overlay: None,
        };
        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().all(|w| !w.message.contains("paste")),
            "groq is not a streaming backend; paste should not warn: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_paste_with_streaming_backend_warns() {
        for backend in ["deepgram-streaming", "openai-realtime", "local-whisper"] {
            let config = Config {
                general: GeneralConfig {
                    backend: backend.to_string(),
                    ..Default::default()
                },
                audio: Default::default(),
                input: InputConfig {
                    paste: true,
                    ..Default::default()
                },
                deepgram: Some(DeepgramConfig {
                    api_key: "test-key".to_string(),
                    model: default_deepgram_model(),
                }),
                groq: None,
                openai: Some(OpenAiConfig {
                    api_key: "test-key".to_string(),
                    model: default_openai_model(),
                }),
                local_whisper: None,
                local_vosk: None,
                local_parakeet: None,
                asr_sidecar: None,
                openai_compatible_realtime: None,
                llm: None,
                tts: None,
                hotkeys: None,
                llm_commands: Vec::new(),
                overlay: None,
            };
            let warnings = config.validate().unwrap();
            let warning = warnings
                .iter()
                .find(|w| w.message.contains("[input] paste = true"))
                .unwrap_or_else(|| {
                    panic!(
                        "backend {backend} streams and ignores paste for dictation; \
                         expected a warning, got: {warnings:?}"
                    )
                });
            assert!(
                warning.message.contains("does not apply to dictation"),
                "the warning must scope itself to dictation, not claim paste is a global \
                 no-op: {}",
                warning.message
            );
            assert!(
                warning.message.contains("Command mode"),
                "the warning must say command mode still pastes: {}",
                warning.message
            );
        }
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
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
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };

        assert!(config.has_any_backend_configured());
    }

    #[test]
    fn config_parses_llm_commands_array() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [[llm_commands]]
            name = "translate-de"
            hotkey = "Super+Shift+T"
            instruction = "Translate the following into German, informal tone."

            [[llm_commands]]
            name = "summarize"
            hotkey = "Super+Shift+S"
            instruction = "Summarize the following in one sentence."
            "#,
        )
        .unwrap();

        assert_eq!(config.llm_commands.len(), 2);
        assert_eq!(config.llm_commands[0].name, "translate-de");
        assert_eq!(config.llm_commands[0].hotkey, "Super+Shift+T");
        assert_eq!(config.llm_commands[1].name, "summarize");

        // Round-trips back out and parses again identically.
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.llm_commands.len(), 2);
    }

    #[test]
    fn config_without_llm_commands_defaults_empty() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"
            "#,
        )
        .unwrap();
        assert!(config.llm_commands.is_empty());
    }

    #[test]
    fn config_validate_warns_llm_commands_without_llm_section() {
        let mut config = Config {
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
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };
        config.llm_commands.push(llm::LlmCommandConfig {
            name: "translate-de".to_string(),
            hotkey: "Super+Shift+T".to_string(),
            set_hotkey: None,
            instruction: "Translate to German.".to_string(),
        });

        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("no [llm] section")));
    }

    #[test]
    fn config_validate_rejects_duplicate_llm_command_names() {
        let mut config = Config {
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
            llm: Some(llm::LlmConfig::default()),
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };
        for _ in 0..2 {
            config.llm_commands.push(llm::LlmCommandConfig {
                name: "dup".to_string(),
                hotkey: "Super+Shift+T".to_string(),
                set_hotkey: None,
                instruction: "Translate to German.".to_string(),
            });
        }

        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("duplicate name")));
    }

    #[test]
    fn config_validate_rejects_invalid_llm_command_hotkey() {
        let mut config = Config {
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
            llm: Some(llm::LlmConfig::default()),
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };
        config.llm_commands.push(llm::LlmCommandConfig {
            name: "translate-de".to_string(),
            hotkey: "NotAKey".to_string(),
            set_hotkey: None,
            instruction: "Translate to German.".to_string(),
        });

        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("invalid hotkey")));
    }

    #[test]
    fn llm_command_set_hotkey_defaults_none_and_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"

            [[llm_commands]]
            name = "no-set"
            hotkey = "Super+Shift+T"
            instruction = "Translate to German."

            [[llm_commands]]
            name = "with-set"
            hotkey = "Super+Shift+U"
            set_hotkey = "Super+Shift+Alt+U"
            instruction = "Summarize."
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm_commands[0].set_hotkey, None);
        assert_eq!(
            cfg.llm_commands[1].set_hotkey.as_deref(),
            Some("Super+Shift+Alt+U")
        );
    }

    #[test]
    fn config_validate_warns_set_hotkey_equal_to_hotkey() {
        let mut config = Config {
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
            llm: Some(llm::LlmConfig::default()),
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        };
        config.llm_commands.push(llm::LlmCommandConfig {
            name: "translate-de".to_string(),
            hotkey: "Super+Shift+T".to_string(),
            set_hotkey: Some("Super+Shift+T".to_string()),
            instruction: "Translate to German.".to_string(),
        });

        let warnings = config.validate().unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("set_hotkey equal to hotkey")));
    }

    /// Minimal valid config for the given backend, with every backend
    /// section populated so validate()'s hard checks pass. Tests mutate the
    /// fields they exercise.
    fn validatable_config(backend: &str) -> Config {
        Config {
            general: GeneralConfig {
                backend: backend.to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            input: Default::default(),
            deepgram: Some(DeepgramConfig {
                api_key: "test-key".to_string(),
                model: default_deepgram_model(),
            }),
            groq: Some(GroqConfig {
                api_key: "test-key".to_string(),
                model: "whisper-large-v3-turbo".to_string(),
            }),
            openai: Some(OpenAiConfig {
                api_key: "test-key".to_string(),
                model: default_openai_model(),
            }),
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
            llm: Some(llm::LlmConfig::default()),
            hotkeys: None,
            llm_commands: Vec::new(),
            overlay: None,
            tts: None,
        }
    }

    fn llm_command(name: &str, hotkey: &str) -> llm::LlmCommandConfig {
        llm::LlmCommandConfig {
            name: name.to_string(),
            hotkey: hotkey.to_string(),
            set_hotkey: None,
            instruction: "Translate to German.".to_string(),
        }
    }

    #[test]
    fn config_validate_warns_hotkey_collision_across_sections() {
        let mut config = validatable_config("groq");
        config.hotkeys = Some(HotkeyConfig {
            toggle: Some("Super+Shift+T".to_string()),
            cancel: None,
            command: None,
            speak: None,
        });
        config
            .llm_commands
            .push(llm_command("german", "Super+Shift+T"));

        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("duplicate hotkey binding"))
            .unwrap_or_else(|| {
                panic!("hotkeys.toggle and llm_commands share a combo; expected a warning, got: {warnings:?}")
            });
        assert!(
            warning.message.contains("[hotkeys] toggle"),
            "the warning must name the [hotkeys] side: {}",
            warning.message
        );
        assert!(
            warning.message.contains("llm_commands 'german' hotkey"),
            "the warning must name the llm_commands side: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_warns_hotkey_collision_between_llm_commands_normalized() {
        // Different spelling (case + modifier order) of the same combo must
        // still collide: the listener matches parsed bindings, not strings.
        let mut config = validatable_config("groq");
        config
            .llm_commands
            .push(llm_command("german", "Super+Shift+T"));
        config
            .llm_commands
            .push(llm_command("summarize", "shift+super+t"));

        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("duplicate hotkey binding"))
            .unwrap_or_else(|| {
                panic!("both entries bind the same combo; expected a warning, got: {warnings:?}")
            });
        assert!(
            warning.message.contains("llm_commands 'german' hotkey")
                && warning.message.contains("llm_commands 'summarize' hotkey"),
            "the warning must name both entries: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_warns_duplicate_bindings_within_hotkeys_section() {
        // "Meta" is an alias for "Super", so these are the same binding.
        let mut config = validatable_config("groq");
        config.hotkeys = Some(HotkeyConfig {
            toggle: Some("Super+D".to_string()),
            cancel: None,
            command: None,
            speak: Some("Meta+D".to_string()),
        });

        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("duplicate hotkey binding"))
            .unwrap_or_else(|| {
                panic!(
                    "toggle and speak bind the same combo; expected a warning, got: {warnings:?}"
                )
            });
        assert!(
            warning.message.contains("[hotkeys] toggle")
                && warning.message.contains("[hotkeys] speak"),
            "the warning must name both fields: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_no_collision_warning_for_distinct_bindings() {
        let mut config = validatable_config("groq");
        config.hotkeys = Some(HotkeyConfig {
            toggle: Some("Super+Shift+D".to_string()),
            cancel: Some("Super+Shift+Escape".to_string()),
            command: Some("Super+Shift+C".to_string()),
            speak: Some("Super+Shift+R".to_string()),
        });
        config
            .llm_commands
            .push(llm_command("german", "Super+Shift+T"));
        config
            .llm_commands
            .push(llm_command("summarize", "Super+Shift+S"));

        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("duplicate hotkey binding")),
            "all bindings are distinct; no collision warning expected: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_no_collision_warning_for_entry_own_set_hotkey() {
        let mut config = validatable_config("groq");
        let mut entry = llm_command("german", "Super+Shift+U");
        entry.set_hotkey = Some("Super+Shift+Alt+U".to_string());
        config.llm_commands.push(entry);

        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("duplicate hotkey binding")),
            "an entry's own distinct hotkey/set_hotkey pair is not a collision: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_set_hotkey_equal_to_hotkey_not_double_reported() {
        // Textually equal set_hotkey already has a dedicated warning; the
        // generic collision pass must not report the same mistake twice.
        let mut config = validatable_config("groq");
        let mut entry = llm_command("german", "Super+Shift+T");
        entry.set_hotkey = Some("Super+Shift+T".to_string());
        config.llm_commands.push(entry);

        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("set_hotkey equal to hotkey")),
            "the dedicated warning must still fire: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("duplicate hotkey binding")),
            "the generic collision warning would be redundant here: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_empty_llm_command_hotkeys_do_not_collide() {
        let mut config = validatable_config("groq");
        config.llm_commands.push(llm_command("german", ""));
        config.llm_commands.push(llm_command("summarize", ""));

        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("duplicate hotkey binding")),
            "empty hotkeys never fire and must not collide with each other: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_warns_llm_commands_with_streaming_backend() {
        for backend in [
            "deepgram-streaming",
            "openai-realtime",
            "openai-compatible-realtime",
        ] {
            let mut config = validatable_config(backend);
            config
                .llm_commands
                .push(llm_command("german", "Super+Shift+T"));

            let warnings = config.validate().unwrap();
            let warning = warnings
                .iter()
                .find(|w| w.message.contains("llm_commands run one-shot"))
                .unwrap_or_else(|| {
                    panic!(
                        "backend {backend} streams but llm_commands transcribe in one batch \
                         call; expected a warning, got: {warnings:?}"
                    )
                });
            assert!(
                warning
                    .message
                    .contains(&format!("backend = \"{backend}\"")),
                "the warning must name the backend: {}",
                warning.message
            );
            assert!(
                warning.message.contains("still work"),
                "the warning must say llm_commands degrade, not fail: {}",
                warning.message
            );
        }
    }

    #[test]
    fn config_validate_no_streaming_warning_for_batch_backend_llm_commands() {
        // local-whisper is deliberately absent from the streaming list here
        // (unlike the paste warning): its transcribe() is a real batch path.
        for backend in ["groq", "local-whisper"] {
            let mut config = validatable_config(backend);
            config
                .llm_commands
                .push(llm_command("german", "Super+Shift+T"));

            let warnings = config.validate().unwrap();
            assert!(
                warnings
                    .iter()
                    .all(|w| !w.message.contains("llm_commands run one-shot")),
                "backend {backend} has a real batch path; no streaming warning expected: \
                 {warnings:?}"
            );
        }
    }
}
