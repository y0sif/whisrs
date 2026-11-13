//! The values whisrs can list for a config key, each with a line saying what
//! it is: models, voices, languages, and the closed choices. The config
//! editor offers them in its pickers, so nobody has to recall a model name
//! or a language code; a value a list leaves out can still be typed.
//!
//! A list from a provider names its source and the day it was checked.
//! Tests hold every serde default to its list, so a default cannot drift
//! away from what is offered.

/// A value, and a short line saying what it is.
pub type Entry = (&'static str, &'static str);

// ---------------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------------

/// `auto`: what every language key takes besides a code.
pub const AUTO_LANGUAGE: Entry = ("auto", "detect the language");

/// Whisper's languages, code and English name, in whisper.cpp's own order
/// (roughly by how much training audio each had).
///
/// Source: the `g_lang` table of whisper.cpp,
/// https://github.com/ggml-org/whisper.cpp/blob/master/src/whisper.cpp
/// (whisper-rs-sys 0.15.0 vendors it), checked 2026-10-29. Under
/// `local-whisper` a test asks whisper.cpp itself for every code.
pub const LANGUAGES: &[Entry] = &[
    ("en", "English"),
    ("zh", "Chinese"),
    ("de", "German"),
    ("es", "Spanish"),
    ("ru", "Russian"),
    ("ko", "Korean"),
    ("fr", "French"),
    ("ja", "Japanese"),
    ("pt", "Portuguese"),
    ("tr", "Turkish"),
    ("pl", "Polish"),
    ("ca", "Catalan"),
    ("nl", "Dutch"),
    ("ar", "Arabic"),
    ("sv", "Swedish"),
    ("it", "Italian"),
    ("id", "Indonesian"),
    ("hi", "Hindi"),
    ("fi", "Finnish"),
    ("vi", "Vietnamese"),
    ("he", "Hebrew"),
    ("uk", "Ukrainian"),
    ("el", "Greek"),
    ("ms", "Malay"),
    ("cs", "Czech"),
    ("ro", "Romanian"),
    ("da", "Danish"),
    ("hu", "Hungarian"),
    ("ta", "Tamil"),
    ("no", "Norwegian"),
    ("th", "Thai"),
    ("ur", "Urdu"),
    ("hr", "Croatian"),
    ("bg", "Bulgarian"),
    ("lt", "Lithuanian"),
    ("la", "Latin"),
    ("mi", "Maori"),
    ("ml", "Malayalam"),
    ("cy", "Welsh"),
    ("sk", "Slovak"),
    ("te", "Telugu"),
    ("fa", "Persian"),
    ("lv", "Latvian"),
    ("bn", "Bengali"),
    ("sr", "Serbian"),
    ("az", "Azerbaijani"),
    ("sl", "Slovenian"),
    ("kn", "Kannada"),
    ("et", "Estonian"),
    ("mk", "Macedonian"),
    ("br", "Breton"),
    ("eu", "Basque"),
    ("is", "Icelandic"),
    ("hy", "Armenian"),
    ("ne", "Nepali"),
    ("mn", "Mongolian"),
    ("bs", "Bosnian"),
    ("kk", "Kazakh"),
    ("sq", "Albanian"),
    ("sw", "Swahili"),
    ("gl", "Galician"),
    ("mr", "Marathi"),
    ("pa", "Punjabi"),
    ("si", "Sinhala"),
    ("km", "Khmer"),
    ("sn", "Shona"),
    ("yo", "Yoruba"),
    ("so", "Somali"),
    ("af", "Afrikaans"),
    ("oc", "Occitan"),
    ("ka", "Georgian"),
    ("be", "Belarusian"),
    ("tg", "Tajik"),
    ("sd", "Sindhi"),
    ("gu", "Gujarati"),
    ("am", "Amharic"),
    ("yi", "Yiddish"),
    ("lo", "Lao"),
    ("uz", "Uzbek"),
    ("fo", "Faroese"),
    ("ht", "Haitian Creole"),
    ("ps", "Pashto"),
    ("tk", "Turkmen"),
    ("nn", "Norwegian Nynorsk"),
    ("mt", "Maltese"),
    ("sa", "Sanskrit"),
    ("lb", "Luxembourgish"),
    ("my", "Burmese"),
    ("bo", "Tibetan"),
    ("tl", "Tagalog"),
    ("mg", "Malagasy"),
    ("as", "Assamese"),
    ("tt", "Tatar"),
    ("haw", "Hawaiian"),
    ("ln", "Lingala"),
    ("ha", "Hausa"),
    ("ba", "Bashkir"),
    ("jw", "Javanese"),
    ("su", "Sundanese"),
    ("yue", "Cantonese"),
];

// ---------------------------------------------------------------------------
// whisrs's own choices
// ---------------------------------------------------------------------------
//
// Source for each list below: the values whisrs itself accepts for the key
// (`Config` in src/config/types.rs and the modules it names), checked
// 2026-10-29. The config editor's `every_choice_parses` test holds them to
// what a config can say.

/// The transcription backends, in the order setup offers them, then the
/// two that are not implemented yet.
pub const BACKENDS: &[Entry] = &[
    ("groq", "Whisper on Groq, sent when you stop"),
    ("deepgram-streaming", "Deepgram, text as you speak"),
    ("deepgram", "Deepgram, sent when you stop"),
    ("openai-realtime", "OpenAI Realtime, text as you speak"),
    ("openai", "OpenAI, sent when you stop"),
    (
        "openai-compatible-realtime",
        "a realtime server of your own (Lemonade)",
    ),
    ("local-whisper", "whisper.cpp on this machine, offline"),
    (
        "asr-sidecar",
        "an HTTP ASR server of your own, or OpenRouter",
    ),
    ("local-vosk", "Vosk on this machine, offline"),
    ("local-parakeet", "NVIDIA Parakeet on this machine, offline"),
];

/// The backends that cannot transcribe yet.
pub const STUB_BACKENDS: &[&str] = &["local-vosk", "local-parakeet"];

/// The backends a recording can be transcribed with: they take a file.
/// `deepgram-streaming` works too, as `deepgram`.
pub const RECORDING_BACKENDS: &[Entry] = &[
    ("groq", "Whisper on Groq"),
    ("openai", "OpenAI transcription"),
    ("deepgram", "Deepgram, with speakers told apart"),
    (
        "asr-sidecar",
        "an HTTP ASR server of your own, or OpenRouter",
    ),
    ("local-whisper", "whisper.cpp on this machine, offline"),
];

/// The backends that only stream live audio.
pub const LIVE_ONLY_BACKENDS: &[Entry] = &[
    ("openai-realtime", "OpenAI Realtime"),
    (
        "openai-compatible-realtime",
        "a realtime server of your own",
    ),
];

/// `[local-whisper] segmentation`.
pub const SEGMENTATIONS: &[Entry] = &[
    ("silence", "types each phrase after a pause"),
    ("window", "the older sliding window"),
    ("none", "types everything when you stop"),
];

/// `[openai-compatible-realtime] profile`: the server's dialect.
pub const REALTIME_PROFILES: &[Entry] = &[
    ("lemonade", "Lemonade: 16 kHz, interim text replaced"),
    ("openai", "OpenAI's wire: 24 kHz, text appended"),
];

/// `[audio] vad`: what decides a sound is speech.
pub const VADS: &[Entry] = &[
    (
        "earshot",
        "a small learned model, steady in background noise",
    ),
    ("rms", "the 0.1.31 loudness test"),
];

/// `[openai-compatible-realtime] turn_detection`.
pub const TURN_DETECTIONS: &[Entry] = &[
    ("server-vad", "the server ends a turn at a pause"),
    ("manual-commit", "whisrs ends it when you stop"),
];

/// `[input] backend`: how text is typed.
pub const INPUT_BACKENDS: &[Entry] = &[
    ("auto", "Wayland virtual keyboard, else uinput"),
    ("uinput", "a virtual keyboard device (/dev/uinput)"),
    ("wayland-vk", "the Wayland virtual keyboard only"),
    ("xtest", "the X server's XTEST (X11)"),
];

/// `[overlay] theme`.
pub const OVERLAY_THEMES: &[Entry] = &[
    ("carbon", "soft white and teal"),
    ("ember", "amber and emerald"),
    ("cyan", "electric blue and green"),
    ("custom", "the colors set below"),
];

/// `[overlay] position`.
pub const OVERLAY_POSITIONS: &[Entry] = &[
    ("bottom-center", "the bottom edge, centred"),
    ("bottom-left", "the bottom left corner"),
    ("bottom-right", "the bottom right corner"),
    ("top-center", "the top edge, centred"),
    ("top-left", "the top left corner"),
    ("top-right", "the top right corner"),
];

/// `[agent_overlay] style`.
pub const AGENT_STYLES: &[Entry] = &[
    ("orb", "rings in the state's colour"),
    ("face", "a small face"),
];

/// `[agent_overlay] corner`.
pub const AGENT_CORNERS: &[Entry] = &[
    ("bottom-right", "the bottom right corner"),
    ("bottom-left", "the bottom left corner"),
    ("top-right", "the top right corner"),
    ("top-left", "the top left corner"),
];

/// `[tts] backend`.
pub const TTS_BACKENDS: &[Entry] = &[
    ("groq", "Orpheus on Groq"),
    ("openai", "OpenAI speech, shuts down 2027-01-06"),
    ("deepgram", "Deepgram Aura-2"),
    (
        "tts-sidecar",
        "an OpenAI-compatible server of your own, or OpenRouter",
    ),
];

/// `[[agents]] harness`.
pub const HARNESSES: &[Entry] = &[
    ("claude", "Claude Code"),
    ("opencode", "OpenCode"),
    ("codex", "OpenAI Codex CLI"),
    ("copilot", "GitHub Copilot CLI"),
];

/// `[[agents]] permissions`.
pub const PERMISSIONS: &[Entry] = &[
    ("auto", "lets the agent work"),
    ("bypass", "skips every check"),
    ("ask", "keeps it read-only"),
];

/// `[[agents]] feedback`: how much the agent says while it works.
pub const FEEDBACK: &[Entry] = &[
    ("brief", "a short acknowledgement, then the reply"),
    ("summary", "only the final reply"),
    ("conversational", "updates at real milestones too"),
];

/// `[recording] diarization`: how the voices are told apart.
pub const DIARIZATIONS: &[Entry] = &[
    ("auto", "the sidecar when its URL is set, else Deepgram"),
    ("backend", "the backend labels speakers (Deepgram)"),
    ("sidecar", "the diarization sidecar at its URL"),
    ("off", "each track is one speaker"),
];

/// `[tts] response_format`. Playback decodes WAV
/// (`audio::playback::decode_wav`); a raw PCM answer (16-bit samples, no
/// header) gets a WAV header put in front of it first, so it plays too.
/// OpenRouter refuses `wav`, and whisrs asks it for `pcm` instead.
pub const TTS_RESPONSE_FORMATS: &[Entry] = &[
    ("wav", "WAV, which most servers send"),
    ("pcm", "raw audio, which OpenRouter sends"),
];

// ---------------------------------------------------------------------------
// Transcription models
// ---------------------------------------------------------------------------

/// `[deepgram] model`, for both deepgram backends (`/v1/listen`). Flux is
/// left out: it needs `/v2/listen`. Deepgram's Whisper models are left out
/// too: they do not stream, and they neither take keyterms nor tell
/// speakers apart. whisrs sends the vocabulary as `keyterm`, which only
/// Nova-3 takes; older models would need `keywords`, which whisrs does not
/// send.
///
/// Source: https://developers.deepgram.com/docs/models-languages-overview
/// and https://developers.deepgram.com/docs/keyterm, checked 2026-10-29.
pub const DEEPGRAM_MODELS: &[Entry] = &[
    ("nova-3", "highest-performing, takes your vocabulary"),
    ("nova-3-medical", "Nova-3 for medical terms"),
    ("nova-3-pharma", "Nova-3 for pharma terms, English"),
    ("nova-2", "for languages nova-3 lacks, no vocabulary"),
    ("nova-2-meeting", "Nova-2 for meetings, English"),
    ("nova-2-phonecall", "Nova-2 for phone calls, English"),
    ("nova-2-finance", "Nova-2 for finance, English"),
    ("nova-2-conversationalai", "Nova-2 for voice bots, English"),
    ("nova-2-voicemail", "Nova-2 for voicemail, English"),
    ("nova-2-video", "Nova-2 for video audio, English"),
    ("nova-2-medical", "Nova-2 for medical terms, English"),
    ("nova-2-drivethru", "Nova-2 for drive-thru orders, English"),
    ("nova-2-automotive", "Nova-2 for in-car audio, English"),
    ("nova-2-atc", "Nova-2 for air traffic control, English"),
];

/// `[groq] model`.
///
/// Source: https://console.groq.com/docs/speech-to-text and
/// https://console.groq.com/docs/deprecations (distil-whisper is gone),
/// checked 2026-10-29.
pub const GROQ_MODELS: &[Entry] = &[
    ("whisper-large-v3-turbo", "fast, multilingual"),
    ("whisper-large-v3", "high accuracy, multilingual"),
];

/// `[openai] model`, shared by the `openai` backend (files, REST) and
/// `openai-realtime` (live). Which one serves each comes from the code: a
/// realtime-only model is refused by the file endpoint, and recordings on
/// `openai` send gpt-transcribe in its place.
///
/// Source: https://developers.openai.com/api/docs/guides/speech-to-text,
/// https://developers.openai.com/api/docs/guides/realtime-transcription and
/// https://developers.openai.com/api/docs/deprecations (four models removed
/// 2027-02-26, replaced by gpt-live-transcribe or gpt-transcribe), checked
/// 2026-10-29.
pub const OPENAI_MODELS: &[Entry] = &[
    ("gpt-transcribe", "both backends, recommended for files"),
    ("gpt-live-transcribe", "openai-realtime only, low latency"),
    (
        "gpt-realtime-whisper",
        "openai-realtime only, takes no prompt",
    ),
    ("gpt-4o-transcribe", "both backends, removed 2027-02-26"),
    ("gpt-4o-mini-transcribe", "openai, removed 2027-02-26"),
    ("whisper-1", "openai, timestamps, removed 2027-02-26"),
];

/// `[asr-sidecar] url`: a server of your own where it listens by default,
/// and OpenRouter. Its models: [`asr_sidecar_models`].
///
/// Sources: the `[asr-sidecar] url` default (src/config/types.rs), and
/// OpenRouter's live API, checked 2026-11-13.
pub const ASR_SIDECAR_URLS: &[Entry] = &[
    (
        "http://127.0.0.1:8765/transcribe",
        "a server of your own on this machine",
    ),
    (OPENROUTER_STT_URL, "OpenRouter, many models, one key"),
];

/// A whisper.cpp model whisrs setup can download: its name (the file is
/// `ggml-<name>.bin`), its size in bytes, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhisperModel {
    pub name: &'static str,
    pub bytes: u64,
    pub about: &'static str,
}

/// The whisper.cpp models setup offers to download for `local-whisper`,
/// smallest first: each size English only (`.en`, better at English) and
/// multilingual, then the large ones. The quantized files are left out but
/// for large-v3-turbo's q5_0 (a third of its size) and q8_0 (about half,
/// with no accuracy lost in testing, for hardware short on memory), and
/// large-v3's q5_0 (a third of its size, for a GPU short on memory). The
/// smaller models' quantized files save little, and medium's are the size
/// of large-v3-turbo-q5_0, the better model.
///
/// Source: https://huggingface.co/ggerganov/whisper.cpp (file names and
/// sizes from https://huggingface.co/api/models/ggerganov/whisper.cpp/tree/main),
/// checked 2026-11-08.
pub const WHISPER_MODELS: &[WhisperModel] = &[
    WhisperModel {
        name: "tiny.en",
        bytes: 77_704_715,
        about: "English, fastest, rough",
    },
    WhisperModel {
        name: "tiny",
        bytes: 77_691_713,
        about: "multilingual, fastest, rough",
    },
    WhisperModel {
        name: "base.en",
        bytes: 147_964_211,
        about: "English, good, real-time on most CPUs",
    },
    WhisperModel {
        name: "base",
        bytes: 147_951_465,
        about: "multilingual, good",
    },
    WhisperModel {
        name: "small.en",
        bytes: 487_614_201,
        about: "English, very good, slower",
    },
    WhisperModel {
        name: "small",
        bytes: 487_601_967,
        about: "multilingual, very good, slower",
    },
    WhisperModel {
        name: "medium.en",
        bytes: 1_533_774_781,
        about: "English, better still, slow on a CPU",
    },
    WhisperModel {
        name: "medium",
        bytes: 1_533_763_059,
        about: "multilingual, slow on a CPU",
    },
    WhisperModel {
        name: "large-v3-turbo-q5_0",
        bytes: 574_041_195,
        about: "multilingual, large-v3-turbo quantized",
    },
    WhisperModel {
        name: "large-v3-turbo-q8_0",
        bytes: 874_188_075,
        about: "multilingual, large-v3-turbo at half the memory",
    },
    WhisperModel {
        name: "large-v3-turbo",
        bytes: 1_624_555_275,
        about: "multilingual, near large-v3, much faster",
    },
    WhisperModel {
        name: "large-v3-q5_0",
        bytes: 1_081_140_203,
        about: "multilingual, large-v3 at a third of the memory",
    },
    WhisperModel {
        name: "large-v3",
        bytes: 3_095_033_483,
        about: "multilingual, most accurate, wants a GPU",
    },
];

/// The model setup offers first: the file `[local-whisper] model_path`
/// defaults to.
pub const DEFAULT_WHISPER_MODEL: &str = "base.en";

// ---------------------------------------------------------------------------
// Read aloud (TTS)
// ---------------------------------------------------------------------------

/// `[tts] model` with `backend = "groq"`.
///
/// Source: https://console.groq.com/docs/text-to-speech/orpheus, checked
/// 2026-10-29.
pub const GROQ_TTS_MODELS: &[Entry] = &[
    ("canopylabs/orpheus-v1-english", "Orpheus, English"),
    ("canopylabs/orpheus-arabic-saudi", "Orpheus, Saudi Arabic"),
];

/// `[tts] voice` with `backend = "groq"`, by the model that has it.
///
/// Source: https://console.groq.com/docs/text-to-speech/orpheus, checked
/// 2026-10-29.
pub const GROQ_TTS_VOICES: &[Entry] = &[
    ("autumn", "English, female"),
    ("diana", "English, female"),
    ("hannah", "English, female"),
    ("austin", "English, male"),
    ("daniel", "English, male"),
    ("troy", "English, male"),
    ("abdullah", "Saudi Arabic, male"),
    ("fahad", "Saudi Arabic, male"),
    ("sultan", "Saudi Arabic, male"),
    ("lulwa", "Saudi Arabic, female"),
    ("noura", "Saudi Arabic, female"),
    ("aisha", "Saudi Arabic, female"),
];

/// `[tts] model` with `backend = "openai"`. All three go on 2027-01-06;
/// OpenAI's replacement only speaks through the Realtime API, which whisrs
/// does not use for speech.
///
/// Source: https://developers.openai.com/api/docs/guides/text-to-speech and
/// https://developers.openai.com/api/docs/deprecations, checked 2026-10-29.
pub const OPENAI_TTS_MODELS: &[Entry] = &[
    ("gpt-4o-mini-tts", "every voice, removed 2027-01-06"),
    ("tts-1", "low latency, removed 2027-01-06"),
    ("tts-1-hd", "higher quality, removed 2027-01-06"),
];

/// `[tts] voice` with `backend = "openai"`.
///
/// Source: https://developers.openai.com/api/docs/guides/text-to-speech,
/// checked 2026-10-29 (tts-1 and tts-1-hd lack ballad, verse, marin and
/// cedar).
pub const OPENAI_TTS_VOICES: &[Entry] = &[
    ("alloy", "every model"),
    ("ash", "every model"),
    ("coral", "every model"),
    ("echo", "every model"),
    ("fable", "every model"),
    ("nova", "every model"),
    ("onyx", "every model"),
    ("sage", "every model"),
    ("shimmer", "every model"),
    ("ballad", "gpt-4o-mini-tts only"),
    ("verse", "gpt-4o-mini-tts only"),
    ("marin", "gpt-4o-mini-tts only, best quality"),
    ("cedar", "gpt-4o-mini-tts only, best quality"),
];

/// `[tts] model` with `backend = "deepgram"`: Aura-2, the voice in the
/// model id, so `[tts] voice` is not used.
///
/// Source: https://developers.deepgram.com/docs/tts-models, checked
/// 2026-10-29.
pub const DEEPGRAM_TTS_MODELS: &[Entry] = &[
    ("aura-2-thalia-en", "English US, female: clear, confident"),
    (
        "aura-2-andromeda-en",
        "English US, female: casual, expressive",
    ),
    ("aura-2-helena-en", "English US, female: caring, natural"),
    ("aura-2-apollo-en", "English US, male: confident, casual"),
    ("aura-2-arcas-en", "English US, male: natural, smooth"),
    ("aura-2-aries-en", "English US, male: warm, energetic"),
    ("aura-2-amalthea-en", "English Filipino, female: cheerful"),
    (
        "aura-2-asteria-en",
        "English US, female: clear, knowledgeable",
    ),
    ("aura-2-athena-en", "English US, female: calm, professional"),
    ("aura-2-atlas-en", "English US, male: enthusiastic"),
    (
        "aura-2-aurora-en",
        "English US, female: cheerful, energetic",
    ),
    (
        "aura-2-callista-en",
        "English US, female: clear, professional",
    ),
    ("aura-2-cora-en", "English US, female: smooth, melodic"),
    (
        "aura-2-cordelia-en",
        "English US, female: approachable, warm",
    ),
    ("aura-2-delia-en", "English US, female: casual, friendly"),
    ("aura-2-draco-en", "English British, male: warm, baritone"),
    ("aura-2-electra-en", "English US, female: professional"),
    ("aura-2-harmonia-en", "English US, female: empathetic, calm"),
    ("aura-2-hera-en", "English US, female: smooth, warm"),
    ("aura-2-hermes-en", "English US, male: expressive, engaging"),
    (
        "aura-2-hyperion-en",
        "English Australian, male: caring, warm",
    ),
    ("aura-2-iris-en", "English US, female: cheerful, positive"),
    ("aura-2-janus-en", "English US, female: southern, smooth"),
    ("aura-2-juno-en", "English US, female: natural, melodic"),
    (
        "aura-2-jupiter-en",
        "English US, male: expressive, baritone",
    ),
    ("aura-2-luna-en", "English US, female: friendly, natural"),
    ("aura-2-mars-en", "English US, male: smooth, baritone"),
    (
        "aura-2-minerva-en",
        "English US, female: positive, friendly",
    ),
    (
        "aura-2-neptune-en",
        "English US, male: professional, patient",
    ),
    ("aura-2-odysseus-en", "English US, male: calm, smooth"),
    (
        "aura-2-ophelia-en",
        "English US, female: expressive, cheerful",
    ),
    ("aura-2-orion-en", "English US, male: approachable, calm"),
    ("aura-2-orpheus-en", "English US, male: professional, clear"),
    ("aura-2-pandora-en", "English British, female: smooth, calm"),
    ("aura-2-phoebe-en", "English US, female: energetic, warm"),
    ("aura-2-pluto-en", "English US, male: smooth, baritone"),
    (
        "aura-2-saturn-en",
        "English US, male: knowledgeable, baritone",
    ),
    (
        "aura-2-selene-en",
        "English US, female: expressive, energetic",
    ),
    ("aura-2-theia-en", "English Australian, female: expressive"),
    ("aura-2-vesta-en", "English US, female: natural, patient"),
    ("aura-2-zeus-en", "English US, male: deep, trustworthy"),
    ("aura-2-agustina-es", "Spanish Peninsular, female"),
    ("aura-2-alvaro-es", "Spanish Peninsular, male"),
    ("aura-2-antonia-es", "Spanish Argentine, female"),
    ("aura-2-aquila-es", "Spanish Latin American, male"),
    ("aura-2-carina-es", "Spanish Peninsular, female"),
    ("aura-2-celeste-es", "Spanish Colombian, female"),
    ("aura-2-diana-es", "Spanish Peninsular, female"),
    ("aura-2-estrella-es", "Spanish Mexican, female"),
    ("aura-2-gloria-es", "Spanish Colombian, female"),
    ("aura-2-javier-es", "Spanish Mexican, male"),
    ("aura-2-luciano-es", "Spanish Mexican, male"),
    ("aura-2-nestor-es", "Spanish Peninsular, male"),
    ("aura-2-olivia-es", "Spanish Mexican, female"),
    ("aura-2-selena-es", "Spanish Latin American, female"),
    ("aura-2-silvia-es", "Spanish Peninsular, female"),
    ("aura-2-sirio-es", "Spanish Mexican, male"),
    ("aura-2-valerio-es", "Spanish Mexican, male"),
    ("aura-2-beatrix-nl", "Dutch, female"),
    ("aura-2-cornelia-nl", "Dutch, female"),
    ("aura-2-daphne-nl", "Dutch, female"),
    ("aura-2-hestia-nl", "Dutch, female"),
    ("aura-2-leda-nl", "Dutch, female"),
    ("aura-2-rhea-nl", "Dutch, female"),
    ("aura-2-lars-nl", "Dutch, male"),
    ("aura-2-roman-nl", "Dutch, male"),
    ("aura-2-sander-nl", "Dutch, male"),
    ("aura-2-agathe-fr", "French, female"),
    ("aura-2-hector-fr", "French, male"),
    ("aura-2-aurelia-de", "German, female"),
    ("aura-2-elara-de", "German, female"),
    ("aura-2-kara-de", "German, female"),
    ("aura-2-lara-de", "German, female"),
    ("aura-2-viktoria-de", "German, female"),
    ("aura-2-fabian-de", "German, male"),
    ("aura-2-julius-de", "German, male"),
    ("aura-2-cinzia-it", "Italian, female"),
    ("aura-2-demetra-it", "Italian, female"),
    ("aura-2-livia-it", "Italian, female"),
    ("aura-2-maia-it", "Italian, female"),
    ("aura-2-melia-it", "Italian, female"),
    ("aura-2-cesare-it", "Italian, male"),
    ("aura-2-dionisio-it", "Italian, male"),
    ("aura-2-elio-it", "Italian, male"),
    ("aura-2-flavio-it", "Italian, male"),
    ("aura-2-ama-ja", "Japanese, female"),
    ("aura-2-izanami-ja", "Japanese, female"),
    ("aura-2-uzume-ja", "Japanese, female"),
    ("aura-2-ebisu-ja", "Japanese, male"),
    ("aura-2-fujin-ja", "Japanese, male"),
];

/// `[tts] model` with `backend = "tts-sidecar"`: Kokoro-FastAPI's.
///
/// Source: https://github.com/remsky/Kokoro-FastAPI, checked 2026-10-29.
pub const SIDECAR_TTS_MODELS: &[Entry] = &[("kokoro", "Kokoro-82M (Kokoro-FastAPI)")];

/// `[tts] voice` with `backend = "tts-sidecar"`: Kokoro's voices, which
/// OpenRouter's `hexgrad/kokoro-82m` has too. Kokoro-FastAPI also takes
/// blends (`af_bella+af_sky`), which can be typed.
///
/// Source: https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md,
/// checked 2026-10-29.
pub const KOKORO_VOICES: &[Entry] = &[
    ("af_heart", "American English, female, best"),
    ("af_bella", "American English, female"),
    ("af_nicole", "American English, female"),
    ("af_aoede", "American English, female"),
    ("af_kore", "American English, female"),
    ("af_sarah", "American English, female"),
    ("af_alloy", "American English, female"),
    ("af_nova", "American English, female"),
    ("af_sky", "American English, female"),
    ("af_jessica", "American English, female"),
    ("af_river", "American English, female"),
    ("am_fenrir", "American English, male"),
    ("am_michael", "American English, male"),
    ("am_puck", "American English, male"),
    ("am_echo", "American English, male"),
    ("am_eric", "American English, male"),
    ("am_liam", "American English, male"),
    ("am_onyx", "American English, male"),
    ("am_santa", "American English, male"),
    ("am_adam", "American English, male"),
    ("bf_emma", "British English, female"),
    ("bf_isabella", "British English, female"),
    ("bf_alice", "British English, female"),
    ("bf_lily", "British English, female"),
    ("bm_fable", "British English, male"),
    ("bm_george", "British English, male"),
    ("bm_lewis", "British English, male"),
    ("bm_daniel", "British English, male"),
    ("jf_alpha", "Japanese, female"),
    ("jf_gongitsune", "Japanese, female"),
    ("jf_nezumi", "Japanese, female"),
    ("jf_tebukuro", "Japanese, female"),
    ("jm_kumo", "Japanese, male"),
    ("zf_xiaobei", "Mandarin, female"),
    ("zf_xiaoni", "Mandarin, female"),
    ("zf_xiaoxiao", "Mandarin, female"),
    ("zf_xiaoyi", "Mandarin, female"),
    ("zm_yunjian", "Mandarin, male"),
    ("zm_yunxi", "Mandarin, male"),
    ("zm_yunxia", "Mandarin, male"),
    ("zm_yunyang", "Mandarin, male"),
    ("ef_dora", "Spanish, female"),
    ("em_alex", "Spanish, male"),
    ("em_santa", "Spanish, male"),
    ("ff_siwis", "French, female"),
    ("hf_alpha", "Hindi, female"),
    ("hf_beta", "Hindi, female"),
    ("hm_omega", "Hindi, male"),
    ("hm_psi", "Hindi, male"),
    ("if_sara", "Italian, female"),
    ("im_nicola", "Italian, male"),
    ("pf_dora", "Brazilian Portuguese, female"),
    ("pm_alex", "Brazilian Portuguese, male"),
    ("pm_santa", "Brazilian Portuguese, male"),
];

/// `[tts] url`, which only `tts-sidecar` reads: a server of your own where
/// it listens by default, and OpenRouter.
///
/// Sources: `tts::DEFAULT_SIDECAR_URL`, and OpenRouter's live API, checked
/// 2026-11-13.
pub const TTS_URLS: &[Entry] = &[
    (
        crate::tts::DEFAULT_SIDECAR_URL,
        "a server of your own on this machine (Kokoro-FastAPI)",
    ),
    (OPENROUTER_TTS_URL, "OpenRouter, many voices, one key"),
];

/// The `[tts] model` list for `backend`; for `tts-sidecar`, OpenRouter's
/// when `url` (`[tts] url`) is there.
pub fn tts_models(backend: &str, url: Option<&str>) -> &'static [Entry] {
    match backend {
        "groq" => GROQ_TTS_MODELS,
        "openai" => OPENAI_TTS_MODELS,
        "deepgram" => DEEPGRAM_TTS_MODELS,
        "tts-sidecar" | "openai-compat" if url.is_some_and(is_openrouter) => OPENROUTER_TTS_MODELS,
        "tts-sidecar" | "openai-compat" => SIDECAR_TTS_MODELS,
        _ => &[],
    }
}

/// The `[tts] voice` list for `backend`; none for deepgram, whose voice is
/// in the model id. On OpenRouter (`url`) each model has voices of its own:
/// `model`'s, or the default model's while it is unset.
pub fn tts_voices(backend: &str, url: Option<&str>, model: Option<&str>) -> &'static [Entry] {
    match backend {
        "groq" => GROQ_TTS_VOICES,
        "openai" => OPENAI_TTS_VOICES,
        "tts-sidecar" | "openai-compat" if url.is_some_and(is_openrouter) => {
            let model = model
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .or_else(|| crate::tts::default_model(backend, url))
                .unwrap_or_default();
            openrouter_tts_voices(model)
        }
        "tts-sidecar" | "openai-compat" => KOKORO_VOICES,
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// LLM
// ---------------------------------------------------------------------------

/// `[llm] api_url`: chat completions endpoints that work.
///
/// Sources: https://developers.openai.com/api/reference/resources/chat,
/// https://console.groq.com/docs/openai, https://openrouter.ai/docs/quickstart,
/// https://ai.google.dev/gemini-api/docs/openai and
/// https://docs.ollama.com/api/openai-compatibility, checked 2026-10-29.
pub const LLM_URLS: &[Entry] = &[
    ("https://api.openai.com/v1/chat/completions", "OpenAI"),
    ("https://api.groq.com/openai/v1/chat/completions", "Groq"),
    (
        "https://openrouter.ai/api/v1/chat/completions",
        "OpenRouter",
    ),
    (
        "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
        "Google Gemini",
    ),
    (
        "http://localhost:11434/v1/chat/completions",
        "Ollama on this machine",
    ),
];

/// `[llm] model` on OpenAI.
///
/// Source: https://developers.openai.com/api/docs/models and
/// https://developers.openai.com/api/docs/deprecations, checked 2026-10-29.
pub const OPENAI_CHAT_MODELS: &[Entry] = &[
    ("gpt-6-luna", "most efficient, for high-volume tasks"),
    ("gpt-6.1-terra", "fast, efficient, clear natural writing"),
    ("gpt-6.1-sol", "near-Astra performance at a lower cost"),
    ("gpt-5.4-mini", "GPT-5.4, faster, for high-volume work"),
    ("gpt-4o-mini", "fast, affordable, removed 2027-01-14"),
];

/// `[llm] model` on Groq.
///
/// Source: https://console.groq.com/docs/models, checked 2026-10-29 (the
/// Llama models are enterprise only since 2026-08-16).
pub const GROQ_CHAT_MODELS: &[Entry] = &[
    ("openai/gpt-oss-20b", "OpenAI open-weight 20B"),
    ("openai/gpt-oss-120b", "OpenAI's flagship open-weight model"),
    ("qwen/qwen3.8-27b", "Qwen 3.8 27B, preview only"),
];

/// `[llm] model` on OpenRouter.
///
/// Source: https://openrouter.ai/api/v1/models (linked from
/// https://openrouter.ai/docs/quickstart), checked 2026-10-29.
pub const OPENROUTER_CHAT_MODELS: &[Entry] = &[
    ("~openai/gpt-luna-latest", "always the latest GPT Luna"),
    ("openai/gpt-6-luna", "GPT-6 Luna, fast and cost-efficient"),
    (
        "anthropic/claude-haiku-5.5",
        "Claude Haiku 5.5, small and fast",
    ),
    ("google/gemini-3.8-flash", "Google's most intelligent Flash"),
    ("openai/gpt-oss-120b", "OpenAI open-weight 117B MoE"),
];

/// `[llm] model` on Gemini's OpenAI-compatible endpoint. The 2.5 models are
/// left out: Google serves them only to accounts that used them recently.
///
/// Source: https://ai.google.dev/gemini-api/docs/models and
/// https://ai.google.dev/gemini-api/docs/openai, checked 2026-10-29.
pub const GEMINI_CHAT_MODELS: &[Entry] = &[
    ("gemini-3.8-flash", "the most capable Flash: coding, agents"),
    (
        "gemini-3.6-flash",
        "the Flash before it, fast and multimodal",
    ),
    (
        "gemini-3.5-flash-lite",
        "the fastest and cheapest, high volume",
    ),
    ("gemini-3.1-flash-lite", "near-frontier at a lower cost"),
    ("gemini-4-argon", "the flagship reasoning model, 1M tokens"),
];

/// `[llm] model` on Ollama: models to `ollama pull` first.
///
/// Source: https://docs.ollama.com/api/openai-compatibility and
/// https://ollama.com/library, checked 2026-10-29.
pub const OLLAMA_CHAT_MODELS: &[Entry] = &[
    ("llama3.2", "Meta Llama 3.2, 1B and 3B"),
    ("qwen2.5", "Qwen 2.5, up to 128K tokens"),
    ("qwen3", "Qwen 3, dense and MoE"),
    ("gemma4", "Gemma 4"),
    ("gpt-oss:20b", "OpenAI open-weight 20B"),
];

/// The `[llm] model` list for the host of `api_url`; none for a host this
/// does not know.
pub fn llm_models(api_url: &str) -> &'static [Entry] {
    let (name, port) = host_of(api_url);
    match name.as_str() {
        "api.openai.com" => OPENAI_CHAT_MODELS,
        "api.groq.com" => GROQ_CHAT_MODELS,
        "openrouter.ai" => OPENROUTER_CHAT_MODELS,
        "generativelanguage.googleapis.com" => GEMINI_CHAT_MODELS,
        "localhost" | "127.0.0.1" | "[::1]" if port == Some("11434") => OLLAMA_CHAT_MODELS,
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// OpenRouter
// ---------------------------------------------------------------------------

/// OpenRouter's speech-to-text endpoint, for the `asr-sidecar` backend.
/// Checked against the live API on 2026-11-13.
pub const OPENROUTER_STT_URL: &str = "https://openrouter.ai/api/v1/audio/transcriptions";

/// OpenRouter's text-to-speech endpoint, for the `tts-sidecar` backend.
/// Checked against the live API on 2026-11-13.
pub const OPENROUTER_TTS_URL: &str = "https://openrouter.ai/api/v1/audio/speech";

/// Whether `url` points at OpenRouter: its host, as reqwest parses it (the
/// one a request goes to, the key with it), is `openrouter.ai`, in any
/// case, on any port or path. A URL reqwest cannot parse (one without a
/// scheme) goes nowhere, so it is not OpenRouter either; nor is a
/// look-alike whose `@openrouter.ai` sits in the query or the fragment.
pub fn is_openrouter(url: &str) -> bool {
    reqwest::Url::parse(url.trim())
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.eq_ignore_ascii_case("openrouter.ai"))
        })
        .unwrap_or(false)
}

/// The host name of `url`, lowercased, and its port. The scheme, user info
/// and path are cut off, and a URL without a scheme works too. Only picks
/// a list ([`llm_models`]); [`is_openrouter`], which decides where a key
/// goes, parses as reqwest does.
fn host_of(url: &str) -> (String, Option<&str>) {
    let rest = url
        .trim()
        .split_once("://")
        .map_or(url.trim(), |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    (name.to_ascii_lowercase(), port)
}

/// `[asr-sidecar] model` on OpenRouter: every speech-to-text model it has,
/// the setup default first, then cheapest first, each with what an hour of
/// audio costs (three are priced per token instead). None of them takes a
/// prompt or vocabulary (`Config::openrouter_prompt_warnings`).
///
/// Source: https://openrouter.ai/api/v1/models?output_modalities=transcription,
/// checked 2026-11-13 (`pricing.prompt` is per second of audio, but per
/// hour for the two MAI-Transcribe models).
pub const OPENROUTER_STT_MODELS: &[Entry] = &[
    (
        "openai/whisper-large-v3-turbo",
        "Whisper Large V3 Turbo, $0.012 per hour",
    ),
    (
        "nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b",
        "Nemotron 3.5 ASR Streaming Multilingual 0.6B, $0.012 per hour",
    ),
    ("qwen/qwen3-asr-0.6b", "Qwen3 ASR 0.6B, $0.012 per hour"),
    (
        "openai/whisper-large-v3",
        "Whisper Large V3, $0.027 per hour",
    ),
    ("qwen/qwen3-asr-1.7b", "Qwen3 ASR 1.7B, $0.027 per hour"),
    (
        "mistralai/voxtral-mini-3b-2507",
        "Voxtral Mini 3B 2507, $0.06 per hour",
    ),
    ("moondream/parakeet-ultra", "Parakeet Ultra, $0.06 per hour"),
    (
        "nvidia/nemotron-3.5-transcribe",
        "Nemotron 3.5 Transcribe, $0.06 per hour",
    ),
    (
        "nvidia/parakeet-tdt-0.6b-v3",
        "Parakeet TDT 0.6B v3, $0.06 per hour",
    ),
    (
        "microsoft/mai-transcribe-2",
        "MAI-Transcribe 2, $0.10 per hour",
    ),
    ("x-ai/grok-stt-1.0", "Grok STT 1.0, $0.10 per hour"),
    ("x-ai/grok-stt-2.0", "Grok STT 2.0, $0.10 per hour"),
    (
        "meta/muse-voice-transcribe-1.0",
        "Muse Voice Transcribe 1.0, $0.18 per hour",
    ),
    (
        "mistralai/voxtral-mini-transcribe",
        "Voxtral Mini Transcribe, $0.18 per hour",
    ),
    (
        "mistralai/voxtral-small-24b-2507-stt",
        "Voxtral Small 24B 2507 STT, $0.18 per hour",
    ),
    (
        "mistralai/voxtral-mini-transcribe-3-0",
        "Voxtral Mini Transcribe 3, $0.21 per hour",
    ),
    ("elevenlabs/scribe-v2", "Scribe v2, $0.22 per hour"),
    (
        "elevenlabs/scribe-v2-medical",
        "Scribe v2 Medical, $0.22 per hour",
    ),
    (
        "mistralai/voxtral-small-transcribe-3-0",
        "Voxtral Small Transcribe 3, $0.24 per hour",
    ),
    ("deepgram/nova-3", "Nova-3, $0.26 per hour"),
    ("openai/gpt-transcribe", "GPT Transcribe, $0.27 per hour"),
    (
        "assemblyai/universal-3-5-pro",
        "Universal-3.5 Pro, $0.30 per hour",
    ),
    (
        "assemblyai/universal-3-6-pro",
        "Universal-3.6 Pro, $0.30 per hour",
    ),
    ("fish-audio/transcribe-1", "Transcribe 1, $0.36 per hour"),
    (
        "fish-audio/transcribe-1-pro",
        "Transcribe 1 Pro, $0.36 per hour",
    ),
    (
        "microsoft/mai-transcribe-1.5",
        "MAI-Transcribe 1.5, $0.36 per hour",
    ),
    ("openai/whisper-1", "Whisper 1, $0.36 per hour"),
    ("google/chirp-3", "Chirp 3, $0.96 per hour"),
    (
        "google/gemini-3.5-transcribe",
        "Gemini 3.5 Transcribe, priced per token",
    ),
    (
        "openai/gpt-4o-mini-transcribe",
        "GPT-4o Mini Transcribe, priced per token",
    ),
    (
        "openai/gpt-4o-transcribe",
        "GPT-4o Transcribe, priced per token",
    ),
];

/// The `[asr-sidecar] model` on OpenRouter: setup picks it when the sidecar
/// URL becomes OpenRouter's, and the daemon sends it in place of the local
/// default model, which OpenRouter does not have.
pub const OPENROUTER_STT_DEFAULT_MODEL: &str = "openai/whisper-large-v3-turbo";

/// `[tts] model` with `backend = "tts-sidecar"` on OpenRouter: a few of its
/// speech models, Kokoro (the default there, `tts::default_model`) first,
/// then free, then cheapest first, each with what 1,000 characters cost.
///
/// Source: https://openrouter.ai/api/v1/models?output_modalities=speech,
/// checked 2026-11-13 (`pricing.prompt` is per character).
pub const OPENROUTER_TTS_MODELS: &[Entry] = &[
    (
        "hexgrad/kokoro-82m",
        "Kokoro 82M, $0.00062 per 1,000 characters",
    ),
    (
        "heygen/heygen-voice-1-turbo:free",
        "HeyGen Voice Turbo, free",
    ),
    (
        "mistralai/voxtral-mini-tts-3-0",
        "Voxtral TTS 3, $0.013 per 1,000 characters",
    ),
    (
        "microsoft/mai-voice-2.1-flash",
        "MAI-Voice-2.1-Flash, $0.015 per 1,000 characters",
    ),
    (
        "elevenlabs/eleven-flash-v2.5",
        "Eleven Flash v2.5, $0.04 per 1,000 characters",
    ),
    (
        "google/gemini-3.8-flash-tts",
        "Gemini 3.8 Flash TTS, priced per token",
    ),
];

// The voices of each model on OpenRouter, from its `supported_voices`
// (Kokoro's are `KOKORO_VOICES`, the same 54). A voice's line says only
// what its id says.
//
// Source: https://openrouter.ai/api/v1/models?output_modalities=speech,
// checked 2026-11-13.

/// `[tts] voice` for `heygen/heygen-voice-1-turbo:free`.
pub const HEYGEN_TTS_VOICES: &[Entry] = &[
    ("nadine", "a HeyGen voice"),
    ("tabitha", "a HeyGen voice"),
    ("bram", "a HeyGen voice"),
    ("gareth", "a HeyGen voice"),
    ("noelle", "a HeyGen voice"),
    ("meredith", "a HeyGen voice"),
    ("sabrina", "a HeyGen voice"),
    ("lucian", "a HeyGen voice"),
    ("weston", "a HeyGen voice"),
    ("vincent", "a HeyGen voice"),
];

/// `[tts] voice` for `mistralai/voxtral-mini-tts-3-0`: the named voices
/// (language and mood), then the numbered ones.
pub const VOXTRAL_TTS_VOICES: &[Entry] = &[
    ("en_paul_neutral", "English, neutral"),
    ("en_paul_angry", "English, angry"),
    ("en_paul_cheerful", "English, cheerful"),
    ("en_paul_confident", "English, confident"),
    ("en_paul_excited", "English, excited"),
    ("en_paul_frustrated", "English, frustrated"),
    ("en_paul_happy", "English, happy"),
    ("en_paul_sad", "English, sad"),
    ("gb_jane_neutral", "British English, neutral"),
    ("gb_jane_confident", "British English, confident"),
    ("gb_jane_confused", "British English, confused"),
    ("gb_jane_curious", "British English, curious"),
    ("gb_jane_frustrated", "British English, frustrated"),
    ("gb_jane_jealousy", "British English, jealousy"),
    ("gb_jane_sad", "British English, sad"),
    ("gb_jane_sarcasm", "British English, sarcasm"),
    ("gb_jane_shameful", "British English, shameful"),
    ("gb_oliver_neutral", "British English, neutral"),
    ("gb_oliver_angry", "British English, angry"),
    ("gb_oliver_cheerful", "British English, cheerful"),
    ("gb_oliver_confident", "British English, confident"),
    ("gb_oliver_curious", "British English, curious"),
    ("gb_oliver_excited", "British English, excited"),
    ("gb_oliver_sad", "British English, sad"),
    ("fr_marie_neutral", "French, neutral"),
    ("fr_marie_angry", "French, angry"),
    ("fr_marie_curious", "French, curious"),
    ("fr_marie_excited", "French, excited"),
    ("fr_marie_happy", "French, happy"),
    ("fr_marie_sad", "French, sad"),
    ("voice_447", "a Voxtral voice"),
    ("voice_478", "a Voxtral voice"),
    ("voice_479", "a Voxtral voice"),
    ("voice_480", "a Voxtral voice"),
    ("voice_481", "a Voxtral voice"),
    ("voice_482", "a Voxtral voice"),
    ("voice_483", "a Voxtral voice"),
    ("voice_485", "a Voxtral voice"),
    ("voice_589", "a Voxtral voice"),
    ("voice_590", "a Voxtral voice"),
    ("voice_591", "a Voxtral voice"),
    ("voice_592", "a Voxtral voice"),
    ("voice_1786", "a Voxtral voice"),
    ("voice_1787", "a Voxtral voice"),
    ("voice_1788", "a Voxtral voice"),
    ("voice_3132", "a Voxtral voice"),
    ("voice_3133", "a Voxtral voice"),
    ("voice_3134", "a Voxtral voice"),
    ("voice_3135", "a Voxtral voice"),
    ("voice_3136", "a Voxtral voice"),
    ("voice_3137", "a Voxtral voice"),
    ("voice_3138", "a Voxtral voice"),
    ("voice_3139", "a Voxtral voice"),
    ("voice_3140", "a Voxtral voice"),
    ("voice_3141", "a Voxtral voice"),
    ("voice_3142", "a Voxtral voice"),
    ("voice_3143", "a Voxtral voice"),
    ("voice_3144", "a Voxtral voice"),
    ("voice_3145", "a Voxtral voice"),
    ("voice_3146", "a Voxtral voice"),
    ("voice_3147", "a Voxtral voice"),
    ("voice_3149", "a Voxtral voice"),
    ("voice_3150", "a Voxtral voice"),
    ("voice_3153", "a Voxtral voice"),
    ("voice_3154", "a Voxtral voice"),
    ("voice_3155", "a Voxtral voice"),
];

/// `[tts] voice` for `microsoft/mai-voice-2.1-flash`, by locale.
pub const MAI_TTS_VOICES: &[Entry] = &[
    ("cs-CZ-Grant:MAI-Voice-2.1-Flash", "Czech"),
    ("cs-CZ-Harper:MAI-Voice-2.1-Flash", "Czech"),
    ("da-DK-Grant:MAI-Voice-2.1-Flash", "Danish"),
    ("da-DK-Harper:MAI-Voice-2.1-Flash", "Danish"),
    ("de-DE-Grant:MAI-Voice-2.1-Flash", "German"),
    ("de-DE-Harper:MAI-Voice-2.1-Flash", "German"),
    ("de-DE-Klaus:MAI-Voice-2.1-Flash", "German"),
    ("de-DE-Mia:MAI-Voice-2.1-Flash", "German"),
    ("en-AU-Isla:MAI-Voice-2.1-Flash", "English Australian"),
    ("en-GB-Emily:MAI-Voice-2.1-Flash", "English British"),
    ("en-GB-Harry:MAI-Voice-2.1-Flash", "English British"),
    ("en-IN-Dhruv:MAI-Voice-2.1-Flash", "English Indian"),
    ("en-IN-Priya:MAI-Voice-2.1-Flash", "English Indian"),
    ("en-US-Ethan:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Grant:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Harper:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Iris:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Jasper:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Olivia:MAI-Voice-2.1-Flash", "English US"),
    ("en-US-Sage:MAI-Voice-2.1-Flash", "English US"),
    ("es-ES-Marta:MAI-Voice-2.1-Flash", "Spanish Peninsular"),
    ("es-MX-Alejo:MAI-Voice-2.1-Flash", "Spanish Mexican"),
    ("es-MX-Grant:MAI-Voice-2.1-Flash", "Spanish Mexican"),
    ("es-MX-Harper:MAI-Voice-2.1-Flash", "Spanish Mexican"),
    ("es-MX-Valeria:MAI-Voice-2.1-Flash", "Spanish Mexican"),
    ("fi-FI-Grant:MAI-Voice-2.1-Flash", "Finnish"),
    ("fi-FI-Harper:MAI-Voice-2.1-Flash", "Finnish"),
    ("fr-FR-Grant:MAI-Voice-2.1-Flash", "French"),
    ("fr-FR-Harper:MAI-Voice-2.1-Flash", "French"),
    ("fr-FR-Marc:MAI-Voice-2.1-Flash", "French"),
    ("fr-FR-Soleil:MAI-Voice-2.1-Flash", "French"),
    ("hi-IN-Arjun:MAI-Voice-2.1-Flash", "Hindi"),
    ("hi-IN-Dhruv:MAI-Voice-2.1-Flash", "Hindi"),
    ("hi-IN-Grant:MAI-Voice-2.1-Flash", "Hindi"),
    ("hi-IN-Harper:MAI-Voice-2.1-Flash", "Hindi"),
    ("hi-IN-Kavya:MAI-Voice-2.1-Flash", "Hindi"),
    ("hi-IN-Priya:MAI-Voice-2.1-Flash", "Hindi"),
    ("hu-HU-Bence:MAI-Voice-2.1-Flash", "Hungarian"),
    ("hu-HU-Grant:MAI-Voice-2.1-Flash", "Hungarian"),
    ("hu-HU-Harper:MAI-Voice-2.1-Flash", "Hungarian"),
    ("hu-HU-Levente:MAI-Voice-2.1-Flash", "Hungarian"),
    ("hu-HU-Lilla:MAI-Voice-2.1-Flash", "Hungarian"),
    ("hu-HU-Reka:MAI-Voice-2.1-Flash", "Hungarian"),
    ("id-ID-Grant:MAI-Voice-2.1-Flash", "Indonesian"),
    ("id-ID-Harper:MAI-Voice-2.1-Flash", "Indonesian"),
    ("it-IT-Grant:MAI-Voice-2.1-Flash", "Italian"),
    ("it-IT-Harper:MAI-Voice-2.1-Flash", "Italian"),
    ("it-IT-Luca:MAI-Voice-2.1-Flash", "Italian"),
    ("it-IT-Rosa:MAI-Voice-2.1-Flash", "Italian"),
    ("ko-KR-Grant:MAI-Voice-2.1-Flash", "Korean"),
    ("ko-KR-Haena:MAI-Voice-2.1-Flash", "Korean"),
    ("ko-KR-Harper:MAI-Voice-2.1-Flash", "Korean"),
    ("ko-KR-Junho:MAI-Voice-2.1-Flash", "Korean"),
    ("nb-NO-Grant:MAI-Voice-2.1-Flash", "Norwegian"),
    ("nb-NO-Harper:MAI-Voice-2.1-Flash", "Norwegian"),
    ("nl-NL-Grant:MAI-Voice-2.1-Flash", "Dutch"),
    ("nl-NL-Harper:MAI-Voice-2.1-Flash", "Dutch"),
    ("nl-NL-Sander:MAI-Voice-2.1-Flash", "Dutch"),
    ("pl-PL-Grant:MAI-Voice-2.1-Flash", "Polish"),
    ("pl-PL-Harper:MAI-Voice-2.1-Flash", "Polish"),
    ("pt-BR-Caio:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-BR-Grant:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-BR-Harper:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-BR-Luana:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-BR-Pedro:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-BR-Rafael:MAI-Voice-2.1-Flash", "Portuguese Brazilian"),
    ("pt-PT-Grant:MAI-Voice-2.1-Flash", "Portuguese European"),
    ("pt-PT-Harper:MAI-Voice-2.1-Flash", "Portuguese European"),
    ("pt-PT-Rui:MAI-Voice-2.1-Flash", "Portuguese European"),
    ("ro-RO-Andrei:MAI-Voice-2.1-Flash", "Romanian"),
    ("ro-RO-Elena:MAI-Voice-2.1-Flash", "Romanian"),
    ("ro-RO-Grant:MAI-Voice-2.1-Flash", "Romanian"),
    ("ro-RO-Harper:MAI-Voice-2.1-Flash", "Romanian"),
    ("ro-RO-Ioana:MAI-Voice-2.1-Flash", "Romanian"),
    ("ro-RO-Radu:MAI-Voice-2.1-Flash", "Romanian"),
    ("ru-RU-Grant:MAI-Voice-2.1-Flash", "Russian"),
    ("ru-RU-Harper:MAI-Voice-2.1-Flash", "Russian"),
    ("ru-RU-Lev:MAI-Voice-2.1-Flash", "Russian"),
    ("ru-RU-Masha:MAI-Voice-2.1-Flash", "Russian"),
    ("sv-SE-Grant:MAI-Voice-2.1-Flash", "Swedish"),
    ("sv-SE-Harper:MAI-Voice-2.1-Flash", "Swedish"),
    ("th-TH-Grant:MAI-Voice-2.1-Flash", "Thai"),
    ("th-TH-Harper:MAI-Voice-2.1-Flash", "Thai"),
    ("th-TH-Krit:MAI-Voice-2.1-Flash", "Thai"),
    ("th-TH-Nattapong:MAI-Voice-2.1-Flash", "Thai"),
    ("tr-TR-Aydin:MAI-Voice-2.1-Flash", "Turkish"),
    ("tr-TR-Elif:MAI-Voice-2.1-Flash", "Turkish"),
    ("tr-TR-Grant:MAI-Voice-2.1-Flash", "Turkish"),
    ("tr-TR-Harper:MAI-Voice-2.1-Flash", "Turkish"),
    ("vi-VN-Grant:MAI-Voice-2.1-Flash", "Vietnamese"),
    ("vi-VN-Harper:MAI-Voice-2.1-Flash", "Vietnamese"),
    ("zh-CN-Bo:MAI-Voice-2.1-Flash", "Mandarin"),
    ("zh-CN-Grant:MAI-Voice-2.1-Flash", "Mandarin"),
    ("zh-CN-Harper:MAI-Voice-2.1-Flash", "Mandarin"),
    ("zh-CN-Lan:MAI-Voice-2.1-Flash", "Mandarin"),
    ("zh-CN-Mei:MAI-Voice-2.1-Flash", "Mandarin"),
    ("zh-CN-Wei:MAI-Voice-2.1-Flash", "Mandarin"),
];

/// `[tts] voice` for `elevenlabs/eleven-flash-v2.5`.
pub const ELEVENLABS_TTS_VOICES: &[Entry] = &[
    ("adam", "an ElevenLabs voice"),
    ("alice", "an ElevenLabs voice"),
    ("bella", "an ElevenLabs voice"),
    ("bill", "an ElevenLabs voice"),
    ("brian", "an ElevenLabs voice"),
    ("callum", "an ElevenLabs voice"),
    ("charlie", "an ElevenLabs voice"),
    ("chris", "an ElevenLabs voice"),
    ("daniel", "an ElevenLabs voice"),
    ("eric", "an ElevenLabs voice"),
    ("george", "an ElevenLabs voice"),
    ("harry", "an ElevenLabs voice"),
    ("jessica", "an ElevenLabs voice"),
    ("laura", "an ElevenLabs voice"),
    ("liam", "an ElevenLabs voice"),
    ("lily", "an ElevenLabs voice"),
    ("matilda", "an ElevenLabs voice"),
    ("river", "an ElevenLabs voice"),
    ("roger", "an ElevenLabs voice"),
    ("sarah", "an ElevenLabs voice"),
    ("will", "an ElevenLabs voice"),
];

/// `[tts] voice` for `google/gemini-3.8-flash-tts`.
pub const GEMINI_TTS_VOICES: &[Entry] = &[
    ("Zephyr", "a Gemini voice"),
    ("Puck", "a Gemini voice"),
    ("Charon", "a Gemini voice"),
    ("Kore", "a Gemini voice"),
    ("Fenrir", "a Gemini voice"),
    ("Leda", "a Gemini voice"),
    ("Orus", "a Gemini voice"),
    ("Aoede", "a Gemini voice"),
    ("Callirrhoe", "a Gemini voice"),
    ("Autonoe", "a Gemini voice"),
    ("Enceladus", "a Gemini voice"),
    ("Iapetus", "a Gemini voice"),
    ("Umbriel", "a Gemini voice"),
    ("Algieba", "a Gemini voice"),
    ("Despina", "a Gemini voice"),
    ("Erinome", "a Gemini voice"),
    ("Algenib", "a Gemini voice"),
    ("Rasalgethi", "a Gemini voice"),
    ("Laomedeia", "a Gemini voice"),
    ("Achernar", "a Gemini voice"),
    ("Alnilam", "a Gemini voice"),
    ("Schedar", "a Gemini voice"),
    ("Gacrux", "a Gemini voice"),
    ("Pulcherrima", "a Gemini voice"),
    ("Achird", "a Gemini voice"),
    ("Zubenelgenubi", "a Gemini voice"),
    ("Vindemiatrix", "a Gemini voice"),
    ("Sadachbia", "a Gemini voice"),
    ("Sadaltager", "a Gemini voice"),
    ("Sulafat", "a Gemini voice"),
];

/// The `[asr-sidecar] model` list for `url`: OpenRouter's on its host, else
/// none (a server of your own names its own).
pub fn asr_sidecar_models(url: &str) -> &'static [Entry] {
    if is_openrouter(url) {
        OPENROUTER_STT_MODELS
    } else {
        &[]
    }
}

/// The `[tts] voice` list of OpenRouter `model`; none for a model this does
/// not list.
fn openrouter_tts_voices(model: &str) -> &'static [Entry] {
    match model {
        "hexgrad/kokoro-82m" => KOKORO_VOICES,
        "heygen/heygen-voice-1-turbo:free" => HEYGEN_TTS_VOICES,
        "mistralai/voxtral-mini-tts-3-0" => VOXTRAL_TTS_VOICES,
        "microsoft/mai-voice-2.1-flash" => MAI_TTS_VOICES,
        "elevenlabs/eleven-flash-v2.5" => ELEVENLABS_TTS_VOICES,
        "google/gemini-3.8-flash-tts" => GEMINI_TTS_VOICES,
        _ => &[],
    }
}

/// The value in `list` whose first part is `value`.
pub fn find(list: &[Entry], value: &str) -> Option<Entry> {
    list.iter().copied().find(|(v, _)| *v == value)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every list's values are distinct, and nothing has a dash a reader
    /// would take for punctuation.
    #[test]
    fn lists_have_distinct_values_and_plain_text() {
        for list in [
            LANGUAGES,
            BACKENDS,
            RECORDING_BACKENDS,
            LIVE_ONLY_BACKENDS,
            SEGMENTATIONS,
            REALTIME_PROFILES,
            TURN_DETECTIONS,
            INPUT_BACKENDS,
            OVERLAY_THEMES,
            OVERLAY_POSITIONS,
            AGENT_STYLES,
            AGENT_CORNERS,
            TTS_BACKENDS,
            HARNESSES,
            PERMISSIONS,
            FEEDBACK,
            DIARIZATIONS,
            TTS_RESPONSE_FORMATS,
            DEEPGRAM_MODELS,
            GROQ_MODELS,
            OPENAI_MODELS,
            GROQ_TTS_MODELS,
            GROQ_TTS_VOICES,
            OPENAI_TTS_MODELS,
            OPENAI_TTS_VOICES,
            DEEPGRAM_TTS_MODELS,
            SIDECAR_TTS_MODELS,
            KOKORO_VOICES,
            LLM_URLS,
            OPENAI_CHAT_MODELS,
            GROQ_CHAT_MODELS,
            OPENROUTER_CHAT_MODELS,
            GEMINI_CHAT_MODELS,
            OLLAMA_CHAT_MODELS,
            ASR_SIDECAR_URLS,
            TTS_URLS,
            OPENROUTER_STT_MODELS,
            OPENROUTER_TTS_MODELS,
            HEYGEN_TTS_VOICES,
            VOXTRAL_TTS_VOICES,
            MAI_TTS_VOICES,
            ELEVENLABS_TTS_VOICES,
            GEMINI_TTS_VOICES,
        ] {
            let values: BTreeSet<&str> = list.iter().map(|(v, _)| *v).collect();
            assert_eq!(values.len(), list.len(), "{list:?}");
            for (value, about) in list {
                assert!(
                    !about.contains(['\u{2013}', '\u{2014}']),
                    "{value}: {about}"
                );
            }
        }
    }

    /// Every code is one `-l` takes as it is, and the list is whisper's
    /// whole table, so `auto` and the codes are all a picker needs.
    #[test]
    fn every_language_code_is_a_valid_override() {
        assert_eq!(LANGUAGES.len(), 100);
        assert_eq!(
            crate::validate_language_override(AUTO_LANGUAGE.0).as_deref(),
            Ok("auto")
        );
        for (code, _) in LANGUAGES {
            assert_eq!(
                crate::validate_language_override(code).as_deref(),
                Ok(*code)
            );
        }
    }

    /// whisper.cpp knows every code, and no more.
    #[cfg(feature = "local-whisper")]
    #[test]
    fn whisper_knows_every_language_code() {
        for (id, (code, _)) in LANGUAGES.iter().enumerate() {
            assert_eq!(
                whisper_rs::get_lang_id(code),
                Some(id as i32),
                "{code} is not whisper's language {id}"
            );
        }
        assert_eq!(
            whisper_rs::get_lang_max_id() as usize + 1,
            LANGUAGES.len(),
            "whisper.cpp has languages this list lacks"
        );
    }

    /// The serde defaults are offered: a default missing from its list
    /// would show as a custom value nobody chose.
    #[test]
    fn every_default_is_listed() {
        let config: crate::Config = toml::from_str("[input]\n[overlay]\n[agent_overlay]\n")
            .expect("an empty config parses");
        assert!(find(LANGUAGES, &config.general.language).is_some());
        assert!(find(BACKENDS, &config.general.backend).is_some());
        let overlay = config.overlay.expect("set above");
        assert!(find(OVERLAY_THEMES, &overlay.theme).is_some());
        assert!(find(OVERLAY_POSITIONS, &overlay.position).is_some());
        let agent_overlay = config.agent_overlay.expect("set above");
        assert!(find(AGENT_STYLES, &agent_overlay.style).is_some());
        assert!(find(AGENT_CORNERS, &agent_overlay.corner).is_some());
        let tts: crate::TtsConfig = toml::from_str("").unwrap();
        assert!(find(TTS_BACKENDS, &tts.backend).is_some());
        assert!(find(TTS_RESPONSE_FORMATS, &tts.response_format).is_some());

        let sections: crate::Config =
            toml::from_str("[deepgram]\n[groq]\n[openai]\n").expect("empty sections parse");
        let model = |m: Option<String>| m.expect("section set above");
        let deepgram = model(sections.deepgram.map(|d| d.model));
        assert!(find(DEEPGRAM_MODELS, &deepgram).is_some(), "{deepgram}");
        let groq = model(sections.groq.map(|g| g.model));
        assert!(find(GROQ_MODELS, &groq).is_some(), "{groq}");
        let openai = model(sections.openai.map(|o| o.model));
        assert!(find(OPENAI_MODELS, &openai).is_some(), "{openai}");
        // openai-realtime with no [openai] section at all.
        let bare: crate::Config = toml::from_str("").unwrap();
        assert!(find(OPENAI_MODELS, &bare.openai_realtime_model()).is_some());

        // Each TTS backend's own model and voice, as the backend fills
        // them in while unset.
        for (backend, _) in TTS_BACKENDS {
            let model = crate::tts::default_model(backend, None).expect("a default model");
            assert!(
                find(tts_models(backend, None), model).is_some(),
                "{backend} {model}"
            );
            match crate::tts::default_voice(backend) {
                Some(voice) => {
                    assert!(
                        find(tts_voices(backend, None, None), voice).is_some(),
                        "{backend} {voice}"
                    )
                }
                None => assert!(tts_voices(backend, None, None).is_empty(), "{backend}"),
            }
        }
        // tts-sidecar on OpenRouter: Kokoro's id there, and its voice.
        let url = Some(OPENROUTER_TTS_URL);
        let model = crate::tts::default_model("tts-sidecar", url).expect("a default model");
        assert!(
            find(tts_models("tts-sidecar", url), model).is_some(),
            "{model}"
        );
        let voice = crate::tts::default_voice("tts-sidecar").expect("a default voice");
        assert!(find(tts_voices("tts-sidecar", url, None), voice).is_some());
        assert!(find(tts_voices("tts-sidecar", url, Some(model)), voice).is_some());
        assert!(find(TTS_URLS, crate::tts::DEFAULT_SIDECAR_URL).is_some());

        // The sidecar's URL; on OpenRouter, the model setup picks there.
        let sidecar: crate::Config = toml::from_str("[asr-sidecar]\n").unwrap();
        let sidecar = sidecar.asr_sidecar.expect("set above");
        assert!(
            find(ASR_SIDECAR_URLS, &sidecar.url).is_some(),
            "{}",
            sidecar.url
        );
        assert!(find(OPENROUTER_STT_MODELS, OPENROUTER_STT_DEFAULT_MODEL).is_some());

        let llm: crate::llm::LlmConfig = toml::from_str("api_key = \"k\"").unwrap();
        assert!(find(LLM_URLS, &llm.api_url).is_some(), "{}", llm.api_url);
        assert!(
            find(llm_models(&llm.api_url), &llm.model).is_some(),
            "{}",
            llm.model
        );
    }

    /// The default model is offered, and is the file the default
    /// `model_path` names; names are distinct and plain.
    #[test]
    fn whisper_models_hold_the_default() {
        let names: BTreeSet<&str> = WHISPER_MODELS.iter().map(|m| m.name).collect();
        assert_eq!(names.len(), WHISPER_MODELS.len());
        assert!(names.contains(DEFAULT_WHISPER_MODEL));
        assert!(crate::config::types::default_whisper_model_path()
            .ends_with(&format!("ggml-{DEFAULT_WHISPER_MODEL}.bin")));
        for model in WHISPER_MODELS {
            assert!(!model.about.contains(['\u{2013}', '\u{2014}']), "{model:?}");
            assert!(model.bytes > 1_000_000, "{model:?}");
        }
    }

    /// Every URL offered has its model list.
    #[test]
    fn every_llm_url_has_models() {
        for (url, _) in LLM_URLS {
            assert!(!llm_models(url).is_empty(), "{url}");
        }
        assert!(!llm_models("http://127.0.0.1:11434/v1/chat/completions").is_empty());
        assert!(llm_models("http://localhost:8080/v1/chat/completions").is_empty());
        assert!(llm_models("https://example.com/v1").is_empty());
    }

    /// Every sidecar URL offered has its models: OpenRouter's lists, and a
    /// server of your own none (any name is typed). Every OpenRouter voice
    /// model has its voices.
    #[test]
    fn every_sidecar_url_has_models() {
        for (url, _) in ASR_SIDECAR_URLS {
            assert_eq!(
                asr_sidecar_models(url).is_empty(),
                !is_openrouter(url),
                "{url}"
            );
        }
        assert!(asr_sidecar_models("http://10.0.0.2:8000/v1/audio/transcriptions").is_empty());
        for (url, _) in TTS_URLS {
            let models = tts_models("tts-sidecar", Some(url));
            assert!(!models.is_empty(), "{url}");
            for (model, _) in models {
                assert!(
                    !tts_voices("tts-sidecar", Some(url), Some(model)).is_empty(),
                    "{url} {model}"
                );
            }
        }
        // Off OpenRouter the sidecar is Kokoro, whatever the model.
        let own = Some("http://10.0.0.2:8880/v1/audio/speech");
        assert_eq!(tts_models("tts-sidecar", own), SIDECAR_TTS_MODELS);
        assert_eq!(
            tts_voices("tts-sidecar", own, Some("elevenlabs/eleven-flash-v2.5")),
            KOKORO_VOICES
        );
        // On OpenRouter, the voices follow the model; one not listed has
        // none, and the other backends ignore the URL.
        let openrouter = Some(OPENROUTER_TTS_URL);
        assert_eq!(
            tts_voices(
                "tts-sidecar",
                openrouter,
                Some("elevenlabs/eleven-flash-v2.5")
            ),
            ELEVENLABS_TTS_VOICES
        );
        assert_eq!(
            tts_voices("tts-sidecar", openrouter, Some(" ")),
            KOKORO_VOICES
        );
        assert!(tts_voices("tts-sidecar", openrouter, Some("acme/new-voice")).is_empty());
        assert_eq!(tts_models("groq", openrouter), GROQ_TTS_MODELS);
    }

    /// OpenRouter is known by its host as reqwest parses it, whatever the
    /// scheme, port, path or case; a look-alike host is not it, nor a URL
    /// whose `@openrouter.ai` is in the query or fragment (reqwest sends
    /// those to the host before them), nor one reqwest cannot parse.
    #[test]
    fn is_openrouter_matches_the_host_only() {
        for url in [
            OPENROUTER_STT_URL,
            OPENROUTER_TTS_URL,
            "https://openrouter.ai/api/v1/chat/completions",
            "  HTTPS://OpenRouter.AI:443/api/v1/audio/speech ",
            "http://user@openrouter.ai/x",
            "https://openrouter.ai",
        ] {
            assert!(is_openrouter(url), "{url}");
        }
        for url in [
            "",
            "https://api.openai.com/v1/audio/speech",
            "https://openrouter.ai.example.com/v1",
            "https://notopenrouter.ai/v1",
            "https://www.openrouter.ai/v1",
            "http://127.0.0.1:8880/v1/audio/speech",
            "https://example.com/openrouter.ai",
            "https://evil.com#@openrouter.ai/",
            "https://evil.com?@openrouter.ai/",
            "https://evil.com/x#@openrouter.ai/api/v1/audio/speech",
            "openrouter.ai/api/v1",
            "openrouter.ai",
        ] {
            assert!(!is_openrouter(url), "{url}");
        }
        // The list a URL picks is as it was: the host before any `@`.
        assert_eq!(
            llm_models("https://openrouter.ai/api/v1/chat/completions"),
            OPENROUTER_CHAT_MODELS
        );
    }
}
