use base64::Engine;
use tracing::warn;

use super::wire::{LemonadeSessionUpdate, LlamaCppSessionUpdate, OpenAiSessionUpdate};
use crate::transcription::StreamText;

/// OpenAI Realtime API rejects transcription prompts longer than this.
pub const PROMPT_MAX_CHARS: usize = 1024;

/// Supported OpenAI-compatible realtime protocol profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeProfile {
    OpenAi,
    Lemonade,
    /// The llama.cpp realtime endpoint (Voxtral and friends): the flat Lemonade
    /// session shape and 16 kHz input, but append-only word deltas.
    LlamaCpp,
}

/// Transcript delta semantics for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaMode {
    AppendOnly,
    ReplaceableInterim,
}

/// Turn detection mode used for a realtime session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDetectionMode {
    ServerVad,
    ManualCommit,
}

impl OpenAiRealtimeProfile {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "lemonade" => Ok(Self::Lemonade),
            "llamacpp" => Ok(Self::LlamaCpp),
            other => anyhow::bail!(
                "unsupported OpenAI-compatible realtime profile '{other}' (supported: openai, lemonade, llamacpp)"
            ),
        }
    }

    pub fn input_sample_rate(self) -> u32 {
        match self {
            Self::OpenAi => 24_000,
            Self::Lemonade | Self::LlamaCpp => 16_000,
        }
    }

    pub fn delta_mode(self) -> DeltaMode {
        match self {
            Self::OpenAi | Self::LlamaCpp => DeltaMode::AppendOnly,
            Self::Lemonade => DeltaMode::ReplaceableInterim,
        }
    }

    /// How the text the engine sends for this profile joins up: append
    /// deltas are fragments (the engine spaces an item's first delta from the
    /// text before it), completed utterances are phrases.
    pub fn stream_text(self) -> StreamText {
        match self.delta_mode() {
            DeltaMode::AppendOnly => StreamText::Fragments,
            DeltaMode::ReplaceableInterim => StreamText::Phrases,
        }
    }

    /// Whether the server answers the end-of-stream commit with a `completed`
    /// event once it has transcribed all the committed audio. llama.cpp does,
    /// so the engine waits for that event after the stop: a server slower than
    /// real time can stay quiet for seconds while it works through a pause in
    /// the speech, and still have the last words to send.
    pub fn completes_every_commit(self) -> bool {
        match self {
            Self::LlamaCpp => true,
            Self::OpenAi | Self::Lemonade => false,
        }
    }

    pub fn session_update(
        self,
        model: &str,
        language: &str,
        languages: &[String],
        prompt: Option<&str>,
        turn_detection: TurnDetectionMode,
    ) -> anyhow::Result<serde_json::Value> {
        let value = match self {
            Self::OpenAi => serde_json::to_value(OpenAiSessionUpdate::new(
                model,
                language,
                languages,
                prompt,
                turn_detection,
            ))?,
            Self::Lemonade => {
                serde_json::to_value(LemonadeSessionUpdate::new(model, turn_detection))?
            }
            Self::LlamaCpp => serde_json::to_value(LlamaCppSessionUpdate::new(
                model,
                turn_detection,
                self.input_sample_rate(),
            ))?,
        };
        Ok(value)
    }

    // Decision: keep this `true` for every profile, including the OpenAI
    // append-only / server-VAD profile. Both currently supported profiles need
    // an explicit commit at end-of-audio to flush any trailing speech, even
    // when server VAD is enabled. The risk that server VAD already finalized
    // the last turn before recording stopped (so the commit has nothing to
    // flush and the post-commit completion never arrives) is handled in the
    // engine: once input has closed and at least one transcript was already
    // typed, a missing post-commit completion finalizes gracefully (after a
    // short grace period) instead of erroring or stalling for the full timeout.
    // That keeps the commit-aware lifecycle the design wants without discarding
    // correct transcripts. The turn-detection parameter stays in the signature
    // because a future provider may need different EOS behavior for server-VAD
    // vs manual-commit sessions.
    pub fn should_send_commit_on_eos(self, _turn_detection: TurnDetectionMode) -> bool {
        true
    }
}

impl TurnDetectionMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "server-vad" => Ok(Self::ServerVad),
            "manual-commit" => Ok(Self::ManualCommit),
            other => anyhow::bail!(
                "unsupported turn detection '{other}' (supported: server-vad, manual-commit)"
            ),
        }
    }
}

pub fn openai_turn_detection_mode_for_model(model: &str) -> TurnDetectionMode {
    if model.eq_ignore_ascii_case("gpt-realtime-whisper")
        || model.eq_ignore_ascii_case("gpt-live-transcribe")
        || model.eq_ignore_ascii_case("gpt-transcribe")
    {
        TurnDetectionMode::ManualCommit
    } else {
        TurnDetectionMode::ServerVad
    }
}

pub fn openai_model_supports_prompt(model: &str) -> bool {
    !model.eq_ignore_ascii_case("gpt-realtime-whisper")
}

pub fn openai_model_supports_languages(model: &str) -> bool {
    model.eq_ignore_ascii_case("gpt-live-transcribe")
        || model.eq_ignore_ascii_case("gpt-transcribe")
}

/// Trim, drop empties, and truncate at the API's 1024-char limit on a char
/// boundary. Truncation is logged so users notice their prompt was clipped.
pub fn clamp_prompt(prompt: Option<&str>) -> Option<String> {
    let trimmed = prompt.map(str::trim).filter(|s| !s.is_empty())?;
    let char_count = trimmed.chars().count();
    if char_count > PROMPT_MAX_CHARS {
        warn!(
            "openai-realtime: transcription prompt is {char_count} chars; \
             truncating to API limit of {PROMPT_MAX_CHARS}"
        );
        Some(trimmed.chars().take(PROMPT_MAX_CHARS).collect())
    } else {
        Some(trimmed.to_string())
    }
}

/// Resample 16kHz i16 samples to 24kHz i16 samples using linear interpolation.
pub fn resample_16k_to_24k(samples: &[i16]) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }

    let ratio = 24_000.0 / 16_000.0;
    let output_len = (samples.len() as f64 * ratio).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_pos = i as f64 / ratio;
        let src_idx = src_pos as usize;
        let frac = src_pos - src_idx as f64;

        let sample = if src_idx + 1 < samples.len() {
            let a = samples[src_idx] as f64;
            let b = samples[src_idx + 1] as f64;
            (a + frac * (b - a)) as i16
        } else if src_idx < samples.len() {
            samples[src_idx]
        } else {
            0
        };

        output.push(sample);
    }

    output
}

/// Encode i16 PCM samples to base64.
pub fn encode_pcm_base64(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_text_follows_the_delta_mode() {
        assert_eq!(
            OpenAiRealtimeProfile::OpenAi.stream_text(),
            StreamText::Fragments
        );
        assert_eq!(
            OpenAiRealtimeProfile::Lemonade.stream_text(),
            StreamText::Phrases
        );
        assert_eq!(
            OpenAiRealtimeProfile::LlamaCpp.stream_text(),
            StreamText::Fragments
        );
    }

    /// llama.cpp reads the client audio at the model's own rate unless
    /// `audio.input.format.rate` says otherwise, so the 16 kHz rate must be on
    /// the wire for the profile to hold for a model of any native rate.
    #[test]
    fn llamacpp_session_update_uses_the_flat_shape_and_declares_the_input_rate() {
        let json = OpenAiRealtimeProfile::LlamaCpp
            .session_update(
                "Voxtral-Mini-4B-Realtime",
                "auto",
                &[],
                Some("ignored"),
                TurnDetectionMode::ServerVad,
            )
            .unwrap();

        assert_eq!(json["type"], "session.update");
        assert_eq!(json["session"]["model"], "Voxtral-Mini-4B-Realtime");
        assert_eq!(json["session"]["turn_detection"]["type"], "server_vad");
        assert!(json["session"].get("language").is_none());
        assert!(json["session"].get("prompt").is_none());
        assert_eq!(
            json["session"]["audio"]["input"]["format"]["type"],
            "audio/pcm"
        );
        assert_eq!(json["session"]["audio"]["input"]["format"]["rate"], 16000);
    }

    #[test]
    fn openai_session_update_serialization() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-4o-mini-transcribe",
                "en",
                &[],
                None,
                TurnDetectionMode::ServerVad,
            )
            .unwrap();

        assert_eq!(json["type"], "session.update");
        assert_eq!(json["session"]["type"], "transcription");
        assert_eq!(
            json["session"]["audio"]["input"]["format"]["type"],
            "audio/pcm"
        );
        assert_eq!(json["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(
            json["session"]["audio"]["input"]["transcription"]["model"],
            "gpt-4o-mini-transcribe"
        );
        assert_eq!(
            json["session"]["audio"]["input"]["transcription"]["language"],
            "en"
        );
        let turn = &json["session"]["audio"]["input"]["turn_detection"];
        assert_eq!(turn["type"], "server_vad");
        assert_eq!(turn["threshold"], 0.5);
        assert_eq!(turn["prefix_padding_ms"], 300);
        assert_eq!(turn["silence_duration_ms"], 500);
    }

    /// Lemonade's session.update carries no language, prompt or audio block,
    /// and a turn detection only for server VAD.
    #[test]
    fn lemonade_session_update_never_carries_language_or_prompt() {
        for (name, language, turn_detection, want_turn_type) in [
            (
                "server vad",
                "auto",
                TurnDetectionMode::ServerVad,
                Some("server_vad"),
            ),
            ("manual commit", "en", TurnDetectionMode::ManualCommit, None),
        ] {
            let json = OpenAiRealtimeProfile::Lemonade
                .session_update(
                    "Whisper-Tiny",
                    language,
                    &[],
                    Some("ignored"),
                    turn_detection,
                )
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(json["type"], "session.update", "{name}");
            assert_eq!(json["session"]["model"], "Whisper-Tiny", "{name}");
            match want_turn_type {
                Some(t) => assert_eq!(json["session"]["turn_detection"]["type"], t, "{name}"),
                None => assert!(json["session"]["turn_detection"].is_null(), "{name}"),
            }
            assert!(json["session"].get("language").is_none(), "{name}");
            assert!(json["session"].get("prompt").is_none(), "{name}");
            assert!(json["session"].get("audio").is_none(), "{name}");
        }
    }

    /// The `audio.input` object of an OpenAI session.update.
    fn openai_input(
        model: &str,
        language: &str,
        languages: &[String],
        prompt: Option<&str>,
        turn_detection: TurnDetectionMode,
    ) -> anyhow::Result<serde_json::Value> {
        let json = OpenAiRealtimeProfile::OpenAi.session_update(
            model,
            language,
            languages,
            prompt,
            turn_detection,
        )?;
        Ok(json["session"]["audio"]["input"].clone())
    }

    #[test]
    fn realtime_whisper_session_update_omits_prompt_and_turn_detection() {
        let input = openai_input(
            "gpt-realtime-whisper",
            "en",
            &[],
            Some("domain prompt is unsupported here"),
            TurnDetectionMode::ManualCommit,
        )
        .unwrap();
        assert!(input["turn_detection"].is_null());
        assert!(input["transcription"].get("prompt").is_none());
    }

    /// The language goes out unless it is "auto", the prompt unless it is
    /// blank.
    #[test]
    fn openai_session_update_sends_language_and_prompt_only_when_set() {
        for (name, language, prompt, field, want) in [
            ("auto language", "auto", None, "language", None),
            (
                "prompt",
                "en",
                Some("Yocto, Hyprland, NixOS"),
                "prompt",
                Some("Yocto, Hyprland, NixOS"),
            ),
            ("blank prompt", "en", Some("   \t\n  "), "prompt", None),
        ] {
            let input = openai_input(
                "gpt-4o-transcribe",
                language,
                &[],
                prompt,
                TurnDetectionMode::ServerVad,
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                input["transcription"].get(field),
                want.map(serde_json::Value::from).as_ref(),
                "{name}"
            );
        }
    }

    #[test]
    fn clamp_prompt_truncates_on_a_char_boundary() {
        for (name, unit, extra) in [("ascii", "a", 500), ("multibyte", "字", 50)] {
            let clamped = clamp_prompt(Some(&unit.repeat(PROMPT_MAX_CHARS + extra)))
                .unwrap_or_else(|| panic!("{name}: no prompt"));
            assert_eq!(clamped.chars().count(), PROMPT_MAX_CHARS, "{name}");
        }
    }

    #[test]
    fn openai_turn_detection_uses_manual_commit_for_realtime_whisper() {
        assert_eq!(
            openai_turn_detection_mode_for_model("gpt-realtime-whisper"),
            TurnDetectionMode::ManualCommit
        );
        assert_eq!(
            openai_turn_detection_mode_for_model("gpt-4o-mini-transcribe"),
            TurnDetectionMode::ServerVad
        );
        assert_eq!(
            openai_turn_detection_mode_for_model("gpt-live-transcribe"),
            TurnDetectionMode::ManualCommit
        );
    }

    #[test]
    fn live_transcribe_sends_two_language_hints_and_prompt_without_server_vad() {
        let languages = vec!["ru".to_string(), "en".to_string()];
        let input = openai_input(
            "gpt-live-transcribe",
            "auto",
            &languages,
            Some("Russian speech may contain English technical terms."),
            openai_turn_detection_mode_for_model("gpt-live-transcribe"),
        )
        .unwrap();
        assert!(input.get("turn_detection").is_none());
        assert_eq!(
            input["transcription"]["languages"],
            serde_json::json!(["ru", "en"])
        );
        assert!(input["transcription"].get("language").is_none());
        assert_eq!(
            input["transcription"]["prompt"],
            "Russian speech may contain English technical terms."
        );
    }

    #[test]
    fn live_transcribe_uses_languages_for_single_language_too() {
        let input = openai_input(
            "gpt-live-transcribe",
            "ru",
            &[],
            None,
            TurnDetectionMode::ManualCommit,
        )
        .unwrap();
        let transcription = &input["transcription"];
        assert_eq!(transcription["languages"], serde_json::json!(["ru"]));
        assert!(transcription.get("language").is_none());
    }

    #[test]
    fn resample_empty() {
        let result = resample_16k_to_24k(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn encode_pcm_base64_roundtrip() {
        let samples: Vec<i16> = vec![1, 2, 3, -1];
        let encoded = encode_pcm_base64(&samples);

        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let decoded_samples: Vec<i16> = decoded_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        assert_eq!(decoded_samples, samples);
    }
}
