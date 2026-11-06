//! Speakers: who said each line of a recording.
//!
//! Without speaker separation each track is one speaker: the mic is the user
//! (`me`) and the system audio is everyone else (`others`, named by the
//! recording's others label). With it ([`plan`]), the voices on the system
//! track, and with `shared_mic` those on the mic, are told apart. Every word
//! takes a voice label, from the backend or from the turns of a diarization
//! sidecar ([`super::diarize`], aligned by [`speaker_at`]), a segment splits
//! where its words change voice ([`split_segment`]), and once both tracks
//! are transcribed the voices become speakers, numbered by first appearance
//! ([`assign`]).
//!
//! Nothing is guessed: a voice is named only when the track says who it is
//! (a single remote voice keeps the others label, a single mic voice is the
//! user); every other voice is `Speaker N`.

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Segment, Speaker, TrackKind, Word};
use crate::transcription::deepgram::{supports_diarization, DIARIZATION_LABEL};
use crate::transcription::factory::canonical_backend_name;
use crate::transcription::verbose_json::join_words;
use crate::{Config, DiarizationMode, RecordingConfig};

/// A voice with less talk time than this ...
pub const MINOR_TALK_MS: u64 = 5_000;

/// ... and less than this share of its track's talk time, in percent, is
/// minor ([`is_minor`]).
pub const MINOR_SHARE_PERCENT: u64 = 2;

/// Id of the user when the mic is one speaker.
pub const ME: &str = "me";

/// Id of the system track when it is one speaker, and of its speech that no
/// voice was found for.
pub const OTHERS: &str = "others";

/// Id of a shared mic's speech that no voice was found for.
pub const MIC: &str = "mic";

/// Name of [`MIC`].
pub const MIC_LABEL: &str = "Mic";

/// [`Speaker::source`] of the user.
pub const SOURCE_SELF: &str = "self";

/// [`Speaker::source`] of a speaker named by its track's label.
pub const SOURCE_CHANNEL: &str = "channel";

/// [`Speaker::source`] of an unnamed speaker, shown as `Speaker N`.
pub const SOURCE_NONE: &str = "none";

/// A word this far from every turn of a diarization sidecar, or nearer,
/// takes the nearest one ([`speaker_at`]).
pub const NEAREST_TURN_MS: u64 = 1000;

/// Start of the note `recording.json` keeps in `errors` when the voices
/// could not be told apart ([`separation_failure`]).
pub const SEPARATION_FAILED: &str = "speakers were not separated: ";

/// One stretch of one voice, from a diarization sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub start_ms: u64,
    pub end_ms: u64,
    /// The sidecar's label, consistent within one track.
    pub speaker: String,
}

/// How the voices of a recording are told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// They are not: one speaker per track.
    Off,
    /// The transcription backend labels each word or segment with a voice.
    /// `label` is what `recording.json` records as `diarization`; `request`
    /// is whether the request has to ask for it
    /// ([`crate::transcription::TranscriptionConfig::diarize`]).
    Backend { label: String, request: bool },
    /// A diarization sidecar at `url` (`[recording] diarization_url`) finds
    /// the turns of each voice, and the words are aligned to them. `label`
    /// is `sidecar:<host>`.
    Sidecar { url: String, label: String },
}

impl Method {
    /// What `recording.json` records as `diarization`.
    pub fn label(&self) -> &str {
        match self {
            Method::Off => "off",
            Method::Backend { label, .. } | Method::Sidecar { label, .. } => label,
        }
    }

    /// The sidecar method for `url`, labelled by its host (and port).
    pub fn sidecar(url: &str) -> Method {
        let host = reqwest::Url::parse(url).ok().and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        });
        Method::Sidecar {
            url: url.to_string(),
            label: match host {
                Some(host) => format!("sidecar:{host}"),
                None => "sidecar".to_string(),
            },
        }
    }
}

/// How one recording's speakers are found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerPlan {
    pub method: Method,
    /// The mic's voices are told apart too.
    pub shared_mic: bool,
}

impl SpeakerPlan {
    /// Whether the voices of track `kind` are told apart: the system track
    /// whenever speakers are separated, the mic only when it is shared.
    pub fn separates(&self, kind: TrackKind) -> bool {
        self.method != Method::Off && (kind == TrackKind::System || self.shared_mic)
    }

    /// Whether the transcription request of track `kind` asks the backend
    /// for speakers.
    pub fn requests_speakers(&self, kind: TrackKind) -> bool {
        self.separates(kind) && matches!(self.method, Method::Backend { request: true, .. })
    }
}

/// How backend `name` labels speakers itself, or `None` when it does not:
/// Deepgram when asked ([`DIARIZATION_LABEL`]: `diarize_model=latest`, on
/// every model but its Whisper ones), and an `asr-sidecar`, which may
/// return `segments[].speaker`.
pub fn backend_speakers(name: &str, config: &Config) -> Option<Method> {
    match canonical_backend_name(name) {
        "deepgram" | "deepgram-streaming" if supports_diarization(&config.deepgram_model()) => {
            Some(Method::Backend {
                label: DIARIZATION_LABEL.to_string(),
                request: true,
            })
        }
        "asr-sidecar" => Some(Method::Backend {
            label: "asr-sidecar".to_string(),
            request: false,
        }),
        _ => None,
    }
}

/// How a recording transcribed with backend `backend` finds its speakers,
/// from `[recording] diarization`: `sidecar` asks the diarization sidecar
/// at `diarization_url`, `backend` takes the speakers the backend labels
/// ([`backend_speakers`]), `off` keeps one speaker per track, as does a
/// mode that cannot work. `auto` is the sidecar when `diarization_url` is
/// set (a sidecar set up is a sidecar wanted), else Deepgram's speakers on
/// Deepgram, else `off`.
pub fn plan(config: &Config, backend: &str, shared_mic: bool) -> SpeakerPlan {
    let recording = RecordingConfig::effective(config);
    let url = recording.diarization_url();
    let method = match recording.diarization {
        DiarizationMode::Off => Method::Off,
        DiarizationMode::Auto => match (url, canonical_backend_name(backend)) {
            (Some(url), _) => Method::sidecar(url),
            (None, "deepgram" | "deepgram-streaming") => {
                backend_speakers(backend, config).unwrap_or(Method::Off)
            }
            _ => Method::Off,
        },
        DiarizationMode::Sidecar => match url {
            Some(url) => Method::sidecar(url),
            None => {
                warn!(
                    "[recording] diarization = \"sidecar\" without a diarization_url; each \
                     track stays one speaker"
                );
                Method::Off
            }
        },
        DiarizationMode::Backend => match backend_speakers(backend, config) {
            Some(method) => method,
            None => {
                warn!(
                    "[recording] diarization = \"backend\", but the {backend} backend does not \
                     label speakers; each track stays one speaker"
                );
                Method::Off
            }
        },
    };
    SpeakerPlan { method, shared_mic }
}

/// The note for `recording.json` `errors` when the voices could not be
/// told apart for `why`.
pub fn separation_note(why: &str) -> String {
    format!("{SEPARATION_FAILED}{why}")
}

/// Why the voices of a processed recording could not be told apart, when
/// they could not ([`separation_note`]).
pub fn separation_failure(meta: &super::RecordingMeta) -> Option<&str> {
    meta.errors
        .iter()
        .find_map(|note| note.strip_prefix(SEPARATION_FAILED))
}

/// The voice of the turn overlapping `start_ms..end_ms` most, else of the
/// nearest turn no more than [`NEAREST_TURN_MS`] away, else none. A span of
/// no length overlaps nothing and goes by distance (0 inside a turn).
/// Earlier turns win ties.
pub fn speaker_at(turns: &[Turn], start_ms: u64, end_ms: u64) -> Option<&str> {
    let end_ms = end_ms.max(start_ms);
    let mut best: Option<(&Turn, u64)> = None;
    for turn in turns {
        let overlap = end_ms
            .min(turn.end_ms)
            .saturating_sub(start_ms.max(turn.start_ms));
        if overlap > 0 && best.is_none_or(|(_, most)| overlap > most) {
            best = Some((turn, overlap));
        }
    }
    if let Some((turn, _)) = best {
        return Some(&turn.speaker);
    }
    let mut nearest: Option<(&Turn, u64)> = None;
    for turn in turns {
        // The gap between the two, 0 when they touch or overlap.
        let distance = start_ms
            .saturating_sub(turn.end_ms)
            .max(turn.start_ms.saturating_sub(end_ms));
        if distance <= NEAREST_TURN_MS && nearest.is_none_or(|(_, least)| distance < least) {
            nearest = Some((turn, distance));
        }
    }
    nearest.map(|(turn, _)| turn.speaker.as_str())
}

/// Where a track's voice labels come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Labels<'a> {
    /// Its voices are not told apart: labels are dropped.
    None,
    /// The labels the backend put on words and segments.
    Backend,
    /// The turns a diarization sidecar found, on the track's timeline
    /// ([`speaker_at`]).
    Turns(&'a [Turn]),
}

impl Labels<'_> {
    fn of_segment(&self, seg: &Segment) -> Option<String> {
        match self {
            Labels::None => None,
            Labels::Backend => seg.speaker.clone(),
            Labels::Turns(turns) => speaker_at(turns, seg.start_ms, seg.end_ms).map(str::to_string),
        }
    }

    fn of_word(&self, word: &Word) -> Option<String> {
        match self {
            Labels::None => None,
            Labels::Backend => word.speaker.clone(),
            Labels::Turns(turns) => {
                speaker_at(turns, word.start_ms, word.end_ms).map(str::to_string)
            }
        }
    }
}

/// Every word takes the label of the last labelled word before it, or of
/// the first one when none comes before: a word the diarizer left out
/// stays with its neighbours rather than splitting the line.
fn fill_gaps(labels: &mut [Option<String>]) {
    let Some(mut last) = labels.iter().flatten().next().cloned() else {
        return;
    };
    for label in labels.iter_mut() {
        match label {
            Some(l) => last = l.clone(),
            None => *label = Some(last.clone()),
        }
    }
}

/// Whether text token `token` is word `word`, case and punctuation aside.
fn same_word(token: &str, word: &str) -> bool {
    let bare = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    bare(token) == bare(word)
}

/// The voice holding most of `words`' time, the earliest of a tie.
fn main_voice(words: &[Word]) -> Option<String> {
    let mut time: Vec<(&str, u64)> = Vec::new();
    for word in words {
        let Some(voice) = word.speaker.as_deref() else {
            continue;
        };
        let length = word.end_ms.saturating_sub(word.start_ms);
        match time.iter_mut().find(|(v, _)| *v == voice) {
            Some((_, total)) => *total += length,
            None => time.push((voice, length)),
        }
    }
    let mut best: Option<(&str, u64)> = None;
    for (voice, total) in time {
        if best.is_none_or(|(_, most)| total > most) {
            best = Some((voice, total));
        }
    }
    best.map(|(voice, _)| voice.to_string())
}

/// `seg` with each part labelled by its voice: split where its words change
/// voice, each part keeping its words. The parts keep the backend's
/// punctuated text when its tokens are the words, in order (case and
/// punctuation aside), else they join the words' own text ([`join_words`])
/// when that gives the text back. Otherwise the text holds something no
/// word covers (Groq and OpenAI give a segment the words whose midpoint is
/// in it, so a word at its edge can be in its text and in no word list):
/// splitting would lose it, so the segment stays whole, with the voice that
/// holds most of its words' time. A segment without word timings (local
/// whisper) takes the segment's own label: the backend's, or the turn it
/// overlaps most. Labels are dropped with [`Labels::None`].
pub fn split_segment(mut seg: Segment, labels: Labels<'_>) -> Vec<Segment> {
    if labels == Labels::None {
        seg.speaker = None;
        for word in seg.words.iter_mut().flatten() {
            word.speaker = None;
        }
        return vec![seg];
    }
    let segment_label = labels.of_segment(&seg);
    let words = match seg.words.take() {
        Some(words) if !words.is_empty() => words,
        _ => {
            seg.speaker = segment_label;
            return vec![seg];
        }
    };
    let mut per_word: Vec<Option<String>> = words.iter().map(|w| labels.of_word(w)).collect();
    fill_gaps(&mut per_word);
    if per_word.iter().all(Option::is_none) {
        per_word.fill(segment_label);
    }
    let words: Vec<Word> = words
        .into_iter()
        .zip(per_word)
        .map(|(word, speaker)| Word { speaker, ..word })
        .collect();
    let groups: Vec<&[Word]> = words.chunk_by(|a, b| a.speaker == b.speaker).collect();
    if groups.len() == 1 {
        seg.speaker = words[0].speaker.clone();
        seg.words = Some(words);
        return vec![seg];
    }
    let tokens: Vec<&str> = seg.text.split_whitespace().collect();
    let keep_tokens = tokens.len() == words.len()
        && tokens
            .iter()
            .zip(&words)
            .all(|(token, word)| same_word(token, &word.text));
    if !keep_tokens
        && join_words(&seg.text, words.iter().map(|w| w.text.as_str())) != seg.text.trim()
    {
        seg.speaker = main_voice(&words);
        seg.words = Some(words);
        return vec![seg];
    }
    let mut at = 0;
    groups
        .into_iter()
        .map(|group| {
            let text = if keep_tokens {
                tokens[at..at + group.len()].join(" ")
            } else {
                join_words(&seg.text, group.iter().map(|w| w.text.as_str()))
            };
            at += group.len();
            let start_ms = group[0].start_ms;
            Segment {
                start_ms,
                end_ms: group[group.len() - 1].end_ms.max(start_ms),
                text,
                speaker: group[0].speaker.clone(),
                words: Some(group.to_vec()),
            }
        })
        .collect()
}

/// Whether a voice that talks `talk_ms` of its track's `track_talk_ms` is
/// minor: under [`MINOR_TALK_MS`] and under [`MINOR_SHARE_PERCENT`] of the
/// track. A minor voice keeps its words; it is only never named after a
/// participant.
pub fn is_minor(talk_ms: u64, track_talk_ms: u64) -> bool {
    talk_ms < MINOR_TALK_MS && talk_ms * 100 < track_talk_ms * MINOR_SHARE_PERCENT
}

/// What [`assign`] needs to know about the recording.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The recording has a mic track ...
    pub has_mic: bool,
    /// ... and a system track. A track that is listed gets its speaker even
    /// when it holds no speech, unless its voices are told apart.
    pub has_system: bool,
    /// [`SpeakerPlan::separates`] for the mic ...
    pub separate_mic: bool,
    /// ... and the system track.
    pub separate_system: bool,
    /// The user's name ([`super::process`]'s `user_name`).
    pub user_name: &'a str,
    /// The label of the system track ([`super::RecordingMeta::others_label`]).
    pub others_label: &'a str,
}

/// One voice found on a track.
struct Voice {
    label: String,
    first_ms: u64,
    talk_ms: u64,
}

fn talk(seg: &Segment) -> u64 {
    seg.end_ms.saturating_sub(seg.start_ms)
}

/// Turn the voice labels of both tracks into speakers, and every segment's
/// `speaker` into its speaker's id. Returns the speakers, the mic's first,
/// each track's in order of first appearance.
///
/// A track whose voices are not told apart, or where no voice was found, is
/// one speaker: `me` (the user) or `others` (the others label). Otherwise
/// its voices are numbered by first appearance: `S1`, `S2`, ... on the
/// system track, `M1`, `M2`, ... on the mic. A track with exactly one voice
/// that is not minor ([`is_minor`]) names it: on the system track it keeps
/// the others label, on the mic it is the user (id `me`), and the track's
/// speech without a voice goes to it. With several, that speech is the
/// track's own speaker (`others`, or `mic` named [`MIC_LABEL`]), and every
/// voice left unnamed is `Speaker N`, numbered by first appearance across
/// both tracks.
pub fn assign(mic: &mut [Segment], system: &mut [Segment], ctx: &Context<'_>) -> Vec<Speaker> {
    let mut found = channel(mic, TrackKind::Mic, ctx);
    found.extend(channel(system, TrackKind::System, ctx));
    let mut unnamed: Vec<usize> = (0..found.len())
        .filter(|&i| found[i].0.source == SOURCE_NONE)
        .collect();
    unnamed.sort_by_key(|&i| (found[i].1, i));
    for (n, i) in unnamed.into_iter().enumerate() {
        found[i].0.name = format!("Speaker {}", n + 1);
    }
    found.into_iter().map(|(speaker, _)| speaker).collect()
}

/// The speakers of one track with the time each first speaks, and its
/// segments relabelled with their ids ([`assign`]).
fn channel(segments: &mut [Segment], kind: TrackKind, ctx: &Context<'_>) -> Vec<(Speaker, u64)> {
    let (present, separate) = match kind {
        TrackKind::Mic => (ctx.has_mic, ctx.separate_mic),
        TrackKind::System => (ctx.has_system, ctx.separate_system),
    };
    let speaker = |id: &str, name: &str, source: &str, talk_ms: u64| Speaker {
        id: id.to_string(),
        name: name.to_string(),
        source: source.to_string(),
        evidence: None,
        talk_ms,
        channel: Some(kind),
        minor: false,
    };
    let (whole_id, whole_name, whole_source) = match kind {
        TrackKind::Mic => (ME, ctx.user_name, SOURCE_SELF),
        TrackKind::System => (OTHERS, ctx.others_label, SOURCE_CHANNEL),
    };
    let track_talk: u64 = segments.iter().map(talk).sum();

    let mut order: Vec<usize> = (0..segments.len()).collect();
    order.sort_by_key(|&i| (segments[i].start_ms, i));
    let mut voices: Vec<Voice> = Vec::new();
    if separate {
        for &i in &order {
            let Some(label) = &segments[i].speaker else {
                continue;
            };
            match voices.iter_mut().find(|v| &v.label == label) {
                Some(voice) => voice.talk_ms += talk(&segments[i]),
                None => voices.push(Voice {
                    label: label.clone(),
                    first_ms: segments[i].start_ms,
                    talk_ms: talk(&segments[i]),
                }),
            }
        }
    }

    if voices.is_empty() {
        for seg in segments.iter_mut() {
            seg.speaker = Some(whole_id.to_string());
        }
        if !present || (separate && segments.is_empty()) {
            return Vec::new();
        }
        let first = order.first().map_or(0, |&i| segments[i].start_ms);
        return vec![(
            speaker(whole_id, whole_name, whole_source, track_talk),
            first,
        )];
    }

    let minor: Vec<bool> = voices
        .iter()
        .map(|v| is_minor(v.talk_ms, track_talk))
        .collect();
    let mut major = (0..voices.len()).filter(|&i| !minor[i]);
    let main = match (major.next(), major.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    };
    let prefix = match kind {
        TrackKind::Mic => "M",
        TrackKind::System => "S",
    };
    let mut n = 0;
    let ids: Vec<String> = (0..voices.len())
        .map(|i| {
            if kind == TrackKind::Mic && Some(i) == main {
                ME.to_string()
            } else {
                n += 1;
                format!("{prefix}{n}")
            }
        })
        .collect();
    let (fallback_id, fallback_name) = match kind {
        TrackKind::Mic => (MIC, MIC_LABEL),
        TrackKind::System => (OTHERS, ctx.others_label),
    };

    let mut unattributed_talk = 0;
    let mut unattributed_first: Option<u64> = None;
    for &i in &order {
        let seg = &mut segments[i];
        let id = match &seg.speaker {
            Some(label) => {
                let voice = voices.iter().position(|v| &v.label == label);
                ids[voice.expect("every label is a voice")].clone()
            }
            None => {
                unattributed_talk += talk(seg);
                unattributed_first.get_or_insert(seg.start_ms);
                match main {
                    Some(main) => ids[main].clone(),
                    None => fallback_id.to_string(),
                }
            }
        };
        seg.speaker = Some(id);
    }

    let mut out: Vec<(Speaker, u64)> = voices
        .iter()
        .enumerate()
        .map(|(i, voice)| {
            let mut s = if Some(i) == main {
                let talk_ms = voice.talk_ms + unattributed_talk;
                speaker(&ids[i], whole_name, whole_source, talk_ms)
            } else {
                speaker(&ids[i], "", SOURCE_NONE, voice.talk_ms)
            };
            s.minor = minor[i];
            (s, voice.first_ms)
        })
        .collect();
    if let (None, Some(first)) = (main, unattributed_first) {
        out.push((
            speaker(
                fallback_id,
                fallback_name,
                SOURCE_CHANNEL,
                unattributed_talk,
            ),
            first,
        ));
        out.sort_by_key(|(_, first)| *first);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(start_ms: u64, end_ms: u64, text: &str, speaker: Option<&str>) -> Word {
        Word {
            start_ms,
            end_ms,
            text: text.into(),
            speaker: speaker.map(str::to_string),
        }
    }

    fn seg(start_ms: u64, end_ms: u64, text: &str, speaker: Option<&str>) -> Segment {
        Segment {
            start_ms,
            end_ms,
            text: text.into(),
            speaker: speaker.map(str::to_string),
            words: None,
        }
    }

    fn lines(segments: &[Segment]) -> Vec<(u64, u64, &str, Option<&str>)> {
        segments
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.as_str(), s.speaker.as_deref()))
            .collect()
    }

    /// One utterance holding two voices splits where the voice changes,
    /// each part with its own words and the backend's punctuation.
    #[test]
    fn a_segment_splits_where_its_words_change_voice() {
        let mut s = seg(1000, 4000, "Ready? Yes, go ahead.", Some("0"));
        s.words = Some(vec![
            word(1000, 1500, "ready", Some("0")),
            word(2000, 2400, "yes", Some("1")),
            word(2500, 2900, "go", Some("1")),
            word(3000, 4000, "ahead", Some("1")),
        ]);
        let out = split_segment(s, Labels::Backend);
        assert_eq!(
            lines(&out),
            [
                (1000, 1500, "Ready?", Some("0")),
                (2000, 4000, "Yes, go ahead.", Some("1")),
            ]
        );
        let words = out[1].words.as_ref().unwrap();
        assert_eq!(words.len(), 3);
        assert!(words.iter().all(|w| w.speaker.as_deref() == Some("1")));
    }

    /// Tokens that disagree with the words fall back to the words' own
    /// text, joined as the backend joined them: `y` and `es` run together
    /// in its text.
    #[test]
    fn a_split_without_matching_tokens_joins_the_words() {
        let mut s = seg(0, 3000, "ok so, yes", None);
        s.words = Some(vec![
            word(0, 500, "ok", Some("A")),
            word(600, 900, "so,", Some("A")),
            word(1000, 1500, "y", Some("B")),
            word(1500, 2000, "es", Some("B")),
        ]);
        let texts: Vec<String> = split_segment(s, Labels::Backend)
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(texts, ["ok so,", "yes"]);
    }

    /// A token no word covers would be lost by a split (Groq and OpenAI
    /// can leave a segment's edge word out of its word list), so the
    /// segment stays whole, with the voice of most of its words' time;
    /// tokens that are not the words, in order, are not split by either.
    #[test]
    fn a_segment_with_a_token_no_word_covers_stays_whole() {
        let mut s = seg(0, 4000, "How are you doing today?", None);
        s.words = Some(vec![
            word(0, 400, "How", Some("A")),
            word(400, 900, "are", Some("A")),
            word(2000, 2300, "you", Some("B")),
            word(2300, 3900, "doing", Some("B")),
        ]);
        let out = split_segment(s, Labels::Backend);
        assert_eq!(
            lines(&out),
            [(0, 4000, "How are you doing today?", Some("B"))]
        );
        assert_eq!(out[0].words.as_ref().unwrap().len(), 4, "the words stay");

        // As many tokens as words, but not the same ones.
        let mut s = seg(0, 2000, "Hello there", None);
        s.words = Some(vec![
            word(0, 900, "yellow", Some("A")),
            word(1000, 2000, "where", Some("B")),
        ]);
        let out = split_segment(s, Labels::Backend);
        assert_eq!(lines(&out), [(0, 2000, "Hello there", Some("B"))]);
    }

    /// A word without a label stays with its neighbours; a segment
    /// without words takes its own label.
    #[test]
    fn unlabelled_words_stay_with_their_neighbours() {
        let mut s = seg(0, 3000, "a b c d", Some("seg"));
        s.words = Some(vec![
            word(0, 500, "a", None),
            word(600, 900, "b", Some("1")),
            word(1000, 1500, "c", None),
            word(1600, 2000, "d", Some("2")),
        ]);
        let out = split_segment(s, Labels::Backend);
        assert_eq!(
            lines(&out),
            [(0, 1500, "a b c", Some("1")), (1600, 2000, "d", Some("2"))]
        );

        // No word has a label: the segment's own label covers them all.
        let mut s = seg(0, 3000, "a b", Some("seg"));
        s.words = Some(vec![word(0, 500, "a", None), word(600, 900, "b", None)]);
        let out = split_segment(s, Labels::Backend);
        assert_eq!(lines(&out), [(0, 3000, "a b", Some("seg"))]);
        assert!(out[0].words.as_ref().unwrap()[0].speaker.as_deref() == Some("seg"));

        let out = split_segment(seg(0, 900, "hi", Some("2")), Labels::Backend);
        assert_eq!(lines(&out), [(0, 900, "hi", Some("2"))]);
    }

    /// A track whose voices are not told apart drops the backend's labels.
    #[test]
    fn labels_are_dropped_when_not_separating() {
        let mut s = seg(0, 3000, "a b", Some("0"));
        s.words = Some(vec![
            word(0, 500, "a", Some("0")),
            word(600, 900, "b", Some("1")),
        ]);
        let out = split_segment(s, Labels::None);
        assert_eq!(lines(&out), [(0, 3000, "a b", None)]);
        assert!(out[0]
            .words
            .as_ref()
            .unwrap()
            .iter()
            .all(|w| w.speaker.is_none()));
    }

    fn turn(start_ms: u64, end_ms: u64, speaker: &str) -> Turn {
        Turn {
            start_ms,
            end_ms,
            speaker: speaker.into(),
        }
    }

    /// A word takes the turn it overlaps most, else the nearest within a
    /// second, else none.
    #[test]
    fn words_take_the_turn_they_overlap_most_else_the_nearest() {
        let turns = [
            turn(0, 2000, "A"),
            turn(1800, 4000, "B"),
            turn(6000, 8000, "A"),
        ];
        assert_eq!(speaker_at(&turns, 500, 900), Some("A"));
        // 300 ms in A, 500 ms in B; on a tie, the earlier turn.
        assert_eq!(speaker_at(&turns, 1700, 2300), Some("B"));
        assert_eq!(speaker_at(&turns, 1700, 2100), Some("A"));
        // In the gap: B ends 500 ms before, A starts 1500 ms after.
        assert_eq!(speaker_at(&turns, 4500, 4600), Some("B"));
        // 1000 ms from B and from the next A: the earlier turn wins a tie.
        assert_eq!(speaker_at(&turns, 5000, 5000), Some("B"));
        // 900 ms from B, 1000 ms from A... then 1100 from B, 900 from A.
        assert_eq!(speaker_at(&turns, 4900, 5000), Some("B"));
        assert_eq!(speaker_at(&turns, 5100, 5100), Some("A"));
        // More than a second from every turn.
        assert_eq!(speaker_at(&turns, 9100, 9500), None);
        assert_eq!(speaker_at(&[], 0, 100), None);
        // A point inside a turn.
        assert_eq!(speaker_at(&turns, 7000, 7000), Some("A"));
    }

    /// A segment splits where its words cross from one turn to another; a
    /// segment without words takes the turn it overlaps most; one far from
    /// every turn keeps no voice.
    #[test]
    fn segments_split_at_the_sidecars_turns() {
        let turns = [turn(0, 2000, "SPEAKER_00"), turn(2000, 5000, "SPEAKER_01")];
        let mut s = seg(500, 3500, "Ready? Yes, go.", None);
        s.words = Some(vec![
            word(500, 1200, "ready", None),
            word(2200, 2600, "yes", None),
            word(2700, 3500, "go", None),
        ]);
        let out = split_segment(s, Labels::Turns(&turns));
        assert_eq!(
            lines(&out),
            [
                (500, 1200, "Ready?", Some("SPEAKER_00")),
                (2200, 3500, "Yes, go.", Some("SPEAKER_01")),
            ]
        );

        // Local whisper: no words, so the whole segment goes by overlap.
        let out = split_segment(seg(1500, 4000, "yes go", None), Labels::Turns(&turns));
        assert_eq!(lines(&out), [(1500, 4000, "yes go", Some("SPEAKER_01"))]);
        let out = split_segment(seg(9000, 9500, "late", None), Labels::Turns(&turns));
        assert_eq!(lines(&out), [(9000, 9500, "late", None)]);
    }

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    /// `auto` asks Deepgram for speakers (except with a Whisper model,
    /// which it cannot diarize) and keeps every other backend at one
    /// speaker per track; `off` always does; `backend` uses what the
    /// backend can do.
    #[test]
    fn plans_by_backend_and_mode() {
        let deepgram = Method::Backend {
            label: "deepgram:latest".into(),
            request: true,
        };
        let sidecar = Method::Backend {
            label: "asr-sidecar".into(),
            request: false,
        };
        let cases = [
            ("", "deepgram", deepgram.clone()),
            ("", "deepgram-streaming", deepgram.clone()),
            ("", "groq", Method::Off),
            ("", "asr-sidecar", Method::Off),
            ("diarization = \"off\"", "deepgram", Method::Off),
            ("diarization = \"backend\"", "deepgram", deepgram.clone()),
            ("diarization = \"backend\"", "asr-sidecar", sidecar),
            ("diarization = \"backend\"", "openai", Method::Off),
        ];
        for (line, backend, want) in cases {
            let cfg = config(&format!("[recording]\n{line}\n"));
            assert_eq!(
                plan(&cfg, backend, false).method,
                want,
                "{line:?} {backend}"
            );
        }

        // A sidecar that is set up wins `auto`, whatever the backend; one
        // that is not leaves `sidecar` off.
        let url = "diarization_url = \"http://127.0.0.1:8770/diarize\"";
        let sidecar = Method::Sidecar {
            url: "http://127.0.0.1:8770/diarize".into(),
            label: "sidecar:127.0.0.1:8770".into(),
        };
        for (line, backend, want) in [
            (url.to_string(), "deepgram", sidecar.clone()),
            (url.to_string(), "local-whisper", sidecar.clone()),
            (
                format!("{url}\ndiarization = \"sidecar\""),
                "groq",
                sidecar.clone(),
            ),
            (
                format!("{url}\ndiarization = \"backend\""),
                "deepgram",
                deepgram.clone(),
            ),
            (
                format!("{url}\ndiarization = \"off\""),
                "deepgram",
                Method::Off,
            ),
            ("diarization = \"sidecar\"".to_string(), "groq", Method::Off),
        ] {
            let cfg = config(&format!("[recording]\n{line}\n"));
            assert_eq!(
                plan(&cfg, backend, false).method,
                want,
                "{line:?} {backend}"
            );
        }
        assert_eq!(
            Method::sidecar("https://diar.lan/x").label(),
            "sidecar:diar.lan"
        );
        // The sidecar never makes the transcription request ask.
        let cfg = config(&format!("[recording]\n{url}\n"));
        assert!(!plan(&cfg, "deepgram", true).requests_speakers(TrackKind::System));
        let whisper = config("[deepgram]\napi_key = \"k\"\nmodel = \"whisper-large\"\n");
        assert_eq!(plan(&whisper, "deepgram", false).method, Method::Off);

        // Only separated tracks ask for speakers: the mic when it is shared.
        let cfg = config("");
        let alone = plan(&cfg, "deepgram", false);
        assert!(alone.requests_speakers(TrackKind::System));
        assert!(!alone.requests_speakers(TrackKind::Mic));
        assert!(plan(&cfg, "deepgram", true).requests_speakers(TrackKind::Mic));
        let backend = config("[recording]\ndiarization = \"backend\"\n");
        assert!(!plan(&backend, "asr-sidecar", true).requests_speakers(TrackKind::System));
    }

    fn ctx(separate_mic: bool, separate_system: bool) -> Context<'static> {
        Context {
            has_mic: true,
            has_system: true,
            separate_mic,
            separate_system,
            user_name: "Yosif",
            others_label: "Others",
        }
    }

    fn ids(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .map(|s| s.speaker.as_deref().unwrap())
            .collect()
    }

    fn names(speakers: &[Speaker]) -> Vec<(&str, &str, &str)> {
        speakers
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str(), s.source.as_str()))
            .collect()
    }

    /// Without separation each track is one speaker, listed even when it
    /// holds no speech, and the backend's labels play no part.
    #[test]
    fn unseparated_tracks_are_one_speaker_each() {
        let mut mic = vec![seg(0, 1000, "hi", None)];
        let mut system: Vec<Segment> = Vec::new();
        let speakers = assign(&mut mic, &mut system, &ctx(false, false));
        assert_eq!(
            names(&speakers),
            [("me", "Yosif", "self"), ("others", "Others", "channel")]
        );
        assert_eq!(ids(&mic), ["me"]);
        assert_eq!(speakers[0].talk_ms, 1000);
        assert_eq!(speakers[0].channel, Some(TrackKind::Mic));
        assert_eq!(speakers[1].talk_ms, 0);

        // No system track recorded: no system speaker.
        let only_mic = Context {
            has_system: false,
            ..ctx(false, false)
        };
        let speakers = assign(&mut mic, &mut [], &only_mic);
        assert_eq!(names(&speakers), [("me", "Yosif", "self")]);
    }

    /// Voices are numbered by first appearance, not by the backend's own
    /// labels, and every unnamed voice is `Speaker N` in that order.
    #[test]
    fn system_voices_get_ids_by_first_appearance() {
        let mut mic = vec![seg(500, 1500, "hello", None)];
        let mut system = vec![
            seg(30_000, 40_000, "third", Some("c2:0")),
            seg(2_000, 12_000, "first", Some("c1:1")),
            seg(14_000, 24_000, "second", Some("c1:0")),
            seg(25_000, 29_000, "first again", Some("c1:1")),
        ];
        let speakers = assign(&mut mic, &mut system, &ctx(false, true));
        assert_eq!(ids(&system), ["S3", "S1", "S2", "S1"]);
        assert_eq!(
            names(&speakers),
            [
                ("me", "Yosif", "self"),
                ("S1", "Speaker 1", "none"),
                ("S2", "Speaker 2", "none"),
                ("S3", "Speaker 3", "none"),
            ]
        );
        assert_eq!(speakers[1].talk_ms, 14_000);
        assert!(speakers.iter().all(|s| !s.minor));
    }

    /// One remote voice keeps the others label, and the system speech no
    /// voice was found for goes to it.
    #[test]
    fn one_remote_voice_keeps_the_others_label() {
        let mut system = vec![
            seg(0, 10_000, "a", Some("0")),
            seg(11_000, 12_000, "b", None),
        ];
        let video = Context {
            others_label: "Video",
            ..ctx(false, true)
        };
        let speakers = assign(&mut [], &mut system, &video);
        assert_eq!(
            names(&speakers),
            [("me", "Yosif", "self"), ("S1", "Video", "channel")]
        );
        assert_eq!(ids(&system), ["S1", "S1"]);
        assert_eq!(speakers[1].talk_ms, 11_000);
    }

    /// With several voices, system speech without a voice is the track's
    /// own speaker, under the others label.
    #[test]
    fn unattributed_speech_of_several_voices_is_the_channel() {
        let mut system = vec![
            seg(0, 10_000, "a", Some("0")),
            seg(10_000, 20_000, "b", Some("1")),
            seg(21_000, 22_000, "c", None),
        ];
        let speakers = assign(&mut [], &mut system, &ctx(false, true));
        assert_eq!(ids(&system), ["S1", "S2", "others"]);
        assert_eq!(
            names(&speakers),
            [
                ("me", "Yosif", "self"),
                ("S1", "Speaker 1", "none"),
                ("S2", "Speaker 2", "none"),
                ("others", "Others", "channel"),
            ]
        );
    }

    /// A voice under 5 s and under 2 % of its track is minor: kept, with
    /// its words, but it does not count as a second voice.
    #[test]
    fn a_short_voice_is_minor() {
        assert!(is_minor(4_999, 250_000));
        assert!(!is_minor(4_999, 249_950), "2 % of the track");
        assert!(!is_minor(5_000, 1_000_000), "5 s");
        assert!(!is_minor(0, 0), "a silent track's only voice");

        let mut system = vec![
            seg(0, 300_000, "the talk", Some("0")),
            seg(300_000, 304_000, "a question", Some("1")),
        ];
        let speakers = assign(&mut [], &mut system, &ctx(false, true));
        assert_eq!(
            names(&speakers),
            [
                ("me", "Yosif", "self"),
                ("S1", "Others", "channel"),
                ("S2", "Speaker 1", "none"),
            ]
        );
        assert!(!speakers[1].minor);
        assert!(speakers[2].minor);
        assert_eq!(ids(&system), ["S1", "S2"]);
    }

    /// A shared mic with one voice is the user; with several, its voices
    /// are `M1`, `M2`, ... and unnamed, numbered with the system's voices
    /// by first appearance.
    #[test]
    fn a_shared_mic_is_the_user_only_with_one_voice() {
        let mut mic = vec![seg(0, 9_000, "hello", Some("0"))];
        let speakers = assign(&mut mic, &mut [], &ctx(true, true));
        assert_eq!(names(&speakers), [("me", "Yosif", "self")]);
        assert_eq!(ids(&mic), ["me"]);

        let mut mic = vec![
            seg(0, 9_000, "one", Some("0")),
            seg(20_000, 29_000, "two", Some("1")),
            seg(30_000, 31_000, "?", None),
        ];
        let mut system = vec![
            seg(10_000, 19_000, "far one", Some("0")),
            seg(40_000, 49_000, "far two", Some("1")),
        ];
        let speakers = assign(&mut mic, &mut system, &ctx(true, true));
        assert_eq!(ids(&mic), ["M1", "M2", "mic"]);
        assert_eq!(
            names(&speakers),
            [
                ("M1", "Speaker 1", "none"),
                ("M2", "Speaker 3", "none"),
                ("mic", "Mic", "channel"),
                ("S1", "Speaker 2", "none"),
                ("S2", "Speaker 4", "none"),
            ]
        );
    }

    /// A separated track where no voice was found is one speaker, as if not
    /// separated; a separated track without speech lists nobody.
    #[test]
    fn a_track_without_voices_is_one_speaker() {
        let mut system = vec![seg(0, 1000, "a", None)];
        let speakers = assign(&mut [], &mut system, &ctx(false, true));
        assert_eq!(
            names(&speakers),
            [("me", "Yosif", "self"), ("others", "Others", "channel")]
        );
        let speakers = assign(&mut [], &mut [], &ctx(false, true));
        assert_eq!(names(&speakers), [("me", "Yosif", "self")]);
    }
}
