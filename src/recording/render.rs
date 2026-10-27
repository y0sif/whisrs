//! Rendering a processed recording as Markdown or plain text.
//!
//! Markdown is the `<id>.md` written next to `recording.json`: flat YAML front
//! matter (readable by Obsidian and other note tools), the summary when the
//! recording has one, then the transcript as speaker turns. Plain text is what `whisrs recording copy`
//! puts on the clipboard.

use chrono::{DateTime, Local};

use super::{RecordingMeta, TrackKind, TrackMeta, TrackQuality};

/// Consecutive segments from one speaker, merged.
pub(crate) struct Turn<'a> {
    pub(crate) start_ms: u64,
    /// A [`super::Speaker::id`].
    pub(crate) speaker: &'a str,
    pub(crate) text: String,
}

/// Merge consecutive same-speaker segments into turns.
pub(crate) fn turns(meta: &RecordingMeta) -> Vec<Turn<'_>> {
    let mut out: Vec<Turn<'_>> = Vec::new();
    for seg in &meta.segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(turn) if turn.speaker == seg.speaker => {
                turn.text.push(' ');
                turn.text.push_str(text);
            }
            _ => out.push(Turn {
                start_ms: seg.start_ms,
                speaker: &seg.speaker,
                text: text.to_string(),
            }),
        }
    }
    out
}

/// Longest line of [`capped_lines`], in characters.
pub(crate) const LINE_CHARS: usize = 1_500;

/// A line of the transcript as the summary and `whisrs mcp` give it ([`capped_lines`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line<'a> {
    /// When the segment its first words come from starts.
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
    /// A [`super::Speaker::id`].
    pub(crate) speaker: &'a str,
    /// On one line: whitespace runs made one space.
    pub(crate) text: String,
}

/// The transcript as lines of at most `max_chars` characters: one speaker's
/// consecutive segments merged until the next would not fit, a segment
/// longer than that split at spaces (a single longer word stays whole), and
/// each line stamped with the start of its first segment. Unlike
/// [`turns`], a long monologue (a lecture) keeps a timestamp every
/// `max_chars` or so.
pub(crate) fn capped_lines(meta: &RecordingMeta, max_chars: usize) -> Vec<Line<'_>> {
    let mut out: Vec<Line<'_>> = Vec::new();
    // The length of each line of `out`, in characters.
    let mut lens: Vec<usize> = Vec::new();
    for seg in &meta.segments {
        let text = seg.text.split_whitespace().collect::<Vec<_>>().join(" ");
        for piece in split_at_spaces(&text, max_chars) {
            let piece_len = piece.chars().count();
            match (out.last_mut(), lens.last_mut()) {
                (Some(line), Some(len))
                    if line.speaker == seg.speaker && *len + 1 + piece_len <= max_chars =>
                {
                    line.text.push(' ');
                    line.text.push_str(&piece);
                    line.end_ms = line.end_ms.max(seg.end_ms);
                    *len += 1 + piece_len;
                }
                _ => {
                    out.push(Line {
                        start_ms: seg.start_ms,
                        end_ms: seg.end_ms,
                        speaker: &seg.speaker,
                        text: piece,
                    });
                    lens.push(piece_len);
                }
            }
        }
    }
    out
}

/// `text` (on one line) in pieces of at most `max` characters, split at
/// spaces; a longer word is a piece of its own. Empty text has none.
fn split_at_spaces(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut len = 0usize;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let word_len = word.chars().count();
        if len > 0 && len + 1 + word_len > max {
            out.push(std::mem::take(&mut piece));
            len = 0;
        }
        if len > 0 {
            piece.push(' ');
            len += 1;
        }
        piece.push_str(word);
        len += word_len;
    }
    if !piece.is_empty() {
        out.push(piece);
    }
    out
}

/// Display name for a speaker id, falling back to the id itself.
pub(crate) fn speaker_name<'a>(meta: &'a RecordingMeta, id: &'a str) -> &'a str {
    meta.speakers
        .iter()
        .find(|s| s.id == id)
        .map_or(id, |s| s.name.as_str())
}

/// `HH:MM:SS` for a millisecond offset.
pub(crate) fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// A double-quoted YAML scalar. Double quotes are the one YAML style where
/// every character can be expressed, so titles with `:`, `#`, quotes or
/// newlines cannot break the front matter.
fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The recording title, or a dated default.
pub(crate) fn title(meta: &RecordingMeta) -> String {
    title_or_default(meta.title.as_deref(), meta.started_at)
}

/// `title`, else `Recording YYYY-MM-DD HH:MM` for `started_at`.
pub(crate) fn title_or_default(title: Option<&str>, started_at: DateTime<Local>) -> String {
    title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Recording {}", started_at.format("%Y-%m-%d %H:%M")))
}

/// Recording length: `ended_at - started_at`, else the longest track.
pub(crate) fn duration_ms(meta: &RecordingMeta) -> u64 {
    duration_ms_of(
        meta.started_at,
        meta.ended_at,
        meta.sample_rate,
        &meta.tracks,
    )
}

/// [`duration_ms`] from the fields it reads.
pub(crate) fn duration_ms_of(
    started_at: DateTime<Local>,
    ended_at: Option<DateTime<Local>>,
    sample_rate: u32,
    tracks: &[TrackMeta],
) -> u64 {
    if let Some(ended) = ended_at {
        let ms = (ended - started_at).num_milliseconds();
        if ms > 0 {
            return ms as u64;
        }
    }
    let rate = u64::from(sample_rate.max(1));
    tracks
        .iter()
        .map(|t| t.offset_ms.max(0) as u64 + t.samples * 1000 / rate)
        .max()
        .unwrap_or(0)
}

/// The front matter's languages, flat for note tools: `language` when every
/// recorded track was transcribed in the same one, else `mic_language` and
/// `system_language`. Empty when no track has a stored language.
fn language_lines(meta: &RecordingMeta) -> String {
    let languages: Vec<(TrackKind, &str)> = meta
        .tracks
        .iter()
        .filter_map(|t| meta.language(t.kind).map(|l| (t.kind, l)))
        .collect();
    match languages.as_slice() {
        [] => String::new(),
        [(_, first), rest @ ..] if rest.iter().all(|(_, l)| l == first) => {
            format!("language: {}\n", yaml_str(first))
        }
        all => all
            .iter()
            .map(|(kind, l)| format!("{}_language: {}\n", kind.file_stem(), yaml_str(l)))
            .collect(),
    }
}

/// The front matter's `audio` block: the levels of each track with a
/// quality report (whole dB, `null` for a level not measured), then
/// `audio_warnings` when any track warned. Empty when no track has a report,
/// so a recording made before tracks were measured renders as it did.
fn audio_lines(meta: &RecordingMeta) -> String {
    let measured: Vec<(TrackKind, &TrackQuality)> = meta
        .tracks
        .iter()
        .filter_map(|t| t.quality.as_ref().map(|q| (t.kind, q)))
        .collect();
    if measured.is_empty() {
        return String::new();
    }
    let db = |v: Option<f32>| v.map_or_else(|| "null".to_string(), |v| format!("{v:.0}"));
    let mut out = String::from("audio:\n");
    for (kind, quality) in &measured {
        out.push_str(&format!(
            "  {}: {{speech_dbfs: {}, noise_dbfs: {}, snr_db: {}}}\n",
            kind.file_stem(),
            db(quality.speech_dbfs),
            db(quality.noise_dbfs),
            db(quality.snr_db)
        ));
    }
    let warnings: Vec<String> = measured
        .iter()
        .flat_map(|(kind, quality)| {
            quality
                .verdicts()
                .into_iter()
                .map(move |v| format!("{}: {}", kind.file_stem(), v.detail()))
        })
        .collect();
    if !warnings.is_empty() {
        out.push_str("audio_warnings:\n");
        for warning in warnings {
            out.push_str(&format!("  - {}\n", yaml_str(&warning)));
        }
    }
    out
}

/// Render the recording as Markdown with YAML front matter.
pub fn render_markdown(meta: &RecordingMeta) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_str(&title(meta))));
    out.push_str(&format!(
        "date: {}\n",
        yaml_str(&meta.started_at.format("%Y-%m-%d").to_string())
    ));
    out.push_str(&format!(
        "start: {}\n",
        yaml_str(&meta.started_at.format("%H:%M").to_string())
    ));
    out.push_str(&format!(
        "duration: {}\n",
        yaml_str(&clock(duration_ms(meta)))
    ));
    // The speakers heard in the transcript: a track's speaker with no line
    // (silent system audio) stays in recording.json only, where `recording
    // name` still finds it.
    let heard: Vec<&str> = meta
        .speakers
        .iter()
        .filter(|s| meta.segments.iter().any(|seg| seg.speaker == s.id))
        .map(|s| s.name.as_str())
        .collect();
    if heard.is_empty() {
        out.push_str("speakers: []\n");
    } else {
        out.push_str("speakers:\n");
        for name in heard {
            out.push_str(&format!("  - {}\n", yaml_str(name)));
        }
    }
    out.push_str("tags: [recording]\n");
    out.push_str(&format!("whisrs_id: {}\n", yaml_str(&meta.id)));
    if let Some(backend) = &meta.backend {
        out.push_str(&format!("backend: {}\n", yaml_str(backend)));
    }
    if let Some(model) = &meta.model {
        out.push_str(&format!("model: {}\n", yaml_str(model)));
    }
    out.push_str(&audio_lines(meta));
    out.push_str(&language_lines(meta));
    out.push_str("---\n\n");
    if let Some(summary) = meta
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("## Summary\n\n");
        if meta.summary_stale {
            out.push_str(&format!(
                "_This summary was made before the transcript or the speaker names last \
                 changed. `whisrs recording summarize {}` makes a new one._\n\n",
                meta.id
            ));
        }
        out.push_str(&format!("{summary}\n\n"));
    }
    out.push_str("## Transcript\n\n");

    let turns = turns(meta);
    if turns.is_empty() {
        out.push_str("_No speech was transcribed._\n");
    }
    for turn in turns {
        out.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            clock(turn.start_ms),
            speaker_name(meta, turn.speaker),
            turn.text
        ));
    }
    // Non-fatal recording notes, e.g. system audio that could not be opened.
    if !meta.errors.is_empty() {
        out.push_str("## Notes\n\n");
        for note in &meta.errors {
            out.push_str(&format!("- {}\n", note.trim()));
        }
        out.push('\n');
    }
    if out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Render the transcript as plain `[HH:MM:SS] Name: text` lines.
pub fn render_text(meta: &RecordingMeta) -> String {
    turns(meta)
        .into_iter()
        .map(|turn| {
            format!(
                "[{}] {}: {}\n",
                clock(turn.start_ms),
                speaker_name(meta, turn.speaker),
                turn.text
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{Speaker, TrackKind, TranscriptSegment};
    use chrono::{Local, TimeZone};

    fn seg(start_ms: u64, speaker: &str, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms: start_ms + 1000,
            speaker: speaker.to_string(),
            channel: if speaker == "me" {
                TrackKind::Mic
            } else {
                TrackKind::System
            },
            text: text.to_string(),
        }
    }

    fn speaker(id: &str, name: &str, source: &str, channel: TrackKind) -> Speaker {
        Speaker {
            id: id.into(),
            name: name.into(),
            source: source.into(),
            evidence: None,
            talk_ms: 0,
            channel: Some(channel),
            minor: false,
        }
    }

    fn meta() -> RecordingMeta {
        let started = Local.with_ymd_and_hms(2026, 10, 2, 14, 5, 0).unwrap();
        let mut meta = RecordingMeta::new(
            "2026-10-02_1405_standup".into(),
            Some("Standup: \"Q4\" #plan".into()),
            started,
        );
        meta.ended_at = Some(started + chrono::Duration::seconds(3725));
        meta.backend = Some("groq".into());
        meta.model = Some("whisper-large-v3-turbo".into());
        meta.tracks = [TrackKind::Mic, TrackKind::System]
            .into_iter()
            .map(|kind| crate::recording::TrackMeta {
                kind,
                file: format!("audio/{}.wav", kind.file_stem()),
                device: "default".into(),
                offset_ms: 0,
                samples: 0,
                quality: None,
            })
            .collect();
        meta.mic_language = Some("de".into());
        meta.system_language = Some("de".into());
        meta.speakers = vec![
            speaker("me", "Yosif", "self", TrackKind::Mic),
            speaker("others", "Others", "channel", TrackKind::System),
        ];
        meta.segments = vec![
            seg(1_000, "me", "Hi all."),
            seg(3_000, "me", "Quick update."),
            seg(65_000, "others", "Sounds good."),
            seg(3_700_000, "me", "Bye."),
        ];
        meta
    }

    #[test]
    fn markdown_has_front_matter_and_merged_turns() {
        let md = render_markdown(&meta());
        let expected = "---\n\
            title: \"Standup: \\\"Q4\\\" #plan\"\n\
            date: \"2026-10-02\"\n\
            start: \"14:05\"\n\
            duration: \"01:02:05\"\n\
            speakers:\n  - \"Yosif\"\n  - \"Others\"\n\
            tags: [recording]\n\
            whisrs_id: \"2026-10-02_1405_standup\"\n\
            backend: \"groq\"\n\
            model: \"whisper-large-v3-turbo\"\n\
            language: \"de\"\n\
            ---\n\n## Transcript\n\n\
            **[00:00:01] Yosif:** Hi all. Quick update.\n\n\
            **[00:01:05] Others:** Sounds good.\n\n\
            **[01:01:40] Yosif:** Bye.\n";
        assert_eq!(md, expected);
    }

    /// A summary goes between the front matter and the transcript; the
    /// front matter gains nothing but the title.
    #[test]
    fn the_summary_comes_before_the_transcript() {
        let mut m = meta();
        let without = render_markdown(&m);
        m.summary =
            Some("  Quick update, then goodbye.\n\n### Next steps\n- [ ] Yosif: ship it\n".into());
        let md = render_markdown(&m);
        assert!(
            md.contains(
                "language: \"de\"\n---\n\n## Summary\n\nQuick update, then goodbye.\n\n\
                 ### Next steps\n- [ ] Yosif: ship it\n\n## Transcript\n\n**[00:00:01] Yosif:**"
            ),
            "{md}"
        );
        let front = |md: &str| md[..md.find("---\n\n").unwrap()].to_string();
        assert_eq!(front(&md), front(&without));
        m.summary = Some("  ".into());
        assert_eq!(render_markdown(&m), without, "a blank summary is none");
    }

    fn quality(speech: Option<f32>, noise: Option<f32>, warnings: &[&str]) -> TrackQuality {
        TrackQuality {
            vad: "earshot".into(),
            speech_dbfs: speech,
            noise_dbfs: noise,
            snr_db: speech.zip(noise).map(|(s, n)| s - n),
            clipped: 0.0,
            dc_offset: 0.0,
            speech_share: 0.3,
            echo_path_db: None,
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
        }
    }

    /// A recording made before tracks were measured renders byte for byte
    /// as it did: the expected text is the one
    /// `markdown_has_front_matter_and_merged_turns` pins, from a meta read
    /// from a `recording.json` with no `quality`.
    #[test]
    fn a_recording_without_quality_renders_as_before() {
        let json = serde_json::to_string(&meta()).unwrap();
        assert!(!json.contains("quality"), "{json}");
        let old: RecordingMeta = serde_json::from_str(&json).unwrap();
        assert!(old.tracks.iter().all(|t| t.quality.is_none()));
        assert_eq!(render_markdown(&old), render_markdown(&meta()));
        assert!(!render_markdown(&old).contains("audio"));
    }

    /// Each measured track gets a line of levels after the model; the
    /// warnings follow only when a track warned.
    #[test]
    fn front_matter_has_an_audio_block_per_measured_track() {
        let mut m = meta();
        m.tracks[0].quality = Some(quality(Some(-41.3), Some(-62.0), &[]));
        m.tracks[1].quality = Some(quality(Some(-24.0), None, &[]));
        let md = render_markdown(&m);
        assert!(
            md.contains(
                "model: \"whisper-large-v3-turbo\"\n\
                 audio:\n  \
                 mic: {speech_dbfs: -41, noise_dbfs: -62, snr_db: 21}\n  \
                 system: {speech_dbfs: -24, noise_dbfs: null, snr_db: null}\n\
                 language: \"de\"\n---\n"
            ),
            "{md}"
        );
        assert!(!md.contains("audio_warnings"), "{md}");

        m.tracks[0].quality = Some(quality(Some(-41.0), Some(-48.0), &["noisy", "dc-offset"]));
        m.tracks[0].quality.as_mut().unwrap().dc_offset = 0.02;
        m.tracks[1].quality = Some(quality(None, None, &["system-silent"]));
        let md = render_markdown(&m);
        assert!(
            md.contains(
                "  system: {speech_dbfs: null, noise_dbfs: null, snr_db: null}\n\
                 audio_warnings:\n  \
                 - \"mic: noisy (speech 7 dB above the background)\"\n  \
                 - \"mic: DC offset (2%)\"\n  \
                 - \"system: system silent (nothing played while the mic heard speech)\"\n\
                 language:"
            ),
            "{md}"
        );
        // A track without a report has no line.
        m.tracks[1].quality = None;
        assert!(!render_markdown(&m).contains("  system:"));
    }

    #[test]
    fn text_is_one_line_per_turn() {
        assert_eq!(
            render_text(&meta()),
            "[00:00:01] Yosif: Hi all. Quick update.\n\
             [00:01:05] Others: Sounds good.\n\
             [01:01:40] Yosif: Bye.\n"
        );
    }

    #[test]
    fn yaml_strings_escape_everything_risky() {
        assert_eq!(yaml_str("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        assert_eq!(yaml_str("x\u{7}"), r#""x\u0007""#);
        assert_eq!(yaml_str("key: value # c"), r#""key: value # c""#);
    }

    #[test]
    fn untitled_empty_recording_still_renders() {
        let mut m = meta();
        m.title = None;
        m.segments.clear();
        m.backend = None;
        m.model = None;
        m.mic_language = None;
        m.system_language = None;
        let md = render_markdown(&m);
        assert!(md.contains("title: \"Recording 2026-10-02 14:05\"\n"));
        assert!(md.contains("_No speech was transcribed._"));
        assert!(!md.contains("backend:"));
        assert!(!md.contains("model:"));
        assert!(!md.contains("language:"));
        assert_eq!(render_text(&m), "");
    }

    /// The front matter between `backend` and its end, where the model and
    /// the languages go.
    fn model_and_languages(meta: &RecordingMeta) -> String {
        let md = render_markdown(meta);
        let start = md.find("backend:").unwrap();
        let end = md.find("---\n\n##").unwrap();
        md[start..end]
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One `language` when the recorded tracks agree, one per track when
    /// they do not; a track that was not recorded does not count.
    #[test]
    fn front_matter_names_the_model_and_languages() {
        let mut m = meta();
        assert_eq!(
            model_and_languages(&m),
            "model: \"whisper-large-v3-turbo\"\nlanguage: \"de\""
        );
        m.system_language = Some("auto".into());
        assert_eq!(
            model_and_languages(&m),
            "model: \"whisper-large-v3-turbo\"\nmic_language: \"de\"\n\
             system_language: \"auto\""
        );
        // Mic only (`system_device = "none"`): its language is the language.
        m.tracks.retain(|t| t.kind == TrackKind::Mic);
        assert_eq!(
            model_and_languages(&m),
            "model: \"whisper-large-v3-turbo\"\nlanguage: \"de\""
        );
    }

    #[test]
    fn the_system_track_takes_the_others_label() {
        let mut m = meta();
        m.speakers[1].name = "Video".into();
        let md = render_markdown(&m);
        assert!(md.contains("  - \"Video\"\n"), "{md}");
        assert!(md.contains("**[00:01:05] Video:** Sounds good."), "{md}");
        assert!(render_text(&m).contains("[00:01:05] Video: Sounds good.\n"));
    }

    /// Separated voices render by name: two voices on the system track,
    /// one of them unnamed, in turns of their own, and in the front matter
    /// as a flat list.
    #[test]
    fn separated_voices_render_by_name() {
        let mut m = meta();
        m.speakers = vec![
            speaker("me", "Yosif", "self", TrackKind::Mic),
            speaker("S1", "Speaker 1", "none", TrackKind::System),
            speaker("S2", "Speaker 2", "none", TrackKind::System),
        ];
        m.segments = vec![
            seg(1_000, "me", "Hi all."),
            seg(5_000, "S1", "Hello."),
            seg(6_000, "S1", "Good to see you."),
            seg(9_000, "S2", "Morning."),
        ];
        let md = render_markdown(&m);
        assert!(
            md.contains("speakers:\n  - \"Yosif\"\n  - \"Speaker 1\"\n  - \"Speaker 2\"\n"),
            "{md}"
        );
        assert!(
            md.contains(
                "**[00:00:05] Speaker 1:** Hello. Good to see you.\n\n\
                 **[00:00:09] Speaker 2:** Morning.\n"
            ),
            "{md}"
        );
        assert_eq!(
            render_text(&m),
            "[00:00:01] Yosif: Hi all.\n\
             [00:00:05] Speaker 1: Hello. Good to see you.\n\
             [00:00:09] Speaker 2: Morning.\n"
        );
    }

    /// The front matter lists the speakers that have a line, whatever
    /// their talk time (an older file has none); the others stay in
    /// recording.json.
    #[test]
    fn front_matter_lists_only_speakers_heard() {
        let mut m = meta();
        m.speakers
            .push(speaker("S9", "Speaker 9", "none", TrackKind::System));
        m.segments.retain(|s| s.speaker == "me");
        let md = render_markdown(&m);
        assert!(md.contains("speakers:\n  - \"Yosif\"\ntags:"), "{md}");
        assert_eq!(m.speakers.len(), 3, "kept in the meta");
        m.segments.clear();
        assert!(render_markdown(&m).contains("speakers: []\n"));
    }

    /// A summary from an earlier transcript says so, with the command
    /// that makes a new one.
    #[test]
    fn a_stale_summary_says_so() {
        let mut m = meta();
        m.summary = Some("Old.".into());
        m.summary_stale = true;
        assert!(
            render_markdown(&m).contains(
                "## Summary\n\n_This summary was made before the transcript or the speaker \
                 names last changed. `whisrs recording summarize 2026-10-02_1405_standup` makes \
                 a new one._\n\nOld.\n\n## Transcript"
            ),
            "{}",
            render_markdown(&m)
        );
        m.summary_stale = false;
        assert!(!render_markdown(&m).contains("last changed"));
    }

    /// One voice for two hours stays many lines, each with its own time and
    /// at most the cap; a new speaker starts a new line; a segment longer
    /// than the cap is split at spaces; no word is lost or reordered.
    #[test]
    fn capped_lines_keep_a_timestamp_through_a_monologue() {
        let mut m = meta();
        m.speakers = vec![speaker("others", "Video", "channel", TrackKind::System)];
        m.segments = (0..1200)
            .map(|i| {
                seg(
                    i * 6_000,
                    "others",
                    &format!("Sentence {i} of the lecture,\n  about verbs."),
                )
            })
            .collect();
        let lines = capped_lines(&m, LINE_CHARS);
        assert!(lines.len() > 30, "{}", lines.len());
        assert_eq!(lines[0].start_ms, 0);
        assert!(lines.windows(2).all(|w| w[0].start_ms < w[1].start_ms));
        assert!(lines.iter().all(|l| l.text.chars().count() <= LINE_CHARS));
        assert!(
            lines[1].text.starts_with("Sentence "),
            "{:?}",
            lines[1].text
        );
        let joined: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
        let want: Vec<String> = (0..1200)
            .map(|i| format!("Sentence {i} of the lecture, about verbs."))
            .collect();
        assert_eq!(joined.join(" "), want.join(" "));

        m.speakers
            .push(speaker("me", "Yosif", "self", TrackKind::Mic));
        m.segments = vec![
            seg(0, "others", &"word ".repeat(40)),
            seg(1_000, "others", "short"),
            seg(2_000, "me", "mine"),
            seg(3_000, "others", "back"),
        ];
        let lines = capped_lines(&m, 50);
        let shown: Vec<(u64, &str, usize)> = lines
            .iter()
            .map(|l| (l.start_ms, l.speaker, l.text.chars().count()))
            .collect();
        assert_eq!(
            shown,
            [
                (0, "others", 49),
                (0, "others", 49),
                (0, "others", 49),
                (0, "others", 49),
                (1_000, "others", 5),
                (2_000, "me", 4),
                (3_000, "others", 4),
            ]
        );
    }

    #[test]
    fn duration_falls_back_to_tracks() {
        let mut m = meta();
        m.ended_at = None;
        m.tracks.push(crate::recording::TrackMeta {
            kind: TrackKind::Mic,
            file: "audio/mic.wav".into(),
            device: "default".into(),
            offset_ms: 500,
            samples: 16_000 * 90,
            quality: None,
        });
        assert_eq!(duration_ms(&m), 90_500);
    }
}
