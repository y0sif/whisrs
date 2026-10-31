//! What the Config screen shows: every config.toml key, as a row of a
//! section, with its kind and a help line.
//!
//! The table is checked against the config types themselves: a test
//! serializes a `Config` with every field set and fails on a key that is
//! neither a row here nor in [`EDITOR_ONLY`], so a new field cannot slip in
//! without a way to edit it.

use whisrs::config::catalog;

/// A section of the Config screen.
pub struct Section {
    pub title: &'static str,
    pub body: Body,
    /// Built without this feature, the section says so instead of showing
    /// its rows, and the file keeps the section as it is.
    pub gate: Option<Gate>,
    /// After its rows, every entry's hotkeys (`[[agents]]`,
    /// `[[llm_commands]]`), so one section shows every binding. They are
    /// the entries' own keys, not copies.
    pub entry_hotkeys: bool,
}

/// What a section holds.
#[derive(Clone, Copy)]
pub enum Body {
    Rows(&'static [Field]),
    /// An array of tables (`[[agents]]`): an entry list, then a form.
    Entries(&'static Entries),
    /// A table of strings (`[replacements]`): a two-column editor.
    Map(&'static MapField),
}

/// One row: a key and how it is edited.
pub struct Field {
    /// The TOML path, dotted (`overlay.colors.recording`). Relative to the
    /// entry for an [`Entries`] field.
    pub path: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    /// Built without this feature, the daemon ignores the key.
    pub gate: Option<Gate>,
}

/// A text value's check: the value as stored, or why it is refused.
pub type Check = fn(&str) -> Result<String, String>;

/// A value a row offers, and what it is.
pub type Opt = catalog::Entry;

/// How a value is shown and changed.
pub enum Kind {
    Bool,
    /// An `Option<bool>`; unset shows `unset`.
    MaybeBool {
        unset: &'static str,
    },
    /// A closed set: `h`/`l` cycle it, `Enter` opens the picker.
    Choice(&'static [Opt]),
    /// An `Option<String>` from a closed set; unset shows `unset`.
    MaybeChoice {
        unset: &'static str,
        choices: &'static [Opt],
    },
    /// A whole number: `h`/`l` step it, `H`/`L` by `big`, `Enter` types it.
    Int(Steps),
    /// A number, the same way; `percent` shows 0 to 1 as a bar.
    Float {
        min: f64,
        max: f64,
        step: f64,
        big: f64,
        percent: bool,
    },
    /// A string with nothing to suggest. With `unset`, an `Option<String>`
    /// that a blank input clears; without, a plain string. `check` refuses
    /// a value, or returns it normalized. Every one is in [`FREE_TEXT`].
    Text {
        unset: Option<&'static str>,
        check: Option<Check>,
    },
    /// A string with values to suggest: `h`/`l` step through them, `Enter`
    /// picks one or takes a typed value through `check`.
    Suggest {
        unset: Option<&'static str>,
        check: Option<Check>,
        from: Source,
    },
    /// A path, completed with `Tab` while typing. `tilde`: the daemon
    /// expands `~`, so a completion keeps it; else it writes the home
    /// folder out. With `from`, picked like [`Kind::Suggest`].
    Path {
        unset: Option<&'static str>,
        tilde: bool,
        from: Option<Source>,
    },
    /// A hex colour, shown with a swatch of it (of the theme's while unset).
    Color,
    /// A hotkey, set in the hotkey dialog. With `unset`, an
    /// `Option<String>`; without, a string that may be empty.
    Hotkey {
        unset: Option<&'static str>,
    },
    /// An API key: shown as where it comes from, typed as bullets.
    Secret,
    /// A list of strings, edited one item at a time; `empty` describes
    /// none. With `items`, an item is picked like [`Kind::Suggest`].
    List {
        empty: &'static str,
        items: Option<(Source, Check)>,
    },
}

/// A whole number's range and steps.
pub struct Steps {
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub big: i64,
    /// After the number: `ms`, `px`. `min` adds the hours, `days` says
    /// `1 day`.
    pub unit: &'static str,
    /// Values shown by name instead (`0` is `off`).
    pub named: &'static [(i64, &'static str)],
}

/// Where a row's suggestions come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    List(&'static [Opt]),
    /// `auto`, then Whisper's languages; without `auto` for a list of
    /// expected languages.
    Languages {
        auto: bool,
    },
    /// The models of the form's `[tts] backend`.
    TtsModels,
    /// The voices of the form's `[tts] backend`.
    TtsVoices,
    /// The chat models of the form's `[llm] api_url` host.
    LlmModels,
    /// Microphones, from the sound server, on a worker.
    Inputs,
    /// What the speakers play (monitor sources), on a worker.
    Monitors,
    /// `ggml-*.bin` in the models folder, on a worker.
    ModelFiles,
}

impl Source {
    /// The worker list it needs, if any.
    pub fn list(self) -> Option<ListKind> {
        match self {
            Source::Inputs | Source::Monitors => Some(ListKind::Devices),
            Source::ModelFiles => Some(ListKind::Models),
            _ => None,
        }
    }
}

/// A list that loads on a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// The sound server's input sources.
    Devices,
    /// The whisper models in the models folder.
    Models,
}

/// An array of tables.
pub struct Entries {
    pub path: &'static str,
    /// One entry, for messages ("agent", "command").
    pub noun: &'static str,
    pub fields: &'static [Field],
    /// The keys a new entry needs to parse, set empty.
    pub required: &'static [&'static str],
    /// The keys shown beside an entry's name in the list.
    pub summary: &'static [&'static str],
}

/// A table of strings.
pub struct MapField {
    pub path: &'static str,
    pub key_label: &'static str,
    pub value_label: &'static str,
    pub help: &'static str,
}

/// A cargo feature a section or key needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Recording,
    LocalWhisper,
    Tray,
    Overlay,
    Hooks,
}

impl Gate {
    pub fn built(self) -> bool {
        match self {
            Gate::Recording => cfg!(feature = "recording"),
            Gate::LocalWhisper => cfg!(feature = "local-whisper"),
            Gate::Tray => cfg!(feature = "tray"),
            Gate::Overlay => cfg!(feature = "overlay"),
            Gate::Hooks => cfg!(feature = "hooks"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Gate::Recording => "recording",
            Gate::LocalWhisper => "local-whisper",
            Gate::Tray => "tray",
            Gate::Overlay => "overlay",
            Gate::Hooks => "hooks",
        }
    }
}

/// What the Recording section says in a build without the feature.
pub const RECORDING_MISSING: &str = "This whisrs was built without the recording feature, so \
     there is nothing to set here, and [recording] is kept as it is. Use a release build, or \
     rebuild with `cargo install whisrs --features recording`.";

/// Keys with no row: edited in config.toml itself (`e`). The backends they
/// configure are not implemented yet, which the backend row's help says.
#[cfg(test)]
pub const EDITOR_ONLY: &[&str] = &["local-vosk.model_path", "local-parakeet.model_path"];

/// The string keys that are typed, with nothing whisrs could suggest, and
/// why. A new string key has to be decided: suggested, or listed here.
#[cfg(test)]
pub const FREE_TEXT: &[(&str, &str)] = &[
    ("general.prompt", "free-form context in your own words"),
    (
        "general.llm_instruction",
        "an instruction in your own words",
    ),
    ("asr-sidecar.url", "where your own server listens"),
    ("asr-sidecar.model", "named by your own server"),
    (
        "openai-compatible-realtime.url",
        "where your own server listens",
    ),
    (
        "openai-compatible-realtime.model",
        "named by your own server",
    ),
    ("tts.url", "where your own server listens"),
    ("recording.user_name", "your name"),
    (
        "recording.diarization_url",
        "where your own sidecar listens",
    ),
    ("recording.summary_instruction", "rules in your own words"),
    ("hooks.on_record_start", "a shell command of your own"),
    ("hooks.on_record_stop", "a shell command of your own"),
    ("llm_commands.name", "a name you choose"),
    (
        "llm_commands.instruction",
        "an instruction in your own words",
    ),
    ("agents.name", "a name you choose"),
    (
        "agents.command",
        "a binary of your own, else the harness on PATH",
    ),
    (
        "agents.reply_instruction",
        "an instruction in your own words",
    ),
];

/// The values the backend row offers: every backend, the two not
/// implemented yet last.
pub const BACKENDS: &[Opt] = catalog::BACKENDS;

/// Why `value` cannot be picked for a backend row in this build, short
/// (`not built in`) and in full; `None` when it can.
pub fn backend_refusal(value: &str) -> Option<(&'static str, String)> {
    if matches!(value, "local-whisper" | "local") && !Gate::LocalWhisper.built() {
        return Some((
            "not built in",
            "this whisrs was built without local-whisper: use a release build, or rebuild \
             with cargo install whisrs --features local-whisper"
                .to_string(),
        ));
    }
    if catalog::STUB_BACKENDS.contains(&value) {
        return Some((
            "coming soon",
            format!("{value} is not implemented yet, so it cannot transcribe"),
        ));
    }
    None
}

/// An `http(s)://` URL, stored trimmed.
fn check_url(input: &str) -> Result<String, String> {
    let input = input.trim();
    let url = reqwest::Url::parse(input).map_err(|e| format!("not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("use http:// or https://, not {}://", url.scheme()));
    }
    Ok(input.to_string())
}

/// A language as `-l` takes it, stored normalized, and one whisrs knows:
/// `auto`, `multi` (Deepgram), or a tag whose primary subtag is in the
/// language table (`en-US`, `zh-Hans`). `-l` itself stays structural
/// (`validate_language_override`): backends accept different sets.
fn check_language(input: &str) -> Result<String, String> {
    let tag = whisrs::validate_language_override(input)?;
    let primary = tag.split('-').next().unwrap_or_default();
    if matches!(tag.as_str(), "auto" | "multi")
        || catalog::find(catalog::LANGUAGES, primary).is_some()
    {
        return Ok(tag);
    }
    Err(format!(
        "{tag} is not a language whisrs knows: pick one from the list (e edits the file for \
         anything else)"
    ))
}

/// A language openai-realtime is told to expect: a code, since `auto` and
/// `multi` say nothing about which.
fn check_expected_language(input: &str) -> Result<String, String> {
    let code = check_language(input)?;
    if matches!(code.as_str(), "auto" | "multi") {
        return Err(format!(
            "{code} is no language: list the ones you expect, or none"
        ));
    }
    Ok(code)
}

/// A pill colour: `#RGB`, `#RRGGBB` or `#RRGGBBAA`.
pub fn check_color(input: &str) -> Result<String, String> {
    let input = input.trim();
    match whisrs::parse_hex_color(input) {
        Some(_) => Ok(input.to_string()),
        None => Err("use a hex colour: #RGB, #RRGGBB or #RRGGBBAA".to_string()),
    }
}

/// A hotkey `parse_hotkey` takes, stored as typed.
pub fn check_hotkey(input: &str) -> Result<String, String> {
    let input = input.trim();
    whisrs::hotkey::parse_hotkey(input)
        .map(|_| input.to_string())
        .map_err(|e| e.to_string())
}

/// An optional string; `unset` says what applies while it is not set.
const fn text(unset: &'static str) -> Kind {
    Kind::Text {
        unset: Some(unset),
        check: None,
    }
}

/// A plain string.
const STRING: Kind = Kind::Text {
    unset: None,
    check: None,
};

/// A string picked from `list`, or typed.
const fn suggest(list: &'static [Opt]) -> Kind {
    Kind::Suggest {
        unset: None,
        check: None,
        from: Source::List(list),
    }
}

/// A language: `auto` or a code, picked or typed.
const fn language(unset: Option<&'static str>) -> Kind {
    Kind::Suggest {
        unset,
        check: Some(check_language),
        from: Source::Languages { auto: true },
    }
}

/// An optional hotkey.
const HOTKEY: Kind = Kind::Hotkey {
    unset: Some("not set"),
};

/// Milliseconds.
const fn ms(min: i64, max: i64, step: i64, big: i64) -> Kind {
    Kind::Int(Steps {
        min,
        max,
        step,
        big,
        unit: "ms",
        named: &[],
    })
}

/// Pixels.
const fn px(min: i64, max: i64, step: i64, big: i64) -> Kind {
    Kind::Int(Steps {
        min,
        max,
        step,
        big,
        unit: "px",
        named: &[],
    })
}

const fn field(path: &'static str, label: &'static str, kind: Kind, help: &'static str) -> Field {
    Field {
        path,
        label,
        kind,
        help,
        gate: None,
    }
}

const fn gated(
    path: &'static str,
    label: &'static str,
    kind: Kind,
    help: &'static str,
    gate: Gate,
) -> Field {
    Field {
        path,
        label,
        kind,
        help,
        gate: Some(gate),
    }
}

const GENERAL: &[Field] = &[
    field(
        "general.language",
        "language",
        language(None),
        "The language you dictate in, or auto. whisrs toggle -l overrides it for one \
         dictation.",
    ),
    field(
        "general.silence_timeout_ms",
        "silence timeout",
        Kind::Int(Steps {
            min: 0,
            max: 600_000,
            step: 250,
            big: 1000,
            unit: "ms",
            named: &[(0, "off")],
        }),
        "Stop dictating after this much silence. off never stops on its own.",
    ),
    field(
        "general.notify",
        "notifications",
        Kind::Bool,
        "Desktop notifications for dictation and errors.",
    ),
    field(
        "general.history",
        "history",
        Kind::Bool,
        "Keep finished dictations for whisrs log. Off stops new entries; whisrs log --clear \
         removes the old ones.",
    ),
    field(
        "general.audio_feedback",
        "audio feedback",
        Kind::Bool,
        "Tones when dictation starts, stops and is done.",
    ),
    field(
        "general.audio_feedback_volume",
        "feedback volume",
        Kind::Float {
            min: 0.0,
            max: 1.0,
            step: 0.05,
            big: 0.25,
            percent: true,
        },
        "Volume of the tones.",
    ),
    gated(
        "general.tray",
        "tray icon",
        Kind::Bool,
        "Show the tray icon.",
        Gate::Tray,
    ),
];

const BACKEND: &[Field] = &[
    field(
        "general.backend",
        "backend",
        Kind::Choice(BACKENDS),
        "The transcription backend. Its key or model is below. local-vosk and local-parakeet \
         are not implemented yet; e edits their sections in config.toml.",
    ),
    field(
        "deepgram.api_key",
        "deepgram api_key",
        Kind::Secret,
        "API key for the deepgram backends. WHISRS_DEEPGRAM_API_KEY wins over it.",
    ),
    field(
        "deepgram.model",
        "deepgram model",
        suggest(catalog::DEEPGRAM_MODELS),
        "The Deepgram model. Nova-3 models take your vocabulary as keyterms; older ones get \
         none of it.",
    ),
    field(
        "groq.api_key",
        "groq api_key",
        Kind::Secret,
        "API key for groq. WHISRS_GROQ_API_KEY wins over it.",
    ),
    field(
        "groq.model",
        "groq model",
        suggest(catalog::GROQ_MODELS),
        "The Whisper model on Groq.",
    ),
    field(
        "openai.api_key",
        "openai api_key",
        Kind::Secret,
        "API key for the openai backends. WHISRS_OPENAI_API_KEY wins over it.",
    ),
    field(
        "openai.model",
        "openai model",
        suggest(catalog::OPENAI_MODELS),
        "The model of both openai backends. A recording on openai sends gpt-transcribe in \
         place of a realtime-only one.",
    ),
    field(
        "openai.languages",
        "openai languages",
        Kind::List {
            empty: "none",
            items: Some((Source::Languages { auto: false }, check_expected_language)),
        },
        "Expected input languages for gpt-live-transcribe or gpt-transcribe on \
         openai-realtime. Overrides [general] language.",
    ),
    gated(
        "local-whisper.model_path",
        "whisper model",
        Kind::Path {
            unset: None,
            tilde: false,
            from: Some(Source::ModelFiles),
        },
        "The ggml model file. Those in the models folder are listed; any other path can be \
         typed, tab completes it. whisrs setup downloads one.",
        Gate::LocalWhisper,
    ),
    gated(
        "local-whisper.segmentation",
        "segmentation",
        Kind::Choice(catalog::SEGMENTATIONS),
        "When whisper types: after each pause, in a sliding window, or all at the end.",
        Gate::LocalWhisper,
    ),
    gated(
        "local-whisper.phrase_silence_ms",
        "phrase silence",
        ms(0, 10_000, 50, 250),
        "The silence that ends a phrase with silence segmentation.",
        Gate::LocalWhisper,
    ),
    gated(
        "local-whisper.flash_attn",
        "flash_attn",
        Kind::Bool,
        "whisper.cpp flash attention. Only helps on some GPU builds.",
        Gate::LocalWhisper,
    ),
    field(
        "asr-sidecar.url",
        "sidecar url",
        STRING,
        "The ASR sidecar's transcription endpoint.",
    ),
    field(
        "asr-sidecar.model",
        "sidecar model",
        STRING,
        "The model name sent to the sidecar.",
    ),
    field(
        "asr-sidecar.api_key",
        "sidecar api_key",
        Kind::Secret,
        "Optional bearer token for the sidecar. WHISRS_ASR_SIDECAR_API_KEY wins over it.",
    ),
    field(
        "openai-compatible-realtime.url",
        "realtime url",
        STRING,
        "WebSocket endpoint of the OpenAI-compatible realtime server.",
    ),
    field(
        "openai-compatible-realtime.model",
        "realtime model",
        STRING,
        "The model the realtime server loads.",
    ),
    field(
        "openai-compatible-realtime.profile",
        "realtime profile",
        Kind::Choice(catalog::REALTIME_PROFILES),
        "Which server dialect to speak.",
    ),
    field(
        "openai-compatible-realtime.turn_detection",
        "turn detection",
        Kind::Choice(catalog::TURN_DETECTIONS),
        "Who decides where a turn ends.",
    ),
    field(
        "openai-compatible-realtime.api_key",
        "realtime api_key",
        Kind::Secret,
        "Optional bearer token for the realtime server.",
    ),
];

const AUDIO: &[Field] = &[field(
    "audio.device",
    "device",
    Kind::Suggest {
        unset: None,
        check: None,
        from: Source::Inputs,
    },
    "The microphone: default, or a source the sound server lists. Monitors, last, record \
     what the speakers play.",
)];

const VOCABULARY: &[Field] = &[
    field(
        "general.vocabulary",
        "vocabulary",
        Kind::List {
            empty: "none",
            items: None,
        },
        "Names and terms sent to the backend as a hint. With a vocabulary.txt, a changed \
         list is written back there whole.",
    ),
    field(
        "general.prompt",
        "prompt",
        text("not set"),
        "Free-form context for the backend: style, register. Local whisper sends it with \
         every phrase, so write it in the language you dictate in.",
    ),
    field(
        "general.remove_filler_words",
        "remove fillers",
        Kind::Bool,
        "Strip filler words (um, uh) from each dictation.",
    ),
    field(
        "general.filler_words",
        "filler words",
        Kind::List {
            empty: "built-in list",
            items: None,
        },
        "Your own filler words. Empty uses the built-in list.",
    ),
];

const TYPING: &[Field] = &[
    field(
        "input.backend",
        "backend",
        Kind::Choice(catalog::INPUT_BACKENDS),
        "How text is typed.",
    ),
    field(
        "input.key_delay_ms",
        "key delay",
        ms(0, 1000, 1, 10),
        "The pause between keys. Raise it if a terminal app drops characters.",
    ),
    field(
        "input.modifier_wait_ms",
        "modifier wait",
        Kind::Int(Steps {
            min: 0,
            max: 600_000,
            step: 1000,
            big: 10_000,
            unit: "ms",
            named: &[(0, "no wait")],
        }),
        "How long to wait for held Super, Alt, Ctrl or Shift before each key; still held, \
         the rest goes to the clipboard. no wait sends it there at once.",
    ),
    field(
        "input.paste",
        "paste",
        Kind::Bool,
        "Paste with Ctrl+V instead of typing (batch backends and command mode).",
    ),
    field(
        "input.clipboard_fallback",
        "clipboard fallback",
        Kind::Bool,
        "Also leave the text in the clipboard, to paste if typing went wrong.",
    ),
    field(
        "input.clipboard_only",
        "clipboard only",
        Kind::Bool,
        "Only copy the text to the clipboard, never type it.",
    ),
    field(
        "input.terminal_classes",
        "terminal classes",
        Kind::List {
            empty: "none",
            items: None,
        },
        "Window classes to treat as terminals, beside the built-in list. Matched whole, any \
         case.",
    ),
];

const HOTKEYS: &[Field] = &[
    field(
        "hotkeys.toggle",
        "toggle",
        HOTKEY,
        "Start and stop dictation. Most setups bind whisrs toggle in the compositor instead.",
    ),
    field(
        "hotkeys.cancel",
        "cancel",
        HOTKEY,
        "Cancel dictation and discard the audio.",
    ),
    field("hotkeys.command", "command", HOTKEY, COMMAND_HOTKEY),
    field("hotkeys.speak", "speak", HOTKEY, SPEAK_HOTKEY),
    gated(
        "hotkeys.recording",
        "recording",
        HOTKEY,
        RECORDING_HOTKEY,
        Gate::Recording,
    ),
    field(
        "hotkeys.next_profile",
        "next_profile",
        HOTKEY,
        "Switch to the next config profile: base, then the profiles by name.",
    ),
];

const COMMAND_HOTKEY: &str = "Command mode: speak an instruction, the LLM rewrites the selection.";
const SPEAK_HOTKEY: &str = "Read the selection aloud ([tts]).";
const RECORDING_HOTKEY: &str = "Start and stop a recording of mic and system audio.";

/// The modes' hotkeys as setup shows them, each on its own mode's step,
/// labelled `hotkey` there (as an agent's is). The same keys as in
/// [`HOTKEYS`].
const MODE_HOTKEYS: &[Field] = &[
    field("hotkeys.command", "hotkey", HOTKEY, COMMAND_HOTKEY),
    field("hotkeys.speak", "hotkey", HOTKEY, SPEAK_HOTKEY),
    gated(
        "hotkeys.recording",
        "hotkey",
        HOTKEY,
        RECORDING_HOTKEY,
        Gate::Recording,
    ),
];

const OVERLAY: &[Field] = &[
    gated(
        "general.overlay",
        "overlay",
        Kind::Bool,
        "Show the recording pill on screen.",
        Gate::Overlay,
    ),
    field(
        "overlay.theme",
        "theme",
        Kind::Choice(catalog::OVERLAY_THEMES),
        "Colours the pill and this screen. custom takes the colors below.",
    ),
    field(
        "overlay.position",
        "position",
        Kind::Choice(catalog::OVERLAY_POSITIONS),
        "Where the pill sits.",
    ),
    field("overlay.width", "width", px(90, 120, 2, 10), "Pill width."),
    field("overlay.height", "height", px(36, 48, 1, 4), "Pill height."),
    field(
        "overlay.colors.recording",
        "recording color",
        Kind::Color,
        "The custom theme's recording colour, and this screen's accent.",
    ),
    field(
        "overlay.colors.transcribing",
        "transcribing color",
        Kind::Color,
        "The custom theme's transcribing colour.",
    ),
    field(
        "overlay.colors.speaking",
        "speaking color",
        Kind::Color,
        "The custom theme's read-aloud colour.",
    ),
    field(
        "overlay.colors.background",
        "background color",
        Kind::Color,
        "The custom theme's pill background.",
    ),
    field(
        "overlay.colors.ring",
        "ring color",
        Kind::Color,
        "The custom theme's pill outline.",
    ),
    field(
        "overlay.colors.glow",
        "glow color",
        Kind::Color,
        "The custom theme's glow.",
    ),
    field(
        "agent_overlay.enabled",
        "agent overlay",
        Kind::MaybeBool {
            unset: "follows overlay",
        },
        "The agent overlay: a row per working agent, in a corner. Only with [[agents]].",
    ),
    field(
        "agent_overlay.style",
        "agent style",
        Kind::Choice(catalog::AGENT_STYLES),
        "How the agent overlay draws each agent.",
    ),
    field(
        "agent_overlay.corner",
        "agent corner",
        Kind::Choice(catalog::AGENT_CORNERS),
        "The agent overlay's corner.",
    ),
];

const TTS: &[Field] = &[
    field(
        "tts.enabled",
        "enabled",
        Kind::Bool,
        "Read the selection aloud with whisrs speak, and speak agent replies.",
    ),
    field(
        "tts.backend",
        "backend",
        Kind::Choice(catalog::TTS_BACKENDS),
        "Who reads the text: a cloud voice, or tts-sidecar, your own OpenAI-compatible server.",
    ),
    field(
        "tts.model",
        "model",
        Kind::Suggest {
            unset: Some("backend default"),
            check: None,
            from: Source::TtsModels,
        },
        "The TTS model, from the backend's list. With deepgram the model is the voice.",
    ),
    field(
        "tts.voice",
        "voice",
        Kind::Suggest {
            unset: Some("backend default"),
            check: None,
            from: Source::TtsVoices,
        },
        "The voice, as the backend spells it. deepgram does not use it: its voice is in the \
         model.",
    ),
    field(
        "tts.api_key",
        "api_key",
        Kind::Secret,
        "Unset, the backend's own key is used ([groq], [openai] or [deepgram]).",
    ),
    field(
        "tts.url",
        "url",
        Kind::Text {
            unset: Some("not set"),
            check: Some(check_url),
        },
        "The tts-sidecar endpoint (/v1/audio/speech).",
    ),
    field(
        "tts.response_format",
        "response format",
        suggest(catalog::TTS_RESPONSE_FORMATS),
        "The audio format asked for. whisrs plays WAV only, so wav is the one that works.",
    ),
];

const LLM: &[Field] = &[
    field(
        "llm.api_key",
        "api_key",
        Kind::Secret,
        "API key for the LLM provider, for command mode, LLM commands and recording \
         summaries.",
    ),
    field(
        "llm.model",
        "model",
        Kind::Suggest {
            unset: None,
            check: None,
            from: Source::LlmModels,
        },
        "The chat model. Suggested for the OpenAI, Groq, OpenRouter, Gemini and Ollama URLs; any \
         other name can be typed.",
    ),
    field(
        "llm.api_url",
        "api_url",
        Kind::Suggest {
            unset: None,
            check: Some(check_url),
            from: Source::List(catalog::LLM_URLS),
        },
        "The chat completions endpoint.",
    ),
    field(
        "general.llm_post_process",
        "post-process",
        Kind::Bool,
        "Run every dictation through the LLM with the instruction below. Batch backends only.",
    ),
    field(
        "general.llm_instruction",
        "instruction",
        STRING,
        "What the LLM does with each dictation when post-processing is on.",
    ),
];

const LLM_COMMAND_FIELDS: &[Field] = &[
    field("name", "name", STRING, "Used by whisrs llm-command <name>."),
    field(
        "hotkey",
        "hotkey",
        Kind::Hotkey { unset: None },
        "Dictate, and the LLM applies the instruction before typing.",
    ),
    field(
        "set_hotkey",
        "set_hotkey",
        HOTKEY,
        "Press it with text selected to make the selection the new instruction.",
    ),
    field(
        "instruction",
        "instruction",
        STRING,
        "What the LLM does with what you said.",
    ),
];

const LLM_COMMANDS: Entries = Entries {
    path: "llm_commands",
    noun: "command",
    fields: LLM_COMMAND_FIELDS,
    required: &["name", "hotkey", "instruction"],
    summary: &["hotkey", "instruction"],
};

const AGENT_FIELDS: &[Field] = &[
    field(
        "name",
        "name",
        STRING,
        "Used by whisrs agent <name>: a-z, 0-9, _ and -.",
    ),
    field(
        "hotkey",
        "hotkey",
        HOTKEY,
        "Records a voice turn for this agent.",
    ),
    field(
        "new_session_hotkey",
        "new session key",
        HOTKEY,
        "Records a voice turn that starts a fresh session.",
    ),
    field(
        "harness",
        "harness",
        Kind::Choice(catalog::HARNESSES),
        "The coding agent this entry drives.",
    ),
    field(
        "dir",
        "dir",
        Kind::Path {
            unset: None,
            tilde: true,
            from: None,
        },
        "The directory the agent works in. It must exist.",
    ),
    field(
        "permissions",
        "permissions",
        Kind::Choice(catalog::PERMISSIONS),
        "What the agent may do without asking.",
    ),
    field(
        "command",
        "command",
        text("harness on PATH"),
        "The harness binary.",
    ),
    field(
        "args",
        "args",
        Kind::List {
            empty: "none",
            items: None,
        },
        "Extra arguments for the harness.",
    ),
    field(
        "reply_instruction",
        "reply instruction",
        text("built-in"),
        "Replaces the built-in voice instruction.",
    ),
    field(
        "feedback",
        "feedback",
        Kind::Choice(catalog::FEEDBACK),
        "How much the agent says while it works.",
    ),
];

const AGENTS: Entries = Entries {
    path: "agents",
    noun: "agent",
    fields: AGENT_FIELDS,
    required: &[],
    summary: &["harness", "hotkey"],
};

const REPLACEMENTS: MapField = MapField {
    path: "replacements",
    key_label: "phrase",
    value_label: "replacement",
    help: "Whole-word fix-ups applied to every dictation and recording transcript: \
           package build becomes PKGBUILD.",
};

const RECORDING: &[Field] = &[
    field(
        "recording.backend",
        "backend",
        Kind::MaybeChoice {
            unset: "same as dictation",
            choices: catalog::RECORDING_BACKENDS,
        },
        "The backend recordings are transcribed with, after you stop. It must take a file: \
         the realtime ones cannot.",
    ),
    field(
        "recording.user_name",
        "your name",
        text("your login name"),
        "Your name in transcripts.",
    ),
    field(
        "recording.language",
        "language",
        language(Some("[general] language")),
        "The language of both tracks, or auto.",
    ),
    field(
        "recording.mic_language",
        "mic language",
        language(Some("as language")),
        "The language of your microphone.",
    ),
    field(
        "recording.system_language",
        "system language",
        language(Some("as language")),
        "The language of the system audio.",
    ),
    field(
        "recording.mic_device",
        "mic device",
        Kind::Suggest {
            unset: Some("[audio] device"),
            check: None,
            from: Source::Inputs,
        },
        "The microphone for recordings.",
    ),
    field(
        "recording.system_device",
        "system device",
        Kind::Suggest {
            unset: Some("default monitor"),
            check: None,
            from: Source::Monitors,
        },
        "What your computer plays: a monitor source, or none for the mic only.",
    ),
    field(
        "recording.max_duration_minutes",
        "max duration",
        Kind::Int(Steps {
            min: 1,
            max: 2160,
            step: 15,
            big: 60,
            unit: "min",
            named: &[],
        }),
        "Stop a recording after this long, at most 36 hours.",
    ),
    field(
        "recording.audio_retention_days",
        "keep audio",
        Kind::Int(Steps {
            min: -1,
            max: 3650,
            step: 1,
            big: 30,
            unit: "days",
            named: &[(-1, "keep forever"), (0, "delete once transcribed")],
        }),
        "How long a recording's audio is kept. The transcript always stays.",
    ),
    field(
        "recording.dir",
        "folder",
        Kind::Path {
            unset: Some("~/.local/share/whisrs/recordings"),
            tilde: true,
            from: None,
        },
        "Where recordings are kept.",
    ),
    field(
        "recording.diarization",
        "speakers",
        Kind::Choice(catalog::DIARIZATIONS),
        "How the voices on the system audio are told apart.",
    ),
    field(
        "recording.diarization_url",
        "diarization url",
        Kind::Text {
            unset: Some("not set"),
            check: Some(check_url),
        },
        "A diarization sidecar, such as http://127.0.0.1:8770/diarize.",
    ),
    field(
        "recording.num_speakers",
        "voices",
        Kind::Int(Steps {
            min: 0,
            max: whisrs::recording::NUM_SPEAKERS_MAX as i64,
            step: 1,
            big: 5,
            unit: "",
            named: &[(0, "auto")],
        }),
        "How many voices the sidecar should find. auto lets it decide.",
    ),
    field(
        "recording.shared_mic",
        "shared mic",
        Kind::Bool,
        "Several people share your mic: tell its voices apart too.",
    ),
    field(
        "recording.name_speakers",
        "name speakers",
        Kind::Bool,
        "Name the speakers with the [llm]. Sends excerpts of each transcript to it.",
    ),
    field(
        "recording.summary",
        "summary",
        Kind::Bool,
        "Summarize each transcript with the [llm]. Sends the whole transcript to it.",
    ),
    field(
        "recording.summary_instruction",
        "summary rules",
        text("default rules"),
        "What a summary should look like, in place of the default rules.",
    ),
];

const HOOKS: &[Field] = &[
    gated(
        "hooks.media_auto_pause",
        "pause media",
        Kind::Bool,
        "Pause the media that is playing while you dictate, and resume exactly that after.",
        Gate::Hooks,
    ),
    gated(
        "hooks.on_record_start",
        "on start",
        text("not set"),
        "A shell command run when dictation starts.",
        Gate::Hooks,
    ),
    gated(
        "hooks.on_record_stop",
        "on stop",
        text("not set"),
        "A shell command run when dictation stops.",
        Gate::Hooks,
    ),
];

/// The Config screen's sections, in order.
pub const SECTIONS: &[Section] = &[
    Section {
        title: "General",
        body: Body::Rows(GENERAL),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Backend & keys",
        body: Body::Rows(BACKEND),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Audio",
        body: Body::Rows(AUDIO),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Vocabulary",
        body: Body::Rows(VOCABULARY),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Replacements",
        body: Body::Map(&REPLACEMENTS),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Typing",
        body: Body::Rows(TYPING),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Hotkeys",
        body: Body::Rows(HOTKEYS),
        gate: None,
        entry_hotkeys: true,
    },
    Section {
        title: "Overlay",
        body: Body::Rows(OVERLAY),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Read aloud (TTS)",
        body: Body::Rows(TTS),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Agents",
        body: Body::Entries(&AGENTS),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "LLM",
        body: Body::Rows(LLM),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "LLM commands",
        body: Body::Entries(&LLM_COMMANDS),
        gate: None,
        entry_hotkeys: false,
    },
    Section {
        title: "Recording",
        body: Body::Rows(RECORDING),
        gate: Some(Gate::Recording),
        entry_hotkeys: false,
    },
    Section {
        title: "Hooks",
        body: Body::Rows(HOOKS),
        gate: None,
        entry_hotkeys: false,
    },
];

/// The row of a section's key at `path` (`general.language`).
pub fn find(path: &str) -> Option<&'static Field> {
    SECTIONS
        .iter()
        .filter_map(|section| match section.body {
            Body::Rows(fields) => Some(fields),
            _ => None,
        })
        .flatten()
        .find(|field| field.path == path)
}

/// [`find`], for a screen that edits a few known rows.
pub fn row(path: &str) -> &'static Field {
    find(path).unwrap_or_else(|| panic!("no row {path}"))
}

/// A mode's hotkey (`hotkeys.speak`) as setup shows it on the mode's
/// step: [`MODE_HOTKEYS`].
pub fn mode_hotkey(path: &str) -> &'static Field {
    MODE_HOTKEYS
        .iter()
        .find(|field| field.path == path)
        .unwrap_or_else(|| panic!("no mode hotkey {path}"))
}

/// The row of `key` in an entry of the array of tables `list` (`agents`,
/// `dir`), for a screen that edits one entry's rows.
pub fn entry_row(list: &str, key: &str) -> &'static Field {
    SECTIONS
        .iter()
        .find_map(|section| match section.body {
            Body::Entries(entries) if entries.path == list => {
                entries.fields.iter().find(|field| field.path == key)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {list} row {key}"))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use whisrs::agent::AgentConfig;
    use whisrs::llm::{LlmCommandConfig, LlmConfig};
    use whisrs::*;

    use super::*;

    /// A `Config` with every field set: struct literals with no `..`, so a
    /// field added to any of these types fails to compile here first.
    fn every_field_set() -> Config {
        let some = |s: &str| Some(s.to_string());
        Config {
            general: GeneralConfig {
                backend: "groq".into(),
                language: "en".into(),
                silence_timeout_ms: 1,
                notify: true,
                history: true,
                remove_filler_words: true,
                filler_words: vec!["um".into()],
                audio_feedback: true,
                audio_feedback_volume: 0.5,
                vocabulary: vec!["whisrs".into()],
                prompt: some("p"),
                tray: true,
                overlay: true,
                llm_post_process: true,
                llm_instruction: "i".into(),
            },
            audio: AudioConfig {
                device: "default".into(),
                vad: "earshot".into(),
            },
            input: InputConfig {
                key_delay_ms: 1,
                modifier_wait_ms: 1,
                backend: InjectorBackend::Auto,
                paste: true,
                clipboard_fallback: true,
                clipboard_only: true,
                terminal_classes: vec!["kitty".into()],
            },
            deepgram: Some(DeepgramConfig {
                api_key: "k".into(),
                model: "m".into(),
            }),
            groq: Some(GroqConfig {
                api_key: "k".into(),
                model: "m".into(),
            }),
            openai: Some(OpenAiConfig {
                api_key: "k".into(),
                model: "m".into(),
                languages: vec!["en".into()],
            }),
            local_whisper: Some(LocalWhisperConfig {
                model_path: "p".into(),
                segmentation: "silence".into(),
                phrase_silence_ms: 1,
                flash_attn: true,
            }),
            local_vosk: Some(LocalVoskConfig {
                model_path: "p".into(),
            }),
            local_parakeet: Some(LocalParakeetConfig {
                model_path: "p".into(),
            }),
            asr_sidecar: Some(AsrSidecarConfig {
                url: "u".into(),
                model: "m".into(),
                api_key: some("k"),
            }),
            openai_compatible_realtime: Some(OpenAiCompatibleRealtimeConfig {
                url: "u".into(),
                model: "m".into(),
                profile: "lemonade".into(),
                turn_detection: "server-vad".into(),
                api_key: some("k"),
            }),
            llm: Some(LlmConfig {
                api_key: "k".into(),
                model: "m".into(),
                api_url: "u".into(),
            }),
            tts: Some(TtsConfig {
                enabled: true,
                backend: "groq".into(),
                model: some("m"),
                voice: some("v"),
                response_format: "wav".into(),
                api_key: some("k"),
                url: some("u"),
            }),
            hotkeys: Some(HotkeyConfig {
                toggle: some("a"),
                cancel: some("b"),
                command: some("c"),
                speak: some("d"),
                recording: some("e"),
                next_profile: some("f"),
            }),
            hooks: Some(HooksConfig {
                media_auto_pause: true,
                on_record_start: some("s"),
                on_record_stop: some("t"),
            }),
            overlay: Some(OverlayConfig {
                theme: "carbon".into(),
                width: 100,
                height: 40,
                position: "bottom-center".into(),
                colors: Some(OverlayColors {
                    background: some("#000"),
                    ring: some("#000"),
                    recording: some("#000"),
                    transcribing: some("#000"),
                    speaking: some("#000"),
                    glow: some("#000"),
                }),
            }),
            agent_overlay: Some(AgentOverlayConfig {
                enabled: Some(true),
                style: "orb".into(),
                corner: "top-left".into(),
            }),
            llm_commands: vec![LlmCommandConfig {
                name: "n".into(),
                hotkey: "h".into(),
                set_hotkey: some("s"),
                instruction: "i".into(),
            }],
            agents: vec![AgentConfig {
                name: "n".into(),
                hotkey: some("h"),
                new_session_hotkey: some("n"),
                harness: "claude".into(),
                dir: "/".into(),
                permissions: "auto".into(),
                command: some("c"),
                args: vec!["a".into()],
                reply_instruction: some("r"),
                feedback: "brief".into(),
            }],
            replacements: BTreeMap::from([("a".to_string(), "b".to_string())]),
            recording: Some(RecordingConfig {
                backend: some("groq"),
                user_name: some("u"),
                language: some("en"),
                mic_language: some("en"),
                system_language: some("en"),
                mic_device: some("m"),
                system_device: some("s"),
                max_duration_minutes: 1,
                audio_retention_days: 1,
                dir: some("d"),
                diarization: DiarizationMode::Auto,
                diarization_url: some("u"),
                num_speakers: 1,
                shared_mic: true,
                name_speakers: true,
                summary: true,
                summary_instruction: some("s"),
            }),
        }
    }

    /// Every path the schema covers: rows, entry fields under their list,
    /// the maps whole.
    fn schema_paths() -> Vec<String> {
        let mut paths = Vec::new();
        for section in SECTIONS {
            match section.body {
                Body::Rows(fields) => paths.extend(fields.iter().map(|f| f.path.to_string())),
                Body::Entries(entries) => paths.extend(
                    entries
                        .fields
                        .iter()
                        .map(|f| format!("{}.{}", entries.path, f.path)),
                ),
                Body::Map(map) => paths.push(map.path.to_string()),
            }
        }
        paths
    }

    /// The leaf keys of a serialized config: a map is one leaf, an array of
    /// tables contributes its entries' keys under the array's name.
    fn leaves(table: &toml::Table, prefix: &str, maps: &[String], out: &mut BTreeSet<String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                _ if maps.contains(&path) => {
                    out.insert(path);
                }
                toml::Value::Table(inner) => leaves(inner, &path, maps, out),
                toml::Value::Array(items) if items.iter().all(toml::Value::is_table) => {
                    for item in items {
                        leaves(item.as_table().unwrap(), &path, maps, out);
                    }
                }
                _ => {
                    out.insert(path);
                }
            }
        }
    }

    /// Acceptance criterion 4: every key is a row or listed as edited in
    /// $EDITOR, and every row names a key that exists.
    #[test]
    fn every_config_key_is_editable() {
        let table = toml::Table::try_from(every_field_set()).expect("Config serializes");
        let maps: Vec<String> = SECTIONS
            .iter()
            .filter_map(|s| match s.body {
                Body::Map(map) => Some(map.path.to_string()),
                _ => None,
            })
            .collect();
        let mut keys = BTreeSet::new();
        leaves(&table, "", &maps, &mut keys);

        let covered: BTreeSet<String> = schema_paths()
            .into_iter()
            .chain(EDITOR_ONLY.iter().map(|p| p.to_string()))
            .collect();
        let missing: Vec<&String> = keys.difference(&covered).collect();
        assert!(
            missing.is_empty(),
            "config keys with no row and not in EDITOR_ONLY: {missing:?}"
        );
        let stale: Vec<&String> = covered.difference(&keys).collect();
        assert!(
            stale.is_empty(),
            "rows for keys that do not exist: {stale:?}"
        );
        // All 21 top-level entries have a home.
        assert_eq!(table.len(), 21);
    }

    /// The field names `T`'s `Deserialize` declares: every one, an `Option`
    /// too, which serializes to nothing when `None` and so cannot be seen in
    /// a serialized config.
    fn fields_of<T: serde::de::DeserializeOwned>() -> Vec<&'static str> {
        struct Fields(Vec<&'static str>);

        impl<'de> serde::Deserializer<'de> for &mut Fields {
            type Error = serde::de::value::Error;

            fn deserialize_any<V: serde::de::Visitor<'de>>(
                self,
                _: V,
            ) -> Result<V::Value, Self::Error> {
                Err(serde::de::Error::custom("fields only"))
            }

            fn deserialize_struct<V: serde::de::Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                _: V,
            ) -> Result<V::Value, Self::Error> {
                self.0 = fields.to_vec();
                Err(serde::de::Error::custom("fields only"))
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map enum identifier ignored_any
            }
        }

        let mut fields = Fields(Vec::new());
        let _ = T::deserialize(&mut fields);
        assert!(
            !fields.0.is_empty(),
            "{} is not a struct",
            std::any::type_name::<T>()
        );
        fields.0
    }

    /// The names serde also accepts for a field or section, which are not
    /// keys of their own.
    const ALIASES: &[&str] = &["local", "asr", "vibevoice", "read"];

    /// Every field of every section has a row, read from the types rather
    /// than from a config: a new `Option` field left `None` above would
    /// otherwise pass [`every_config_key_is_editable`] with no row.
    #[test]
    fn every_field_has_a_row_even_one_never_set() {
        let covered: BTreeSet<String> = schema_paths()
            .into_iter()
            .chain(EDITOR_ONLY.iter().map(|p| p.to_string()))
            .collect();
        let sections = [
            ("general", fields_of::<GeneralConfig>()),
            ("audio", fields_of::<AudioConfig>()),
            ("input", fields_of::<InputConfig>()),
            ("deepgram", fields_of::<DeepgramConfig>()),
            ("groq", fields_of::<GroqConfig>()),
            ("openai", fields_of::<OpenAiConfig>()),
            ("local-whisper", fields_of::<LocalWhisperConfig>()),
            ("local-vosk", fields_of::<LocalVoskConfig>()),
            ("local-parakeet", fields_of::<LocalParakeetConfig>()),
            ("asr-sidecar", fields_of::<AsrSidecarConfig>()),
            (
                "openai-compatible-realtime",
                fields_of::<OpenAiCompatibleRealtimeConfig>(),
            ),
            ("llm", fields_of::<LlmConfig>()),
            ("tts", fields_of::<TtsConfig>()),
            ("hotkeys", fields_of::<HotkeyConfig>()),
            ("hooks", fields_of::<HooksConfig>()),
            ("overlay", fields_of::<OverlayConfig>()),
            ("overlay.colors", fields_of::<OverlayColors>()),
            ("agent_overlay", fields_of::<AgentOverlayConfig>()),
            ("llm_commands", fields_of::<LlmCommandConfig>()),
            ("agents", fields_of::<AgentConfig>()),
            ("recording", fields_of::<RecordingConfig>()),
        ];
        // Every section of `Config` is listed here, but the one map.
        let top: BTreeSet<&str> = fields_of::<Config>()
            .into_iter()
            .filter(|name| !ALIASES.contains(name))
            .collect();
        let listed: BTreeSet<&str> = sections
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !name.contains('.'))
            .chain(["replacements"])
            .collect();
        assert_eq!(top, listed);

        for (section, fields) in &sections {
            for field in fields.iter().filter(|name| !ALIASES.contains(name)) {
                let path = format!("{section}.{field}");
                if sections.iter().any(|(name, _)| *name == path) {
                    continue;
                }
                assert!(
                    covered.contains(&path),
                    "{path} has no row and is not in EDITOR_ONLY"
                );
            }
        }
    }

    /// Each path appears once: two rows for one key would fight.
    #[test]
    fn no_key_has_two_rows() {
        let paths = schema_paths();
        let unique: BTreeSet<&String> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len());
    }

    /// The hotkey rows cover every `[hotkeys]` field, taken from serde so a
    /// new field cannot be forgotten: a field with no row could not be set.
    #[test]
    fn every_hotkey_field_has_a_row() {
        let all_set = every_field_set().hotkeys.expect("set");
        let fields = toml::Table::try_from(all_set).expect("HotkeyConfig serializes");
        let rows: Vec<&str> = HOTKEYS.iter().map(|f| f.path).collect();
        for field in fields.keys() {
            assert!(
                rows.contains(&format!("hotkeys.{field}").as_str()),
                "no row for [hotkeys] {field}"
            );
        }
        assert_eq!(rows.len(), fields.len());
    }

    /// The backend choice offers exactly the backends `Config::validate`
    /// routes, its aliases aside, and refuses the two stubs as coming soon.
    #[test]
    fn the_backend_choice_is_every_primary_backend() {
        let offered: BTreeSet<&str> = BACKENDS
            .iter()
            .map(|(value, _)| *value)
            .filter(|value| !catalog::STUB_BACKENDS.contains(value))
            .collect();
        let primary: BTreeSet<&str> = whisrs::primary_backend_names().into_iter().collect();
        assert_eq!(offered, primary);
        for stub in catalog::STUB_BACKENDS {
            assert_eq!(backend_refusal(stub).map(|r| r.0), Some("coming soon"));
        }
        assert_eq!(backend_refusal("groq"), None);
        assert_eq!(
            backend_refusal("local-whisper").is_some(),
            !cfg!(feature = "local-whisper")
        );
    }

    /// Every row with its path, entry fields under their list's path.
    fn every_field() -> Vec<(String, &'static Field)> {
        let mut out = Vec::new();
        for section in SECTIONS {
            match section.body {
                Body::Rows(fields) => out.extend(fields.iter().map(|f| (f.path.to_string(), f))),
                Body::Entries(entries) => out.extend(
                    entries
                        .fields
                        .iter()
                        .map(|f| (format!("{}.{}", entries.path, f.path), f)),
                ),
                Body::Map(_) => {}
            }
        }
        out
    }

    /// The TOML that sets `path` (an entry field's under a new entry with
    /// its required keys) to `value`.
    fn setting(path: &str, value: &str) -> String {
        for section in SECTIONS {
            if let Body::Entries(entries) = section.body {
                if let Some(key) = path.strip_prefix(&format!("{}.", entries.path)) {
                    let mut toml = format!("[[{}]]\n", entries.path);
                    for required in entries.required.iter().filter(|r| **r != key) {
                        toml.push_str(&format!("{required} = \"x\"\n"));
                    }
                    if !entries.required.contains(&"name") && key != "name" {
                        toml.push_str("name = \"x\"\n");
                    }
                    return toml + &format!("{key} = \"{value}\"\n");
                }
            }
        }
        let (table, key) = path.rsplit_once('.').unwrap();
        format!("[{table}]\n{key} = \"{value}\"\n")
    }

    /// The choices name values the types accept: a config with each one
    /// still parses, entries' choices too.
    #[test]
    fn every_choice_parses() {
        for (path, field) in every_field() {
            let choices = match field.kind {
                Kind::Choice(choices) | Kind::MaybeChoice { choices, .. } => choices,
                _ => continue,
            };
            for (choice, _) in choices {
                let toml = setting(&path, choice);
                assert!(
                    toml::from_str::<Config>(&toml).is_ok(),
                    "{path} = {choice} does not parse:\n{toml}"
                );
            }
        }
    }

    /// Every typed string is decided: a `Text` row is in [`FREE_TEXT`]
    /// with a reason, and [`FREE_TEXT`] names only `Text` rows. A new
    /// string key fails here until it is suggested or listed.
    #[test]
    fn every_free_text_field_is_listed_with_a_reason() {
        let text: BTreeSet<String> = every_field()
            .into_iter()
            .filter(|(_, field)| matches!(field.kind, Kind::Text { .. }))
            .map(|(path, _)| path)
            .collect();
        let listed: BTreeSet<String> = FREE_TEXT.iter().map(|(p, _)| p.to_string()).collect();
        let unlisted: Vec<&String> = text.difference(&listed).collect();
        assert!(
            unlisted.is_empty(),
            "typed with no suggestions and not in FREE_TEXT: {unlisted:?}"
        );
        let stale: Vec<&String> = listed.difference(&text).collect();
        assert!(
            stale.is_empty(),
            "in FREE_TEXT but not a Text row: {stale:?}"
        );
        assert_eq!(listed.len(), FREE_TEXT.len(), "a FREE_TEXT path twice");
        for (path, why) in FREE_TEXT {
            assert!(!why.is_empty(), "{path} has no reason");
        }
    }

    /// Each number row's default is in its range, and so is each value it
    /// names; the big step is bigger than the step.
    #[test]
    fn steppers_hold_their_defaults() {
        let defaults = toml::Table::try_from(
            toml::from_str::<Config>("[local-whisper]\n[overlay]\n[recording]\n").unwrap(),
        )
        .unwrap();
        let at = |path: &str| {
            let mut value = defaults.get(path.split('.').next().unwrap())?;
            for seg in path.split('.').skip(1) {
                value = value.get(seg)?;
            }
            Some(value.clone())
        };
        for (path, field) in every_field() {
            match &field.kind {
                Kind::Int(steps) => {
                    assert!(steps.big > steps.step, "{path}");
                    for (n, _) in steps.named {
                        assert!((steps.min..=steps.max).contains(n), "{path} names {n}");
                    }
                    let n = at(&path).and_then(|v| v.as_integer()).expect(&path);
                    assert!((steps.min..=steps.max).contains(&n), "{path} = {n}");
                }
                Kind::Float {
                    min,
                    max,
                    step,
                    big,
                    ..
                } => {
                    assert!(big > step, "{path}");
                    let n = at(&path).and_then(|v| v.as_float()).expect(&path);
                    assert!((*min..=*max).contains(&n), "{path} = {n}");
                }
                _ => {}
            }
        }
    }

    /// Every hotkey, `[hotkeys]` and entries alike, is set in the hotkey
    /// dialog.
    #[test]
    fn every_hotkey_row_uses_the_dialog() {
        for (path, field) in every_field() {
            let hotkey = path.starts_with("hotkeys.") || path.ends_with("hotkey");
            assert_eq!(hotkey, matches!(field.kind, Kind::Hotkey { .. }), "{path}");
        }
    }

    #[test]
    fn a_hotkey_is_checked_by_the_parser() {
        assert_eq!(check_hotkey(" Super+Shift+D "), Ok("Super+Shift+D".into()));
        assert!(check_hotkey("D").is_err());
        assert!(check_hotkey("Super+Nope").is_err());
    }

    #[test]
    fn checks_normalize_or_refuse() {
        assert_eq!(
            check_url(" http://127.0.0.1:8770/x "),
            Ok("http://127.0.0.1:8770/x".into())
        );
        assert!(check_url("ws://127.0.0.1").is_err());
        assert!(check_url("127.0.0.1:8770").is_err());
        assert_eq!(check_language("DE"), Ok("de".into()));
        assert!(check_language("german").is_err());
        // Well-formed is not enough: the primary subtag must be a known code.
        assert_eq!(
            check_language("zz"),
            Err(
                "zz is not a language whisrs knows: pick one from the list (e edits the \
                 file for anything else)"
                    .into()
            )
        );
        assert!(check_language("zz-US").is_err());
        assert_eq!(check_language("en_us"), Ok("en-US".into()));
        assert_eq!(check_language("pt-BR"), Ok("pt-BR".into()));
        assert_eq!(check_language("zh-Hans"), Ok("zh-Hans".into()));
        assert_eq!(check_language("yue"), Ok("yue".into()));
        assert_eq!(check_language("Multi"), Ok("multi".into()));
        assert_eq!(check_language("auto"), Ok("auto".into()));
        assert!(check_expected_language("zz").is_err());
        assert_eq!(check_expected_language("UK"), Ok("uk".into()));
        assert!(check_expected_language("auto").is_err());
        assert!(check_expected_language("Multi").is_err());
        assert_eq!(check_color(" #abc "), Ok("#abc".into()));
        assert!(check_color("#é1").is_err());
    }

    #[test]
    fn help_and_labels_have_no_dashes() {
        let mut texts = vec![RECORDING_MISSING, REPLACEMENTS.help];
        for section in SECTIONS {
            texts.push(section.title);
            let fields = match section.body {
                Body::Rows(fields) => fields,
                Body::Entries(entries) => entries.fields,
                Body::Map(_) => continue,
            };
            for field in fields {
                texts.extend([field.label, field.help]);
            }
        }
        for text in texts {
            assert!(!text.contains(['\u{2013}', '\u{2014}']), "{text}");
        }
    }
}
