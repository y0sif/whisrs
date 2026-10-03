//! Configuration structs, defaults, and validation for `config.toml`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::hotkey;
use crate::llm;
use crate::transcription::deepgram;
use crate::transcription::openai_realtime_protocol::{
    openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, TurnDetectionMode,
};
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
    /// Recording-lifecycle hooks: media pause + shell commands on record start/stop.
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
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

/// Recording-lifecycle hooks. `media_auto_pause` pauses the MPRIS players
/// that are currently playing and resumes exactly those on stop (no external
/// tools). `on_record_start`/`on_record_stop` run shell commands
/// fire-and-forget when a recording session begins/ends.  The child inherits
/// the daemon's environment and stdout/stderr (goes to the journal under
/// systemd --user).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// Pause the MPRIS players that are playing when recording starts; resume
    /// exactly those on stop. Media the user paused themselves is left alone.
    #[serde(default)]
    pub media_auto_pause: bool,
    /// Shell command run when recording starts.
    #[serde(default)]
    pub on_record_start: Option<String>,
    /// Shell command run when recording stops.
    #[serde(default)]
    pub on_record_stop: Option<String>,
}

/// Visual configuration for the recording overlay.
///
/// The shape is intentionally clamped tight (90–120 × 36–48) to keep the
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
    /// Pill height in pixels (clamped to 36..=48).
    #[serde(default = "default_overlay_height")]
    pub height: u32,
    /// Screen corner or edge the pill sits at: `"bottom-center"` (default),
    /// `"bottom-left"`, `"bottom-right"`, `"top-left"`, `"top-center"` or
    /// `"top-right"`. `-middle` is accepted for `-center`. Unknown values
    /// fall back to `"bottom-center"`; [`Config::validate`] warns.
    #[serde(default = "default_overlay_position")]
    pub position: String,
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
fn default_overlay_position() -> String {
    "bottom-center".to_string()
}

/// Where the overlay pill is placed on screen. See [`OverlayConfig::position`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayPosition {
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
    TopLeft,
    TopCenter,
    TopRight,
}

/// Horizontal placement of the overlay pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayHAlign {
    Left,
    Center,
    Right,
}

impl OverlayPosition {
    /// Parse a config string (trimmed, case-insensitive). `None` if unknown.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "bottom-left" => Self::BottomLeft,
            "bottom-center" | "bottom-middle" => Self::BottomCenter,
            "bottom-right" => Self::BottomRight,
            "top-left" => Self::TopLeft,
            "top-center" | "top-middle" => Self::TopCenter,
            "top-right" => Self::TopRight,
            _ => return None,
        })
    }

    /// Canonical config spelling, also sent to the GNOME extension.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BottomLeft => "bottom-left",
            Self::BottomCenter => "bottom-center",
            Self::BottomRight => "bottom-right",
            Self::TopLeft => "top-left",
            Self::TopCenter => "top-center",
            Self::TopRight => "top-right",
        }
    }

    /// Whether the pill sits at the top edge (and grows down from it).
    pub fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopCenter | Self::TopRight)
    }

    pub fn h_align(self) -> OverlayHAlign {
        match self {
            Self::BottomLeft | Self::TopLeft => OverlayHAlign::Left,
            Self::BottomCenter | Self::TopCenter => OverlayHAlign::Center,
            Self::BottomRight | Self::TopRight => OverlayHAlign::Right,
        }
    }
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            theme: default_overlay_theme(),
            width: default_overlay_width(),
            height: default_overlay_height(),
            position: default_overlay_position(),
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
    /// Parsed [`Self::position`]; unknown values fall back to the default.
    pub fn position(&self) -> OverlayPosition {
        OverlayPosition::parse(&self.position).unwrap_or_default()
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
    /// Enable the on-screen recording overlay.
    #[serde(default)]
    pub overlay: bool,
    /// Run every finished dictation through the shared `[llm]` backend, using
    /// [`Self::llm_instruction`], before the text is injected. Off by default,
    /// so existing setups keep typing the raw transcript.
    ///
    /// Unlike `[[llm_commands]]` — which does the same rewrite but needs a
    /// dedicated hotkey per entry — this is simply on for `whisrs toggle`
    /// (issue #85). Batch path only, and that is not an oversight: streaming
    /// backends type partials at the cursor as they arrive, so there is never
    /// a whole transcript to hand the LLM. [`Config::validate`] warns when
    /// this is paired with one of them.
    #[serde(default)]
    pub llm_post_process: bool,
    /// Instruction applied to the transcript when [`Self::llm_post_process`]
    /// is on — the LLM's "voice instruction", the same role an
    /// `[[llm_commands]]` entry's `instruction` plays.
    ///
    /// Deliberately *not* [`Self::prompt`]: that one is a hint for the
    /// *transcription* backend and never reaches the LLM. Defaults to a
    /// conservative cleanup pass so flipping the flag alone does something
    /// sensible; blank means "post-process nothing" (and is warned about).
    #[serde(default = "default_llm_instruction")]
    pub llm_instruction: String,
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
            llm_post_process: false,
            llm_instruction: default_llm_instruction(),
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
    /// How long to wait, in milliseconds, for physically held modifier keys
    /// (Super, Alt, Ctrl, Shift) to be released before sending a synthetic
    /// keystroke with a batch backend or in command mode (typing, paste,
    /// line clear, selection copy). Default 10000.
    ///
    /// A keystroke sent while a modifier is held becomes a shortcut: stopping
    /// a dictation with `Super+W` and keeping Super down would otherwise turn
    /// the typed text into `Super+<letter>` compositor binds (#154). whisrs
    /// never types while a modifier is held. If one is still down when this
    /// cap runs out, the text is copied to the clipboard instead of typed,
    /// and a warning is logged.
    ///
    /// Streaming backends ignore this cap: each delta waits for the release
    /// however long it takes, later deltas queue behind it and are typed in
    /// order, and nothing goes to the clipboard. `whisrs cancel` ends the
    /// wait and discards the waiting text.
    ///
    /// `0` means don't wait: if a modifier is held, the text goes straight
    /// to the clipboard; otherwise it is typed at once. When `/dev/input`
    /// cannot be read, the check cannot see held keys and keys are sent as
    /// before.
    #[serde(default = "default_modifier_wait_ms")]
    pub modifier_wait_ms: u64,
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
    /// Leave the injected text in the system clipboard as a manual-fix
    /// fallback for silent injection failures. Off by default.
    ///
    /// Injection can fail silently: a compositor that drops keystrokes from
    /// a freshly-created uinput device, a TUI that eats characters, a window
    /// that loses focus mid-injection. When it does, nothing on screen tells
    /// you the text was mangled or lost. With this on, the final text is
    /// always left in the clipboard afterwards, so a single Ctrl+V pastes
    /// exactly what was dictated and you can correct from there instead of
    /// re-dictating.
    ///
    /// What "afterwards" means per path:
    /// - Typing mode (`paste = false`): the text is copied after the
    ///   keystroke injection runs, whether that injection succeeded or
    ///   failed — the clipboard copy *is* the fallback for the failure case.
    /// - Paste mode (`paste = true`): pasting already puts the text on the
    ///   clipboard, so the usual restore of the previous clipboard contents
    ///   is skipped entirely; the transcribed text simply stays there.
    /// - Streaming dictation: the full accumulated transcript is copied once
    ///   the recording stops. `whisrs cancel` copies nothing — cancel
    ///   discards, and it has to discard identically on both paths (the batch
    ///   path throws the audio away and never reaches an injection at all).
    ///
    /// Trade-off: the clipboard is clobbered on every dictation — anything
    /// copied beforehand is gone, and it is not restored. That is the point
    /// of the feature (the fallback only works because the text is there),
    /// but it also means the clipboard no longer survives a dictation. In
    /// paste mode a non-text clipboard (an image, a file list) is still
    /// protected: an unreadable clipboard makes the paste path fall back to
    /// typing without touching the clipboard, since overwriting content that
    /// can never be restored is worse than losing the fallback (issue #69).
    #[serde(default)]
    pub clipboard_fallback: bool,
    /// Copy-only mode: the final text is written to the system clipboard
    /// and never injected at the cursor — no keystrokes, no Ctrl+V.
    ///
    /// This is the terminal form of [`Self::clipboard_fallback`]: instead
    /// of *also* copying after injecting, the clipboard *is* the output.
    /// Dictation then works like a "dictate to clipboard" tool — record,
    /// stop, paste wherever you like. Command mode follows the same rule:
    /// the rewritten text lands in the clipboard and the selection is left
    /// untouched.
    ///
    /// Takes precedence over both `paste` and `clipboard_fallback` (they
    /// become no-ops), so `whisrsd` never injects while this is set.
    #[serde(default)]
    pub clipboard_only: bool,
    /// Extra window classes to treat as terminal emulators, checked alongside
    /// the built-in list. Empty by default.
    ///
    /// Terminal detection picks Ctrl+Shift+C / Ctrl+Shift+V over Ctrl+C /
    /// Ctrl+V and, in command mode, sends Ctrl+A then Ctrl+K to clear the
    /// prompt line before injecting. That last one *empties the field* if it
    /// fires in a GUI text input (#70), so the built-in list stays
    /// conservative and this is the explicit, opt-in escape hatch for the two
    /// cases it cannot know about: an `st` build with a custom `termname` in
    /// `config.h`, and scratchpad/dropdown classes such as `Alacritty-float`,
    /// `kitty-dropdown` or `wezterm-quake` (#92).
    ///
    /// Entries are compared case-insensitively against the *whole* focused
    /// window class. They are never substring-matched, and they never go
    /// through the reverse-DNS leaf stage the built-in list uses — so listing
    /// a generic name like `warp` matches a window whose class is exactly
    /// `warp`, and leaves `app.drey.Warp` (GNOME's Magic Wormhole client)
    /// alone. Write the class exactly as the compositor reports it
    /// (`hyprctl activewindow`, `niri msg focused-window`, `swaymsg -t
    /// get_tree`, `xprop WM_CLASS`). Sway reports `app_id` for Wayland views
    /// and `window_properties.class` for XWayland ones, falling back to
    /// `window_properties.instance`; on X11 the class is the second of the two
    /// strings in `WM_CLASS`, falling back to the first.
    #[serde(default)]
    pub terminal_classes: Vec<String>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            key_delay_ms: default_key_delay_ms(),
            modifier_wait_ms: default_modifier_wait_ms(),
            backend: InjectorBackend::default(),
            paste: false,
            clipboard_fallback: false,
            clipboard_only: false,
            terminal_classes: Vec::new(),
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
    /// Optional in the config file: an empty value means "use
    /// `WHISRS_DEEPGRAM_API_KEY` from the environment" (validation and the
    /// backend factories treat an empty key as absent).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_deepgram_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroqConfig {
    /// Optional in the config file: an empty value means "use
    /// `WHISRS_GROQ_API_KEY` from the environment" (validation and the
    /// backend factories treat an empty key as absent).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_groq_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    /// Optional in the config file: an empty value means "use
    /// `WHISRS_OPENAI_API_KEY` from the environment" (validation and the
    /// backend factories treat an empty key as absent).
    #[serde(default)]
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
    /// Path to the ggml model file. Optional in `config.toml`: a
    /// `[local-whisper]` section kept only for `segmentation` gets
    /// [`default_whisper_model_path`], the same path `whisrs setup`
    /// downloads to.
    #[serde(default = "default_whisper_model_path")]
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

impl Default for LocalWhisperConfig {
    /// What a fully absent `[local-whisper]` section resolves to. Matches
    /// what serde builds for a section that omits every key, so the daemon
    /// loads the same model either way.
    fn default() -> Self {
        Self::new(default_whisper_model_path())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalVoskConfig {
    /// Path to the Vosk model directory. Optional in `config.toml`: empty is
    /// the modelled "absent" state and [`Config::validate`] warns on it.
    #[serde(default)]
    pub model_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalParakeetConfig {
    /// Path to the Parakeet model directory. Optional in `config.toml`: empty
    /// is the modelled "absent" state and [`Config::validate`] warns on it.
    #[serde(default)]
    pub model_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsrSidecarConfig {
    #[serde(default = "default_asr_sidecar_url")]
    pub url: String,
    #[serde(default = "default_asr_sidecar_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatibleRealtimeConfig {
    /// WebSocket endpoint. Optional in `config.toml` so that omitting it
    /// reaches [`Config::validate`]'s "no WebSocket URL configured" error
    /// instead of failing the whole-config parse.
    #[serde(default)]
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
/// The `[local-whisper] model_path` a config gets when the section omits it.
///
/// Single source for the path `whisrs setup` downloads to: it is the serde
/// default on [`LocalWhisperConfig::model_path`], the fallback
/// [`Config::validate`] checks for existence, and (via
/// [`LocalWhisperConfig::default`]) the fallback the daemon's backend factory
/// uses when the whole section is absent.
///
/// `pub` rather than private like the other `default_*` helpers because that
/// factory lives in the `whisrsd` binary crate, which cannot see items private
/// to this one. Its `local_whisper_fallback_is_the_shared_default` test names
/// this function so the pin is a shared reference and not a fourth copy of the
/// literal, which is the divergence the pin exists to catch.
pub fn default_whisper_model_path() -> String {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("whisrs/models/ggml-base.en.bin")
        .to_string_lossy()
        .to_string()
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
/// Default toggle-path post-processing instruction. Conservative on purpose:
/// dictation is content, not a request, so the out-of-the-box behavior is a
/// cleanup pass that must not reword anything.
fn default_llm_instruction() -> String {
    "Fix punctuation, capitalization and obvious transcription errors in the following text. \
     Keep the wording and the meaning unchanged. Return only the corrected text, with no \
     explanations and no quotes."
        .to_string()
}
fn default_key_delay_ms() -> u64 {
    2
}
fn default_modifier_wait_ms() -> u64 {
    10_000
}
/// The `[deepgram] model` a config gets when the section omits it.
///
/// `pub(crate)` so `whisrs setup` writes this rather than its own copy of the
/// string. It is the single source for the default: [`Config::deepgram_model`]
/// routes through it, and the daemon's model resolution routes through that.
pub(crate) fn default_deepgram_model() -> String {
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

/// One step of a path into a parsed `config.toml` document: a table key or an
/// index into an array of tables (`[[llm_commands]]`).
#[derive(Debug, Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

/// Keys in a parsed `config.toml` document that the configuration schema
/// does not know.
///
/// The known set is derived from serde itself: the parsed [`Config`] is
/// serialized back into a `toml::Table`, and the two tables are diffed
/// recursively. A key present in the document but absent from the
/// reserialization is a *candidate* the running binary may have silently
/// dropped; `key_is_ignored` then confirms each one by pruning it and
/// re-parsing. This avoids a hand-maintained field-name list, which would go
/// stale, and avoids a new dependency.
///
/// Returns `[]` when the document is not valid TOML or does not deserialize,
/// because those cases already produce their own error in the daemon's
/// `load_config`.
pub fn unknown_config_keys(contents: &str) -> Vec<String> {
    let mut unknown: Vec<String> = unknown_config_key_paths(contents)
        .iter()
        .map(|path| render_path(path))
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Everything the unknown-key analysis derives from one config file: the parsed
/// document, the reserialized (schema-known) view of it, and the confirmed
/// unknown key paths. Kept together so a caller that needs more than the
/// rendered list ([`PreservedKeys`]) never repeats the expensive confirmation.
struct UnknownKeyScan {
    document: toml::Table,
    known: toml::Value,
    paths: Vec<Vec<Seg>>,
}

/// Why a config file has no unknown-key scan, and so no [`PreservedKeys`] set
/// describing it.
///
/// Every variant means the same thing to a caller about to rewrite the file:
/// there is no preserve set for these bytes, so merging the reserialized struct
/// into them would delete every key the schema does not carry — issue #134. The
/// `Display` text completes the sentence "existing config at `<path>` …", which
/// is how `write_config_to` reports the divert.
#[derive(Debug)]
pub(crate) enum UndescribableConfig {
    /// Not valid TOML at all.
    Unparseable(toml::de::Error),
    /// Valid TOML that does not deserialize into [`Config`]: a type error, or an
    /// alias section beside the canonical one (`duplicate field`).
    NotAConfig(toml::de::Error),
    /// A [`Config`] that does not survive `toml::Value::try_from`, so there is
    /// nothing to diff the document against. Unreachable today — every field is
    /// `#[serde(default)]` and the `Option` sections serialize as omitted — and
    /// named rather than folded into the others so that if a field ever does
    /// stop reserializing, the divert is the documented outcome instead of a
    /// silent merge against an empty preserve set.
    Unserializable(toml::ser::Error),
}

impl fmt::Display for UndescribableConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Same wording as `write_config_to`'s own `DocumentMut` parse arm,
            // which rejects an unparseable file before this is ever consulted.
            Self::Unparseable(e) => write!(f, "is not valid TOML ({e})"),
            Self::NotAConfig(e) => write!(f, "does not deserialize into a whisrs config ({e})"),
            Self::Unserializable(e) => {
                write!(f, "does not round-trip through the config schema ({e})")
            }
        }
    }
}

/// Run the diff prefilter and the pruning confirmation over `contents`.
///
/// Fails in exactly the cases [`unknown_config_keys`] reports nothing for: not
/// valid TOML, valid TOML that does not deserialize, or a `Config` that does not
/// reserialize.
fn scan_unknown_keys(contents: &str) -> Result<UnknownKeyScan, UndescribableConfig> {
    let document = contents
        .parse::<toml::Table>()
        .map_err(UndescribableConfig::Unparseable)?;
    let config = toml::from_str::<Config>(contents).map_err(UndescribableConfig::NotAConfig)?;
    let known = toml::Value::try_from(&config).map_err(UndescribableConfig::Unserializable)?;
    let mut candidates = Vec::new();
    diff_config_tables(
        &document,
        known.as_table().expect("Config serializes to a table"),
        &[],
        &mut candidates,
    );
    // The diff is only a prefilter, so a valid config pays for nothing: with no
    // candidates there is no second parse.
    let paths = candidates
        .into_iter()
        .filter(|path| key_is_ignored(&document, &known, path))
        .collect();
    Ok(UnknownKeyScan {
        document,
        known,
        paths,
    })
}

/// The confirmed-unknown key paths of `contents`, before rendering. Same walk
/// (and same cost) as [`unknown_config_keys`], which is its only difference
/// from that function's `Vec<String>`.
fn unknown_config_key_paths(contents: &str) -> Vec<Vec<Seg>> {
    scan_unknown_keys(contents)
        .map(|scan| scan.paths)
        .unwrap_or_default()
}

/// The one message the user gets about unknown config keys, shared by every
/// path that loads a config: the daemon at startup, `whisrs setup`, and
/// `whisrs config` (issue #116 — only the daemon used to say anything).
///
/// Returns `None` when there is nothing to report: the running binary does not
/// read these keys. `write_config` keeps almost all of them in the file rather
/// than deleting them — the exception is a section that is entirely unknown but
/// whose *name* is a serde alias for one the writer emits (`[asr]`,
/// `[vibevoice]`, `[local]`), which is dropped whole so the rewritten file
/// cannot carry both spellings (see [`PreservedKeys`]).
pub fn unknown_keys_warning(config_path: &Path, unknown: &[String]) -> Option<String> {
    if unknown.is_empty() {
        return None;
    }
    Some(format!(
        "Unknown keys in config at {} ignored: {}",
        config_path.display(),
        unknown.join(", ")
    ))
}

/// The confirmed-unknown keys of one config file, shaped like the document so
/// the format-preserving writer can look them up as it walks it.
///
/// `write_config` rebuilds the on-disk file to mirror the [`Config`] struct,
/// which used to delete every key the struct never knew about — including the
/// very typo the load-time warning had just told the user to fix (issue #116).
/// Keys named here survive that rewrite instead.
///
/// Build this from the *on-disk* text on every write, never from the fresh
/// serialization and never cached: it describes bytes that are already in the
/// file, and a stale set would make repeated writes non-byte-stable.
#[derive(Debug, Default)]
pub(crate) struct PreservedKeys {
    /// Confirmed-unknown leaf keys at this level.
    leaves: BTreeSet<String>,
    /// Sub-tables holding preserved paths.
    tables: BTreeMap<String, PreservedKeys>,
    /// Array-of-tables elements holding preserved paths, keyed by their
    /// **on-disk** index — `Seg::Index` comes from walking the document, so a
    /// caller that has re-matched entries must not index with a fresh one.
    elements: BTreeMap<usize, PreservedKeys>,
    /// Whether deleting the *whole* on-disk table this node describes leaves a
    /// file that still deserializes to the same [`Config`]. See
    /// [`PreservedKeys::table_prunable`].
    table_prunable: bool,
}

/// The empty set, returned for every level of the document with nothing to
/// preserve — which is every level of a config that has no unknown keys.
static EMPTY_PRESERVED: PreservedKeys = PreservedKeys {
    leaves: BTreeSet::new(),
    tables: BTreeMap::new(),
    elements: BTreeMap::new(),
    table_prunable: false,
};

impl PreservedKeys {
    /// See [`EMPTY_PRESERVED`].
    pub(crate) const EMPTY: &'static PreservedKeys = &EMPTY_PRESERVED;

    /// Collect the confirmed-unknown keys of a config file.
    ///
    /// Fails rather than returning an empty set when the file cannot be
    /// described at all, so a caller that is about to merge into those bytes
    /// cannot mistake "nothing to preserve" for "nothing describable here"
    /// (issue #134). The empty set means the file was understood and has no
    /// unknown keys; [`UndescribableConfig`] means the merge must not run.
    pub(crate) fn from_config_str(contents: &str) -> Result<Self, UndescribableConfig> {
        let scan = scan_unknown_keys(contents)?;
        let mut root = Self::default();
        for path in &scan.paths {
            root.insert(path);
        }
        root.record_prunable_tables(&scan.document, &scan.document, &scan.known, &[]);
        Ok(root)
    }

    /// True when nothing at this level (or below it) is preserved.
    pub(crate) fn is_empty(&self) -> bool {
        self.leaves.is_empty() && self.tables.is_empty() && self.elements.is_empty()
    }

    /// Whether `key` is a confirmed-unknown leaf at this level.
    pub(crate) fn contains_leaf(&self, key: &str) -> bool {
        self.leaves.contains(key)
    }

    /// The node for the sub-table `key`, or the empty set.
    pub(crate) fn table(&self, key: &str) -> &PreservedKeys {
        self.tables.get(key).unwrap_or(Self::EMPTY)
    }

    /// The node for the array-of-tables element at **on-disk** `index`, or the
    /// empty set.
    pub(crate) fn element(&self, index: usize) -> &PreservedKeys {
        self.elements.get(&index).unwrap_or(Self::EMPTY)
    }

    /// Whether removing this node's entire on-disk table would leave a file
    /// that still deserializes to the same [`Config`] — true for `[bogus]`,
    /// false for `[asr]`.
    ///
    /// Only the writer can tell whether that question is even being asked:
    /// keeping a whole table matters only when the table is *absent from the
    /// fresh serialization*, and this module cannot see the fresh document.
    /// So the answer is recorded here and consulted there, in
    /// `setup::is_preserved`, which by construction runs only for keys `fresh`
    /// does not carry. Deciding it eagerly here instead is what made the guard
    /// eat `[hooks]`, `[hotkeys]` and `[overlay.colors]` — sections `fresh`
    /// *does* carry, where the whole-table question never comes up.
    pub(crate) fn table_prunable(&self) -> bool {
        self.table_prunable
    }

    fn insert(&mut self, path: &[Seg]) {
        match path {
            // A bare index is never produced: the walk only reports leaf keys.
            [] | [Seg::Index(_)] => {}
            [Seg::Key(key)] => {
                self.leaves.insert(key.clone());
            }
            [Seg::Key(key), rest @ ..] => self.tables.entry(key.clone()).or_default().insert(rest),
            [Seg::Index(index), rest @ ..] => self.elements.entry(*index).or_default().insert(rest),
        }
    }

    /// Record, for every node the writer could keep as a *whole* table, whether
    /// removing that table changes how the file parses.
    ///
    /// `[asr] bogus = 1` is the case that forces this. Every leaf of `[asr]` is
    /// confirmed-unknown — pruning `bogus` leaves an empty table that still
    /// deserializes to a default `asr-sidecar` section — so the leaf rule alone
    /// keeps `[asr]` while the writer also inserts the canonical
    /// `[asr-sidecar]`. `asr` is a serde *alias* for that section, so the file
    /// then fails to deserialize with `duplicate field`, the daemon falls back
    /// to defaults, and the user's whole config is discarded: strictly worse
    /// than the missing-warning bug. `[asr]` does not prune cleanly, so the
    /// flag says so and the writer drops it.
    ///
    /// This only *records* the answer; nothing is dropped here. Acting on it
    /// eagerly deleted the nodes for `[hooks]`, `[hotkeys]`, `[overlay]` and
    /// `[llm]` too, because those sections are all-optional structs whose
    /// removal also changes the parse (`Some(default)` becomes `None`) — and
    /// they are the exact typo'd sections this feature exists to protect.
    /// The difference the writer can see, and this module cannot, is whether
    /// the fresh serialization already carries the key: see
    /// [`PreservedKeys::table_prunable`].
    ///
    /// Costs one extra prune-and-reparse per fully-unknown table, and nothing
    /// at all for a config with no unknown keys.
    ///
    /// # The guard is safe by accident, not by construction
    ///
    /// The question it asks is "does deleting this whole table change the
    /// parse?", and it stands in for the one that actually matters: "is this
    /// table name a serde alias for a section the writer also emits?". Those
    /// two agree only because **every aliased section in the schema is an
    /// `Option<_>`** (`Config::local_whisper`, `Config::asr_sidecar`): deleting
    /// `[asr]` turns `Some(default)` into `None`, the parse changes, the guard
    /// says "not prunable", and the writer drops it instead of emitting it
    /// beside the canonical `[asr-sidecar]`.
    ///
    /// Add `#[serde(alias = ...)]` to a **non-`Option`** `#[serde(default)]`
    /// section and that stops holding. Deleting such a section reparses to the
    /// same default, so the guard calls it prunable, the writer keeps the
    /// aliased spelling *and* writes the canonical one, serde rejects the
    /// result with `duplicate field`, and the daemon falls back to defaults —
    /// the user's entire config, silently discarded. That is strictly worse
    /// than the key-eating bug this module exists to fix. Demonstrated by
    /// adding `alias = "in"` to the non-`Option` `Config::input`: `[in] bogus =
    /// 1` rewrites to a file holding both `[in]` and `[input]`.
    ///
    /// This is the same trap as the defaulted trait method in CLAUDE.md
    /// (`WindowTracker::get_focused_window_class`): a rule that happens to hold
    /// for every implementor today and fails silently for the next one. If you
    /// need such an alias, do not rely on this guard — decide alias-ness
    /// explicitly (compare the on-disk table name against the schema's alias
    /// list) rather than inferring it from a reparse.
    /// `every_serde_alias_names_an_optional_section` is the tripwire: it fails
    /// the moment an alias lands on a non-`Option` field.
    fn record_prunable_tables(
        &mut self,
        doc: &toml::Table,
        document: &toml::Table,
        known: &toml::Value,
        prefix: &[Seg],
    ) {
        for key in self.tables.keys().cloned().collect::<Vec<_>>() {
            let Some(child_doc) = doc.get(&key) else {
                continue;
            };
            let mut path = prefix.to_vec();
            path.push(Seg::Key(key.clone()));
            match child_doc {
                toml::Value::Table(child_doc) => {
                    // Confirm only what the writer could act on: a node that
                    // does not cover its table is never kept whole, so it never
                    // needs the flag and never pays for one.
                    let prunable = self
                        .tables
                        .get(&key)
                        .is_some_and(|child| child.covers(child_doc))
                        && key_is_ignored(document, known, &path);
                    if let Some(child) = self.tables.get_mut(&key) {
                        child.table_prunable = prunable;
                        // Descend even through a covered table: the writer asks
                        // the same question again for every sub-table inside a
                        // section it keeps (`[hooks.bogus]` under `[hooks]`).
                        child.record_prunable_tables(child_doc, document, known, &path);
                    }
                }
                // `[[llm_commands]]`: the array itself is always in `fresh`, so
                // only the tables *inside* its entries can be kept whole.
                toml::Value::Array(array) => {
                    if let Some(child) = self.tables.get_mut(&key) {
                        child.record_prunable_elements(array, document, known, &path);
                    }
                }
                _ => {}
            }
        }
    }

    /// [`Self::record_prunable_tables`] for the array-of-tables elements at
    /// this level, indexed by their **on-disk** position.
    fn record_prunable_elements(
        &mut self,
        array: &[toml::Value],
        document: &toml::Table,
        known: &toml::Value,
        prefix: &[Seg],
    ) {
        for index in self.elements.keys().copied().collect::<Vec<_>>() {
            let Some(toml::Value::Table(element_doc)) = array.get(index) else {
                continue;
            };
            let mut path = prefix.to_vec();
            path.push(Seg::Index(index));
            if let Some(child) = self.elements.get_mut(&index) {
                child.record_prunable_tables(element_doc, document, known, &path);
            }
        }
    }

    /// Whether every leaf of `doc` is named by this node — i.e. whether the
    /// writer would keep the whole table rather than individual keys inside it.
    ///
    /// A sub-table with *no leaves at all* (`t = {}`, `a = { b = {} }`, a bare
    /// `[bogus.emptysub]` header) satisfies that vacuously, and it has no node
    /// here — there was no leaf to build one from. Reading a missing node as
    /// "not covered" is what let a single stray `{}` anywhere inside `[bogus]`
    /// delete the entire section, comments and real keys included: issue #116's
    /// own symptom, in the code written to fix it. Hence [`has_no_leaves`]
    /// rather than `is_some_and`, and no `!doc.is_empty()` guard — an empty
    /// table covers nothing, which is not the same as failing to cover.
    fn covers(&self, doc: &toml::Table) -> bool {
        doc.iter().all(|(key, value)| match value {
            toml::Value::Table(inner) => match self.tables.get(key) {
                Some(child) => child.covers(inner),
                None => has_no_leaves(inner),
            },
            _ => self.leaves.contains(key),
        })
    }
}

/// Whether `doc` holds no leaf keys at all — only (possibly nested) empty
/// tables. "Every leaf of this subtree is confirmed-unknown" is then vacuously
/// true, so such a table must never veto its parent's preservation.
///
/// Mirrors [`collect_unknown_leaves`]: anything that is not a table is a leaf,
/// arrays included.
fn has_no_leaves(doc: &toml::Table) -> bool {
    doc.values().all(|value| match value {
        toml::Value::Table(inner) => has_no_leaves(inner),
        _ => false,
    })
}

/// Render a path as the dotted form shown to the user (`input.past`,
/// `llm_commands[0].bogus`).
fn render_path(path: &[Seg]) -> String {
    let mut rendered = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !rendered.is_empty() {
                    rendered.push('.');
                }
                rendered.push_str(key);
            }
            Seg::Index(index) => rendered.push_str(&format!("[{index}]")),
        }
    }
    rendered
}

/// Whether the binary genuinely ignores the key at `path`, decided by removing
/// it and re-parsing.
///
/// The reserialize-and-diff prefilter cannot see `#[serde(alias = "...")]`:
/// serde accepts the alias but emits the *canonical* name, so every alias key
/// (`[hotkeys] read`, the `[local]` and `[asr]` sections) is absent from the
/// reserialized table and looks unknown while actually driving a field. Pruning
/// settles it — if the config is byte-identical without the key, nothing read
/// it; if it changes, or the pruned document no longer deserializes, the key
/// fed a field under a name serde accepts but does not emit. Do not "simplify"
/// this away: without it the daemon warns about working settings on every start.
fn key_is_ignored(document: &toml::Table, known: &toml::Value, path: &[Seg]) -> bool {
    let mut pruned = toml::Value::Table(document.clone());
    if !prune_path(&mut pruned, path) {
        // Unreachable — the path came from walking this same document. Keep the
        // prefilter's verdict rather than silently dropping the warning.
        return true;
    }
    let Ok(config) = pruned.try_into::<Config>() else {
        return false;
    };
    let Ok(reserialized) = toml::Value::try_from(&config) else {
        return false;
    };
    // Compare rendered forms rather than `==`: `toml::Value` equality is float
    // equality, so a single `nan` in the document (`audio_feedback_volume` is
    // the one float) would make every comparison false and silence every
    // warning for the whole file.
    format!("{reserialized:?}") == format!("{known:?}")
}

/// Remove the value at `path` from `value`. Returns whether anything was removed.
fn prune_path(value: &mut toml::Value, path: &[Seg]) -> bool {
    match path {
        [] => false,
        [Seg::Key(key)] => value
            .as_table_mut()
            .is_some_and(|table| table.remove(key).is_some()),
        [Seg::Index(index)] => match value.as_array_mut() {
            Some(array) if *index < array.len() => {
                array.remove(*index);
                true
            }
            _ => false,
        },
        [Seg::Key(key), rest @ ..] => value
            .as_table_mut()
            .and_then(|table| table.get_mut(key))
            .is_some_and(|inner| prune_path(inner, rest)),
        [Seg::Index(index), rest @ ..] => value
            .as_array_mut()
            .and_then(|array| array.get_mut(*index))
            .is_some_and(|inner| prune_path(inner, rest)),
    }
}

/// Collect every leaf key of `table` (below `prefix`) into `out`.
fn collect_unknown_leaves(table: &toml::Table, prefix: &[Seg], out: &mut Vec<Vec<Seg>>) {
    for (key, value) in table {
        let mut path = prefix.to_vec();
        path.push(Seg::Key(key.clone()));
        match value {
            toml::Value::Table(inner) => collect_unknown_leaves(inner, &path, out),
            _ => out.push(path),
        }
    }
}

/// Recursively compare a parsed document table against the reserialized
/// (schema-known) table, appending the paths of candidate unknown keys to `out`.
fn diff_config_tables(
    document: &toml::Table,
    known: &toml::Table,
    prefix: &[Seg],
    out: &mut Vec<Vec<Seg>>,
) {
    for (key, value) in document {
        let mut path = prefix.to_vec();
        path.push(Seg::Key(key.clone()));
        match known.get(key) {
            None => match value {
                // A whole section the schema dropped (e.g. an Option section
                // whose only keys are unknown): report each leaf, so the user
                // learns which key inside it is the typo.
                toml::Value::Table(inner) => collect_unknown_leaves(inner, &path, out),
                _ => out.push(path),
            },
            Some(toml::Value::Table(known_table)) => {
                if let toml::Value::Table(document_table) = value {
                    diff_config_tables(document_table, known_table, &path, out);
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
                            let mut item_path = path.clone();
                            item_path.push(Seg::Index(index));
                            diff_config_tables(document_table, known_table, &item_path, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// What a `[general] backend` name is, for the two readers of
/// [`BACKEND_NAMES`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendNameKind {
    /// A backend that transcribes. These are the names the unknown-backend
    /// error advertises as "Valid options".
    Primary,
    /// An accepted second spelling of a primary — `local`, `asr`,
    /// `vibevoice`. [`Config::validate`] routes them, so every gate keyed on
    /// the backend string has to answer for them too, but they are not
    /// advertised: one name per backend in the error message.
    Alias,
    /// Parses and routes, then bails at transcription time with "not yet
    /// implemented" (`local-vosk`, `local-parakeet`). Named in the error only
    /// to say *not* to pick one, and never recommended by a warning — see
    /// `assert_no_stub_backend_advice`.
    Stub,
}

/// One `[general] backend` string [`Config::validate`] accepts.
struct BackendName {
    name: &'static str,
    kind: BackendNameKind,
}

/// Every `[general] backend` string [`Config::validate`] accepts, in the order
/// the unknown-backend error lists them.
///
/// One list, three readers, which is the point. `validate` rejects anything
/// not in here *before* the per-backend match, so a name absent from this
/// const cannot be selected at all however many match arms it grows; the
/// unknown-backend message is built from it, so the "Valid options" list
/// cannot drift from what is actually routed; and
/// `config_inert_prompt_gate_agrees_with_every_sends_prompt_impl` iterates it
/// to require a case per name, so a new backend whose `sends_prompt` is false
/// cannot reach a user without the inert-prompt gate being taught about it.
///
/// That chain is the whole reason this is a const rather than three hand-kept
/// lists. The test used to enumerate its own cases: a reviewer added a
/// promptless `acme-realtime` to `validate` and to `create_backend`, touched
/// neither the gate nor the test, and all 663 tests stayed green.
const BACKEND_NAMES: &[BackendName] = &[
    BackendName {
        name: "deepgram",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "deepgram-streaming",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "groq",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "openai",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "openai-realtime",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "openai-compatible-realtime",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "local-whisper",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "asr-sidecar",
        kind: BackendNameKind::Primary,
    },
    BackendName {
        name: "local",
        kind: BackendNameKind::Alias,
    },
    BackendName {
        name: "asr",
        kind: BackendNameKind::Alias,
    },
    BackendName {
        name: "vibevoice",
        kind: BackendNameKind::Alias,
    },
    BackendName {
        name: "local-vosk",
        kind: BackendNameKind::Stub,
    },
    BackendName {
        name: "local-parakeet",
        kind: BackendNameKind::Stub,
    },
];

/// The names of every [`BACKEND_NAMES`] entry of one kind, in order.
fn backend_names_of(kind: BackendNameKind) -> Vec<&'static str> {
    BACKEND_NAMES
        .iter()
        .filter(|b| b.kind == kind)
        .map(|b| b.name)
        .collect()
}

/// What `[general] vocabulary` actually reaches on Deepgram, given the
/// `[deepgram] model` in this config and the terms in the list.
///
/// Deepgram is the one backend with a second hint channel — the vocabulary
/// rides as `keyterm` query params instead of in a prompt — which makes it the
/// one backend the other warnings in this file can send a user to. Both ways
/// that channel can be dead are in the user's own config rather than in
/// Deepgram: a pre-Nova-3 model rejects the parameter outright, and the
/// keyterm limits can drop every term of a list on a model that does take it.
/// A recommendation that ignores either one names a way out that does not
/// work, which is the one thing a warning here may never do.
///
/// Resolved by [`Config::deepgram_hint_channel`], read twice by
/// [`Config::inert_prompt_warnings`]: once to pick the Deepgram prompt
/// message, once to decide whether the Lemonade vocabulary message may offer
/// deepgram at all.
enum DeepgramHintChannel {
    /// The model takes `keyterm` and the configured terms reach the wire — or
    /// there are no terms yet, so terms added now would. Pointing a user here
    /// is real advice.
    Live,
    /// The model takes `keyterm`, but every configured term is dropped by the
    /// keyterm limits, so the channel carries nothing as configured. Carries
    /// the usable count, which is the number
    /// [`Config::deepgram_keyterm_warnings`] reports as "0 of N".
    NothingFits { usable: usize },
    /// The model rejects `keyterm` outright: it is a Nova-3/Flux feature and
    /// Deepgram answers 400 on anything older.
    Unsupported,
}

impl Config {
    /// Validate the configuration and return a list of warnings.
    ///
    /// Returns `Err` for fatal issues (e.g., no backend configured).
    /// Returns `Ok(warnings)` with non-fatal warnings.
    pub fn validate(&self) -> Result<Vec<ConfigWarning>, WhisrsError> {
        let mut warnings = Vec::new();
        let backend = self.general.backend.as_str();

        // Routability is decided here, off [`BACKEND_NAMES`], and not by the
        // arms below. A backend that grows a match arm without an entry in
        // that const is rejected as unknown, which is what makes the const the
        // single place a new backend has to be registered — and therefore what
        // makes the inert-prompt gate test, which iterates the same const, an
        // actual totality check rather than a hand-kept list that agrees with
        // another hand-kept list.
        if !BACKEND_NAMES.iter().any(|b| b.name == backend) {
            return Err(WhisrsError::Config(format!(
                "Unknown backend '{backend}'. Valid options: {}. ({} are also \
                 accepted here but are not implemented yet — they fail at \
                 transcription time, so do not pick one to get out of this error.)",
                backend_names_of(BackendNameKind::Primary).join(", "),
                backend_names_of(BackendNameKind::Stub).join(" and ")
            )));
        }

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
                    .unwrap_or_else(default_whisper_model_path);
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
            // Unreachable for an unknown name: the [`BACKEND_NAMES`] check
            // above has already returned. A name that is in the const but has
            // no arm here simply has no config prerequisites to check — the
            // aliases fall through to their primary's arm above, so this is
            // the empty case, not a silent skip of a check that exists.
            _ => {}
        }

        if let Some(overlay) = &self.overlay {
            if OverlayPosition::parse(&overlay.position).is_none() {
                warnings.push(ConfigWarning {
                    message: format!(
                        "[overlay] position = {:?} is not a known position, using \
                         \"bottom-center\". Use one of: bottom-left, bottom-center, \
                         bottom-right, top-left, top-center, top-right.",
                        overlay.position
                    ),
                });
            }
        }

        if self.general.silence_timeout_ms == 0 {
            warnings.push(ConfigWarning {
                message: "silence_timeout_ms is 0 — auto-stop is effectively disabled".to_string(),
            });
        }

        warnings.extend(self.deepgram_keyterm_warnings(backend));
        warnings.extend(self.inert_prompt_warnings(backend));

        // Streaming backends (including local-whisper, which always streams
        // regardless of its `segmentation` mode) type dictated text
        // incrementally as it arrives and never go through the
        // paste-injection path, so `[input] paste` does not apply to
        // dictation with them. Command mode is unaffected whatever backend
        // transcribed the instruction: the instruction is never injected, and
        // the LLM result goes out in a single injection call through the same
        // wrapper the batch dictation path uses.
        //
        // The "switch to" list must stay limited to backends that actually
        // transcribe: `local-vosk` and `local-parakeet` parse as valid config
        // but their `transcribe()` bails with "not yet implemented", so
        // recommending them would trade a no-op flag for broken dictation.
        // Keep them out of every recommendation here until they are real.
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
                     a non-streaming backend (deepgram, groq, openai, asr-sidecar) to use \
                     paste injection for dictation too."
                ),
            });
        }

        // `[input] clipboard_only` is the terminal form of copy-to-clipboard:
        // nothing is ever injected, so both of the injection-shaping keys
        // below are accepted and then ignored. A key that parses fine and
        // silently does nothing is the failure mode this project warns about
        // at load time rather than leaving to be discovered in the journal.
        if self.input.clipboard_only && self.input.paste {
            warnings.push(ConfigWarning {
                message: "[input] paste = true is ignored while clipboard_only = true: \
                          copy-only mode never injects, so there is no paste to perform. \
                          Set clipboard_only = false to paste at the cursor again."
                    .to_string(),
            });
        }

        if self.input.clipboard_only && self.input.clipboard_fallback {
            warnings.push(ConfigWarning {
                message: "[input] clipboard_fallback = true is ignored while \
                          clipboard_only = true: the fallback copies the text *in addition* \
                          to injecting it, and copy-only mode already copies it and never \
                          injects. Set clipboard_only = false to get injection plus the \
                          clipboard copy."
                    .to_string(),
            });
        }

        // Toggle-path LLM post-processing (issue #85). Same shape as the
        // llm_commands block below — missing [llm] section, empty instruction,
        // streaming backend — but the failure modes differ, so the wording
        // does too.
        if self.general.llm_post_process {
            if self.llm.is_none() {
                warnings.push(ConfigWarning {
                    message: "[general] llm_post_process = true but no [llm] section — add \
                              [llm] api_key (or set WHISRS_OPENAI_API_KEY / \
                              WHISRS_GROQ_API_KEY) or every dictation will fall back to the \
                              raw transcript"
                        .to_string(),
                });
            }

            if self.general.llm_instruction.trim().is_empty() {
                warnings.push(ConfigWarning {
                    message: "[general] llm_post_process = true but llm_instruction is empty \
                              — there is nothing to apply, so dictation is typed unmodified"
                        .to_string(),
                });
            }

            // Same backend list as the `[input] paste` warning above — both
            // the matched set and the recommended replacements, and for the
            // same reasons (see the note there on the unimplemented stubs).
            // With these, dictation never reaches the batch path, so there is
            // never a whole transcript to post-process. local-whisper belongs
            // here even though its `transcribe()` is a real batch path (which
            // is why the llm_commands warning below excludes it) — dictation
            // with it always streams. Unlike llm_commands there is no degraded
            // mode: the flag does nothing at all.
            if matches!(
                backend,
                "deepgram-streaming"
                    | "openai-realtime"
                    | "openai-compatible-realtime"
                    | "local-whisper"
                    | "local"
            ) {
                warnings.push(ConfigWarning {
                    message: format!(
                        "[general] llm_post_process = true does not apply to dictation with \
                         backend = \"{backend}\": streaming backends (deepgram-streaming, \
                         openai-realtime, openai-compatible-realtime, local-whisper) type text \
                         incrementally as it arrives, so there is never a whole transcript to \
                         post-process. Nothing runs — dictation is typed unmodified. Switch to \
                         a non-streaming backend (deepgram, groq, openai, asr-sidecar) to \
                         post-process dictation, or use an [[llm_commands]] hotkey, which \
                         works whatever the backend."
                    ),
                });
            }
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

    /// The Deepgram model this config will actually transcribe with.
    ///
    /// `[deepgram]` is an optional section: a config that names the backend but
    /// omits the section still gets the section's own serde default. Resolving
    /// through [`default_deepgram_model`] rather than repeating the string
    /// keeps the keyterm gate below in step with that default — a literal copy
    /// rots silently the day the default moves.
    ///
    /// `pub`, not private, because the model that actually goes on the wire is
    /// resolved by the daemon's `get_model_for_backend`, and the daemon is a
    /// separate binary crate. It used to carry its own `"nova-3"` literal, and
    /// that literal was unpinned: flipping it to `"nova-2"` left the whole test
    /// suite green while every request would have 400'd with `validate` silent,
    /// because the gate here inspected a different string than the wire used.
    /// One function, one answer.
    pub fn deepgram_model(&self) -> String {
        self.deepgram
            .as_ref()
            .map(|d| d.model.clone())
            .unwrap_or_else(default_deepgram_model)
    }

    /// The OpenAI realtime model this config will actually transcribe with.
    ///
    /// Not [`default_openai_model`], on purpose. `[openai] model` is shared
    /// with the plain `openai` REST backend, whose serde default is
    /// `gpt-4o-mini-transcribe` — a different string, and one that maps to a
    /// different turn-detection mode. Resolving the realtime fallback through
    /// it would silently flip the gate in [`Config::inert_prompt_warnings`] to
    /// the opposite answer from the one the wire gets.
    ///
    /// `pub` for the same reason [`Config::deepgram_model`] above is, and
    /// `get_model_for_backend` calls it rather than keeping its own literal.
    /// `whisrs setup` still writes the string by hand into the `[openai]`
    /// section it creates; that copy is harmless, because the value is read
    /// back through here, and it is the last one.
    pub fn openai_realtime_model(&self) -> String {
        self.openai
            .as_ref()
            .map(|o| o.model.clone())
            .unwrap_or_else(|| "gpt-realtime-whisper".to_string())
    }

    /// Load-time warnings about `[general] vocabulary` reaching Deepgram.
    ///
    /// The vocabulary rides to Deepgram as repeated `keyterm` query params, and
    /// both ways it can fail to arrive are invisible at run time: the backend
    /// logs the drop at `debug!`, which the daemon's default `info` filter
    /// hides. Say it once at load instead, the same way the GNOME/KDE
    /// window-tracker gap is reported.
    ///
    /// The `backend` gate is load-bearing, not decoration. `whisrs setup`
    /// writes a `[deepgram]` section, and people leave it behind when they
    /// switch backends — without the gate a `backend = "groq"` user with a
    /// stale `[deepgram] model = "nova-2"` and any vocabulary at all gets a
    /// bogus "vocabulary is ignored" warning on every daemon start, about a
    /// backend they are not using.
    fn deepgram_keyterm_warnings(&self, backend: &str) -> Vec<ConfigWarning> {
        let mut warnings = Vec::new();
        if !matches!(backend, "deepgram" | "deepgram-streaming") {
            return warnings;
        }

        // Blank entries are not terms, so a vocabulary of nothing but blanks
        // has nothing to warn about.
        let usable = deepgram::usable_keyterms(&self.general.vocabulary).count();
        if usable == 0 {
            return warnings;
        }

        let model = self.deepgram_model();
        if !deepgram::supports_keyterm(&model) {
            warnings.push(ConfigWarning {
                message: format!(
                    "[general] vocabulary is ignored with [deepgram] model = \"{model}\": \
                     keyterm prompting is a Nova-3/Flux feature and Deepgram rejects the \
                     parameter on older models, so the {usable} term(s) are dropped from every \
                     request. Switch to a nova-3 model to bias transcription toward them."
                ),
            });
            return warnings;
        }

        // One list, one count: `effective_keyterms` is the same function the
        // request builder slices with, so the number named here is the number
        // that goes on the wire.
        //
        // "N of M", not "the first N": `effective_keyterms` skips a term that
        // does not fit and keeps going, so the surviving terms are not a prefix
        // of the list.
        let effective = deepgram::effective_keyterms(&self.general.vocabulary).len();
        if effective < usable {
            warnings.push(ConfigWarning {
                message: format!(
                    "[general] vocabulary: {effective} of {usable} usable term(s) reach \
                     Deepgram. Keyterms are capped at {} bytes of query string, {} terms and \
                     {} words per request, because every term rides in the request URI: an \
                     oversized URI is rejected by the edge as a bare 400 that never mentions \
                     the vocabulary, and Deepgram's own 500-token keyterm cap is answered the \
                     same way. Terms that do not fit are skipped individually, so the ones \
                     that arrive are not necessarily the first ones. Trim the list to keep it \
                     predictable.",
                    deepgram::KEYTERM_QUERY_BUDGET_BYTES,
                    deepgram::KEYTERM_MAX_TERMS,
                    deepgram::KEYTERM_MAX_WORDS
                ),
            });
        }
        warnings
    }

    /// Resolve [`DeepgramHintChannel`] for this config.
    ///
    /// Reads the same two functions the request builder does —
    /// [`deepgram::supports_keyterm`] and [`deepgram::effective_keyterms`] —
    /// so the answer a warning is built on is the answer the wire gets. An
    /// empty vocabulary is [`DeepgramHintChannel::Live`] on a model that takes
    /// keyterm: there is nothing being dropped, and terms added now would
    /// arrive, so "use `[general] vocabulary`" is advice that works.
    ///
    /// Not gated on `[general] backend`, on purpose, and that is the
    /// difference from [`Config::deepgram_keyterm_warnings`]. This answers
    /// "would the vocabulary reach Deepgram *if* the user went there", which
    /// is the question a warning on another backend has to ask before naming
    /// deepgram as a destination.
    fn deepgram_hint_channel(&self) -> DeepgramHintChannel {
        if !deepgram::supports_keyterm(&self.deepgram_model()) {
            return DeepgramHintChannel::Unsupported;
        }
        let usable = deepgram::usable_keyterms(&self.general.vocabulary).count();
        if usable > 0 && deepgram::effective_keyterms(&self.general.vocabulary).is_empty() {
            return DeepgramHintChannel::NothingFits { usable };
        }
        DeepgramHintChannel::Live
    }

    /// Load-time warnings about `[general] prompt` and `[general] vocabulary`
    /// being discarded by a backend that puts no prompt on the wire (#140).
    ///
    /// Both keys are documented as hints to the transcription backend, and on
    /// Deepgram and the two realtime backends the hint is built, handed to the
    /// backend, and thrown away because the wire format has nowhere to put it.
    /// Nothing says so at run time, and nothing can: there is no request field
    /// to log as missing, so the only symptom is a vocabulary that never seems
    /// to help. Say it once at load, the way the keyterm gate above does.
    ///
    /// The backend gate must mirror each backend's
    /// `TranscriptionBackend::sends_prompt`, which is the authority — that is
    /// the flag the pipeline reads, and a warning that disagrees with it is
    /// worse than no warning at all. `openai-realtime` is per-model rather
    /// than per-backend there (manual-commit models get `prompt = None` in the
    /// `session.update`, server-VAD models get a real one), so it is gated on
    /// the same [`openai_turn_detection_mode_for_model`] call the backend
    /// itself makes, through [`Config::openai_realtime_model`].
    ///
    /// Deepgram is deliberately absent from the vocabulary warning, and that
    /// asymmetry is the whole point of splitting the two. Deepgram has a
    /// second channel the realtime backends do not: the terms ride as
    /// `keyterm` query params and really do bias transcription, so only the
    /// free-form `prompt` is inert there. Warning about vocabulary on Deepgram
    /// would push people off the one hint that works, and would contradict
    /// [`Config::deepgram_keyterm_warnings`], which reports the real reasons a
    /// keyterm gets dropped — both of which this function has to route around,
    /// because that warning has two ways to say the vocabulary is not
    /// arriving. On a pre-Nova-3 `[deepgram] model` the second channel is dead
    /// for every term; on a model that does take keyterm, the keyterm limits
    /// can still drop every term in the list. Either way the prompt warning
    /// must not send the user to a `[general] vocabulary` the warning right
    /// above it has just called dropped, so the destination comes off
    /// [`Config::deepgram_hint_channel`]: the model switch when keyterm is
    /// unsupported, trimming the list when nothing fits, and the vocabulary
    /// itself only when it would actually carry terms.
    ///
    /// `local-vosk` and `local-parakeet` answer `false` as well, but their
    /// `transcribe()` bails with "not yet implemented" — nothing is discarded
    /// because nothing is transcribed. They get no warning, and like every
    /// other recommendation in this file they are never named as a way out.
    ///
    /// Neither is `local-whisper`, which is why the "switch to" lists here
    /// stop at groq, openai and asr-sidecar. It is a real backend and it does
    /// send the prompt, but the `whisrs-linux-{x86_64,aarch64}-minimal`
    /// release artifacts are built `--no-default-features --features
    /// tray,overlay,hooks`, and in those binaries the stub that stands in for
    /// it bails with "local-whisper feature not enabled". Half the artifacts
    /// this project ships cannot act on that advice, and a warning cannot tell
    /// which binary it is running in. The test helper
    /// `assert_inert_prompt_advice_is_reachable` holds the line, and it bans
    /// the name from the whole message rather than from the recommendation
    /// sentences alone: a message here has no other reason to say it, so
    /// there is nothing to lose by refusing every phrasing at once.
    fn inert_prompt_warnings(&self, backend: &str) -> Vec<ConfigWarning> {
        /// Which promptless backend the two messages below are describing.
        ///
        /// An enum rather than the raw backend string or an `Option<String>`
        /// with a `None` catch-all, because both message bodies are
        /// backend-specific prose. The catch-all this replaced printed "the
        /// Deepgram API has no prompt field" for anything that was not
        /// `openai-realtime`, and hardcoded the literal
        /// `openai-compatible-realtime` into the vocabulary message — both
        /// true only for as long as Deepgram and Lemonade were the whole
        /// list, and both silently wrong for the next backend added to the
        /// gate. A new promptless backend now needs a variant here, and a
        /// variant with no arm is a compile error in both matches rather
        /// than a warning naming an API the user is not talking to.
        enum InertPromptCase {
            /// `deepgram` / `deepgram-streaming`: neither the REST nor the
            /// WebSocket API has a prompt field, but the vocabulary has a
            /// second channel — the `keyterm` query params — so only the
            /// prompt is inert. Carries the resolved `[deepgram] model` and
            /// the state of that channel, because whether it is worth pointing
            /// at depends on both the model and the terms.
            Deepgram {
                model: String,
                channel: DeepgramHintChannel,
            },
            /// `openai-compatible-realtime`: `LemonadeSessionUpdate::new`
            /// takes no prompt argument at all, so both keys are inert.
            Lemonade,
            /// `openai-realtime` on a manual-commit model, which is what
            /// `whisrs setup` writes. Carries the model, because the
            /// server-VAD models on the same backend do send the prompt and
            /// the message has to name which one the user is on.
            OpenAiRealtimeManualCommit { model: String },
        }

        let mut warnings = Vec::new();

        // Resolved once, so the two message bodies below cannot disagree
        // about which backend they are describing. Every backend that does
        // send the prompt returns early here.
        let case = match backend {
            "deepgram" | "deepgram-streaming" => InertPromptCase::Deepgram {
                model: self.deepgram_model(),
                channel: self.deepgram_hint_channel(),
            },
            "openai-compatible-realtime" => InertPromptCase::Lemonade,
            "openai-realtime" => {
                let model = self.openai_realtime_model();
                match openai_turn_detection_mode_for_model(&model) {
                    TurnDetectionMode::ManualCommit => {
                        InertPromptCase::OpenAiRealtimeManualCommit { model }
                    }
                    // Server-VAD models carry a real `prompt`, so neither key
                    // is inert here.
                    TurnDetectionMode::ServerVad => return warnings,
                }
            }
            _ => return warnings,
        };

        // Blank is absent. For the prompt that matches the daemon's
        // `transcription_prompt` exactly: it trims and filters the prompt
        // before joining, so a whitespace-only prompt loses nothing and must
        // not be reported as if it did.
        //
        // For the vocabulary it does not match, and the difference is
        // deliberate rather than a bug in either place. `transcription_prompt`
        // joins *every* entry — `vocabulary = ["a", "   "]` really does put
        // both into the runtime prompt — so a blank term is not filtered on
        // the way to the wire. Counting it here would still be wrong: a
        // whitespace entry biases nothing, so reporting it as a lost term
        // would name a loss the user cannot act on. The count is
        // `deepgram::usable_keyterms`'s definition of a term, which is the
        // one the rest of this file already uses.
        let has_prompt = self
            .general
            .prompt
            .as_deref()
            .map(str::trim)
            .is_some_and(|p| !p.is_empty());
        let terms = self
            .general
            .vocabulary
            .iter()
            .filter(|term| !term.trim().is_empty())
            .count();

        if has_prompt {
            warnings.push(ConfigWarning {
                message: match &case {
                    // Deepgram with a live keyterm channel: the prompt is gone,
                    // but the vocabulary really does arrive, so send them
                    // there. Live includes "no vocabulary set yet" — nothing
                    // is being dropped, and terms added now would arrive.
                    InertPromptCase::Deepgram {
                        channel: DeepgramHintChannel::Live,
                        ..
                    } => {
                        format!(
                            "[general] prompt is ignored with backend = \"{backend}\": the \
                             Deepgram API has no prompt field, so the hint is dropped before the \
                             request is built and nothing reaches the model. Use [general] \
                             vocabulary instead, which rides to Deepgram as keyterm query params \
                             and does bias transcription."
                        )
                    }
                    // Deepgram on a model that takes keyterm, with a
                    // vocabulary the keyterm limits drop whole:
                    // `deepgram_keyterm_warnings` has just reported "0 of N
                    // usable term(s) reach Deepgram", so "use [general]
                    // vocabulary, it does bias transcription" would recommend
                    // a channel that is delivering nothing. The channel is not
                    // dead, though — trimming the list is what revives it, and
                    // that is a different way out from the model switch below.
                    InertPromptCase::Deepgram {
                        model,
                        channel: DeepgramHintChannel::NothingFits { usable },
                    } => format!(
                        "[general] prompt is ignored with backend = \"{backend}\": the Deepgram \
                         API has no prompt field, so the hint is dropped before the request is \
                         built and nothing reaches the model. Deepgram's own hint channel is the \
                         keyterm query param, which [deepgram] model = \"{model}\" does take — \
                         but none of the {usable} term(s) in [general] vocabulary fit the keyterm \
                         limits, so that channel is carrying nothing either, as the [general] \
                         vocabulary warning alongside this one says. Trim [general] vocabulary \
                         until at least one term fits to get a hint channel back."
                    ),
                    // Deepgram on a pre-Nova-3 model: the vocabulary channel
                    // is dead for every term, and `deepgram_keyterm_warnings`
                    // has already said so in the line right above this one.
                    // Recommending `[general] vocabulary` here would recommend
                    // exactly what that warning just called dropped, so point
                    // at the model switch that revives it instead.
                    InertPromptCase::Deepgram {
                        model,
                        channel: DeepgramHintChannel::Unsupported,
                    } => format!(
                        "[general] prompt is ignored with backend = \"{backend}\": the Deepgram \
                         API has no prompt field, so the hint is dropped before the request is \
                         built and nothing reaches the model. Deepgram's own hint channel is the \
                         keyterm query param, and [deepgram] model = \"{model}\" does not take \
                         that either, because keyterm is a Nova-3/Flux feature, so nothing \
                         biases transcription on this model at all. Switch [deepgram] model to \
                         a nova-3 model to get a hint channel back."
                    ),
                    InertPromptCase::Lemonade => format!(
                        "[general] prompt is ignored with backend = \"{backend}\": the Lemonade \
                         session.update carries no prompt field, so the hint never reaches the \
                         wire. Switch to a backend that sends it (groq, openai, asr-sidecar) to \
                         use a prompt."
                    ),
                    InertPromptCase::OpenAiRealtimeManualCommit { model } => format!(
                        "[general] prompt is ignored with backend = \"{backend}\" on [openai] \
                         model = \"{model}\": manual-commit models get prompt = None in the \
                         session.update, so the hint never reaches the wire. Switch to a \
                         server-VAD model such as gpt-4o-transcribe to send it, or to a backend \
                         that always does (groq, openai, asr-sidecar)."
                    ),
                },
            });
        }

        if terms > 0 {
            let message = match &case {
                // The second channel: on Deepgram the vocabulary is not
                // folded into the prompt at all, it becomes `keyterm` params.
                // Only the prompt is lost there, so this warning must never
                // fire — including on a pre-Nova-3 model, where the terms
                // really are dropped but
                // [`Config::deepgram_keyterm_warnings`] is the one that says
                // so, with the model named.
                InertPromptCase::Deepgram { .. } => None,
                InertPromptCase::Lemonade => {
                    // "or to deepgram" is a way out only if the vocabulary
                    // would ride the keyterm channel once the user got there,
                    // and that is decided by the `[deepgram] model` already in
                    // this config — routinely a stale one, because `whisrs
                    // setup` writes a `[deepgram]` section and people leave it
                    // behind when they switch backends. That staleness is the
                    // exact scenario `deepgram_keyterm_warnings` is gated on
                    // the active backend for, so nothing else is watching:
                    // follow this clause with a leftover `model = "nova-2"`,
                    // or with a vocabulary the keyterm limits drop whole, and
                    // the terms are dropped again at the destination with no
                    // warning saying why.
                    let deepgram_clause = match self.deepgram_hint_channel() {
                        DeepgramHintChannel::Live => {
                            ", or to deepgram, which sends vocabulary as keyterm query params"
                        }
                        DeepgramHintChannel::NothingFits { .. }
                        | DeepgramHintChannel::Unsupported => "",
                    };
                    Some(format!(
                        "[general] vocabulary is ignored with backend = \"{backend}\": the \
                         {terms} term(s) are folded into the transcription prompt, and the \
                         Lemonade session.update carries no prompt field, so they reach nothing. \
                         Switch to a backend that sends the prompt (groq, openai, \
                         asr-sidecar){deepgram_clause}."
                    ))
                }
                InertPromptCase::OpenAiRealtimeManualCommit { model } => Some(format!(
                    "[general] vocabulary is ignored with backend = \"{backend}\" on [openai] \
                     model = \"{model}\": the {terms} term(s) are folded into the transcription \
                     prompt, and manual-commit models get prompt = None in the session.update, \
                     so they reach nothing. Switch to a server-VAD model such as \
                     gpt-4o-transcribe, or to a backend that always sends the prompt (groq, \
                     openai, asr-sidecar)."
                )),
            };
            if let Some(message) = message {
                warnings.push(ConfigWarning { message });
            }
        }

        warnings
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
    fn config_input_clipboard_fallback_roundtrip() {
        // Opt-in: an `[input]` table written before the key existed (or
        // without it) keeps the current behavior.
        let absent: InputConfig = toml::from_str("").unwrap();
        assert!(!absent.clipboard_fallback);
        assert!(!absent.clipboard_only);

        let cfg: InputConfig =
            toml::from_str("clipboard_fallback = true\nclipboard_only = true").unwrap();
        assert!(cfg.clipboard_fallback);
        assert!(cfg.clipboard_only);

        // Round-trips back out and parses again identically.
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("clipboard_fallback = true"));
        assert!(serialized.contains("clipboard_only = true"));
        let reparsed: InputConfig = toml::from_str(&serialized).unwrap();
        assert!(reparsed.clipboard_fallback);
        assert!(reparsed.clipboard_only);
    }

    #[test]
    fn cloud_backend_sections_parse_without_api_key() {
        // Regression: `api_key` used to be a required field, so a `[groq]`
        // section kept for its `model` (with the key supplied via the
        // WHISRS_GROQ_API_KEY env var) was a TOML parse error, and the daemon
        // discarded the *whole* config: backend, hotkeys and every other
        // section silently reverted to defaults. The key is optional now; an
        // empty value means "resolve from the environment".
        let cfg: Config =
            toml::from_str("[general]\nbackend = \"groq\"\n[groq]\nmodel = \"whisper-large-v3\"\n")
                .unwrap();
        assert_eq!(cfg.general.backend, "groq");
        let groq = cfg.groq.expect("groq section should deserialize");
        assert!(groq.api_key.is_empty());
        assert_eq!(groq.model, "whisper-large-v3");

        // Same for the other cloud sections.
        let cfg: Config =
            toml::from_str("[general]\nbackend = \"deepgram\"\n[deepgram]\nmodel = \"nova-3\"\n[openai]\nmodel = \"gpt-4o-transcribe\"\n")
                .unwrap();
        assert_eq!(cfg.deepgram.unwrap().api_key, "");
        assert_eq!(cfg.openai.unwrap().api_key, "");
    }

    #[test]
    fn local_whisper_section_parses_without_model_path() {
        // Same whole-config-discard trap as the cloud `api_key` fields: a
        // `[local-whisper]` section kept only to pin `segmentation` used to
        // be a TOML parse error. `model_path` defaults to the path `whisrs
        // setup` downloads to, never to the empty string, which would reach
        // `LocalWhisperBackend::new("")`.
        let cfg: Config = toml::from_str(
            "[general]\nbackend = \"deepgram\"\n[local-whisper]\nsegmentation = \"silence\"\n",
        )
        .unwrap();
        let local = cfg
            .local_whisper
            .expect("local-whisper section should deserialize");
        assert_eq!(local.model_path, default_whisper_model_path());
        assert!(!local.model_path.is_empty());
        assert_eq!(local.segmentation, "silence");
        assert_eq!(local.phrase_silence_ms, default_phrase_silence_ms());
    }

    #[test]
    fn local_model_sections_parse_without_model_path() {
        // Empty is the modelled "absent" state for these two: `validate`
        // already treats `model_path.is_empty()` as "model directory not
        // found. Run 'whisrs setup'", so an omitted key must reach that
        // warning rather than discard the whole config.
        let cfg: Config =
            toml::from_str("[general]\nbackend = \"deepgram\"\n[local-vosk]\n[local-parakeet]\n")
                .unwrap();
        assert_eq!(
            cfg.local_vosk
                .expect("local-vosk section should deserialize")
                .model_path,
            ""
        );
        assert_eq!(
            cfg.local_parakeet
                .expect("local-parakeet section should deserialize")
                .model_path,
            ""
        );
    }

    #[test]
    fn openai_compatible_realtime_without_url_reaches_validate() {
        // `url` used to be required, so omitting it failed the whole-config
        // parse and `validate`'s dedicated error was unreachable from a
        // config file. Now the section deserializes and the user gets the
        // error that names the key to add.
        let cfg: Config = toml::from_str(
            "[general]\nbackend = \"openai-compatible-realtime\"\n\
             [openai-compatible-realtime]\nmodel = \"Whisper-Tiny\"\n",
        )
        .unwrap();
        let realtime = cfg
            .openai_compatible_realtime
            .as_ref()
            .expect("openai-compatible-realtime section should deserialize");
        assert_eq!(realtime.url, "");
        assert_eq!(realtime.model, "Whisper-Tiny");

        // This arm reads no `WHISRS_*_API_KEY`, so the outcome does not
        // depend on the ambient environment.
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("no WebSocket URL configured"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn whisper_model_path_has_one_source() {
        // Two routes reach a `model_path` the user never wrote: serde, for a
        // `[local-whisper]` section that omits the key, and
        // `LocalWhisperConfig::default`, which is what `validate` and the
        // daemon's backend factory fall back to when the section is absent
        // entirely. Both must land on `default_whisper_model_path`, or the
        // daemon warns about one file and loads another.
        let defaulted: Config =
            toml::from_str("[general]\nbackend = \"local-whisper\"\n[local-whisper]\n").unwrap();
        let from_serde = defaulted
            .local_whisper
            .expect("local-whisper section should deserialize");
        let from_default = LocalWhisperConfig::default();

        assert_eq!(from_serde.model_path, default_whisper_model_path());
        assert_eq!(from_default.model_path, default_whisper_model_path());
        assert_eq!(from_serde.model_path, from_default.model_path);
        assert_eq!(from_serde.segmentation, from_default.segmentation);
        assert_eq!(from_serde.phrase_silence_ms, from_default.phrase_silence_ms);
    }

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
        // A whole section the schema never heard of: every leaf inside it is
        // reported, nested ones included, so the user sees which key to fix.
        let unknown = unknown_config_keys("[bogus]\nfoo = 1\n[bogus.nested]\nbar = 2\n");
        assert_eq!(unknown, vec!["bogus.foo", "bogus.nested.bar"]);
    }

    #[test]
    fn hotkey_alias_key_is_not_reported() {
        // `read` is a serde alias for `speak`, so it works (see
        // `hotkey_speak_read_alias`) but reserializes as `speak`. The
        // confirmation pass must clear it instead of warning on every start.
        let unknown = unknown_config_keys("[hotkeys]\nread = \"Super+Shift+R\"\n");
        assert!(unknown.is_empty(), "alias key reported: {unknown:?}");
    }

    #[test]
    fn alias_sections_are_not_reported() {
        // `[local]` aliases `[local-whisper]`, `[asr]` aliases `[asr-sidecar]`.
        let unknown = unknown_config_keys("[local]\nmodel_path = \"/models/ggml.bin\"\n");
        assert!(unknown.is_empty(), "[local] reported: {unknown:?}");

        let unknown = unknown_config_keys("[asr]\nurl = \"http://127.0.0.1:9999/transcribe\"\n");
        assert!(unknown.is_empty(), "[asr] reported: {unknown:?}");
    }

    #[test]
    fn alias_key_alongside_typo_reports_only_the_typo() {
        // The confirmation pass must not swallow real unknowns that sit next
        // to an alias.
        let unknown =
            unknown_config_keys("[hotkeys]\nread = \"Super+Shift+R\"\nbogus = \"Super+X\"\n");
        assert_eq!(unknown, vec!["hotkeys.bogus"]);

        let unknown =
            unknown_config_keys("[local]\nmodel_path = \"/models/ggml.bin\"\nbogus = 1\n");
        assert_eq!(unknown, vec!["local.bogus"]);
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

    /// The other half of that contract, and the one #116/#117 actually rest on:
    /// a file that is valid TOML but does not deserialize reports no unknown
    /// keys rather than warning about every key in it. `scan_unknown_keys`
    /// returns `Err` for it since issue #134 made the failure visible to
    /// `write_config_to`, so this pins that `unknown_config_keys` still swallows
    /// that error into an empty list instead of propagating or panicking.
    #[test]
    fn valid_toml_that_is_not_a_config_reports_nothing() {
        let unknown = unknown_config_keys(
            "[general]\nbackend = \"groq\"\nsilence_timeout_ms = \"2000\"\nbogus = 1\n",
        );
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn non_finite_float_does_not_silence_the_report() {
        // `nan != nan`, so comparing parsed values directly would suppress every
        // warning in the file, not just the one in this section.
        let unknown = unknown_config_keys(
            "[general]\naudio_feedback_volume = nan\nbogus = 1\n[input]\npast = true\n",
        );
        assert_eq!(unknown, vec!["general.bogus", "input.past"]);
    }

    #[test]
    fn unknown_keys_warning_is_none_when_nothing_is_unknown() {
        assert!(unknown_keys_warning(Path::new("/tmp/config.toml"), &[]).is_none());
    }

    #[test]
    fn unknown_keys_warning_is_the_one_message_every_path_prints() {
        // Byte-identical to what the daemon logged before this literal was
        // shared with the two interactive flows (issue #116).
        let warning = unknown_keys_warning(
            Path::new("/home/u/.config/whisrs/config.toml"),
            &["input.past".to_string(), "deepgram.bogus".to_string()],
        );
        assert_eq!(
            warning.as_deref(),
            Some(
                "Unknown keys in config at /home/u/.config/whisrs/config.toml ignored: \
                 input.past, deepgram.bogus"
            )
        );
    }

    #[test]
    fn a_clean_config_preserves_nothing() {
        let preserved = PreservedKeys::from_config_str(
            "[general]\nbackend = \"groq\"\n[input]\npaste = true\n",
        )
        .expect("fixture is a config the preserve set can describe");
        assert!(preserved.is_empty());
    }

    #[test]
    fn preserved_keys_name_a_confirmed_typo() {
        let preserved = PreservedKeys::from_config_str("[input]\npast = true\n")
            .expect("fixture is a config the preserve set can describe");
        assert!(preserved.table("input").contains_leaf("past"));
        assert!(!preserved.contains_leaf("input"));
    }

    #[test]
    fn preserved_keys_cover_a_wholly_unknown_nested_section() {
        // Both leaves are confirmed-unknown and pruning `[bogus]` as a whole
        // changes nothing, so the writer may keep the entire section.
        let preserved =
            PreservedKeys::from_config_str("[bogus]\nfoo = 1\n[bogus.nested]\nbar = 2\n")
                .expect("fixture is a config the preserve set can describe");
        let bogus = preserved.table("bogus");
        assert!(bogus.contains_leaf("foo"));
        assert!(bogus.table("nested").contains_leaf("bar"));
    }

    #[test]
    fn preserved_keys_exclude_a_known_sibling() {
        // The subtree rule's whole point: `model_path` really drives a field
        // (through the `local` -> `local-whisper` alias), so `[local]` is a
        // *mixed* subtree and the writer must drop it rather than emit it
        // beside the canonical section.
        let preserved = PreservedKeys::from_config_str(
            "[general]\nbackend = \"local\"\n[local]\nmodel_path = \"/m.bin\"\nbogus = 1\n",
        )
        .expect("fixture is a config the preserve set can describe");
        let local = preserved.table("local");
        assert!(local.contains_leaf("bogus"));
        assert!(!local.contains_leaf("model_path"));
    }

    #[test]
    fn a_wholly_unknown_alias_section_is_not_prunable() {
        for section in ["asr", "vibevoice"] {
            let contents = format!("[general]\nbackend = \"groq\"\n[{section}]\nbogus = 1\n");

            // Every leaf of the section is confirmed-unknown — pruning `bogus`
            // leaves an empty table that still deserializes to a default
            // `asr-sidecar` section — so the leaf rule alone would keep it.
            assert_eq!(
                unknown_config_keys(&contents),
                vec![format!("{section}.bogus")],
                "fixture no longer exercises the whole-table guard"
            );

            // But the name is a serde alias for `asr-sidecar`, which the writer
            // also emits: keeping both makes the file fail to deserialize
            // outright. The leaf still has to be *named* (the warning quotes
            // it); it is the whole-table flag that stops the writer keeping it.
            let preserved = PreservedKeys::from_config_str(&contents)
                .expect("fixture is a config the preserve set can describe");
            assert!(preserved.table(section).contains_leaf("bogus"));
            assert!(
                !preserved.table(section).table_prunable(),
                "`[{section}]` marked prunable, so the rewrite would carry both spellings"
            );
        }
    }

    #[test]
    fn a_wholly_unknown_root_inline_alias_is_not_prunable() {
        // Same section, spelled as a root inline table. `toml` parses it to the
        // same `Value::Table`, so the guard must reach it too.
        let preserved =
            PreservedKeys::from_config_str("asr = { bogus = 1 }\n[general]\nbackend = \"groq\"\n")
                .expect("fixture is a config the preserve set can describe");
        assert!(preserved.table("asr").contains_leaf("bogus"));
        assert!(!preserved.table("asr").table_prunable());
    }

    #[test]
    fn a_wholly_unknown_section_is_prunable() {
        // The other side of the same flag: `[bogus]` is not an alias for
        // anything, so deleting it changes nothing and the writer may keep it.
        let preserved =
            PreservedKeys::from_config_str("[bogus]\nfoo = 1\n[bogus.nested]\nbar = 2\n")
                .expect("fixture is a config the preserve set can describe");
        assert!(preserved.table("bogus").table_prunable());
        assert!(
            preserved.table("bogus").table("nested").table_prunable(),
            "the flag must be recorded below a covered table too"
        );
    }

    #[test]
    fn an_all_optional_section_keeps_its_only_key_when_that_key_is_a_typo() {
        // The class of config the whole feature exists for, and the one the
        // eager version of the guard deleted: a section the schema *knows*
        // whose fields are all optional, holding nothing but a typo. Removing
        // such a section changes the parse (`Some(default)` -> `None`), so it
        // is not prunable — but its name is in the fresh serialization, so the
        // writer never asks, and the leaf must survive to be preserved there.
        for (section, key) in [
            ("hotkeys", "speek"),
            ("hooks", "media_autopause"),
            ("llm", "bogus"),
            ("tts", "bogus"),
        ] {
            let contents = format!("[general]\nbackend = \"groq\"\n[{section}]\n{key} = 1\n");
            let preserved = PreservedKeys::from_config_str(&contents)
                .expect("fixture is a config the preserve set can describe");
            assert!(
                preserved.table(section).contains_leaf(key),
                "`{section}.{key}` dropped from the preserve set"
            );
        }
    }

    #[test]
    fn an_empty_table_is_covered_vacuously_and_keeps_its_section_prunable() {
        // An empty table has no leaves, so "every leaf of this subtree is
        // confirmed-unknown" is trivially true of it — but it contributes no
        // path, so there is no node for it and `covers` used to read the
        // missing node as a failure. One `{}` then made `[bogus]` unprunable
        // and the writer deleted the section around it.
        for shape in [
            "empty_table = {}",
            "a = { b = {} }",
            "a = { b = { c = {} } }",
        ] {
            let contents =
                format!("[general]\nbackend = \"groq\"\n[bogus]\nimportant = 1\n{shape}\n");

            // The empty table names nothing, so it is not in the warning.
            assert_eq!(
                unknown_config_keys(&contents),
                vec!["bogus.important".to_string()],
                "fixture no longer exercises the leafless-table rule ({shape})"
            );

            let preserved = PreservedKeys::from_config_str(&contents)
                .expect("fixture is a config the preserve set can describe");
            assert!(preserved.table("bogus").contains_leaf("important"));
            assert!(
                preserved.table("bogus").table_prunable(),
                "`{shape}` vetoed `[bogus]`, so the writer would delete the section"
            );
        }
    }

    #[test]
    fn a_bare_empty_subtable_header_keeps_its_section_prunable() {
        // Same rule, spelled as a header rather than an inline `{}`.
        let preserved = PreservedKeys::from_config_str(
            "[general]\nbackend = \"groq\"\n[bogus]\nimportant = 1\n[bogus.emptysub]\n",
        )
        .expect("fixture is a config the preserve set can describe");
        assert!(preserved.table("bogus").table_prunable());
    }

    #[test]
    fn an_empty_subtable_does_not_make_an_alias_section_prunable() {
        // The vacuous rule makes `[asr]` *cover* itself, so the whole-table
        // question now gets asked where it previously was skipped. The answer
        // must still be no: `asr` is a serde alias for the `asr-sidecar`
        // section the writer emits, and keeping both is a `duplicate field`
        // error, i.e. the daemon discards the user's entire config.
        let contents = "[general]\nbackend = \"groq\"\n[asr]\nbogus = 1\n[asr.empty]\n";
        let preserved = PreservedKeys::from_config_str(contents)
            .expect("fixture is a config the preserve set can describe");
        assert!(preserved.table("asr").contains_leaf("bogus"));
        assert!(
            !preserved.table("asr").table_prunable(),
            "`[asr]` marked prunable, so the rewrite would carry both spellings"
        );
    }

    /// The tripwire for the hazard documented on `record_prunable_tables`.
    ///
    /// That guard infers "this table name is a serde alias" from "deleting the
    /// table changes the parse". The two agree only because every aliased field
    /// in the schema is an `Option<_>`. Put an alias on a non-`Option`
    /// `#[serde(default)]` field and deleting it reparses to the same default,
    /// the guard calls it prunable, and the writer emits both the alias and the
    /// canonical name — `duplicate field`, config discarded, defaults loaded.
    /// Demonstrated with `alias = "in"` on `Config::input`.
    ///
    /// So pin both halves: which aliases exist, and that each one sits on an
    /// `Option`. Adding an alias fails this test; the fix is to read the
    /// warning on `record_prunable_tables` before touching the assertion.
    ///
    /// This is a text scan, not reflection, so it sees exactly the files listed
    /// below: `src/config/types.rs` and `src/llm.rs` — today's complete set of
    /// modules declaring a `Deserialize` type reachable from [`Config`]
    /// (`llm::LlmConfig` and `llm::LlmCommandConfig` are the ones outside this
    /// file). Put a config struct in a third module and its aliases are
    /// invisible here until that file joins the list.
    #[test]
    fn every_serde_alias_names_an_optional_section() {
        const NEEDLE: &str = "alias = \"";

        // alias -> (file, the source line of the field it annotates)
        let mut found: BTreeMap<String, (&str, &str)> = BTreeMap::new();
        for (file, source) in [
            ("src/config/types.rs", include_str!("types.rs")),
            ("src/llm.rs", include_str!("../llm.rs")),
        ] {
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.trim_start().starts_with("#[serde(") || !line.contains(NEEDLE) {
                    continue;
                }
                let field = *lines
                    .get(index + 1)
                    .expect("a serde attribute is followed by the field it annotates");
                for chunk in line.split(NEEDLE).skip(1) {
                    let alias = chunk.split('"').next().expect("alias is a quoted string");
                    found.insert(alias.to_string(), (file, field));
                }
            }
        }

        assert_eq!(
            found.keys().cloned().collect::<Vec<_>>(),
            vec![
                "asr".to_string(),
                "local".to_string(),
                "read".to_string(),
                "vibevoice".to_string()
            ],
            "the schema's serde aliases changed; see the hazard note on \
             `record_prunable_tables` before updating this list"
        );

        for (alias, (file, field)) in &found {
            assert!(
                field.contains("Option<"),
                "`{alias}` aliases the non-`Option` field `{}` in {file}. The \
                 whole-table guard in `record_prunable_tables` infers \
                 alias-ness from a reparse, which only works for `Option<_>`: \
                 deleting a non-`Option` `#[serde(default)]` section reparses \
                 to the same value, so the guard would keep the aliased \
                 spelling *and* the canonical one, and serde rejects that with \
                 `duplicate field`.",
                field.trim()
            );
        }

        // The section aliases (`read` is a key alias, covered by
        // `a_hotkey_alias_is_recanonicalized_while_the_typo_beside_it_survives`)
        // are the ones the guard has to drop whole. `local` joined the loop
        // when #137 gave `LocalWhisperConfig::model_path` a serde default:
        // `[local] bogus = 1` deserializes now, so `[local]` can be wholly
        // unknown exactly like `[asr]`.
        for section in ["asr", "vibevoice", "local"] {
            let contents = format!("[general]\nbackend = \"groq\"\n[{section}]\nbogus = 1\n");
            // Without this the row can pass for the wrong reason: a fixture
            // that stops deserializing reports nothing, so the node is empty
            // and `table_prunable` is false however the guard behaves.
            assert_eq!(
                unknown_config_keys(&contents),
                vec![format!("{section}.bogus")],
                "fixture no longer exercises the whole-table guard"
            );
            assert!(
                !PreservedKeys::from_config_str(&contents)
                    .expect("fixture is a config the preserve set can describe")
                    .table(section)
                    .table_prunable(),
                "`[{section}]` marked prunable, so the rewrite would carry both spellings"
            );
        }
    }

    #[test]
    fn a_nested_all_optional_section_keeps_its_typo() {
        // `[overlay.colors]`: both levels are all-optional, so the eager guard
        // pruned the `overlay` node and took `colors` down with it.
        let preserved = PreservedKeys::from_config_str(
            "[general]\nbackend = \"groq\"\n[overlay.colors]\nbogos = 1\n",
        )
        .expect("fixture is a config the preserve set can describe");
        assert!(preserved
            .table("overlay")
            .table("colors")
            .contains_leaf("bogos"));
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
            hooks: None,
            llm_commands: Vec::new(),
            overlay: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Unknown backend"));
        assert!(err.to_string().contains("openai-compatible-realtime"));
    }

    #[test]
    fn overlay_position_parsing() {
        assert_eq!(
            OverlayPosition::parse("top-left"),
            Some(OverlayPosition::TopLeft)
        );
        assert_eq!(
            OverlayPosition::parse(" Top-Middle "),
            Some(OverlayPosition::TopCenter)
        );
        assert_eq!(
            OverlayPosition::parse("bottom-middle"),
            Some(OverlayPosition::BottomCenter)
        );
        assert_eq!(OverlayPosition::parse("middle"), None);
        for p in [
            OverlayPosition::BottomLeft,
            OverlayPosition::BottomCenter,
            OverlayPosition::BottomRight,
            OverlayPosition::TopLeft,
            OverlayPosition::TopCenter,
            OverlayPosition::TopRight,
        ] {
            assert_eq!(OverlayPosition::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn overlay_position_defaults_to_bottom_center() {
        let config: Config = toml::from_str("[overlay]\ntheme = \"ember\"\n").unwrap();
        let overlay = config.overlay.unwrap();
        assert_eq!(overlay.position, "bottom-center");
        assert_eq!(overlay.position(), OverlayPosition::BottomCenter);
    }

    #[test]
    fn config_validate_warns_unknown_overlay_position() {
        let mut config = validatable_config("groq");
        config.overlay = Some(OverlayConfig {
            position: "upper-left".to_string(),
            ..OverlayConfig::default()
        });
        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("[overlay] position = \"upper-left\"")),
            "{warnings:?}"
        );
        assert_eq!(
            config.overlay.unwrap().position(),
            OverlayPosition::BottomCenter
        );

        let mut config = validatable_config("groq");
        config.overlay = Some(OverlayConfig {
            position: "top-right".to_string(),
            ..OverlayConfig::default()
        });
        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("[overlay] position")),
            "{warnings:?}"
        );
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
                hooks: None,
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
            assert_no_stub_backend_advice(&warning.message);
        }
    }

    #[test]
    fn modifier_wait_defaults_to_ten_seconds() {
        assert_eq!(InputConfig::default().modifier_wait_ms, 10_000);
        let parsed: InputConfig = toml::from_str("key_delay_ms = 2\n").unwrap();
        assert_eq!(
            parsed.modifier_wait_ms, 10_000,
            "an absent key gets the default"
        );
    }

    #[test]
    fn modifier_wait_round_trips_and_is_a_known_key() {
        let input = InputConfig {
            modifier_wait_ms: 0,
            ..Default::default()
        };
        let text = toml::to_string(&input).unwrap();
        let back: InputConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.modifier_wait_ms, 0);
        assert!(
            unknown_config_keys("[input]\nmodifier_wait_ms = 2500\n").is_empty(),
            "modifier_wait_ms must not trip the unknown-key warning"
        );
    }

    /// Build a groq (non-streaming, so no paste/streaming warning of its own)
    /// config with the given `[input]` section, for the copy-only warnings.
    fn config_with_input(input: InputConfig) -> Config {
        Config {
            general: GeneralConfig {
                backend: "groq".to_string(),
                ..Default::default()
            },
            audio: Default::default(),
            input,
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
            hooks: None,
            llm_commands: Vec::new(),
            overlay: None,
        }
    }

    #[test]
    fn config_validate_clipboard_only_with_paste_warns() {
        let config = config_with_input(InputConfig {
            paste: true,
            clipboard_only: true,
            ..Default::default()
        });
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("[input] paste = true is ignored"))
            .unwrap_or_else(|| {
                panic!(
                    "clipboard_only never injects, so paste is a silent no-op; \
                     expected a warning, got: {warnings:?}"
                )
            });
        assert!(
            warning.message.contains("clipboard_only = true"),
            "the warning must name the key that overrode paste: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_clipboard_only_with_fallback_warns() {
        let config = config_with_input(InputConfig {
            clipboard_fallback: true,
            clipboard_only: true,
            ..Default::default()
        });
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| {
                w.message
                    .contains("[input] clipboard_fallback = true is ignored")
            })
            .unwrap_or_else(|| {
                panic!(
                    "clipboard_only already copies and never injects, so the fallback is a \
                     silent no-op; expected a warning, got: {warnings:?}"
                )
            });
        assert!(
            warning.message.contains("clipboard_only = true"),
            "the warning must name the key that overrode clipboard_fallback: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_clipboard_only_alone_no_warning() {
        // Copy-only on its own overrides nothing, so it must stay quiet —
        // these warnings are about *ignored* keys, not about the mode.
        let config = config_with_input(InputConfig {
            clipboard_only: true,
            ..Default::default()
        });
        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().all(|w| !w.message.contains("is ignored")),
            "clipboard_only alone overrides nothing and must not warn: {warnings:?}"
        );
    }

    /// The sentences of `message` that tell the user where to go.
    ///
    /// Every recommendation in this file opens with an imperative verb, and
    /// the set is small enough to enumerate: "Switch ...", "Use ...", "Trim
    /// ...". The split exists because a backend name in a warning means two
    /// different things — `[input] paste` and `[general] llm_post_process`
    /// both *describe* local-whisper as one of the streaming backends that
    /// ignore the key, in the same message whose closing sentence recommends
    /// something else entirely — and only the recommending half is what
    /// [`assert_no_stub_backend_advice`] may police.
    ///
    /// A new recommendation opening with a verb not listed here does not slip
    /// through silently: the caller asserts it found at least one sentence, so
    /// the omission fails the test that guards the message.
    fn recommendation_sentences(message: &str) -> Vec<&str> {
        message
            .split_terminator('.')
            .map(str::trim)
            .filter(|sentence| {
                ["Switch", "Use ", "Trim"]
                    .iter()
                    .any(|verb| sentence.starts_with(verb))
            })
            .collect()
    }

    /// `local-vosk` and `local-parakeet` parse as valid config but their
    /// `transcribe()` bails with "not yet implemented", so a warning that
    /// tells the user to switch to one trades a no-op setting for dictation
    /// that does not work at all. No warning may recommend them.
    ///
    /// `local-whisper` is the third name no warning may send the user to, for
    /// a different reason. It is a real backend in the default build, but the
    /// `whisrs-linux-{x86_64,aarch64}-minimal` release artifacts are built
    /// `--no-default-features --features tray,overlay,hooks` (see
    /// `.github/workflows/release.yml`), and in those binaries the stub that
    /// stands in for it bails with "local-whisper feature not enabled". Half
    /// the artifacts this project ships cannot act on the advice, and the
    /// warning has no way to tell which binary it is running in. `origin/main`
    /// never named it as a destination; the #140 warnings were the first to,
    /// which is what this check stops coming back.
    ///
    /// Checked per recommendation sentence rather than over the whole message,
    /// because unlike the two stubs, local-whisper is legitimately *described*
    /// by two of the warnings this helper guards.
    fn assert_no_stub_backend_advice(message: &str) {
        for stub in ["local-vosk", "local-parakeet"] {
            assert!(
                !message.contains(stub),
                "{stub} is an unimplemented stub — recommending it breaks dictation \
                 outright: {message}"
            );
        }

        let recommendations = recommendation_sentences(message);
        assert!(
            !recommendations.is_empty(),
            "every warning guarded by this helper has to end somewhere the user can go, and \
             this one has no sentence opening with a verb recommendation_sentences knows. \
             Either the message lost its way out, or it found a new way to phrase one and \
             that verb has to be added there — leaving it unlisted would make this check \
             silently stop applying: {message}"
        );
        for sentence in recommendations {
            assert!(
                !sentence.contains("local-whisper"),
                "local-whisper is inert in the minimal release artifacts, which ship with \
                 --no-default-features: recommending it hands half the users a backend that \
                 bails with \"local-whisper feature not enabled\". Name groq, openai or \
                 asr-sidecar instead: {sentence}"
            );
        }
    }

    /// [`assert_no_stub_backend_advice`], tightened for the messages
    /// [`Config::inert_prompt_warnings`] builds: there `local-whisper` is
    /// banned from the *whole* message, not only from the sentences
    /// [`recommendation_sentences`] recognises as recommendations.
    ///
    /// The sentence scoping is a hole as soon as it is the only check. It
    /// finds recommendations by their opening verb, so a sentence that
    /// recommends without one of those verbs is not a recommendation as far
    /// as it is concerned: appending "You can also run local-whisper
    /// offline." to any of these messages left the whole suite green, because
    /// the new sentence opened with none of the listed verbs and the message
    /// still carried a qualifying `Switch ...` sentence, so the non-empty
    /// assertion was satisfied too. Widening the verb list only moves the
    /// hole — the next phrasing is one sentence away.
    ///
    /// These messages can afford the flat ban because none of them has any
    /// business naming `local-whisper` at all: they are about `[general]
    /// prompt` and `[general] vocabulary` on backends that drop them, and
    /// local-whisper is neither the subject nor an admissible destination.
    /// The two warnings that legitimately do name it — `[input] paste` and
    /// `[general] llm_post_process`, which *describe* it as one of the
    /// streaming backends that ignore the key, the second even interpolating
    /// `backend = "local-whisper"` into its own text — are not built here,
    /// and keep the looser helper.
    fn assert_inert_prompt_advice_is_reachable(message: &str) {
        assert_no_stub_backend_advice(message);
        assert!(
            !message.contains("local-whisper"),
            "local-whisper is inert in the minimal release artifacts, which ship with \
             --no-default-features, and nothing in an inert-prompt warning needs to name it: \
             any mention at all reads as a way out, and it bails with \"local-whisper feature \
             not enabled\" for half the users we ship to. Name groq, openai or asr-sidecar \
             instead: {message}"
        );
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
                api_key: None,
            }),
            openai_compatible_realtime: None,
            llm: None,
            tts: None,
            hotkeys: None,
            hooks: None,
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

    /// A deepgram config on `model` with the given vocabulary terms.
    fn deepgram_config_with_vocabulary(model: &str, vocabulary: &[String]) -> Config {
        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.backend = "deepgram".to_string();
        config.general.vocabulary = vocabulary.to_vec();
        config.deepgram = Some(DeepgramConfig {
            api_key: "test-key".to_string(),
            model: model.to_string(),
        });
        config
    }

    #[test]
    fn config_validate_warns_vocabulary_ignored_on_pre_nova3_deepgram_model() {
        // The backend drops the terms at `debug!`, which the daemon's default
        // `info` filter hides — so this has to be said once at load.
        let vocabulary = vec!["whisrs".to_string(), "Hyprland".to_string()];
        let config = deepgram_config_with_vocabulary("nova-2", &vocabulary);
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("vocabulary"))
            .unwrap_or_else(|| panic!("nova-2 + vocabulary must warn: {warnings:?}"));
        assert!(
            warning.message.contains("nova-2"),
            "the warning must name the model: {}",
            warning.message
        );
        assert!(
            warning.message.contains("ignored") || warning.message.contains("dropped"),
            "the warning must say the terms do not reach Deepgram: {}",
            warning.message
        );
    }

    #[test]
    fn config_validate_does_not_warn_about_vocabulary_on_nova3() {
        let vocabulary = vec!["whisrs".to_string()];
        let config = deepgram_config_with_vocabulary("nova-3", &vocabulary);
        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().all(|w| !w.message.contains("vocabulary")),
            "nova-3 supports keyterm; nothing to warn about: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_does_not_warn_without_vocabulary() {
        let config = deepgram_config_with_vocabulary("nova-2", &[]);
        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().all(|w| !w.message.contains("vocabulary")),
            "no vocabulary means nothing is being dropped: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_warns_when_vocabulary_exceeds_the_keyterm_budget() {
        // An unbounded vocabulary builds an oversized request URI and
        // Deepgram's edge answers a bare 400 that never mentions it.
        let vocabulary: Vec<String> = (0..1000).map(|i| format!("term{i:04}")).collect();
        let config = deepgram_config_with_vocabulary("nova-3", &vocabulary);
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("vocabulary"))
            .unwrap_or_else(|| panic!("an oversized vocabulary must warn: {warnings:?}"));
        let fitting = deepgram::effective_keyterms(&vocabulary).len();
        assert!(
            warning.message.contains(&format!(
                "{fitting} of {} usable term(s) reach",
                vocabulary.len()
            )),
            "the warning must name how many terms are actually sent ({fitting}): {}",
            warning.message
        );
        // All three limits, spelled with their units — a bare `contains("200")`
        // would be satisfied by the term count itself.
        assert!(
            warning.message.contains(&format!(
                "{} bytes of query string",
                deepgram::KEYTERM_QUERY_BUDGET_BYTES
            )),
            "the warning must name the byte budget: {}",
            warning.message
        );
        assert!(
            warning
                .message
                .contains(&format!("{} terms and", deepgram::KEYTERM_MAX_TERMS)),
            "the warning must name the term cap: {}",
            warning.message
        );
        assert!(
            warning.message.contains(&format!(
                "{} words per request",
                deepgram::KEYTERM_MAX_WORDS
            )),
            "the warning must name the word cap: {}",
            warning.message
        );
    }

    /// The term count `Config::validate` advertises, parsed back out of its
    /// warning text.
    ///
    /// The whole point of the warning is that the number it names is the number
    /// of `keyterm` params the request carries. Reading it back out of the
    /// message is what lets a test compare the two, rather than compare
    /// `effective_keyterms` against itself.
    fn advertised_keyterm_count(warnings: &[ConfigWarning]) -> Option<usize> {
        let message = &warnings
            .iter()
            .find(|w| w.message.starts_with("[general] vocabulary: "))?
            .message;
        message
            .trim_start_matches("[general] vocabulary: ")
            .split(' ')
            .next()?
            .parse()
            .ok()
    }

    #[test]
    fn config_validate_advertises_the_count_that_goes_on_the_wire() {
        // The no-drift invariant, in every shape that trips a different limit.
        // This broke once already: blanks were charged against the byte budget
        // and filtered afterwards, so `validate` promised 335 while the request
        // carried 135.
        let cases: Vec<(&str, Vec<String>)> = vec![
            // Byte-budget-bound: 1000 short single-word terms.
            (
                "byte budget",
                (0..1000).map(|i| format!("term{i:04}")).collect(),
            ),
            // Term-cap-bound: terms short enough that 200 of them use barely
            // half the byte budget.
            ("term cap", (0..500).map(|i| format!("t{i:03}")).collect()),
            // Word-cap-bound: three words each, so 300 words arrives at 100
            // terms, well before the byte budget's 195.
            ("word cap", vec!["ab cd ef".to_string(); 400]),
            // Blanks interleaved: blanks are not terms and must cost nothing.
            (
                "blanks interleaved",
                (0..1000)
                    .flat_map(|i| ["   ".to_string(), format!("term{i:04}")])
                    .collect(),
            ),
            // One oversized term first: it must be skipped, not fatal.
            (
                "long term first",
                vec![
                    "x".repeat(5000),
                    "whisrs".to_string(),
                    "Hyprland".to_string(),
                ],
            ),
        ];

        for (label, vocabulary) in cases {
            let config = deepgram_config_with_vocabulary("nova-3", &vocabulary);
            let warnings = config.validate().expect("a keyed deepgram config is valid");
            let advertised = advertised_keyterm_count(&warnings).unwrap_or_else(|| {
                panic!("{label}: a truncated vocabulary must warn: {warnings:?}")
            });
            let on_wire = deepgram::effective_keyterms(&config.general.vocabulary).len();
            assert_eq!(
                advertised, on_wire,
                "{label}: validate advertised {advertised} but the request carries {on_wire}"
            );
        }
    }

    #[test]
    fn config_validate_does_not_warn_about_deepgram_vocabulary_on_another_backend() {
        // `whisrs setup` writes a [deepgram] section, and people leave it
        // behind when they switch backends. Without the backend gate, this
        // groq user gets a "vocabulary is ignored" warning on every daemon
        // start about a backend they are not using.
        let mut config = deepgram_config_with_vocabulary(
            "nova-2",
            &["whisrs".to_string(), "Hyprland".to_string()],
        );
        config.general.backend = "groq".to_string();
        config.groq = Some(GroqConfig {
            api_key: "test-key".to_string(),
            model: default_groq_model(),
        });
        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().all(|w| !w.message.contains("vocabulary")),
            "a stale [deepgram] section must not warn on another backend: {warnings:?}"
        );
    }

    #[test]
    fn deepgram_keyterm_warnings_without_a_deepgram_section_use_the_default_model() {
        // `[deepgram]` is optional; the section's serde default (nova-3)
        // supports keyterm, so an absent section must not produce the
        // "vocabulary is ignored" warning. Goes through the warning builder
        // rather than `validate` because `validate` rejects a deepgram backend
        // with no section and no `WHISRS_DEEPGRAM_API_KEY` before it gets here.
        let mut config = deepgram_config_with_vocabulary("nova-3", &["whisrs".to_string()]);
        config.deepgram = None;
        assert_eq!(config.deepgram_model(), default_deepgram_model());
        let warnings = config.deepgram_keyterm_warnings("deepgram");
        assert!(
            warnings.is_empty(),
            "the default model supports keyterm; nothing to warn about: {warnings:?}"
        );
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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
            hooks: None,
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

    // ── Toggle-path LLM post-processing (issue #85) ─────────────────────

    #[test]
    fn general_llm_post_process_defaults_off_with_a_usable_instruction() {
        // Configs written before the keys existed keep the old behavior.
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"
            "#,
        )
        .unwrap();
        assert!(!config.general.llm_post_process);
        // The instruction still defaults to something usable, so turning the
        // flag on alone is a working configuration.
        assert!(config
            .general
            .llm_instruction
            .contains("Return only the corrected text"));
    }

    #[test]
    fn general_llm_post_process_parses_and_roundtrips() {
        let config: Config = toml::from_str(
            r#"
            [general]
            backend = "groq"
            llm_post_process = true
            llm_instruction = "Translate the following text into German. Return only the translation."
            "#,
        )
        .unwrap();
        assert!(config.general.llm_post_process);
        assert_eq!(
            config.general.llm_instruction,
            "Translate the following text into German. Return only the translation."
        );

        // Round-trips back out and parses again identically.
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert!(reparsed.general.llm_post_process);
        assert_eq!(
            reparsed.general.llm_instruction,
            config.general.llm_instruction
        );
    }

    #[test]
    fn config_validate_warns_llm_post_process_with_streaming_backend() {
        // local-whisper is in this list even though it is absent from the
        // llm_commands one: its transcribe() is a real batch path, but
        // dictation with it always streams, so the flag no-ops there too.
        for backend in [
            "deepgram-streaming",
            "openai-realtime",
            "openai-compatible-realtime",
            "local-whisper",
        ] {
            let mut config = validatable_config(backend);
            config.general.llm_post_process = true;

            let warnings = config.validate().unwrap();
            let warning = warnings
                .iter()
                .find(|w| {
                    w.message
                        .contains("[general] llm_post_process = true does not apply")
                })
                .unwrap_or_else(|| {
                    panic!(
                        "backend {backend} streams dictation, so there is no whole transcript \
                         to post-process; expected a warning, got: {warnings:?}"
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
                warning.message.contains("Nothing runs"),
                "the warning must say the flag does nothing, not that it degrades: {}",
                warning.message
            );
            assert!(
                warning.message.contains("asr-sidecar"),
                "the warning must name a backend the user can actually switch to: {}",
                warning.message
            );
            assert_no_stub_backend_advice(&warning.message);
        }
    }

    #[test]
    fn config_validate_no_llm_post_process_warning_for_batch_backend() {
        for backend in ["groq", "deepgram", "openai"] {
            let mut config = validatable_config(backend);
            config.general.llm_post_process = true;

            let warnings = config.validate().unwrap();
            assert!(
                warnings.iter().all(|w| !w
                    .message
                    .contains("[general] llm_post_process = true does not apply")),
                "backend {backend} goes through the batch path; no streaming warning expected: \
                 {warnings:?}"
            );
        }
    }

    #[test]
    fn config_validate_quiet_when_llm_post_process_is_off() {
        // The flag is what triggers the warning — a streaming backend on its
        // own must stay quiet about post-processing.
        let config = validatable_config("openai-realtime");
        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .all(|w| !w.message.contains("llm_post_process")),
            "post-processing is off; no warning expected: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_warns_llm_post_process_without_llm_section() {
        let mut config = validatable_config("groq");
        config.general.llm_post_process = true;
        config.llm = None;

        let warnings = config.validate().unwrap();
        assert!(
            warnings.iter().any(|w| w
                .message
                .contains("llm_post_process = true but no [llm] section")),
            "expected a missing-[llm] warning, got: {warnings:?}"
        );
    }

    #[test]
    fn config_validate_warns_llm_post_process_with_empty_instruction() {
        let mut config = validatable_config("groq");
        config.general.llm_post_process = true;
        config.general.llm_instruction = "   ".to_string();

        let warnings = config.validate().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("llm_instruction is empty")),
            "expected an empty-instruction warning, got: {warnings:?}"
        );
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

    // -----------------------------------------------------------------------
    // Inert [general] prompt / vocabulary (#140)
    // -----------------------------------------------------------------------

    /// The backends whose `sends_prompt` is false for a reason that matters:
    /// they transcribe for real, and the prompt is dropped on the way to the
    /// wire. `local-vosk`/`local-parakeet` also answer false but transcribe
    /// nothing, so they are deliberately absent.
    const PROMPTLESS_BACKENDS: [&str; 4] = [
        "deepgram",
        "deepgram-streaming",
        "openai-realtime",
        "openai-compatible-realtime",
    ];

    /// A config on `backend` with a prompt and a vocabulary that both have
    /// something to lose. `openai-realtime` is pinned to the manual-commit
    /// model `whisrs setup` writes, which is the promptless case.
    fn inert_prompt_config(backend: &str) -> Config {
        let mut config = validatable_config(backend);
        config.general.prompt = Some("Transcribe technical dictation.".to_string());
        config.general.vocabulary = vec![
            "whisrs".to_string(),
            "Hyprland".to_string(),
            "   ".to_string(),
        ];
        if backend == "openai-realtime" {
            config.openai.as_mut().unwrap().model = "gpt-realtime-whisper".to_string();
        }
        config
    }

    #[test]
    fn config_openai_realtime_model_falls_back_to_the_daemon_literal() {
        // Not default_openai_model(): that is the REST backend's
        // gpt-4o-mini-transcribe, which is server-VAD and would invert the
        // gate below.
        let mut config = validatable_config("openai-realtime");
        config.openai = None;
        assert_eq!(config.openai_realtime_model(), "gpt-realtime-whisper");
        assert_ne!(config.openai_realtime_model(), default_openai_model());
    }

    /// Every "[general] prompt is ignored" message has to end somewhere the
    /// user can actually go. Telling them a key they set does nothing and
    /// stopping there leaves them exactly where #140 found them.
    ///
    /// Asserted per shape rather than as "one of these words appears": the
    /// message shapes offer different escape hatches, and a check that
    /// accepts any of them passes on a message that has had its closing
    /// sentence deleted, because the diagnosis half still names a backend.
    fn assert_prompt_warning_offers_a_way_out(config: &Config, backend: &str, message: &str) {
        let required: &[&str] = match backend {
            "deepgram" | "deepgram-streaming" => match config.deepgram_hint_channel() {
                // The second channel is live, so it is the way out.
                DeepgramHintChannel::Live => &["[general] vocabulary", "keyterm query params"],
                // The model takes keyterm but every term is dropped by the
                // limits, and `deepgram_keyterm_warnings` has said "0 of N" in
                // the same breath. The channel is revived by trimming, not by
                // being pointed at as it stands.
                DeepgramHintChannel::NothingFits { .. } => &["Trim [general] vocabulary"],
                // The channel is dead on this model, and
                // `deepgram_keyterm_warnings` says so in the same breath. Only
                // the model switch revives it, so that has to be what this
                // names.
                DeepgramHintChannel::Unsupported => &["Switch [deepgram] model to a nova-3 model"],
            },
            // No hint channel at all: another model on the same backend, or
            // another backend entirely.
            "openai-realtime" => &["gpt-4o-transcribe", "groq"],
            "openai-compatible-realtime" => &["groq"],
            other => panic!(
                "backend {other} warns that [general] prompt is ignored, but this assertion does \
                 not know what it should offer instead. Add the shape here rather than letting a \
                 message with no way out through: {message}"
            ),
        };
        for needle in required {
            assert!(
                message.contains(needle),
                "the [general] prompt warning for backend {backend} must name a destination that \
                 actually works; expected {needle:?} in: {message}"
            );
        }
    }

    /// Pull the "[general] prompt is ignored" warning out of `validate()`,
    /// checking everything every shape of it must satisfy on the way.
    fn prompt_warning_through_validate(config: &Config, backend: &str) -> String {
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("[general] prompt is ignored"))
            .unwrap_or_else(|| {
                panic!(
                    "backend {backend} puts no prompt on the wire, so [general] prompt is \
                     discarded; expected a warning, got: {warnings:?}"
                )
            });
        assert!(
            warning
                .message
                .contains(&format!("backend = \"{backend}\"")),
            "the warning must name the backend: {}",
            warning.message
        );
        assert_inert_prompt_advice_is_reachable(&warning.message);
        assert_prompt_warning_offers_a_way_out(config, backend, &warning.message);
        warning.message.clone()
    }

    #[test]
    fn config_validate_warns_prompt_ignored_on_promptless_backends() {
        // Through validate(), not the helper, so the wiring is pinned too.
        for backend in PROMPTLESS_BACKENDS {
            let mut config = inert_prompt_config(backend);
            config.general.vocabulary.clear();

            prompt_warning_through_validate(&config, backend);
        }

        // The shape the loop above cannot reach: same backend string, a
        // `[deepgram] model` that takes no keyterm, and therefore a different
        // way out. `validatable_config` pins the default nova-3.
        for backend in ["deepgram", "deepgram-streaming"] {
            let mut config = inert_prompt_config(backend);
            config.general.vocabulary.clear();
            config.deepgram.as_mut().unwrap().model = "nova-2".to_string();

            prompt_warning_through_validate(&config, backend);
        }
    }

    #[test]
    fn config_inert_prompt_on_deepgram_without_keyterm_points_at_the_model_not_the_vocabulary() {
        // Both warnings fire here, back to back, and they have to agree.
        // `deepgram_keyterm_warnings` has just told the user the vocabulary is
        // dropped because keyterm is a Nova-3/Flux feature; a prompt warning
        // answering "use [general] vocabulary instead" would recommend, in the
        // very next line, the thing that was called dead in the previous one.
        for backend in ["deepgram", "deepgram-streaming"] {
            let mut config = inert_prompt_config(backend);
            config.deepgram.as_mut().unwrap().model = "nova-2".to_string();

            let warnings = config.validate().unwrap();
            let keyterm = warnings
                .iter()
                .find(|w| {
                    w.message
                        .contains("[general] vocabulary is ignored with [deepgram] model")
                })
                .unwrap_or_else(|| {
                    panic!(
                        "nova-2 rejects keyterm, so the vocabulary really is dropped; this test \
                         needs both warnings present to show they agree, got: {warnings:?}"
                    )
                });
            assert!(
                keyterm.message.contains("nova-3"),
                "the keyterm warning's way out is the model switch: {}",
                keyterm.message
            );

            let prompt = prompt_warning_through_validate(&config, backend);
            assert!(
                !prompt.contains("[general] vocabulary"),
                "[general] vocabulary is dropped on this model — the warning above says so — so \
                 the prompt warning must not send the user to it: {prompt}"
            );
            assert!(
                prompt.contains("nova-3"),
                "the prompt warning must point at the switch that gives Deepgram a hint channel \
                 back: {prompt}"
            );
        }
    }

    /// One term nothing can make fit: past the query budget on its own, so
    /// `effective_keyterms` drops it whatever else is in the list. Sized off
    /// the constant rather than the 5000 characters the report used, so it
    /// cannot quietly start fitting if the budget is raised.
    fn unfittable_keyterm() -> String {
        "x".repeat(deepgram::KEYTERM_QUERY_BUDGET_BYTES + 1)
    }

    #[test]
    fn config_inert_prompt_on_deepgram_with_no_keyterm_room_points_at_trimming() {
        // The *other* way `deepgram_keyterm_warnings` reports the vocabulary
        // as dropped, and the one a split on `supports_keyterm` alone misses:
        // nova-3 does take keyterm, so the model test passes, but a term past
        // the query budget leaves `effective_keyterms` empty and zero terms go
        // on the wire. "Use [general] vocabulary instead ... it does bias
        // transcription" would then recommend a channel delivering nothing, in
        // the line right after the one saying 0 of 1 reach Deepgram — the same
        // contradiction the nova-2 arm was split out to stop.
        for backend in ["deepgram", "deepgram-streaming"] {
            let mut config = inert_prompt_config(backend);
            // `validatable_config` pins the default nova-3, so the keyterm
            // channel is supported here and only the terms are the problem.
            config.general.vocabulary = vec![unfittable_keyterm()];
            assert!(
                deepgram::supports_keyterm(&config.deepgram_model()),
                "this case is about a model that does take keyterm; a pre-Nova-3 one is the \
                 test above"
            );

            let warnings = config.validate().unwrap();
            let keyterm = warnings
                .iter()
                .find(|w| w.message.contains("usable term(s) reach"))
                .unwrap_or_else(|| {
                    panic!(
                        "one oversized term is dropped by the keyterm limits; this test needs \
                         both warnings present to show they agree, got: {warnings:?}"
                    )
                });
            assert!(
                keyterm.message.contains("0 of 1 usable term(s) reach"),
                "the vocabulary has to be delivering nothing for this case to exist: {}",
                keyterm.message
            );
            assert!(
                keyterm.message.contains("Trim the list"),
                "the keyterm warning's way out is the trim; the prompt warning has to point at \
                 the same one: {}",
                keyterm.message
            );

            let prompt = prompt_warning_through_validate(&config, backend);
            assert!(
                !prompt.contains("Use [general] vocabulary instead"),
                "[general] vocabulary carries zero terms on this config — the warning above \
                 says so — so the prompt warning must not recommend it as the channel that \
                 works: {prompt}"
            );
            assert!(
                !prompt.contains("does bias transcription"),
                "nothing reaches the model through either channel here; claiming the \
                 vocabulary biases transcription is the contradiction: {prompt}"
            );
            assert!(
                prompt.contains("Trim [general] vocabulary"),
                "trimming is what revives the channel, so that is where the warning has to \
                 point: {prompt}"
            );
        }
    }

    /// A vocabulary the keyterm limits drop *part* of: more terms than
    /// [`deepgram::KEYTERM_MAX_TERMS`] admits, each short enough that the term
    /// count is what bites rather than [`deepgram::KEYTERM_QUERY_BUDGET_BYTES`]
    /// or [`deepgram::KEYTERM_MAX_WORDS`].
    ///
    /// Sized off the cap rather than written out as a count, so it cannot rot
    /// into an all-fit list when a limit moves: asking for more terms than the
    /// term cap admits leaves at least one on the floor whatever the three
    /// numbers are, and a raised cap that hands the byte budget or the word
    /// cap the decision instead still drops some and keeps some. The caller
    /// asserts the split is real before relying on it, so if some future
    /// combination of limits does make this list fit whole, the test says so
    /// instead of quietly passing.
    fn partially_dropped_vocabulary() -> Vec<String> {
        (0..deepgram::KEYTERM_MAX_TERMS + 5)
            .map(|i| format!("term{i}"))
            .collect()
    }

    #[test]
    fn config_inert_prompt_on_deepgram_with_a_partial_keyterm_drop_points_at_the_vocabulary() {
        // The Live/NothingFits boundary, which nothing else in this file
        // stands on. `deepgram_hint_channel` asks whether `effective_keyterms`
        // is *empty*, not whether it is *short*, and that is the whole reason
        // the NothingFits arm can say "none of the N term(s) ... fit" — it is
        // only reached when none do. Widening the test to `effective < usable`
        // leaves every other test here green while making this config print
        // "none of the 205 term(s) in [general] vocabulary fit the keyterm
        // limits" directly beneath `deepgram_keyterm_warnings`' "200 of 205
        // usable term(s) reach Deepgram": the same contradiction the
        // three-state channel was introduced to remove, with the numbers
        // swapped. The two warnings are built from one pair of functions
        // precisely so they cannot disagree about a number, and a partial drop
        // is the only shape where the wrong test disagrees.
        for backend in ["deepgram", "deepgram-streaming"] {
            let mut config = inert_prompt_config(backend);
            // `validatable_config` pins the default nova-3, so the model takes
            // keyterm and only the limits decide how much arrives.
            config.general.vocabulary = partially_dropped_vocabulary();
            assert!(
                deepgram::supports_keyterm(&config.deepgram_model()),
                "a partial drop needs a model that takes keyterm at all"
            );

            let usable = deepgram::usable_keyterms(&config.general.vocabulary).count();
            let effective = deepgram::effective_keyterms(&config.general.vocabulary).len();
            assert!(
                effective > 0 && effective < usable,
                "this fixture is a boundary case only while the limits drop *some* of it; \
                 {effective} of {usable} terms reach the wire"
            );

            let warnings = config.validate().unwrap();
            let keyterm = warnings
                .iter()
                .find(|w| w.message.contains("usable term(s) reach"))
                .unwrap_or_else(|| {
                    panic!(
                        "the keyterm limits drop part of this vocabulary; this test needs both \
                         warnings present to show they agree, got: {warnings:?}"
                    )
                });
            assert!(
                keyterm
                    .message
                    .contains(&format!("{effective} of {usable} usable term(s) reach")),
                "the keyterm warning has to report the partial delivery this case is about: {}",
                keyterm.message
            );

            assert!(
                matches!(config.deepgram_hint_channel(), DeepgramHintChannel::Live),
                "{effective} of {usable} term(s) are reaching Deepgram, so the keyterm channel \
                 is live and the prompt warning is allowed to point at it"
            );

            let prompt = prompt_warning_through_validate(&config, backend);
            assert!(
                prompt.contains("Use [general] vocabulary instead"),
                "part of the vocabulary does reach the model, so the channel that works is \
                 still the way out: {prompt}"
            );
            assert!(
                !prompt.contains("fit the keyterm limits"),
                "that clause is the NothingFits prose and it says none of the terms fit, while \
                 {effective} of {usable} do — printing it here contradicts the keyterm warning \
                 one line above: {prompt}"
            );
            assert!(
                !prompt.contains("Trim [general] vocabulary"),
                "trimming is the NothingFits way out; with a live channel the warning has to \
                 point at the channel instead: {prompt}"
            );
        }
    }

    /// Pull the "[general] vocabulary is ignored with backend" warning out of
    /// `validate()`, with the checks every shape of it must satisfy.
    fn vocabulary_warning_through_validate(config: &Config, backend: &str) -> String {
        let warnings = config.validate().unwrap();
        let warning = warnings
            .iter()
            .find(|w| {
                w.message
                    .contains("[general] vocabulary is ignored with backend")
            })
            .unwrap_or_else(|| {
                panic!(
                    "backend {backend} folds the vocabulary into a prompt it never sends; \
                     expected a warning, got: {warnings:?}"
                )
            });
        assert_inert_prompt_advice_is_reachable(&warning.message);
        warning.message.clone()
    }

    #[test]
    fn config_inert_prompt_lemonade_vocabulary_offers_deepgram_only_when_keyterm_would_land() {
        // "or to deepgram, which sends vocabulary as keyterm query params" is
        // advice about a config the user does not have yet, so it has to be
        // checked against the `[deepgram] model` they do have. Nothing else
        // will: `deepgram_keyterm_warnings` is gated on the *active* backend,
        // which is Lemonade here, so it stays silent about the stale section
        // this clause is about to send them to.
        let backend = "openai-compatible-realtime";

        let live = inert_prompt_config(backend);
        let message = vocabulary_warning_through_validate(&live, backend);
        assert!(
            message.contains("or to deepgram, which sends vocabulary as keyterm query params"),
            "the default nova-3 takes keyterm and the terms fit, so deepgram really is a way \
             out and the clause belongs: {message}"
        );

        // The scenario `deepgram_keyterm_warnings`' own doc comment calls
        // routine: `whisrs setup` wrote a `[deepgram]` section, the user
        // switched backends, and the model left behind predates keyterm.
        // Following the clause lands them on a config where the vocabulary is
        // dropped again, and this time nothing warns.
        let mut stale = inert_prompt_config(backend);
        stale.deepgram.as_mut().unwrap().model = "nova-2".to_string();
        let message = vocabulary_warning_through_validate(&stale, backend);
        assert!(
            !message.contains("deepgram"),
            "[deepgram] model = \"nova-2\" rejects keyterm, so deepgram is not a destination \
             for this vocabulary and must not be named: {message}"
        );
        assert!(
            message.contains("groq"),
            "dropping the deepgram clause must not leave the warning without a way out: \
             {message}"
        );

        // The other way the channel dies, the one Finding 1 is about: a model
        // that does take keyterm, and terms the limits drop whole.
        let mut unfittable = inert_prompt_config(backend);
        unfittable.general.vocabulary = vec![unfittable_keyterm()];
        let message = vocabulary_warning_through_validate(&unfittable, backend);
        assert!(
            !message.contains("deepgram"),
            "every term is dropped by the keyterm limits, so switching to deepgram would \
             deliver none of them: {message}"
        );
    }

    #[test]
    fn config_inert_prompt_silent_for_backends_that_send_the_prompt() {
        // The helper directly, not validate(): asr-sidecar's hard URL check
        // rejects validatable_config outright, and the gate under test is the
        // backend match, not the API-key plumbing.
        for backend in ["groq", "openai", "asr-sidecar", "local-whisper"] {
            let config = inert_prompt_config(backend);
            let warnings = config.inert_prompt_warnings(backend);
            assert!(
                warnings.is_empty(),
                "backend {backend} sends the prompt, so neither key is inert: {warnings:?}"
            );
        }
    }

    #[test]
    fn config_inert_prompt_never_warns_about_vocabulary_on_deepgram() {
        // The regression guard for this change. Deepgram is promptless, so
        // warning A fires — but the vocabulary rides as `keyterm` query params
        // and really does bias transcription, so warning B must not. Telling a
        // Deepgram user their vocabulary is ignored would push them off the one
        // hint that works.
        for backend in ["deepgram", "deepgram-streaming"] {
            let config = inert_prompt_config(backend);
            let warnings = config.inert_prompt_warnings(backend);

            assert!(
                warnings
                    .iter()
                    .any(|w| w.message.contains("[general] prompt is ignored")),
                "backend {backend} has no prompt field; the prompt warning must still fire: \
                 {warnings:?}"
            );
            assert!(
                warnings.iter().all(|w| !w
                    .message
                    .contains("[general] vocabulary is ignored with backend")),
                "backend {backend} sends [general] vocabulary as keyterm query params — it is \
                 not ignored, and saying so would send the user away from the hint that works: \
                 {warnings:?}"
            );
        }
    }

    #[test]
    fn config_inert_prompt_warns_vocabulary_on_realtime_backends_with_the_term_count() {
        for backend in ["openai-realtime", "openai-compatible-realtime"] {
            let config = inert_prompt_config(backend);
            let warnings = config.inert_prompt_warnings(backend);
            let warning = warnings
                .iter()
                .find(|w| {
                    w.message
                        .contains("[general] vocabulary is ignored with backend")
                })
                .unwrap_or_else(|| {
                    panic!(
                        "backend {backend} folds the vocabulary into a prompt it never sends; \
                         expected a warning, got: {warnings:?}"
                    )
                });
            assert!(
                warning
                    .message
                    .contains(&format!("backend = \"{backend}\"")),
                "the warning must name the backend: {}",
                warning.message
            );
            // Two usable terms; the blank third one is not a term.
            assert!(
                warning.message.contains("2 term(s)"),
                "the warning must count only the non-blank terms: {}",
                warning.message
            );
            assert!(
                warning.message.contains("groq"),
                "the warning must name a backend the user can actually switch to: {}",
                warning.message
            );
            assert_inert_prompt_advice_is_reachable(&warning.message);
        }
    }

    #[test]
    fn config_inert_prompt_silent_for_server_vad_openai_realtime_model() {
        let mut config = inert_prompt_config("openai-realtime");
        config.openai.as_mut().unwrap().model = "gpt-4o-transcribe".to_string();

        let warnings = config.inert_prompt_warnings("openai-realtime");
        assert!(
            warnings.is_empty(),
            "server-VAD models get a real prompt in the session.update; nothing is inert: \
             {warnings:?}"
        );
    }

    #[test]
    fn config_inert_prompt_silent_for_blank_prompt_and_blank_vocabulary() {
        // Blank is absent. The prompt half is literally that — the daemon's
        // `transcription_prompt` trims and filters it away. The vocabulary
        // half is not: blank entries are joined into the runtime prompt like
        // any other. Either way there is nothing to report, because a
        // whitespace term biases nothing.
        for backend in PROMPTLESS_BACKENDS {
            let mut config = inert_prompt_config(backend);
            config.general.prompt = Some("   \t ".to_string());
            config.general.vocabulary = vec![String::new(), "  ".to_string()];

            let warnings = config.inert_prompt_warnings(backend);
            assert!(
                warnings.is_empty(),
                "backend {backend} has nothing to lose: a whitespace-only prompt never reaches \
                 the request at all, and a vocabulary of blanks carries no term to drop: \
                 {warnings:?}"
            );
        }
    }

    /// The gate and `sends_prompt` both read
    /// `openai_turn_detection_mode_for_model`, so the totality test below
    /// cannot catch that mapping itself moving: both sides move with it and
    /// keep agreeing. This pins its absolute answers instead. The mixed-case
    /// row is the one that matters in practice, since the mapping is
    /// `eq_ignore_ascii_case` and a config is free to spell the model with
    /// capitals; making it case-sensitive leaves every other test green.
    #[test]
    fn openai_turn_detection_mapping_is_case_insensitive() {
        for (model, manual_commit) in [
            ("gpt-realtime-whisper", true),
            ("GPT-Realtime-Whisper", true),
            ("gpt-4o-transcribe", false),
        ] {
            assert_eq!(
                matches!(
                    openai_turn_detection_mode_for_model(model),
                    TurnDetectionMode::ManualCommit
                ),
                manual_commit,
                "the turn-detection mapping for {model} moved; the inert-prompt gate                  and every backend's sends_prompt move silently with it"
            );
        }
    }

    /// One case for the test below: a backend name `validate()` routes, the
    /// model that name resolves to, and the real backend object that answers
    /// `sends_prompt` for it.
    struct SendsPromptCase {
        /// The `[general] backend` string, aliases included.
        name: &'static str,
        /// The model the daemon would put in the request for that name. Only
        /// the realtime and Deepgram arms read it, but it is carried for every
        /// case so the config and the request cannot describe different models.
        model: &'static str,
        backend: Box<dyn crate::transcription::TranscriptionBackend>,
    }

    /// The gate in `inert_prompt_warnings` is a hand-written list of backend
    /// name strings, and `TranscriptionBackend::sends_prompt` is the authority
    /// it exists to mirror. That method is *required* rather than defaulted
    /// precisely so a new backend cannot inherit a wrong answer — the same
    /// rule the `WindowTracker::get_focused_window_class` case study in
    /// CLAUDE.md is about. A string list one layer up quietly rebuilds the
    /// trap: a new promptless backend gets no warning, and nothing turns red.
    ///
    /// So ask the real impls. Instantiate every backend `Config::validate`
    /// can route to, hand each one a request carrying a prompt, and require
    /// the gate to produce the prompt warning exactly when `sends_prompt` is
    /// false.
    ///
    /// What makes that "every" rather than "the ones someone remembered": the
    /// case list is checked against `BACKEND_NAMES` at the end, and `validate`
    /// rejects any `[general] backend` outside that const before it reaches a
    /// match arm. So a new backend cannot be selectable without an entry in
    /// the const, and cannot have an entry in the const without a case here.
    /// [`BACKEND_NAMES`]' own doc records what the hand-written case list this
    /// replaced let through.
    ///
    /// Two limits, stated rather than implied. The daemon's `create_backend`
    /// lives in a separate binary crate and cannot be called from here, so
    /// what is proved is agreement for every name `validate` routes; a backend
    /// wired into the factory and nowhere else is not covered, but it is also
    /// not reachable, because `validate` rejects its name first. And the two
    /// local-whisper names are skipped when the feature is off, where the real
    /// impl is not compiled in at all — the default-feature run covers them.
    #[test]
    fn config_inert_prompt_gate_agrees_with_every_sends_prompt_impl() {
        use crate::transcription::asr_sidecar::AsrSidecarBackend;
        use crate::transcription::deepgram::{DeepgramRestBackend, DeepgramStreamingBackend};
        use crate::transcription::groq::GroqBackend;
        use crate::transcription::openai_compatible_realtime::OpenAiCompatibleRealtimeBackend;
        use crate::transcription::openai_realtime::OpenAIRealtimeBackend;
        use crate::transcription::openai_rest::OpenAIRestBackend;
        use crate::transcription::TranscriptionConfig;

        // `local-vosk` and `local-parakeet` are deliberately absent. They
        // answer `sends_prompt() == false` like the four promptless backends,
        // but their `transcribe()` bails with "not yet implemented" — nothing
        // is discarded because nothing is transcribed, so a warning about a
        // dropped hint would be describing a request that never happens. They
        // are the one exception to the rule this test enforces, and it is
        // written down here rather than left to be rediscovered.
        let mut cases = vec![
            SendsPromptCase {
                name: "deepgram",
                model: "nova-3",
                backend: Box::new(DeepgramRestBackend::new(String::new())),
            },
            SendsPromptCase {
                name: "deepgram-streaming",
                model: "nova-3",
                backend: Box::new(DeepgramStreamingBackend::new(String::new())),
            },
            SendsPromptCase {
                name: "groq",
                model: "whisper-large-v3-turbo",
                backend: Box::new(GroqBackend::new(String::new())),
            },
            SendsPromptCase {
                name: "openai",
                model: "gpt-4o-mini-transcribe",
                backend: Box::new(OpenAIRestBackend::new(String::new())),
            },
            // Both sides of the per-model split, on one backend struct: the
            // manual-commit model `whisrs setup` writes, and a server-VAD one.
            // The gate has to follow the model here, not the backend name.
            SendsPromptCase {
                name: "openai-realtime",
                model: "gpt-realtime-whisper",
                backend: Box::new(OpenAIRealtimeBackend::new(String::new())),
            },
            SendsPromptCase {
                name: "openai-realtime",
                model: "gpt-4o-transcribe",
                backend: Box::new(OpenAIRealtimeBackend::new(String::new())),
            },
            // The mapping is `eq_ignore_ascii_case`, so a config that spells
            // the model with capitals still resolves to manual-commit. The
            // gate reads the same function, so it has to agree here too.
            SendsPromptCase {
                name: "openai-realtime",
                model: "GPT-Realtime-Whisper",
                backend: Box::new(OpenAIRealtimeBackend::new(String::new())),
            },
            SendsPromptCase {
                name: "openai-compatible-realtime",
                model: "Whisper-Tiny",
                backend: Box::new(
                    OpenAiCompatibleRealtimeBackend::new(
                        "ws://localhost:1234/realtime".to_string(),
                        "Whisper-Tiny".to_string(),
                        "lemonade".to_string(),
                        "server-vad".to_string(),
                        None,
                    )
                    .expect("the same values validatable_config carries"),
                ),
            },
        ];
        // The aliases are names `validate()` routes too, so the gate has to
        // answer for them as well — `asr`/`vibevoice` reach the sidecar and
        // `local` reaches whisper.cpp.
        for name in ["asr-sidecar", "asr", "vibevoice"] {
            cases.push(SendsPromptCase {
                name,
                model: "",
                backend: Box::new(AsrSidecarBackend::new(
                    "http://127.0.0.1:8765/transcribe".to_string(),
                    None,
                )),
            });
        }
        // Gated so the test still compiles under --no-default-features: the
        // stub that stands in for the real backend there answers `false`
        // (there is no request-building code to read the answer off), which
        // is the opposite of what whisper.cpp does.
        #[cfg(feature = "local-whisper")]
        for name in ["local-whisper", "local"] {
            cases.push(SendsPromptCase {
                name,
                model: "",
                backend: Box::new(
                    crate::transcription::local_whisper::LocalWhisperBackend::new(String::new()),
                ),
            });
        }

        // The half that makes this total. Every name a user can select is in
        // `BACKEND_NAMES` — `validate()` rejects the rest before the match —
        // so requiring a case per entry is requiring a case per backend.
        for entry in BACKEND_NAMES {
            if entry.kind == BackendNameKind::Stub {
                // The documented exception above: nothing is discarded because
                // nothing is transcribed.
                continue;
            }
            if !cfg!(feature = "local-whisper") && matches!(entry.name, "local-whisper" | "local") {
                // The real impl is not compiled in, so there is no answer to
                // agree with. The default-feature run is where these are
                // covered.
                continue;
            }
            assert!(
                cases.iter().any(|case| case.name == entry.name),
                "{} is a backend validate() routes, so a user can select it and hit the \
                 inert-prompt gate, but this test has no case for it. Add one — with the \
                 backend's real impl, not a guess at its sends_prompt answer.",
                entry.name
            );
        }

        for case in &cases {
            // Every name here must be one `validate()` actually routes, or
            // the agreement proved below is about a string nobody can set.
            if let Err(e) = validatable_config(case.name).validate() {
                let message = e.to_string();
                assert!(
                    !message.contains("Unknown backend"),
                    "{} is not a backend validate() can see: {message}",
                    case.name
                );
            }

            let mut config = inert_prompt_config(case.name);
            // One model, read by both sides: the gate resolves it from the
            // config, the impl reads it off the request.
            if !case.model.is_empty() {
                match case.name {
                    "deepgram" | "deepgram-streaming" => {
                        config.deepgram.as_mut().unwrap().model = case.model.to_string();
                    }
                    "openai" | "openai-realtime" => {
                        config.openai.as_mut().unwrap().model = case.model.to_string();
                    }
                    _ => {}
                }
            }
            let request = TranscriptionConfig {
                language: "en".to_string(),
                model: case.model.to_string(),
                prompt: config.general.prompt.clone(),
                keyterms: Vec::new(),
            };
            assert!(
                request
                    .prompt
                    .as_deref()
                    .is_some_and(|p| !p.trim().is_empty()),
                "the request must carry a prompt, or neither side has anything to answer about"
            );

            let warned = config
                .inert_prompt_warnings(case.name)
                .iter()
                .any(|w| w.message.contains("[general] prompt is ignored"));
            assert_eq!(
                warned,
                !case.backend.sends_prompt(&request),
                "backend {} on model {:?} answers sends_prompt() == {}; the [general] prompt \
                 warning must fire exactly when that is false, and the gate in \
                 inert_prompt_warnings says otherwise",
                case.name,
                case.model,
                case.backend.sends_prompt(&request)
            );
        }
    }
}
