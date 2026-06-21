use base64::Engine;
use tracing::warn;

use super::wire::{LemonadeSessionUpdate, OpenAiSessionUpdate};

/// OpenAI Realtime API rejects transcription prompts longer than this.
pub const PROMPT_MAX_CHARS: usize = 1024;

/// Supported OpenAI-compatible realtime protocol profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeProfile {
    OpenAi,
    Lemonade,
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
            other => anyhow::bail!(
                "unsupported OpenAI-compatible realtime profile '{other}' (supported: openai, lemonade)"
            ),
        }
    }

    pub fn input_sample_rate(self) -> u32 {
        match self {
            Self::OpenAi => 24_000,
            Self::Lemonade => 16_000,
        }
    }

    pub fn delta_mode(self) -> DeltaMode {
        match self {
            Self::OpenAi => DeltaMode::AppendOnly,
            Self::Lemonade => DeltaMode::ReplaceableInterim,
        }
    }

    pub fn session_update(
        self,
        model: &str,
        language: &str,
        prompt: Option<&str>,
        turn_detection: TurnDetectionMode,
    ) -> anyhow::Result<serde_json::Value> {
        let value = match self {
            Self::OpenAi => serde_json::to_value(OpenAiSessionUpdate::new(
                model,
                language,
                prompt,
                turn_detection,
            ))?,
            Self::Lemonade => {
                serde_json::to_value(LemonadeSessionUpdate::new(model, turn_detection))?
            }
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
    if model.eq_ignore_ascii_case("gpt-realtime-whisper") {
        TurnDetectionMode::ManualCommit
    } else {
        TurnDetectionMode::ServerVad
    }
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
    fn parse_profile() {
        assert_eq!(
            OpenAiRealtimeProfile::parse("openai").unwrap(),
            OpenAiRealtimeProfile::OpenAi
        );
        assert_eq!(
            OpenAiRealtimeProfile::parse("lemonade").unwrap(),
            OpenAiRealtimeProfile::Lemonade
        );
        assert!(OpenAiRealtimeProfile::parse("unknown").is_err());
    }

    #[test]
    fn parse_turn_detection_mode() {
        assert_eq!(
            TurnDetectionMode::parse("server-vad").unwrap(),
            TurnDetectionMode::ServerVad
        );
        assert_eq!(
            TurnDetectionMode::parse("manual-commit").unwrap(),
            TurnDetectionMode::ManualCommit
        );
        assert!(TurnDetectionMode::parse("unknown").is_err());
    }

    #[test]
    fn profile_sample_rates_and_delta_modes() {
        assert_eq!(OpenAiRealtimeProfile::OpenAi.input_sample_rate(), 24_000);
        assert_eq!(OpenAiRealtimeProfile::Lemonade.input_sample_rate(), 16_000);
        assert_eq!(
            OpenAiRealtimeProfile::OpenAi.delta_mode(),
            DeltaMode::AppendOnly
        );
        assert_eq!(
            OpenAiRealtimeProfile::Lemonade.delta_mode(),
            DeltaMode::ReplaceableInterim
        );
    }

    #[test]
    fn openai_session_update_serialization() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-4o-mini-transcribe",
                "en",
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

    #[test]
    fn lemonade_session_update_serialization() {
        let json = OpenAiRealtimeProfile::Lemonade
            .session_update(
                "Whisper-Tiny",
                "auto",
                Some("ignored"),
                TurnDetectionMode::ServerVad,
            )
            .unwrap();

        assert_eq!(json["type"], "session.update");
        assert_eq!(json["session"]["model"], "Whisper-Tiny");
        assert_eq!(json["session"]["turn_detection"]["type"], "server_vad");
        assert!(json["session"].get("language").is_none());
        assert!(json["session"].get("prompt").is_none());
    }

    #[test]
    fn lemonade_manual_commit_omits_turn_detection() {
        let json = OpenAiRealtimeProfile::Lemonade
            .session_update(
                "Whisper-Tiny",
                "en",
                Some("ignored"),
                TurnDetectionMode::ManualCommit,
            )
            .unwrap();

        assert!(json["session"]["turn_detection"].is_null());
        assert!(json["session"].get("language").is_none());
        assert!(json["session"].get("prompt").is_none());
    }

    #[test]
    fn manual_commit_session_update_omits_prompt_and_turn_detection() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-realtime-whisper",
                "en",
                Some("domain prompt is unsupported here"),
                TurnDetectionMode::ManualCommit,
            )
            .unwrap();
        assert!(json["session"]["audio"]["input"]["turn_detection"].is_null());
        assert!(json["session"]["audio"]["input"]["transcription"]
            .get("prompt")
            .is_none());
    }

    #[test]
    fn session_update_auto_language_omitted() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-4o-transcribe",
                "auto",
                None,
                TurnDetectionMode::ServerVad,
            )
            .unwrap();

        assert!(json["session"]["audio"]["input"]["transcription"]
            .get("language")
            .is_none());
    }

    #[test]
    fn session_update_with_prompt_includes_field() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-4o-transcribe",
                "en",
                Some("Yocto, Hyprland, NixOS"),
                TurnDetectionMode::ServerVad,
            )
            .unwrap();
        assert_eq!(
            json["session"]["audio"]["input"]["transcription"]["prompt"],
            "Yocto, Hyprland, NixOS"
        );
    }

    #[test]
    fn session_update_blank_prompt_omits_field() {
        let json = OpenAiRealtimeProfile::OpenAi
            .session_update(
                "gpt-4o-transcribe",
                "en",
                Some("   \t\n  "),
                TurnDetectionMode::ServerVad,
            )
            .unwrap();
        assert!(json["session"]["audio"]["input"]["transcription"]
            .get("prompt")
            .is_none());
    }

    #[test]
    fn clamp_prompt_truncates_at_limit() {
        let long = "a".repeat(PROMPT_MAX_CHARS + 500);
        let clamped = clamp_prompt(Some(&long)).unwrap();
        assert_eq!(clamped.chars().count(), PROMPT_MAX_CHARS);
    }

    #[test]
    fn clamp_prompt_handles_multibyte_at_boundary() {
        let long: String = "字".repeat(PROMPT_MAX_CHARS + 50);
        let clamped = clamp_prompt(Some(&long)).unwrap();
        assert_eq!(clamped.chars().count(), PROMPT_MAX_CHARS);
        assert!(clamped.is_char_boundary(0));
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
    }

    #[test]
    fn resample_empty() {
        let result = resample_16k_to_24k(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn resample_ratio() {
        let input: Vec<i16> = vec![100; 16_000];
        let output = resample_16k_to_24k(&input);
        assert!(
            (output.len() as i64 - 24_000).abs() <= 2,
            "expected ~24000, got {}",
            output.len()
        );
    }

    #[test]
    fn lemonade_uses_16k_audio_directly() {
        let input: Vec<i16> = vec![7; 160];
        assert_eq!(OpenAiRealtimeProfile::Lemonade.input_sample_rate(), 16_000);
        assert_eq!(input.len(), 160);
    }

    #[test]
    fn encode_pcm_base64_roundtrip() {
        let samples: Vec<i16> = vec![1, 2, 3, -1];
        let encoded = encode_pcm_base64(&samples);

        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let decoded_samples: Vec<i16> = decoded_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(decoded_samples, samples);
    }
}
