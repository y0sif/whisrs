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
    ("asr-sidecar", "an HTTP ASR server of your own"),
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
    ("asr-sidecar", "an HTTP ASR server of your own"),
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
    ("openai", "OpenAI speech"),
    ("deepgram", "Deepgram Aura-2"),
    ("tts-sidecar", "a local OpenAI-compatible server"),
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

/// `[tts] response_format`. Playback decodes WAV only
/// (`audio::playback::decode_wav`), so it is the one format offered.
pub const TTS_RESPONSE_FORMATS: &[Entry] = &[("wav", "the only format whisrs plays")];

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

/// `[tts] voice` with `backend = "tts-sidecar"`: Kokoro's voices. Kokoro-
/// FastAPI also takes blends (`af_bella+af_sky`), which can be typed.
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

/// The `[tts] model` list for `backend`.
pub fn tts_models(backend: &str) -> &'static [Entry] {
    match backend {
        "groq" => GROQ_TTS_MODELS,
        "openai" => OPENAI_TTS_MODELS,
        "deepgram" => DEEPGRAM_TTS_MODELS,
        "tts-sidecar" | "openai-compat" => SIDECAR_TTS_MODELS,
        _ => &[],
    }
}

/// The `[tts] voice` list for `backend`; none for deepgram, whose voice is
/// in the model id.
pub fn tts_voices(backend: &str) -> &'static [Entry] {
    match backend {
        "groq" => GROQ_TTS_VOICES,
        "openai" => OPENAI_TTS_VOICES,
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
/// https://console.groq.com/docs/openai, https://openrouter.ai/docs/quickstart
/// and https://docs.ollama.com/api/openai-compatibility, checked 2026-10-29.
pub const LLM_URLS: &[Entry] = &[
    ("https://api.openai.com/v1/chat/completions", "OpenAI"),
    ("https://api.groq.com/openai/v1/chat/completions", "Groq"),
    (
        "https://openrouter.ai/api/v1/chat/completions",
        "OpenRouter",
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
    let rest = api_url
        .trim()
        .split_once("://")
        .map_or(api_url.trim(), |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    match name.to_ascii_lowercase().as_str() {
        "api.openai.com" => OPENAI_CHAT_MODELS,
        "api.groq.com" => GROQ_CHAT_MODELS,
        "openrouter.ai" => OPENROUTER_CHAT_MODELS,
        "localhost" | "127.0.0.1" | "[::1]" if port == Some("11434") => OLLAMA_CHAT_MODELS,
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
            OLLAMA_CHAT_MODELS,
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
            let model = crate::tts::default_model(backend).expect("a default model");
            assert!(
                find(tts_models(backend), model).is_some(),
                "{backend} {model}"
            );
            match crate::tts::default_voice(backend) {
                Some(voice) => {
                    assert!(
                        find(tts_voices(backend), voice).is_some(),
                        "{backend} {voice}"
                    )
                }
                None => assert!(tts_voices(backend).is_empty(), "{backend}"),
            }
        }

        let llm: crate::llm::LlmConfig = toml::from_str("api_key = \"k\"").unwrap();
        assert!(find(LLM_URLS, &llm.api_url).is_some(), "{}", llm.api_url);
        assert!(
            find(llm_models(&llm.api_url), &llm.model).is_some(),
            "{}",
            llm.model
        );
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
}
