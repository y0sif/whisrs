//! Whisper `verbose_json` responses with segment timestamps.
//!
//! Groq and OpenAI `whisper-1` answer `response_format=verbose_json` with the
//! same shape: `segments[]` carrying start/end seconds plus Whisper's own
//! confidence numbers, and, when word granularity is requested, a top-level
//! `words[]`. Both recording paths parse it here, as does the ASR sidecar's
//! recording request, whose servers may send `words[]` alone (nemo-speech)
//! or one segment for the whole clip; [`split_sentences`] cuts those into
//! sentence-sized segments by their word timings. A `null` reads as a
//! missing key, and a word missing a time (WhisperX leaves numerals
//! unaligned) takes it from its neighbours.

use std::cmp::Reverse;
use std::ops::Range;

use serde::Deserialize;

use super::{secs_to_ms, Segment, Transcript, Word};

/// A segment is dropped as non-speech when Whisper is fairly sure there was
/// no speech *and* the decoded tokens were improbable. Same rule (and
/// thresholds) as Whisper's own reference decoder uses to skip silent windows.
pub const NO_SPEECH_PROB_THRESHOLD: f64 = 0.6;
/// See [`NO_SPEECH_PROB_THRESHOLD`].
pub const LOW_AVG_LOGPROB_THRESHOLD: f64 = -1.0;

/// Whisper's no-speech heuristic: both conditions must hold, so a quiet but
/// confidently decoded segment survives.
pub fn is_no_speech(no_speech_prob: Option<f64>, avg_logprob: Option<f64>) -> bool {
    match (no_speech_prob, avg_logprob) {
        (Some(p), Some(lp)) => p > NO_SPEECH_PROB_THRESHOLD && lp < LOW_AVG_LOGPROB_THRESHOLD,
        _ => false,
    }
}

/// A `verbose_json` body as read. Each key is `None` when it is missing or
/// `null`: a server that sends `"words": null` sent no words.
#[derive(Debug, Deserialize)]
pub(crate) struct VerboseResponse {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    segments: Option<Vec<VerboseSegment>>,
    #[serde(default)]
    words: Option<Vec<VerboseWord>>,
}

#[derive(Debug, Deserialize)]
struct VerboseSegment {
    start: f64,
    end: f64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    no_speech_prob: Option<f64>,
    #[serde(default)]
    avg_logprob: Option<f64>,
    /// A diarizing ASR sidecar labels each segment's voice, with a string or
    /// a number. Groq and OpenAI send none.
    #[serde(default)]
    speaker: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct VerboseWord {
    word: String,
    /// Missing (or `null`) on a word the server could not align: WhisperX
    /// leaves numerals untimed. Such a word takes its times from its
    /// neighbours ([`VerboseResponse::timed_words`]).
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    end: Option<f64>,
}

/// Parse a `verbose_json` body into timed segments.
///
/// Segments flagged by [`is_no_speech`] and segments with blank text are
/// dropped. Words are attached to the segment containing their midpoint, and
/// a segment with words is narrowed to the span they cover; a segment with no
/// words gets `words: None`. A segment's `speaker` label (a diarizing
/// sidecar's) is kept. A body with text but no
/// `segments` array (a server that ignored the granularity) becomes a single
/// segment spanning its words, or `0..0` when there are none.
///
/// A `null` reads as a missing key, and a word missing a time takes it from
/// its neighbours.
pub fn parse_verbose_segments(body: &str) -> anyhow::Result<Transcript> {
    Ok(VerboseResponse::parse(body)?.into_transcript())
}

impl VerboseResponse {
    /// Read `body`.
    pub(crate) fn parse(body: &str) -> serde_json::Result<Self> {
        serde_json::from_str(body)
    }

    /// The text, trimmed; `None` when the key is missing or `null`.
    pub(crate) fn text(&self) -> Option<&str> {
        self.text.as_deref().map(str::trim)
    }

    /// Whether a `segments` or a `words` key came, even an empty one.
    pub(crate) fn has_timing_keys(&self) -> bool {
        self.segments.is_some() || self.words.is_some()
    }

    /// Whether `segments` holds one or more.
    pub(crate) fn has_segments(&self) -> bool {
        self.segments
            .as_ref()
            .is_some_and(|segments| !segments.is_empty())
    }

    /// Every word, in order, its text trimmed; none when no word carries
    /// both times.
    ///
    /// A word missing a time (WhisperX leaves numerals unaligned) takes it
    /// from its neighbours rather than lose its text when a segment is cut:
    /// its start is the end of the nearest word before it that has one, else
    /// the first segment's start (never after the next word's start), else
    /// the next word's start; its end is the start of the nearest word after
    /// it that has one, else the last segment's end, else its own start, and
    /// never comes before its start. A time it has is kept. Groq and OpenAI
    /// time every word, so theirs come as they are.
    fn timed_words(&self) -> Vec<Word> {
        let words = self.words.as_deref().unwrap_or_default();
        if !words.iter().any(|w| w.start.is_some() && w.end.is_some()) {
            return Vec::new();
        }
        let segments = self.segments.as_deref().unwrap_or_default();
        let first_start = segments.iter().map(|s| secs_to_ms(s.start)).min();
        let last_end = segments.iter().map(|s| secs_to_ms(s.end)).max();
        let mut previous_end = None;
        let mut out = Vec::with_capacity(words.len());
        for (i, w) in words.iter().enumerate() {
            let (start_ms, end_ms) = match (w.start, w.end) {
                (Some(start), Some(end)) => (secs_to_ms(start), secs_to_ms(end)),
                (start, end) => {
                    let next_start = || words[i + 1..].iter().find_map(|w| w.start).map(secs_to_ms);
                    // With no end before it, the word with both times comes
                    // after it, so there is a next start.
                    let start = start.map(secs_to_ms).or(previous_end).unwrap_or_else(|| {
                        let next = next_start().unwrap_or_default();
                        first_start.map_or(next, |first| first.min(next))
                    });
                    let end = end
                        .map(secs_to_ms)
                        .or_else(next_start)
                        .unwrap_or_else(|| last_end.unwrap_or(start));
                    (start, end.max(start))
                }
            };
            if let Some(end) = w.end {
                previous_end = Some(secs_to_ms(end));
            }
            out.push(Word {
                start_ms,
                end_ms,
                text: w.word.trim().to_string(),
                speaker: None,
            });
        }
        out
    }

    /// The words as one segment, for a server that sends them with no
    /// segments to attach them to (nemo-speech), or beside an empty
    /// `segments`: spanning the words, with the text, or with the words' own
    /// joined by spaces when the text is blank or missing. `None` without a
    /// word that carries both times ([`Self::timed_words`]).
    pub(crate) fn words_segment(&self) -> Option<Segment> {
        let words = self.timed_words();
        let start_ms = words.first()?.start_ms;
        let end_ms = words.last()?.end_ms.max(start_ms);
        let text = match self.text() {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        };
        Some(Segment {
            start_ms,
            end_ms,
            text,
            speaker: None,
            words: Some(words),
        })
    }

    /// Its segments, as [`parse_verbose_segments`] describes them.
    pub(crate) fn into_transcript(self) -> Transcript {
        let words = self.timed_words();

        let Some(raw_segments) = self.segments else {
            let text = self.text.as_deref().unwrap_or_default().trim();
            if text.is_empty() {
                return Transcript::default();
            }
            let start_ms = words.first().map_or(0, |w| w.start_ms);
            let end_ms = words.last().map_or(start_ms, |w| w.end_ms);
            return Transcript {
                segments: vec![Segment {
                    start_ms,
                    end_ms,
                    text: text.to_string(),
                    speaker: None,
                    words: (!words.is_empty()).then_some(words),
                }],
            };
        };

        let mut segments = Vec::with_capacity(raw_segments.len());
        for seg in raw_segments {
            let text = seg.text.as_deref().unwrap_or_default().trim();
            if text.is_empty() || is_no_speech(seg.no_speech_prob, seg.avg_logprob) {
                continue;
            }
            let start_ms = secs_to_ms(seg.start);
            let end_ms = secs_to_ms(seg.end).max(start_ms);
            let seg_words: Vec<Word> = words
                .iter()
                .filter(|w| {
                    let mid = (w.start_ms + w.end_ms) / 2;
                    mid >= start_ms && mid < end_ms.max(start_ms + 1)
                })
                .cloned()
                .collect();
            // Whisper stretches a segment back over leading silence (a first
            // segment starts at 0 even when speech begins seconds in), so its
            // words, timed independently, give the tighter span when present.
            let (start_ms, end_ms) = match (seg_words.first(), seg_words.last()) {
                (Some(first), Some(last)) => {
                    let start = first.start_ms.clamp(start_ms, end_ms);
                    (start, last.end_ms.clamp(start, end_ms))
                }
                _ => (start_ms, end_ms),
            };
            segments.push(Segment {
                start_ms,
                end_ms,
                text: text.to_string(),
                speaker: seg.speaker.and_then(speaker_label),
                words: (!seg_words.is_empty()).then_some(seg_words),
            });
        }
        Transcript { segments }
    }
}

/// A speaker label as a diarizing server sends it: a string, or a number
/// taken as its digits.
fn speaker_label(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// What a word ends with when it ends a sentence: the full stop, question
/// and exclamation marks (Latin and CJK) and the ellipsis.
const SENTENCE_ENDS: [char; 7] = ['.', '?', '!', '。', '？', '！', '…'];

/// What may close a sentence after its final mark: quotes and brackets
/// (`done."`, `「好。」`).
const CLOSERS: [char; 8] = ['"', '\'', '”', '’', ')', ']', '」', '』'];

/// Whether `word` ends a sentence: with one of [`SENTENCE_ENDS`], maybe
/// inside closing quotes or brackets ([`CLOSERS`]).
fn ends_sentence(word: &str) -> bool {
    word.trim_end()
        .trim_end_matches(CLOSERS)
        .ends_with(SENTENCE_ENDS)
}

/// A pause between two words at least this long ends a segment in
/// [`split_sentences`], punctuated or not. Word times from nemo-speech are
/// rough (about +-0.5 s): Nemotron can show a false 0.8 s pause inside
/// speech, and Parakeet stretches a word over the pause after it, so a
/// shorter cut would split mid-sentence and a longer one would miss real
/// pauses.
pub(crate) const SENTENCE_GAP_MS: u64 = 1000;

/// The longest a segment may run: [`split_sentences`] cuts a longer piece
/// at its largest pause, so an unpunctuated monologue (Parakeet on a quiet
/// voice returns no punctuation) still becomes lines a reader can follow,
/// and the ASR sidecar takes a longer segment without word timings as no
/// timing at all. Whisper's own segments never exceed its 30 s window.
pub(crate) const MAX_SEGMENT_MS: u64 = 30_000;

/// `seg` cut into sentence-sized segments by its word timings.
///
/// A cut falls after a word ending a sentence ([`SENTENCE_ENDS`], maybe
/// inside closing quotes or brackets) and where a pause between two words
/// reaches [`SENTENCE_GAP_MS`]. A piece
/// still longer than [`MAX_SEGMENT_MS`] is cut at its largest pause, again
/// and again, until each piece fits or holds one word; among equal pauses
/// the one nearest the middle of the piece wins, so a run whose pauses all
/// read 0 halves instead of shedding a word at a time.
///
/// The segment's text speaks for its words where it can. When it splits on
/// whitespace into exactly as many tokens as there are words, the tokens
/// decide the sentence ends and make each piece's text, so punctuation the
/// words lack (OpenAI `whisper-1` times bare words) is kept. Otherwise the
/// words do, if they rebuild the text ([`join_words`]); words that do
/// neither (a word the server left out, or spelled another way) cannot say
/// where its sentences end, so such a segment is cut only at the length
/// cap, its pieces taking the words' text.
///
/// A segment that needs no cut, or has fewer than two words, is returned as
/// it is, with the server's own text. Each piece spans its first word's
/// start to its last word's end, keeps its words and the segment's
/// speaker, and joins its tokens with single spaces, or its words' text the
/// way the segment's text joins them ([`join_words`]): with single spaces,
/// or none for a script written without them.
pub(crate) fn split_sentences(seg: Segment) -> Vec<Segment> {
    let Some(words) = seg.words.as_deref().filter(|words| words.len() > 1) else {
        return vec![seg];
    };
    let tokens: Vec<&str> = seg.text.split_whitespace().collect();
    let keep_tokens = tokens.len() == words.len();
    let at_sentences = keep_tokens
        || join_words(&seg.text, words.iter().map(|w| w.text.as_str())) == seg.text.trim();
    let mut pieces = Vec::new();
    let mut start = 0;
    for (i, word) in words.iter().enumerate() {
        let ends_piece = match words.get(i + 1) {
            None => true,
            Some(_) if !at_sentences => false,
            Some(next) => {
                let text = if keep_tokens {
                    tokens[i]
                } else {
                    word.text.as_str()
                };
                ends_sentence(text) || pause_ms(word, next) >= SENTENCE_GAP_MS
            }
        };
        if ends_piece {
            cap_length(words, start..i + 1, &mut pieces);
            start = i + 1;
        }
    }
    if pieces.len() == 1 {
        return vec![seg];
    }
    pieces
        .into_iter()
        .map(|range| {
            let group = &words[range.clone()];
            let text = if keep_tokens {
                tokens[range].join(" ")
            } else {
                join_words(&seg.text, group.iter().map(|w| w.text.as_str()))
            };
            let start_ms = group[0].start_ms;
            Segment {
                start_ms,
                end_ms: group[group.len() - 1].end_ms.max(start_ms),
                text,
                speaker: seg.speaker.clone(),
                words: Some(group.to_vec()),
            }
        })
        .collect()
}

/// The silence between `word` and the `next` one (0 when they overlap).
fn pause_ms(word: &Word, next: &Word) -> u64 {
    next.start_ms.saturating_sub(word.end_ms)
}

/// Push `range` of `words` onto `out` in order, cut at its largest pause
/// until every piece spans at most [`MAX_SEGMENT_MS`] or holds one word
/// (ties go to the cut nearest the piece's middle).
fn cap_length(words: &[Word], range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let mut pending = vec![range];
    while let Some(range) = pending.pop() {
        let piece = &words[range.clone()];
        let first_ms = piece[0].start_ms;
        let span_ms = piece[piece.len() - 1].end_ms.saturating_sub(first_ms);
        if piece.len() < 2 || span_ms <= MAX_SEGMENT_MS {
            out.push(range);
            continue;
        }
        let middle_ms = first_ms + span_ms / 2;
        let cut = (range.start + 1..range.end)
            .max_by_key(|&i| {
                let pause = pause_ms(&words[i - 1], &words[i]);
                (pause, Reverse(words[i].start_ms.abs_diff(middle_ms)))
            })
            .expect("a piece of two words or more has a pause");
        // Popped left first, so the pieces come out in order.
        pending.push(cut..range.end);
        pending.push(range.start..cut);
    }
}

/// `words` joined the way the backend's own `text` joins them: with a
/// space, or with none where the text runs two of them together, or holds
/// no space at all (Chinese, Japanese).
pub(crate) fn join_words<'a>(text: &str, words: impl IntoIterator<Item = &'a str>) -> String {
    let unspaced = !text.contains(char::is_whitespace);
    let mut out = String::new();
    let mut previous: Option<&str> = None;
    for word in words {
        if let Some(previous) = previous {
            let spaced = text.contains(&format!("{previous} {word}"));
            let glued = !spaced && (unspaced || text.contains(&format!("{previous}{word}")));
            if !glued {
                out.push(' ');
            }
        }
        out.push_str(word);
        previous = Some(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real Groq `whisper-large-v3-turbo` response with
    /// `timestamp_granularities[]=segment` and `=word`.
    const GROQ_VERBOSE: &str = r#"{
        "task": "transcribe",
        "language": "English",
        "duration": 7.4,
        "text": " Hello there. How are you doing today?",
        "segments": [
            {"id": 0, "seek": 0, "start": 0.0, "end": 1.52, "text": " Hello there.",
             "tokens": [50365, 2425], "temperature": 0.0, "avg_logprob": -0.21,
             "compression_ratio": 0.8, "no_speech_prob": 0.01},
            {"id": 1, "seek": 0, "start": 2.0, "end": 4.1, "text": " How are you doing today?",
             "tokens": [50465], "temperature": 0.0, "avg_logprob": -0.3,
             "compression_ratio": 0.9, "no_speech_prob": 0.02},
            {"id": 2, "seek": 0, "start": 5.0, "end": 7.4, "text": " Thank you.",
             "tokens": [50615], "temperature": 0.0, "avg_logprob": -1.4,
             "compression_ratio": 0.6, "no_speech_prob": 0.93}
        ],
        "words": [
            {"word": "Hello", "start": 0.0, "end": 0.6},
            {"word": "there.", "start": 0.6, "end": 1.5},
            {"word": "How", "start": 2.0, "end": 2.3},
            {"word": "are", "start": 2.3, "end": 2.5},
            {"word": "you", "start": 2.5, "end": 2.8},
            {"word": "doing", "start": 2.8, "end": 3.3},
            {"word": "today?", "start": 3.3, "end": 4.1}
        ]
    }"#;

    /// OpenAI `whisper-1` verbose_json: same schema, and with segment-only
    /// granularity no `words` key at all.
    const WHISPER1_VERBOSE: &str = r#"{
        "task": "transcribe",
        "language": "english",
        "duration": 3.2,
        "text": "Ship it on Friday.",
        "segments": [
            {"id": 0, "seek": 0, "start": 0.24, "end": 3.2, "text": " Ship it on Friday.",
             "tokens": [1], "temperature": 0.0, "avg_logprob": -0.18,
             "compression_ratio": 0.7, "no_speech_prob": 0.004}
        ]
    }"#;

    #[test]
    fn groq_segments_carry_times_and_words() {
        let t = parse_verbose_segments(GROQ_VERBOSE).unwrap();
        assert_eq!(t.segments.len(), 2, "the no-speech segment must be dropped");
        let first = &t.segments[0];
        assert_eq!(
            (first.start_ms, first.end_ms),
            (0, 1500),
            "narrowed to its words"
        );
        assert_eq!(first.text, "Hello there.");
        let words: Vec<_> = first
            .words
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(words, ["Hello", "there."]);
        let second = &t.segments[1];
        assert_eq!((second.start_ms, second.end_ms), (2000, 4100));
        assert_eq!(second.words.as_ref().unwrap().len(), 5);
    }

    #[test]
    fn whisper1_segments_without_words() {
        let t = parse_verbose_segments(WHISPER1_VERBOSE).unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.segments[0].start_ms, 240);
        assert_eq!(t.segments[0].end_ms, 3200);
        assert_eq!(t.segments[0].text, "Ship it on Friday.");
        assert!(t.segments[0].words.is_none());
    }

    #[test]
    fn no_speech_needs_both_conditions() {
        assert!(is_no_speech(Some(0.9), Some(-1.5)));
        // Confident decode of quiet speech survives.
        assert!(!is_no_speech(Some(0.9), Some(-0.2)));
        // Low logprob but clearly speech survives.
        assert!(!is_no_speech(Some(0.1), Some(-1.5)));
        assert!(!is_no_speech(None, Some(-3.0)));
    }

    #[test]
    fn missing_segments_becomes_one_segment() {
        let body = r#"{"text": " hi all ", "words": [
            {"word": "hi", "start": 1.0, "end": 1.2},
            {"word": "all", "start": 1.3, "end": 1.6}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!((t.segments[0].start_ms, t.segments[0].end_ms), (1000, 1600));
        assert_eq!(t.segments[0].text, "hi all");
    }

    #[test]
    fn empty_response_is_empty_transcript() {
        assert!(parse_verbose_segments(r#"{"text": ""}"#)
            .unwrap()
            .segments
            .is_empty());
        assert!(parse_verbose_segments(r#"{"text": "", "segments": []}"#)
            .unwrap()
            .segments
            .is_empty());
    }

    #[test]
    fn segment_is_narrowed_to_its_words() {
        // Real Groq shape: speech starts at 7 s, the segment claims 0 s.
        let body = r#"{"text": "Thanks.", "segments": [
            {"start": 0.0, "end": 10.4, "text": " Thanks. Ready to ship.",
             "avg_logprob": -0.2, "no_speech_prob": 0.05}],
            "words": [{"word": "Thanks.", "start": 7.0, "end": 7.6},
                      {"word": "Ready", "start": 8.0, "end": 8.3},
                      {"word": "to", "start": 8.3, "end": 8.4},
                      {"word": "ship.", "start": 8.4, "end": 9.9}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        assert_eq!((t.segments[0].start_ms, t.segments[0].end_ms), (7000, 9900));
    }

    /// A diarizing sidecar's label, a string or a number, stays on its
    /// segment; Groq and OpenAI send none.
    #[test]
    fn segment_speaker_is_kept() {
        let body = r#"{"text": "hi. yes", "segments": [
            {"start": 0.5, "end": 1.25, "text": " hi.", "speaker": "SPEAKER_00"},
            {"start": 2.0, "end": 2.4, "text": "yes", "speaker": 1},
            {"start": 3.0, "end": 3.4, "text": "no", "speaker": null}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        let speakers: Vec<_> = t.segments.iter().map(|s| s.speaker.as_deref()).collect();
        assert_eq!(speakers, [Some("SPEAKER_00"), Some("1"), None]);
        let t = parse_verbose_segments(GROQ_VERBOSE).unwrap();
        assert!(t.segments.iter().all(|s| s.speaker.is_none()));
    }

    fn word(start_ms: u64, end_ms: u64, text: &str) -> Word {
        Word {
            start_ms,
            end_ms,
            text: text.to_string(),
            speaker: None,
        }
    }

    /// A segment over `words`, its text their own joined with spaces.
    fn spoken(words: Vec<Word>) -> Segment {
        let text = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Segment {
            start_ms: words[0].start_ms,
            end_ms: words[words.len() - 1].end_ms,
            text,
            speaker: None,
            words: Some(words),
        }
    }

    /// `(start_ms, end_ms, text)` of each piece.
    fn lines(pieces: &[Segment]) -> Vec<(u64, u64, &str)> {
        pieces
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.as_str()))
            .collect()
    }

    #[test]
    fn splits_after_sentence_punctuation() {
        let seg = spoken(vec![
            word(0, 400, "Hello"),
            word(400, 900, "there."),
            word(1000, 1200, "How"),
            word(1200, 1400, "are"),
            word(1400, 1800, "you?"),
            word(1900, 2300, "Fine!"),
            word(2300, 2600, "So…"),
            word(2700, 3000, "go"),
        ]);
        let pieces = split_sentences(seg);
        assert_eq!(
            lines(&pieces),
            [
                (0, 900, "Hello there."),
                (1000, 1800, "How are you?"),
                (1900, 2300, "Fine!"),
                (2300, 2600, "So…"),
                (2700, 3000, "go"),
            ]
        );
        let counts: Vec<usize> = pieces
            .iter()
            .map(|s| s.words.as_ref().unwrap().len())
            .collect();
        assert_eq!(counts, [2, 3, 1, 1, 1], "each piece keeps its words");
    }

    #[test]
    fn splits_at_a_pause_of_a_second() {
        let seg = spoken(vec![
            word(0, 200, "so"),
            // 999 ms: still the same line.
            word(1199, 1400, "we"),
            word(1400, 1700, "ship"),
            // 1000 ms: a new one.
            word(2700, 3000, "Friday"),
        ]);
        assert_eq!(
            lines(&split_sentences(seg)),
            [(0, 1700, "so we ship"), (2700, 3000, "Friday")]
        );
    }

    /// `count` words of 500 ms from `from_ms`, 100 ms apart, named from `first`.
    fn run(from_ms: u64, count: u64, first: u64) -> Vec<Word> {
        (0..count)
            .map(|i| {
                let start = from_ms + i * 600;
                word(start, start + 500, &format!("w{}", first + i))
            })
            .collect()
    }

    /// 31.2 s with no punctuation and no pause of a second: cut once, at
    /// the largest pause (800 ms), not the next largest (700 ms).
    #[test]
    fn caps_a_long_piece_at_its_largest_pause() {
        let mut words = run(0, 20, 0); // ends at 11.9 s
        words.extend(run(12_700, 10, 20)); // 800 ms later, ends at 18.6 s
        words.extend(run(19_300, 20, 30)); // 700 ms later, ends at 31.2 s
        let pieces = split_sentences(spoken(words));
        let spans: Vec<(u64, u64)> = pieces.iter().map(|s| (s.start_ms, s.end_ms)).collect();
        assert_eq!(spans, [(0, 11_900), (12_700, 31_200)]);
        assert!(pieces[0].text.starts_with("w0 w1 ") && pieces[0].text.ends_with(" w19"));
        assert!(pieces[1].text.starts_with("w20 ") && pieces[1].text.ends_with(" w49"));
    }

    /// Parakeet on a quiet voice: no punctuation, and every word stretched
    /// over the pause after it, so all pauses read 0. The 70 s run halves,
    /// and halves again, rather than shedding a word at a time.
    #[test]
    fn halves_an_unpunctuated_run_without_pauses() {
        let words: Vec<Word> = (0..100)
            .map(|i| word(i * 700, (i + 1) * 700, &format!("w{i}")))
            .collect();
        let pieces = split_sentences(spoken(words));
        let spans: Vec<(u64, u64)> = pieces.iter().map(|s| (s.start_ms, s.end_ms)).collect();
        assert_eq!(
            spans,
            [
                (0, 17_500),
                (17_500, 35_000),
                (35_000, 52_500),
                (52_500, 70_000)
            ]
        );
        assert!(pieces.iter().all(|s| s.words.as_ref().unwrap().len() == 25));
        assert!(pieces[1].text.starts_with("w25 w26 ") && pieces[1].text.ends_with(" w49"));
    }

    /// One word longer than the cap has nothing to cut at, and stays.
    #[test]
    fn a_single_long_word_stays_whole() {
        let seg = spoken(vec![word(0, 1000, "Hi."), word(2000, 40_000, "hmmm")]);
        assert_eq!(
            lines(&split_sentences(seg)),
            [(0, 1000, "Hi."), (2000, 40_000, "hmmm")]
        );
    }

    /// Whisper words carry a leading space (OpenRouter's
    /// whisper-large-v3-turbo). Here a number comes as two words the text
    /// glues, so the words, not the text's tokens, make each piece's text:
    /// trimmed, then joined with one space, or none where the text glues
    /// them.
    #[test]
    fn whisper_words_with_a_leading_space_join_with_one() {
        let body = r#"{"text": " We met in 1990. Okay, first item.", "segments": [
            {"start": 0.0, "end": 4.0, "text": " We met in 1990. Okay, first item."}],
            "words": [{"word": " We", "start": 0.2, "end": 0.4},
                      {"word": " met", "start": 0.4, "end": 0.6},
                      {"word": " in", "start": 0.6, "end": 0.7},
                      {"word": " 19", "start": 0.7, "end": 0.9},
                      {"word": "90.", "start": 0.9, "end": 1.1},
                      {"word": " Okay,", "start": 1.5, "end": 1.9},
                      {"word": " first", "start": 1.9, "end": 2.2},
                      {"word": " item.", "start": 2.2, "end": 2.6}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        let seg = &t.segments[0];
        assert_ne!(
            seg.words.as_ref().unwrap().len(),
            seg.text.split_whitespace().count(),
            "the words, not the tokens, make the text"
        );
        let pieces: Vec<Segment> = t.segments.into_iter().flat_map(split_sentences).collect();
        assert_eq!(
            lines(&pieces),
            [
                (200, 1100, "We met in 1990."),
                (1500, 2600, "Okay, first item.")
            ]
        );
    }

    /// Text written without spaces stays without them.
    #[test]
    fn unspaced_text_joins_without_spaces() {
        let seg = Segment {
            text: "好的。我们开始吧。".to_string(),
            ..spoken(vec![
                word(0, 500, "好的。"),
                word(600, 900, "我们"),
                word(900, 1500, "开始吧。"),
            ])
        };
        assert_eq!(
            lines(&split_sentences(seg)),
            [(0, 500, "好的。"), (600, 1500, "我们开始吧。")]
        );
    }

    /// A segment that needs no cut keeps the server's own text (here a
    /// comma no word carries), as does one with a single word or none.
    #[test]
    fn a_segment_needing_no_split_is_kept_as_it_is() {
        let seg = Segment {
            text: "Ship it on Friday, okay".to_string(),
            ..spoken(vec![
                word(0, 300, "Ship"),
                word(300, 400, "it"),
                word(400, 600, "on"),
                word(600, 1100, "Friday"),
                word(1200, 1500, "okay"),
            ])
        };
        assert_eq!(split_sentences(seg.clone()), [seg]);
        let one = spoken(vec![word(0, 40_000, "Hello.")]);
        assert_eq!(split_sentences(one.clone()), [one]);
        let wordless = Segment {
            start_ms: 0,
            end_ms: 45_000,
            text: "A. B.".to_string(),
            speaker: None,
            words: None,
        };
        assert_eq!(split_sentences(wordless.clone()), [wordless]);
    }

    #[test]
    fn pieces_keep_the_speaker() {
        let seg = Segment {
            speaker: Some("SPEAKER_01".to_string()),
            ..spoken(vec![word(0, 400, "Yes."), word(500, 900, "Agreed.")])
        };
        let pieces = split_sentences(seg);
        assert_eq!(pieces.len(), 2);
        assert!(pieces
            .iter()
            .all(|s| s.speaker.as_deref() == Some("SPEAKER_01")));
    }

    /// `null` reads as a missing key, at the top and in a segment.
    #[test]
    fn nulls_read_as_missing_keys() {
        let t =
            parse_verbose_segments(r#"{"text": null, "segments": null, "words": null}"#).unwrap();
        assert!(t.segments.is_empty());
        let body = r#"{"text": "yes", "words": null, "segments": [
            {"start": 0.0, "end": 1.0, "text": null},
            {"start": 1.0, "end": 2.0, "text": " yes"}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        assert_eq!(lines(&t.segments), [(1000, 2000, "yes")]);
        assert!(t.segments[0].words.is_none());
        // Text and `"segments": null`: one segment, as with no key.
        let t = parse_verbose_segments(r#"{"text": "hi", "segments": null}"#).unwrap();
        assert_eq!(lines(&t.segments), [(0, 0, "hi")]);
    }

    /// WhisperX leaves a numeral unaligned, without `start` and `end`: the
    /// word takes them from its neighbours, the end of the word before and
    /// the start of the one after. A word with one time keeps it and takes
    /// the other, here the segment's end.
    #[test]
    fn a_word_without_times_takes_them_from_its_neighbours() {
        let body = r#"{"text": "It was 1990. then", "segments": [
            {"start": 0.0, "end": 2.0, "text": "It was 1990. then"}],
            "words": [{"word": "It", "start": 0.1, "end": 0.3},
                      {"word": "was", "start": 0.3, "end": 0.6},
                      {"word": "1990.", "score": 0.0},
                      {"word": "then", "start": 1.6, "end": null}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        assert_eq!(lines(&t.segments), [(100, 2000, "It was 1990. then")]);
        let words: Vec<(u64, u64, &str)> = t.segments[0]
            .words
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| (w.start_ms, w.end_ms, w.text.as_str()))
            .collect();
        assert_eq!(
            words,
            [
                (100, 300, "It"),
                (300, 600, "was"),
                (600, 1600, "1990."),
                (1600, 2000, "then")
            ]
        );
    }

    /// An unaligned word before every timed one starts with the first
    /// segment, or, with no segment, at the next word; one after every
    /// timed one ends with the last segment, or where it starts. With no
    /// word timed at all there are no words, as before.
    #[test]
    fn an_unaligned_word_at_either_end_takes_the_segments_bounds() {
        let body = r#"{"text": "So, 1990. Bye 2000", "segments": [
            {"start": 0.5, "end": 2.0, "text": "So, 1990."},
            {"start": 2.0, "end": 3.0, "text": "Bye 2000"}],
            "words": [{"word": "So,"},
                      {"word": "1990.", "start": 1.0, "end": 1.8},
                      {"word": "Bye", "start": 2.1, "end": 2.4},
                      {"word": "2000"}]}"#;
        let t = parse_verbose_segments(body).unwrap();
        let spans: Vec<Vec<(u64, u64)>> = t
            .segments
            .iter()
            .map(|s| {
                let words = s.words.as_ref().unwrap();
                words.iter().map(|w| (w.start_ms, w.end_ms)).collect()
            })
            .collect();
        assert_eq!(
            spans,
            [
                vec![(500, 1000), (1000, 1800)],
                vec![(2100, 2400), (2400, 3000)]
            ]
        );

        let words_only = r#"{"text": "So, 1990. Bye 2000", "words": [
            {"word": "So,"},
            {"word": "1990.", "start": 1.0, "end": 1.8},
            {"word": "Bye", "start": 2.1, "end": 2.4},
            {"word": "2000"}]}"#;
        let reply = VerboseResponse::parse(words_only).unwrap();
        let words: Vec<(u64, u64)> = reply
            .timed_words()
            .iter()
            .map(|w| (w.start_ms, w.end_ms))
            .collect();
        assert_eq!(
            words,
            [(1000, 1000), (1000, 1800), (2100, 2400), (2400, 2400)]
        );

        let untimed = r#"{"text": "It was 1990.", "words": [
            {"word": "It"}, {"word": "was", "start": 0.3}, {"word": "1990."}]}"#;
        assert!(VerboseResponse::parse(untimed)
            .unwrap()
            .timed_words()
            .is_empty());
    }

    /// OpenAI `whisper-1` times bare words under a punctuated text. As many
    /// tokens as words: the tokens find the sentence ends and make the
    /// pieces' text, punctuation kept; the words stay as they came.
    #[test]
    fn bare_words_under_punctuated_text_split_at_its_sentences() {
        let seg = Segment {
            text: "Hello there. How are you? Fine, thanks.".to_string(),
            ..spoken(vec![
                word(0, 400, "Hello"),
                word(400, 900, "there"),
                word(1000, 1200, "How"),
                word(1200, 1400, "are"),
                word(1400, 1800, "you"),
                word(1900, 2300, "Fine"),
                word(2300, 2700, "thanks"),
            ])
        };
        let pieces = split_sentences(seg);
        assert_eq!(
            lines(&pieces),
            [
                (0, 900, "Hello there."),
                (1000, 1800, "How are you?"),
                (1900, 2700, "Fine, thanks."),
            ]
        );
        assert_eq!(pieces[1].words.as_ref().unwrap()[2].text, "you");
    }

    /// Words that neither match the text's tokens in number nor rebuild it
    /// (a word the server left out) cannot say where its sentences end: no cut
    /// at a sentence or a pause, so the segment keeps its text, and only the
    /// length cap cuts a longer one, its pieces taking the words' text.
    #[test]
    fn words_that_do_not_rebuild_the_text_only_cut_at_the_cap() {
        let seg = Segment {
            text: "Okay. We meet at 10 tomorrow.".to_string(),
            ..spoken(vec![
                word(0, 400, "Okay."),
                // A pause of 1.6 s, and no word for "10".
                word(2000, 2200, "We"),
                word(2200, 2500, "meet"),
                word(2500, 2700, "at"),
                word(2800, 3400, "tomorrow."),
            ])
        };
        assert_eq!(split_sentences(seg.clone()), [seg]);

        // 31.2 s, the text one token longer than its words.
        let mut words = run(0, 20, 0); // ends at 11.9 s
        words.extend(run(12_700, 10, 20)); // 800 ms later, ends at 18.6 s
        words.extend(run(19_300, 20, 30)); // 700 ms later, ends at 31.2 s
        let text = format!("{} more.", spoken(words.clone()).text);
        let pieces = split_sentences(Segment {
            text,
            ..spoken(words)
        });
        let spans: Vec<(u64, u64)> = pieces.iter().map(|s| (s.start_ms, s.end_ms)).collect();
        assert_eq!(spans, [(0, 11_900), (12_700, 31_200)]);
        assert!(pieces[0].text.starts_with("w0 w1 ") && pieces[0].text.ends_with(" w19"));
        assert!(pieces[1].text.starts_with("w20 ") && pieces[1].text.ends_with(" w49"));
    }

    /// A sentence end inside closing quotes or brackets still ends it.
    #[test]
    fn a_sentence_end_inside_closing_quotes_still_ends_it() {
        let seg = spoken(vec![
            word(0, 300, "He"),
            word(300, 600, "said"),
            word(600, 900, "\"we're"),
            word(900, 1300, "done.\""),
            word(1400, 1700, "Then"),
            word(1700, 2000, "left."),
        ]);
        assert_eq!(
            lines(&split_sentences(seg)),
            [
                (0, 1300, "He said \"we're done.\""),
                (1400, 2000, "Then left.")
            ]
        );
        let seg = Segment {
            text: "「好。」我们走吧。".to_string(),
            ..spoken(vec![
                word(0, 500, "「好。」"),
                word(600, 900, "我们"),
                word(900, 1500, "走吧。"),
            ])
        };
        assert_eq!(
            lines(&split_sentences(seg)),
            [(0, 500, "「好。」"), (600, 1500, "我们走吧。")]
        );
    }
}
