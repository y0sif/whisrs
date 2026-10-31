//! Silence-delimited phrase segmentation for local streaming ASR.
//!
//! [`PhraseSplitter`] is a pure, incremental splitter: feed it i16 PCM samples
//! and it yields complete phrases — runs of speech delimited by a configurable
//! stretch of silence. Each returned phrase is a contiguous slice of the input
//! audio (plus a little silence padding on both sides), so a decoder can
//! transcribe each phrase exactly once with no overlap and no deduplication.
//!
//! Rules:
//! - Each fixed-size frame (100 ms in production) is speech or silence by
//!   the configured detector (`[audio] vad`): with earshot, a frame is
//!   speech when a 16 ms speech frame of a streaming
//!   [`SpeechDetector`] ends inside it, so its flag is final the moment the
//!   frame is complete; with the RMS test, when its RMS reaches the
//!   threshold. Every frame's RMS energy is kept too, to pick the quietest
//!   frame at a forced cut.
//! - A phrase ends once `split_silence_frames` consecutive silent frames
//!   follow speech.
//! - Phrases with fewer than `min_speech_frames` frames containing speech are
//!   discarded as noise — unless the phrase continues a forced cut, in which
//!   case a single speech frame is enough to keep it (speech was flowing at
//!   the cut, so a short tail is real speech, not a noise blip). A
//!   continuation with zero speech frames is still discarded.
//! - Emitted phrases include up to `pad_frames` of surrounding silence.
//! - A phrase that reaches `max_phrase_frames` (the soft cap) without a
//!   silence split is cut at the quietest *silent* frame in the recent
//!   lookback window. If every recent frame is voiced, the cut is deferred
//!   (the search re-runs each frame) until the first silent frame
//!   arrives or the phrase reaches `hard_max_phrase_frames` (the hard
//!   ceiling, sized to stay inside whisper's 30 s context window), where it
//!   is cut at the quietest recent frame even if voiced, so continuous
//!   pause-free speech still emits. No padding is added at a forced boundary
//!   (the audio is contiguous, and padding would duplicate samples).
//! - Each emitted [`Phrase`] records whether it *starts* at a forced cut
//!   (`continuation`), so a decoder can treat it as mid-sentence text.
//! - [`PhraseSplitter::flush`] emits the trailing in-progress phrase at end of
//!   stream (if it carries enough speech) exactly once.
//!
//! Local whisper recordings then group consecutive phrases into decode
//! windows ([`decode_windows`]): one `full()` call per window instead of per
//! phrase. A batch dictation longer than one window is grouped much the same
//! way, into windows of about even length ([`dictation_windows`]).

use std::collections::VecDeque;
use std::ops::Range;

use audio_silence_gate::{
    rms_energy, FrameDecision, SpeechDetector, Vad, VadConfig, VAD_FRAME, VAD_SAMPLE_RATE,
};

/// Analysis frame length in milliseconds.
const FRAME_MS: usize = 100;
/// Silence padding kept around each phrase, in milliseconds.
const PAD_MS: usize = 100;
/// Minimum speech (frames containing speech, ~ms) required to keep a phrase.
const MIN_PHRASE_SPEECH_MS: usize = 250;
/// Soft cap on phrase length: past this, cut at the quietest silent frame
/// found in the lookback window.
const MAX_PHRASE_SECS: usize = 20;
/// Hard ceiling on phrase length: past this, cut at the quietest recent
/// frame even if it is voiced. Sized to stay inside whisper's 30 s context
/// window.
const HARD_MAX_PHRASE_SECS: usize = 28;
/// How far back to look for the quietest frame when force-splitting.
const QUIET_SEARCH_MS: usize = 2000;
/// Longest decode window a phrase joins: the hard phrase ceiling, so a
/// window fits in one 30 s mel window. A window of one phrase can run a
/// little past it: a phrase flushed at the end of the stream keeps its
/// 100 ms leading pad before up to 28 s of speech. That bounds the audio,
/// not the decoder passes: whisper can still take a second pass over the
/// rest of a window, which is why recording windows decode without carried
/// text (`n_max_text_ctx` 0).
pub const MAX_DECODE_WINDOW_SECS: usize = HARD_MAX_PHRASE_SECS;
/// A run of at least this much exact-zero audio is a join: the second of
/// digital silence a condensed recording chunk puts between two speech regions
/// (`crate::recording::speech::JOIN_GAP`). The runs are found in the raw
/// samples, so a join always shows its full second; the threshold sits a
/// little under it. A microphone never records that many exact zeros (its
/// noise floor keeps flipping the low bits); a system track can, when
/// nothing plays, and then grouping merely stops there as at a join.
const JOIN_ZERO_MS: usize = 900;

/// A completed phrase of contiguous audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// The phrase samples (contiguous input audio plus silence padding).
    pub samples: Vec<i16>,
    /// True when this phrase starts at a forced cut boundary (mid-speech),
    /// i.e. it continues the previous phrase rather than starting fresh.
    pub continuation: bool,
    /// Index of the phrase's first sample in the whole stream fed so far,
    /// so a caller can place the phrase on the input's timeline.
    pub start_sample: u64,
}

/// Consecutive phrases decoded together, with the pauses between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeWindow {
    /// The window's samples in the input: from its first phrase's start to
    /// its last phrase's end.
    pub range: Range<usize>,
    /// Whether the first phrase starts at a forced cut
    /// ([`Phrase::continuation`]), so the window's text continues a sentence.
    pub continuation: bool,
    /// How many phrases the window holds.
    pub phrases: usize,
}

/// Group the `phrases` a splitter emitted for `samples` (fed from sample 0)
/// into decode windows: the next phrase joins the current window while the
/// window would still end within [`MAX_DECODE_WINDOW_SECS`] of its start
/// and no join ([`JOIN_ZERO_MS`] of exact zeros) lies between them.
///
/// Every phrase lands in exactly one window, in order. Whisper pads each
/// call to a 30 s window, so decoding phrases one by one costs a whole
/// encoder pass for a two-second "yeah"; a shared window costs one, and
/// keeps the pauses between its phrases as natural context. A join marks
/// two speech regions that may be minutes apart, so it always ends a window.
pub fn decode_windows(
    samples: &[i16],
    phrases: &[Phrase],
    sample_rate: usize,
) -> Vec<DecodeWindow> {
    let joins = zero_runs(samples, JOIN_ZERO_MS * sample_rate / 1000);
    group_windows(phrases, sample_rate, &joins, false)
}

/// [`decode_windows`] for one continuous take, a dictation, with the
/// windows evened out: a window's share is the span from its start to the
/// last phrase's end, split evenly over the fewest windows of
/// [`MAX_DECODE_WINDOW_SECS`] that span needs, and the next phrase joins the
/// window only if that leaves the window's length at least as close to its
/// share as it is now. Packing to the cap would leave a last window of a
/// second or two, with little speech to anchor its decode, where trailing
/// noise can come out as a standalone hallucination; so would closing a
/// window only once it reaches its share, when the phrase that reaches it
/// is long (phrases of 14 s, 13 s and 2 s would be windows of 27.5 s and
/// 2 s). The share is worked out again at each window's start, so what one
/// window ends over or under its share is spread over the windows left
/// instead of landing on the last one.
///
/// A dictation has no joins, so only the length ends a window: a noise gate
/// or a muted mic records its pauses as exact zeros, and ending a window at
/// each of those would cost a whole encoder pass per phrase.
pub fn dictation_windows(phrases: &[Phrase], sample_rate: usize) -> Vec<DecodeWindow> {
    group_windows(phrases, sample_rate, &[], true)
}

/// The grouping behind [`decode_windows`] and [`dictation_windows`]: the
/// next phrase joins the current window while the window would still end
/// within [`MAX_DECODE_WINDOW_SECS`] of its start, no join lies between
/// them and, when `even`, joining leaves the window's length at least as
/// close to its even share of the span left from its start as it is now.
///
/// Every window opens with a phrase and the loop visits each phrase once,
/// so no window is empty and the grouping always ends.
fn group_windows(
    phrases: &[Phrase],
    sample_rate: usize,
    joins: &[Range<usize>],
    even: bool,
) -> Vec<DecodeWindow> {
    let max_len = MAX_DECODE_WINDOW_SECS * sample_rate;
    let span_end = phrases
        .last()
        .map_or(0, |p| p.start_sample as usize + p.samples.len());
    // The current window's even share, set when it opens; read only when
    // `even`.
    let mut target: usize = 0;
    let mut windows: Vec<DecodeWindow> = Vec::new();
    for phrase in phrases {
        let start = phrase.start_sample as usize;
        let end = start + phrase.samples.len();
        if let Some(window) = windows.last_mut() {
            // A join between them reaches past the window's end into this
            // phrase. Measured from the window's end, not its last phrase's
            // start: a phrase's leading pad may lie in the join before it.
            let joined = joins
                .iter()
                .any(|j| j.start < end && j.end > window.range.end);
            // Without `even`, a window packs phrases up to the cap. With
            // it, a tie joins.
            let evens_out = !even || {
                let grown = window.range.end.max(end) - window.range.start;
                target.abs_diff(grown) <= target.abs_diff(window.range.len())
            };
            if !joined && end.saturating_sub(window.range.start) <= max_len && evens_out {
                window.range.end = window.range.end.max(end);
                window.phrases += 1;
                continue;
            }
        }
        if even {
            let left = span_end.saturating_sub(start);
            target = left / left.div_ceil(max_len).max(1);
        }
        windows.push(DecodeWindow {
            range: start..end,
            continuation: phrase.continuation,
            phrases: 1,
        });
    }
    windows
}

/// Runs of at least `min` exact-zero samples.
fn zero_runs(samples: &[i16], min: usize) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &s) in samples.iter().enumerate() {
        match (s == 0, start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                if i - from >= min {
                    runs.push(from..i);
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        if samples.len() - from >= min {
            runs.push(from..samples.len());
        }
    }
    runs
}

/// What decides whether a splitter frame holds speech.
enum SpeechSource {
    /// The 0.1.31 rule: a frame is speech when its RMS reaches the threshold.
    Rms(f64),
    /// A speech detector over the stream ([`VadFrames`]).
    Vad(Box<VadFrames>),
    /// Flags read from a script, one per frame, then silence.
    #[cfg(test)]
    Script(VecDeque<bool>),
}

/// A [`SpeechDetector`]'s 16 ms decisions turned into splitter frames: a
/// frame is speech when a 16 ms speech frame ends inside it. Each 16 ms frame
/// counts toward exactly one splitter frame, and a frame's flag is final
/// once its last sample is fed, so the splitter never waits on the detector.
struct VadFrames {
    detector: SpeechDetector,
    /// Flags of the frames not analyzed yet, the next one first.
    ahead: VecDeque<bool>,
    /// Frames analyzed since the detector started or was reset.
    analyzed: u64,
}

impl VadFrames {
    fn new(cfg: VadConfig) -> Self {
        Self {
            detector: SpeechDetector::new(cfg, VAD_SAMPLE_RATE),
            ahead: VecDeque::new(),
            analyzed: 0,
        }
    }

    /// Feed the stream's next samples (`frame_len`: samples per splitter
    /// frame).
    fn feed(&mut self, samples: &[i16], frame_len: usize) {
        let Self {
            detector,
            ahead,
            analyzed,
        } = self;
        detector.feed(samples, |decision| {
            mark_frame(ahead, *analyzed, decision, frame_len);
        });
    }

    /// The flag of the next frame, which the caller analyzes now.
    fn next(&mut self) -> bool {
        self.analyzed += 1;
        self.ahead.pop_front().unwrap_or(false)
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.ahead.clear();
        self.analyzed = 0;
    }
}

/// Mark the splitter frame a speech `decision` ends in, in `ahead` (the
/// flags from frame `analyzed` on). Decisions arrive only for complete 16 ms
/// frames, so one never lands on a frame already analyzed: that frame was
/// complete, and its 16 ms frames decided, before it was analyzed.
fn mark_frame(
    ahead: &mut VecDeque<bool>,
    analyzed: u64,
    decision: FrameDecision,
    frame_len: usize,
) {
    if !decision.speech {
        return;
    }
    let last_sample = decision.start_sample + VAD_FRAME as u64 - 1;
    let Some(at) = (last_sample / frame_len as u64).checked_sub(analyzed) else {
        return;
    };
    let at = at as usize;
    if ahead.len() <= at {
        ahead.resize(at + 1, false);
    }
    ahead[at] = true;
}

/// Incremental silence-delimited phrase splitter over i16 PCM samples.
pub struct PhraseSplitter {
    // Configuration (all in frames unless noted).
    frame_len: usize,
    source: SpeechSource,
    split_silence_frames: usize,
    min_speech_frames: usize,
    max_phrase_frames: usize,
    pad_frames: usize,
    quiet_search_frames: usize,
    hard_max_phrase_frames: usize,

    // Rolling state. `buffer` holds samples not yet consumed by an emitted or
    // discarded phrase; `energies[i]` is the RMS of frame `i` of `buffer`,
    // `speech[i]` whether it holds speech.
    buffer: Vec<i16>,
    energies: Vec<f64>,
    speech: Vec<bool>,
    in_phrase: bool,
    /// First frame of the emitted slice (speech onset minus padding).
    emit_start_frame: usize,
    /// First frame that contained speech in the current phrase.
    phrase_start_frame: usize,
    /// Frames containing speech in the current phrase.
    speech_frames: usize,
    /// Consecutive silent frames at the current analysis position.
    trailing_silence: usize,
    /// True while the open phrase started at a forced cut boundary.
    continuation: bool,
    /// Samples dropped from the front of `buffer` since the stream began, so
    /// `consumed + k` is the stream index of `buffer[k]`.
    consumed: u64,
}

impl PhraseSplitter {
    /// Create a splitter with production defaults for the given sample rate.
    ///
    /// `cfg` decides speech: [`Vad::Earshot`] at 16 kHz runs a streaming
    /// [`SpeechDetector`]; [`Vad::Rms`], or any other rate, compares each
    /// frame's RMS with [`VadConfig::rms_threshold`], the 0.1.31 rule.
    /// `phrase_silence_ms` is how much continuous silence ends a phrase.
    pub fn new(sample_rate: usize, cfg: VadConfig, phrase_silence_ms: u64) -> Self {
        let frame_len = (sample_rate * FRAME_MS / 1000).max(1);
        let mut splitter = Self::with_params(
            frame_len,
            cfg.rms_threshold,
            (phrase_silence_ms as usize).div_ceil(FRAME_MS).max(1),
            MIN_PHRASE_SPEECH_MS.div_ceil(FRAME_MS).max(1),
            (MAX_PHRASE_SECS * 1000 / FRAME_MS).max(2),
            PAD_MS / FRAME_MS,
            (QUIET_SEARCH_MS / FRAME_MS).max(1),
            (HARD_MAX_PHRASE_SECS * 1000 / FRAME_MS).max(2),
        );
        if cfg.vad == Vad::Earshot && sample_rate == VAD_SAMPLE_RATE as usize {
            splitter.source = SpeechSource::Vad(Box::new(VadFrames::new(cfg)));
        }
        splitter
    }

    /// Create a splitter with explicit frame-level parameters (used by
    /// tests), deciding speech with the RMS test at `threshold` (normalized,
    /// 0.0 to 1.0).
    ///
    /// `max_phrase_frames` is the soft cap (cut only at a silent frame);
    /// `hard_max_phrase_frames` is the hard ceiling (cut at the quietest
    /// recent frame regardless), clamped to at least the soft cap.
    #[allow(clippy::too_many_arguments)]
    pub fn with_params(
        frame_len: usize,
        threshold: f64,
        split_silence_frames: usize,
        min_speech_frames: usize,
        max_phrase_frames: usize,
        pad_frames: usize,
        quiet_search_frames: usize,
        hard_max_phrase_frames: usize,
    ) -> Self {
        let max_phrase_frames = max_phrase_frames.max(2);
        Self {
            frame_len: frame_len.max(1),
            source: SpeechSource::Rms(threshold),
            split_silence_frames: split_silence_frames.max(1),
            min_speech_frames: min_speech_frames.max(1),
            max_phrase_frames,
            pad_frames,
            quiet_search_frames: quiet_search_frames.max(1),
            hard_max_phrase_frames: hard_max_phrase_frames.max(max_phrase_frames),
            buffer: Vec::new(),
            energies: Vec::new(),
            speech: Vec::new(),
            in_phrase: false,
            emit_start_frame: 0,
            phrase_start_frame: 0,
            speech_frames: 0,
            trailing_silence: 0,
            continuation: false,
            consumed: 0,
        }
    }

    /// Feed samples; returns zero or more completed phrases in FIFO order.
    pub fn feed(&mut self, samples: &[i16]) -> Vec<Phrase> {
        self.buffer.extend_from_slice(samples);
        let frame_len = self.frame_len;
        if let SpeechSource::Vad(vad) = &mut self.source {
            vad.feed(samples, frame_len);
        }
        let mut out = Vec::new();
        // Analyze every complete frame not yet analyzed. `process_frame` may
        // drop consumed frames from the front, so recompute the index each
        // iteration.
        while (self.energies.len() + 1) * frame_len <= self.buffer.len() {
            let i = self.energies.len();
            let energy = rms_energy(&self.buffer[i * frame_len..(i + 1) * frame_len]);
            let speech = match &mut self.source {
                SpeechSource::Rms(threshold) => energy >= *threshold,
                SpeechSource::Vad(vad) => vad.next(),
                #[cfg(test)]
                SpeechSource::Script(flags) => flags.pop_front().unwrap_or(false),
            };
            self.energies.push(energy);
            self.speech.push(speech);
            self.process_frame(i, speech, &mut out);
        }
        out
    }

    /// End of stream: emit the trailing in-progress phrase, if it carries
    /// enough speech. Resets the splitter; subsequent calls return `None`.
    pub fn flush(&mut self) -> Option<Phrase> {
        // A partial (<1 frame) unanalyzed tail without an open phrase can hold
        // at most one frame of speech — below any sensible minimum. Discard.
        if !self.in_phrase || !self.keep_phrase() {
            self.clear();
            return None;
        }

        // Trim excess trailing silence down to the padding; keep the partial
        // unanalyzed tail when speech may still be running into it.
        let end_sample = if self.trailing_silence > self.pad_frames {
            let speech_end = self.energies.len() - self.trailing_silence;
            (speech_end + self.pad_frames) * self.frame_len
        } else {
            self.buffer.len()
        };
        let phrase = Phrase {
            samples: self.buffer[self.emit_start_frame * self.frame_len..end_sample].to_vec(),
            continuation: self.continuation,
            start_sample: self.stream_index(self.emit_start_frame),
        };
        self.clear();
        Some(phrase)
    }

    /// Noise gate: keep a phrase carrying at least `min_speech_frames` of
    /// speech — or any speech at all when it continues a forced cut, since
    /// speech was flowing at the cut and a short tail is real speech, not a
    /// noise blip. A continuation with zero speech frames is still discarded.
    fn keep_phrase(&self) -> bool {
        self.speech_frames >= self.min_speech_frames
            || (self.continuation && self.speech_frames >= 1)
    }

    /// Process the just-analyzed frame `i` (index into `energies`).
    fn process_frame(&mut self, i: usize, speech: bool, out: &mut Vec<Phrase>) {
        let silent = !speech;

        if !self.in_phrase {
            if silent {
                // Keep only `pad_frames` of leading silence so long pauses
                // don't grow the buffer.
                if self.energies.len() > self.pad_frames {
                    let drop = self.energies.len() - self.pad_frames;
                    self.drop_frames(drop);
                }
            } else {
                self.in_phrase = true;
                self.phrase_start_frame = i;
                self.emit_start_frame = i.saturating_sub(self.pad_frames);
                self.speech_frames = 1;
                self.trailing_silence = 0;
                // Natural onset: this phrase does not continue a forced cut.
                self.continuation = false;
            }
            return;
        }

        if silent {
            self.trailing_silence += 1;
            if self.trailing_silence >= self.split_silence_frames {
                self.end_phrase(i, out);
                return;
            }
        } else {
            self.speech_frames += 1;
            self.trailing_silence = 0;
        }

        // Length caps. Past the hard ceiling, cut unconditionally at the
        // quietest recent frame; past the soft cap, cut only at a genuinely
        // silent frame, deferring (re-checked every frame) until one
        // arrives.
        let phrase_len = i + 1 - self.phrase_start_frame;
        if phrase_len >= self.hard_max_phrase_frames {
            self.force_split(i, out);
        } else if phrase_len >= self.max_phrase_frames {
            self.try_soft_split(i, out);
        }
    }

    /// Natural end of phrase: enough consecutive silence after speech.
    fn end_phrase(&mut self, i: usize, out: &mut Vec<Phrase>) {
        // One past the last frame that contained speech.
        let speech_end = i + 1 - self.trailing_silence;
        let end_frame = (speech_end + self.pad_frames).min(self.energies.len());

        if self.keep_phrase() {
            out.push(Phrase {
                samples: self.buffer
                    [self.emit_start_frame * self.frame_len..end_frame * self.frame_len]
                    .to_vec(),
                continuation: self.continuation,
                start_sample: self.stream_index(self.emit_start_frame),
            });
        }

        self.in_phrase = false;
        self.continuation = false;
        self.speech_frames = 0;
        self.trailing_silence = 0;
        // Keep up to `pad_frames` of the gap as leading padding for the next
        // phrase; everything earlier is consumed.
        let keep_from = self
            .energies
            .len()
            .saturating_sub(self.pad_frames)
            .max(speech_end);
        self.drop_frames(keep_from);
    }

    /// Forced split at the hard ceiling: cut at the quietest recent frame
    /// boundary — voiced or not — so a pause-free phrase still emits before
    /// it outgrows the decoder's context window.
    fn force_split(&mut self, i: usize, out: &mut Vec<Phrase>) {
        let search_start = self.split_search_start(i);
        let mut split_frame = search_start;
        let mut min_energy = f64::INFINITY;
        for j in search_start..=i {
            if self.energies[j] < min_energy {
                min_energy = self.energies[j];
                split_frame = j;
            }
        }
        self.cut_at(split_frame, out);
    }

    /// Soft-cap split: cut at the quietest *silent* frame in the lookback
    /// window, if any. When every recent frame is voiced this does nothing:
    /// the search re-runs on each subsequent frame, so the first arriving
    /// silent frame becomes the cut point.
    fn try_soft_split(&mut self, i: usize, out: &mut Vec<Phrase>) {
        let search_start = self.split_search_start(i);
        let mut split_frame = None;
        let mut min_energy = f64::INFINITY;
        for j in search_start..=i {
            let e = self.energies[j];
            if !self.speech[j] && e < min_energy {
                min_energy = e;
                split_frame = Some(j);
            }
        }
        if let Some(split_frame) = split_frame {
            self.cut_at(split_frame, out);
        }
    }

    /// First frame eligible as a forced-cut point when processing frame `i`.
    fn split_search_start(&self, i: usize) -> usize {
        (i + 1)
            .saturating_sub(self.quiet_search_frames)
            .max(self.phrase_start_frame + 1)
    }

    /// Cut the open phrase at `split_frame`: emit everything before it; the
    /// split frame starts the remainder, which continues as an open phrase
    /// marked as a continuation. No padding is duplicated across the
    /// boundary (the audio is contiguous).
    fn cut_at(&mut self, split_frame: usize, out: &mut Vec<Phrase>) {
        out.push(Phrase {
            samples: self.buffer
                [self.emit_start_frame * self.frame_len..split_frame * self.frame_len]
                .to_vec(),
            continuation: self.continuation,
            start_sample: self.stream_index(self.emit_start_frame),
        });

        self.drop_frames(split_frame);
        self.phrase_start_frame = 0;
        self.emit_start_frame = 0;
        self.continuation = true;
        self.speech_frames = self.speech.iter().filter(|&&s| s).count();
        self.trailing_silence = self.speech.iter().rev().take_while(|&&s| !s).count();
    }

    /// Stream index of the first sample of buffered frame `frame`.
    fn stream_index(&self, frame: usize) -> u64 {
        self.consumed + (frame * self.frame_len) as u64
    }

    /// Drop the first `n` analyzed frames (and their samples) from the front.
    fn drop_frames(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.consumed += (n * self.frame_len) as u64;
        self.buffer.drain(..n * self.frame_len);
        self.energies.drain(..n);
        self.speech.drain(..n);
    }

    fn clear(&mut self) {
        self.consumed += self.buffer.len() as u64;
        self.buffer.clear();
        self.energies.clear();
        self.speech.clear();
        // The frame grid restarts at the new `consumed`, and the detector
        // with it.
        if let SpeechSource::Vad(vad) = &mut self.source {
            vad.reset();
        }
        self.in_phrase = false;
        self.emit_start_frame = 0;
        self.phrase_start_frame = 0;
        self.speech_frames = 0;
        self.trailing_silence = 0;
        self.continuation = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 10;
    const SPEECH: i16 = 1000; // normalized RMS ~0.031, above the 0.01 threshold
    const QUIET_SPEECH: i16 = 500; // ~0.015 — voiced but the quietest around

    /// frame_len=10, threshold=0.01, split after 3 silent frames, min 2 speech
    /// frames, soft cap at 100 frames, 1 pad frame, search 5 frames for the
    /// quietest split point, hard ceiling at 140 frames.
    fn splitter() -> PhraseSplitter {
        PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 100, 1, 5, 140)
    }

    /// Splitter that hits its caps quickly: soft cap 6 frames, search 3,
    /// hard ceiling `hard` frames.
    fn capped_splitter(hard: usize) -> PhraseSplitter {
        PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 6, 1, 3, hard)
    }

    fn frames(pattern: &[(i16, usize)]) -> Vec<i16> {
        let mut out = Vec::new();
        for &(value, count) in pattern {
            out.extend(std::iter::repeat_n(value, count * FRAME));
        }
        out
    }

    /// Concatenate the emitted phrases' samples in order.
    fn rejoin(phrases: &[Phrase]) -> Vec<i16> {
        phrases
            .iter()
            .flat_map(|p| p.samples.iter().copied())
            .collect()
    }

    #[test]
    fn basic_split_two_phrases() {
        let mut s = splitter();
        // 2 silence, 5 speech, 4 silence, 3 speech, 4 silence (frames).
        let audio = frames(&[(0, 2), (SPEECH, 5), (0, 4), (SPEECH, 3), (0, 4)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        // Phrase 1: 1 pad + 5 speech + 1 pad frames.
        assert_eq!(phrases[0].samples.len(), 7 * FRAME);
        // Phrase 2: 1 pad + 3 speech + 1 pad frames.
        assert_eq!(phrases[1].samples.len(), 5 * FRAME);
    }

    #[test]
    fn padding_included_around_speech() {
        let mut s = splitter();
        let audio = frames(&[(0, 3), (SPEECH, 4), (0, 4)]);
        let phrases = s.feed(&audio);

        assert_eq!(phrases.len(), 1);
        let p = &phrases[0].samples;
        assert_eq!(p.len(), 6 * FRAME);
        // Leading pad frame is silence, then speech, then trailing pad frame.
        assert!(p[..FRAME].iter().all(|&x| x == 0));
        assert!(p[FRAME..5 * FRAME].iter().all(|&x| x == SPEECH));
        assert!(p[5 * FRAME..].iter().all(|&x| x == 0));
    }

    #[test]
    fn no_leading_pad_when_speech_starts_at_zero() {
        let mut s = splitter();
        let audio = frames(&[(SPEECH, 4), (0, 4)]);
        let phrases = s.feed(&audio);

        assert_eq!(phrases.len(), 1);
        // No silence exists before the speech: 4 speech + 1 trailing pad.
        assert_eq!(phrases[0].samples.len(), 5 * FRAME);
        assert_eq!(phrases[0].samples[0], SPEECH);
    }

    #[test]
    fn short_blip_discarded_as_noise() {
        let mut s = splitter();
        // 1 speech frame < min_speech_frames (2).
        let audio = frames(&[(0, 2), (SPEECH, 1), (0, 4)]);
        let phrases = s.feed(&audio);
        assert!(phrases.is_empty());
        assert!(s.flush().is_none());
    }

    #[test]
    fn incremental_feeding_matches_single_feed() {
        let audio = frames(&[(0, 2), (SPEECH, 5), (0, 4), (SPEECH, 3), (0, 4)]);

        let mut whole = splitter();
        let mut expected = whole.feed(&audio);
        expected.extend(whole.flush());

        let mut incremental = splitter();
        let mut got = Vec::new();
        for chunk in audio.chunks(7) {
            got.extend(incremental.feed(chunk));
        }
        got.extend(incremental.flush());

        assert_eq!(expected, got);
    }

    #[test]
    fn cap_split_lands_on_dip_not_quiet_voiced_frame() {
        // The quiet-but-voiced frame at index 4 (rms ~0.015, above the 0.01
        // threshold) is the quietest around when the soft cap fires, but it
        // is not silence: the cut must defer past it and land on the true
        // sub-threshold dip at index 8.
        let mut s = capped_splitter(20);
        let audio = frames(&[
            (SPEECH, 4),
            (QUIET_SPEECH, 1),
            (SPEECH, 3),
            (0, 1),
            (SPEECH, 2),
        ]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        // First phrase runs through the quiet-but-voiced frame to the dip.
        assert_eq!(phrases[0].samples.len(), 8 * FRAME);
        assert_eq!(phrases[0].samples[4 * FRAME], QUIET_SPEECH);
        assert!(!phrases[0].continuation);
        // Continuation starts at the dip, flushed at end.
        assert_eq!(phrases[1].samples[0], 0);
        assert!(phrases[1].continuation);
        assert_eq!(rejoin(&phrases), audio);
    }

    #[test]
    fn soft_cap_defers_until_first_dip() {
        // All-voiced speech past the soft cap (6): no cut until the first
        // sub-threshold frame arrives (index 9, before the ceiling of 20),
        // and the cut lands exactly on it.
        let mut s = capped_splitter(20);
        let audio = frames(&[(SPEECH, 9), (0, 1), (SPEECH, 2)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        assert_eq!(phrases[0].samples.len(), 9 * FRAME);
        assert!(phrases[0].samples.iter().all(|&x| x == SPEECH));
        assert_eq!(phrases[1].samples[0], 0, "cut must land on the dip");
        assert_eq!(rejoin(&phrases), audio);
    }

    #[test]
    fn hard_ceiling_forces_voiced_cut() {
        // All-voiced speech with no dip ever: the soft cap keeps deferring,
        // and the hard ceiling (10) forces a cut on a voiced frame via the
        // unconditional argmin.
        let mut s = capped_splitter(10);
        let audio = frames(&[(SPEECH, 14)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        // Ceiling fired at frame 10; argmin over the 3-frame lookback of
        // equal energies picks its start (frame 7).
        assert_eq!(phrases[0].samples.len(), 7 * FRAME);
        assert!(!phrases[0].continuation);
        assert!(phrases[1].continuation);
        assert_eq!(rejoin(&phrases), audio);
    }

    #[test]
    fn continuous_speech_is_emitted_without_loss_or_duplication() {
        // 25 frames of pause-free speech with a ceiling of 8: every sample
        // must be emitted exactly once across forced splits + final flush.
        let mut s = capped_splitter(8);
        let audio = frames(&[(SPEECH, 25)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert!(
            phrases.len() >= 4,
            "expected multiple forced splits, got {}",
            phrases.len()
        );
        assert!(!phrases[0].continuation);
        assert!(phrases[1..].iter().all(|p| p.continuation));
        let rejoined = rejoin(&phrases);
        assert_eq!(rejoined, audio);
    }

    #[test]
    fn continuation_flag_tracks_forced_cuts() {
        // Natural phrases carry continuation == false.
        let mut s = splitter();
        let audio = frames(&[(0, 2), (SPEECH, 5), (0, 4), (SPEECH, 3), (0, 4)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());
        assert_eq!(phrases.len(), 2);
        assert!(phrases.iter().all(|p| !p.continuation));

        // A forced cut marks the remainder as a continuation; the next
        // naturally started phrase is not one.
        let mut s = capped_splitter(10);
        let audio = frames(&[
            (SPEECH, 4),
            (0, 1),
            (SPEECH, 4),
            (0, 4),
            (SPEECH, 3),
            (0, 4),
        ]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());
        assert_eq!(phrases.len(), 3);
        assert!(!phrases[0].continuation);
        assert!(phrases[1].continuation);
        assert!(!phrases[2].continuation);
    }

    #[test]
    fn short_continuation_tail_survives_natural_end() {
        // min_speech = 3. After a forced cut, a 1-speech-frame tail followed
        // by a natural silence end must be emitted via the continuation
        // bypass, not dropped as a noise blip.
        let mut s = PhraseSplitter::with_params(FRAME, 0.01, 3, 3, 6, 1, 3, 10);
        let audio = frames(&[(SPEECH, 6), (0, 1), (SPEECH, 1), (0, 4)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        assert!(phrases[1].continuation);
        // Dip frame + 1 speech frame + 1 trailing pad.
        assert_eq!(phrases[1].samples.len(), 3 * FRAME);
        assert!(phrases[1].samples[FRAME..2 * FRAME]
            .iter()
            .all(|&x| x == SPEECH));
    }

    #[test]
    fn continuation_with_zero_speech_is_discarded() {
        // The remainder after a forced cut holds no speech at all: the
        // continuation bypass must not resurrect pure silence.
        let mut s = capped_splitter(20);
        let audio = frames(&[(SPEECH, 8), (0, 4)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 1);
        assert_eq!(phrases[0].samples.len(), 8 * FRAME);
    }

    #[test]
    fn forced_cut_tail_is_not_discarded_at_flush() {
        // Production-ratio params: frame=10, threshold 0.01, split_silence 15,
        // min_speech 3, soft cap 200, pad 1, quiet search 20, ceiling 280.
        let mut s = PhraseSplitter::with_params(FRAME, 0.01, 15, 3, 200, 1, 20, 280);
        // 199 frames of speech, a genuine sub-threshold dip at frame 199,
        // then one more speech frame. The cut lands on the dip; the
        // 1-speech-frame tail after it must still be emitted at flush via
        // the continuation bypass of the noise gate.
        let audio = frames(&[(SPEECH, 199), (0, 1), (SPEECH, 1)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        assert!(phrases[1].continuation);
        let rejoined = rejoin(&phrases);
        assert_eq!(
            rejoined.len(),
            audio.len(),
            "forced-cut tail was dropped at flush (samples lost)"
        );
        assert_eq!(rejoined, audio);
    }

    #[test]
    fn flush_emits_trailing_phrase_once() {
        let mut s = splitter();
        // Speech that never sees enough trailing silence to split.
        let audio = frames(&[(0, 1), (SPEECH, 4), (0, 1)]);
        assert!(s.feed(&audio).is_empty());

        let phrase = s.flush().expect("trailing phrase should flush");
        // 1 pad + 4 speech + 1 trailing silence frame (<= pad, kept).
        assert_eq!(phrase.samples.len(), 6 * FRAME);
        assert!(!phrase.continuation);
        assert!(s.flush().is_none(), "flush must emit exactly once");
    }

    #[test]
    fn flush_trims_excess_trailing_silence() {
        let mut s = splitter();
        // 2 trailing silent frames (< split threshold of 3, > pad of 1).
        let audio = frames(&[(0, 1), (SPEECH, 4), (0, 2)]);
        assert!(s.feed(&audio).is_empty());

        let phrase = s.flush().expect("trailing phrase should flush");
        // Trimmed to 1 pad + 4 speech + 1 pad.
        assert_eq!(phrase.samples.len(), 6 * FRAME);
    }

    #[test]
    fn pure_silence_yields_nothing() {
        let mut s = splitter();
        assert!(s.feed(&frames(&[(0, 50)])).is_empty());
        assert!(s.flush().is_none());
    }

    #[test]
    fn long_leading_silence_does_not_grow_buffer() {
        let mut s = splitter();
        s.feed(&frames(&[(0, 1000)]));
        // Only the pad frame (plus at most a partial tail) may remain.
        assert!(s.buffer.len() <= 2 * FRAME);
    }

    /// Every emitted phrase is the exact input slice at `start_sample`.
    fn assert_phrases_at_their_offsets(audio: &[i16], phrases: &[Phrase]) {
        for p in phrases {
            let start = p.start_sample as usize;
            assert_eq!(
                &audio[start..start + p.samples.len()],
                p.samples.as_slice(),
                "phrase at {start} is not the input slice at that offset"
            );
        }
    }

    #[test]
    fn start_sample_places_natural_phrases_on_the_input_timeline() {
        let mut s = splitter();
        let audio = frames(&[(0, 20), (SPEECH, 4), (0, 10), (SPEECH, 4), (0, 2)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());
        assert_eq!(phrases.len(), 2);
        // Speech onset minus one pad frame.
        assert_eq!(phrases[0].start_sample, (19 * FRAME) as u64);
        assert_eq!(phrases[1].start_sample, (33 * FRAME) as u64);
        assert_phrases_at_their_offsets(&audio, &phrases);
    }

    #[test]
    fn start_sample_survives_forced_cuts_and_chunked_feeding() {
        let mut s = splitter();
        let audio = frames(&[(0, 7), (SPEECH, 300), (0, 5)]);
        let mut phrases = Vec::new();
        // Odd-sized chunks so frame boundaries never line up with feeds.
        for chunk in audio.chunks(37) {
            phrases.extend(s.feed(chunk));
        }
        phrases.extend(s.flush());
        assert!(phrases.len() >= 3, "300 voiced frames must force-cut");
        assert!(phrases[1].continuation);
        assert_phrases_at_their_offsets(&audio, &phrases);
        // Contiguous across forced cuts: each continuation starts where the
        // previous phrase ended.
        for pair in phrases.windows(2) {
            if pair[1].continuation {
                assert_eq!(
                    pair[1].start_sample,
                    pair[0].start_sample + pair[0].samples.len() as u64
                );
            }
        }
    }

    const RATE: usize = 16_000;

    /// `secs` of a 2 kHz square wave, loud enough to be speech.
    fn voice(secs: f64) -> Vec<i16> {
        let n = (secs * RATE as f64) as usize;
        (0..n)
            .map(|i| if i % 8 < 4 { 3000 } else { -3000 })
            .collect()
    }

    /// `secs` of a recorded pause: room noise, quiet but never exact zeros.
    fn pause(secs: f64) -> Vec<i16> {
        let n = (secs * RATE as f64) as usize;
        (0..n).map(|i| [3, -2, 1, -3][i % 4]).collect()
    }

    fn phrases_of(audio: &[i16]) -> Vec<Phrase> {
        let mut splitter = PhraseSplitter::new(RATE, VadConfig::close(Vad::Rms), 400);
        let mut phrases = splitter.feed(audio);
        phrases.extend(splitter.flush());
        phrases
    }

    /// Windows hold whole phrases, each exactly once and in order, never
    /// more than the cap, never across a join, and a sentence split by a
    /// short pause decodes in one window.
    #[test]
    fn decode_windows_group_phrases_up_to_the_cap_and_never_across_a_join() {
        let mut audio = Vec::new();
        // A sentence with a 0.6 s pause in it: two phrases at 400 ms.
        audio.extend(voice(2.0));
        audio.extend(pause(0.6));
        audio.extend(voice(3.0));
        audio.extend(pause(0.5));
        // A join, then 40 s of short turns: more than one window's worth.
        let join_at = audio.len() + RATE / 2;
        audio.extend(std::iter::repeat_n(0i16, RATE));
        for _ in 0..16 {
            audio.extend(voice(1.5));
            audio.extend(pause(1.0));
        }
        let phrases = phrases_of(&audio);
        assert!(phrases.len() >= 18, "{} phrases", phrases.len());
        let windows = decode_windows(&audio, &phrases, RATE);

        assert_eq!(
            windows.iter().map(|w| w.phrases).sum::<usize>(),
            phrases.len()
        );
        let mut next = 0;
        for window in &windows {
            assert!(
                window.range.len() <= MAX_DECODE_WINDOW_SECS * RATE,
                "{window:?}"
            );
            assert!(
                !window.range.contains(&join_at),
                "{window:?} crosses the join"
            );
            for phrase in &phrases[next..next + window.phrases] {
                let start = phrase.start_sample as usize;
                assert!(window.range.start <= start);
                assert!(start + phrase.samples.len() <= window.range.end);
            }
            next += window.phrases;
        }
        // The sentence and its pause, in one window that ends at the join;
        // the 40 s of turns after it, in two.
        assert_eq!(windows[0].phrases, 2);
        assert!(windows[0].range.end <= join_at);
        assert_eq!(windows.len(), 3, "{windows:?}");
    }

    #[test]
    fn a_pause_of_room_noise_is_not_a_join() {
        let mut audio = voice(1.0);
        audio.extend(pause(2.0));
        audio.extend(voice(1.0));
        let phrases = phrases_of(&audio);
        assert_eq!(phrases.len(), 2);
        assert_eq!(decode_windows(&audio, &phrases, RATE).len(), 1);
        // The same gap as digital silence is.
        let mut audio = voice(1.0);
        audio.extend(std::iter::repeat_n(0i16, 2 * RATE));
        audio.extend(voice(1.0));
        let phrases = phrases_of(&audio);
        assert_eq!(decode_windows(&audio, &phrases, RATE).len(), 2);
        assert!(decode_windows(&[], &[], RATE).is_empty());
    }

    /// The first phrase after a join has its leading pad in the join's
    /// zeros; that join lies before it, not between it and the next phrase.
    #[test]
    fn a_join_before_a_window_does_not_split_it() {
        let mut audio = voice(1.0);
        audio.extend(std::iter::repeat_n(0i16, RATE));
        audio.extend(voice(1.0));
        audio.extend(pause(2.0));
        audio.extend(voice(1.0));
        let phrases = phrases_of(&audio);
        assert_eq!(phrases.len(), 3);
        let windows = decode_windows(&audio, &phrases, RATE);
        let counts: Vec<usize> = windows.iter().map(|w| w.phrases).collect();
        assert_eq!(counts, [1, 2], "{windows:?}");
    }

    #[test]
    fn a_window_starting_at_a_forced_cut_is_a_continuation() {
        // 35 s without a pause: cut at the hard ceiling, the rest continues.
        let audio = voice(35.0);
        let phrases = phrases_of(&audio);
        let windows = decode_windows(&audio, &phrases, RATE);
        assert!(windows.len() >= 2);
        assert!(!windows[0].continuation);
        assert!(windows[1].continuation);
        assert!(windows
            .iter()
            .all(|w| w.range.len() <= MAX_DECODE_WINDOW_SECS * RATE));
    }

    /// A noise gate or a muted mic records a dictation's pauses as exact
    /// zeros. A recording ends a window at each of those (a join); a
    /// dictation has no joins, so it packs phrases up to the cap across them.
    #[test]
    fn dictation_windows_pack_across_exact_zero_pauses() {
        // 60 s: 24 turns of 1.5 s, each followed by a second of exact zeros.
        let mut audio = Vec::new();
        for _ in 0..24 {
            audio.extend(voice(1.5));
            audio.extend(std::iter::repeat_n(0i16, RATE));
        }
        let phrases = phrases_of(&audio);
        assert_eq!(phrases.len(), 24);

        let recording = decode_windows(&audio, &phrases, RATE);
        assert_eq!(recording.len(), 24, "every join ends a recording window");
        let dictation = dictation_windows(&phrases, RATE);
        assert_eq!(dictation.len(), 3, "{dictation:?}");

        for windows in [&recording, &dictation] {
            assert_eq!(
                windows.iter().map(|w| w.phrases).sum::<usize>(),
                phrases.len()
            );
            assert!(windows
                .iter()
                .all(|w| w.range.len() <= MAX_DECODE_WINDOW_SECS * RATE));
        }
        assert!(dictation_windows(&[], RATE).is_empty());
    }

    /// Every phrase in exactly one window, in order, no window empty and
    /// none past the cap.
    fn assert_whole_phrases_in_order(windows: &[DecodeWindow], phrases: &[Phrase]) {
        assert_eq!(
            windows.iter().map(|w| w.phrases).sum::<usize>(),
            phrases.len()
        );
        let mut next = 0;
        for (i, window) in windows.iter().enumerate() {
            assert!(window.phrases > 0, "{window:?}");
            assert!(
                window.range.len() <= MAX_DECODE_WINDOW_SECS * RATE,
                "{window:?}"
            );
            if i > 0 {
                assert!(windows[i - 1].range.end <= window.range.start);
            }
            for phrase in &phrases[next..next + window.phrases] {
                let start = phrase.start_sample as usize;
                assert!(window.range.start <= start);
                assert!(start + phrase.samples.len() <= window.range.end);
            }
            next += window.phrases;
        }
    }

    /// From the first phrase's start to the last one's end.
    fn span_of(phrases: &[Phrase]) -> usize {
        let last = phrases.last().expect("phrases");
        last.start_sample as usize + last.samples.len() - phrases[0].start_sample as usize
    }

    /// A 30 s dictation of short turns: packed to the cap, its last window
    /// would hold one turn; evened out, it is two windows of 14.1 s and
    /// 14.2 s.
    #[test]
    fn dictation_windows_split_the_span_evenly() {
        let mut audio = Vec::new();
        for _ in 0..12 {
            audio.extend(voice(1.5));
            audio.extend(pause(1.0));
        }
        let phrases = phrases_of(&audio);
        assert_eq!(phrases.len(), 12);
        let span = span_of(&phrases);

        let packed = decode_windows(&audio, &phrases, RATE);
        assert_eq!(packed.len(), 2, "{packed:?}");
        assert!(packed[1].range.len() < 3 * RATE, "{packed:?}");

        let windows = dictation_windows(&phrases, RATE);
        assert_eq!(windows.len(), 2, "{windows:?}");
        assert_whole_phrases_in_order(&windows, &phrases);
        for window in &windows {
            assert!(window.range.len() >= span / 3, "{windows:?}");
        }
    }

    /// A long dictation of uneven turns and pauses: as few windows as fit
    /// the cap, none past it or under half its share of the span, every
    /// phrase in exactly one, in order.
    #[test]
    fn long_dictation_windows_stay_under_the_cap() {
        let mut audio = Vec::new();
        for i in 0..50 {
            audio.extend(voice(1.0 + (i % 5) as f64 * 0.7));
            audio.extend(pause(0.5 + (i % 3) as f64 * 0.5));
        }
        let phrases = phrases_of(&audio);
        assert_eq!(phrases.len(), 50);
        let span = span_of(&phrases);

        let windows = dictation_windows(&phrases, RATE);
        assert_eq!(
            windows.len(),
            span.div_ceil(MAX_DECODE_WINDOW_SECS * RATE),
            "{windows:?}"
        );
        assert_whole_phrases_in_order(&windows, &phrases);
        for window in &windows {
            assert!(
                window.range.len() >= span / windows.len() / 2,
                "{windows:?}"
            );
        }
    }

    /// A phrase of `secs` starting `start_secs` into the input, for
    /// grouping tests that need exact lengths.
    fn phrase_at(start_secs: f64, secs: f64) -> Phrase {
        Phrase {
            samples: vec![0; (secs * RATE as f64) as usize],
            continuation: false,
            start_sample: (start_secs * RATE as f64) as u64,
        }
    }

    /// Phrases of 14 s, 13 s and 2 s, 0.5 s apart: the first window is under
    /// its 15 s share, but taking the 13 s phrase would put it 12.5 s over,
    /// so it closes at 14 s and the rest is one window of 15.5 s, not
    /// 27.5 s and a 2 s scrap. A join that leaves the window as far from
    /// its share as before still happens.
    #[test]
    fn dictation_windows_do_not_overshoot_into_a_short_last_window() {
        let lens = |windows: &[DecodeWindow]| -> Vec<usize> {
            windows.iter().map(|w| w.range.len()).collect()
        };
        let phrases = [
            phrase_at(0.0, 14.0),
            phrase_at(14.5, 13.0),
            phrase_at(28.0, 2.0),
        ];
        let windows = dictation_windows(&phrases, RATE);
        assert_eq!(lens(&windows), [14 * RATE, 31 * RATE / 2], "{windows:?}");
        assert_whole_phrases_in_order(&windows, &phrases);
        // Packed to the cap (no samples, so no joins).
        let packed = decode_windows(&[], &phrases, RATE);
        assert_eq!(lens(&packed), [55 * RATE / 2, 2 * RATE], "{packed:?}");

        // A 15 s share: 13 s is 2 s under it and 17 s is 2 s over, a tie.
        let phrases = [
            phrase_at(0.0, 13.0),
            phrase_at(13.5, 3.5),
            phrase_at(17.5, 12.5),
        ];
        let windows = dictation_windows(&phrases, RATE);
        assert_eq!(lens(&windows), [17 * RATE, 25 * RATE / 2], "{windows:?}");
        assert_whole_phrases_in_order(&windows, &phrases);
    }

    // ── The speech source ([audio] vad) ─────────────────────────────────

    /// The 0.1.31 splitter, verbatim but for its comments, as the reference
    /// the RMS path must match.
    mod v0_1_31 {
        use super::super::{
            rms_energy, Phrase, FRAME_MS, HARD_MAX_PHRASE_SECS, MAX_PHRASE_SECS,
            MIN_PHRASE_SPEECH_MS, PAD_MS, QUIET_SEARCH_MS,
        };

        pub struct PhraseSplitter {
            frame_len: usize,
            threshold: f64,
            split_silence_frames: usize,
            min_speech_frames: usize,
            max_phrase_frames: usize,
            pad_frames: usize,
            quiet_search_frames: usize,
            hard_max_phrase_frames: usize,

            buffer: Vec<i16>,
            energies: Vec<f64>,
            in_phrase: bool,
            emit_start_frame: usize,
            phrase_start_frame: usize,
            speech_frames: usize,
            trailing_silence: usize,
            continuation: bool,
            consumed: u64,
        }

        impl PhraseSplitter {
            pub fn new(sample_rate: usize, threshold: f64, phrase_silence_ms: u64) -> Self {
                let frame_len = (sample_rate * FRAME_MS / 1000).max(1);
                Self::with_params(
                    frame_len,
                    threshold,
                    (phrase_silence_ms as usize).div_ceil(FRAME_MS).max(1),
                    MIN_PHRASE_SPEECH_MS.div_ceil(FRAME_MS).max(1),
                    (MAX_PHRASE_SECS * 1000 / FRAME_MS).max(2),
                    PAD_MS / FRAME_MS,
                    (QUIET_SEARCH_MS / FRAME_MS).max(1),
                    (HARD_MAX_PHRASE_SECS * 1000 / FRAME_MS).max(2),
                )
            }

            #[allow(clippy::too_many_arguments)]
            pub fn with_params(
                frame_len: usize,
                threshold: f64,
                split_silence_frames: usize,
                min_speech_frames: usize,
                max_phrase_frames: usize,
                pad_frames: usize,
                quiet_search_frames: usize,
                hard_max_phrase_frames: usize,
            ) -> Self {
                let max_phrase_frames = max_phrase_frames.max(2);
                Self {
                    frame_len: frame_len.max(1),
                    threshold,
                    split_silence_frames: split_silence_frames.max(1),
                    min_speech_frames: min_speech_frames.max(1),
                    max_phrase_frames,
                    pad_frames,
                    quiet_search_frames: quiet_search_frames.max(1),
                    hard_max_phrase_frames: hard_max_phrase_frames.max(max_phrase_frames),
                    buffer: Vec::new(),
                    energies: Vec::new(),
                    in_phrase: false,
                    emit_start_frame: 0,
                    phrase_start_frame: 0,
                    speech_frames: 0,
                    trailing_silence: 0,
                    continuation: false,
                    consumed: 0,
                }
            }

            pub fn feed(&mut self, samples: &[i16]) -> Vec<Phrase> {
                self.buffer.extend_from_slice(samples);
                let mut out = Vec::new();
                while (self.energies.len() + 1) * self.frame_len <= self.buffer.len() {
                    let i = self.energies.len();
                    let energy =
                        rms_energy(&self.buffer[i * self.frame_len..(i + 1) * self.frame_len]);
                    self.energies.push(energy);
                    self.process_frame(i, energy, &mut out);
                }
                out
            }

            pub fn flush(&mut self) -> Option<Phrase> {
                if !self.in_phrase || !self.keep_phrase() {
                    self.clear();
                    return None;
                }

                let end_sample = if self.trailing_silence > self.pad_frames {
                    let speech_end = self.energies.len() - self.trailing_silence;
                    (speech_end + self.pad_frames) * self.frame_len
                } else {
                    self.buffer.len()
                };
                let phrase = Phrase {
                    samples: self.buffer[self.emit_start_frame * self.frame_len..end_sample]
                        .to_vec(),
                    continuation: self.continuation,
                    start_sample: self.stream_index(self.emit_start_frame),
                };
                self.clear();
                Some(phrase)
            }

            fn keep_phrase(&self) -> bool {
                self.speech_frames >= self.min_speech_frames
                    || (self.continuation && self.speech_frames >= 1)
            }

            fn process_frame(&mut self, i: usize, energy: f64, out: &mut Vec<Phrase>) {
                let silent = energy < self.threshold;

                if !self.in_phrase {
                    if silent {
                        if self.energies.len() > self.pad_frames {
                            let drop = self.energies.len() - self.pad_frames;
                            self.drop_frames(drop);
                        }
                    } else {
                        self.in_phrase = true;
                        self.phrase_start_frame = i;
                        self.emit_start_frame = i.saturating_sub(self.pad_frames);
                        self.speech_frames = 1;
                        self.trailing_silence = 0;
                        self.continuation = false;
                    }
                    return;
                }

                if silent {
                    self.trailing_silence += 1;
                    if self.trailing_silence >= self.split_silence_frames {
                        self.end_phrase(i, out);
                        return;
                    }
                } else {
                    self.speech_frames += 1;
                    self.trailing_silence = 0;
                }

                let phrase_len = i + 1 - self.phrase_start_frame;
                if phrase_len >= self.hard_max_phrase_frames {
                    self.force_split(i, out);
                } else if phrase_len >= self.max_phrase_frames {
                    self.try_soft_split(i, out);
                }
            }

            fn end_phrase(&mut self, i: usize, out: &mut Vec<Phrase>) {
                let speech_end = i + 1 - self.trailing_silence;
                let end_frame = (speech_end + self.pad_frames).min(self.energies.len());

                if self.keep_phrase() {
                    out.push(Phrase {
                        samples: self.buffer
                            [self.emit_start_frame * self.frame_len..end_frame * self.frame_len]
                            .to_vec(),
                        continuation: self.continuation,
                        start_sample: self.stream_index(self.emit_start_frame),
                    });
                }

                self.in_phrase = false;
                self.continuation = false;
                self.speech_frames = 0;
                self.trailing_silence = 0;
                let keep_from = self
                    .energies
                    .len()
                    .saturating_sub(self.pad_frames)
                    .max(speech_end);
                self.drop_frames(keep_from);
            }

            fn force_split(&mut self, i: usize, out: &mut Vec<Phrase>) {
                let search_start = self.split_search_start(i);
                let mut split_frame = search_start;
                let mut min_energy = f64::INFINITY;
                for j in search_start..=i {
                    if self.energies[j] < min_energy {
                        min_energy = self.energies[j];
                        split_frame = j;
                    }
                }
                self.cut_at(split_frame, out);
            }

            fn try_soft_split(&mut self, i: usize, out: &mut Vec<Phrase>) {
                let search_start = self.split_search_start(i);
                let mut split_frame = None;
                let mut min_energy = f64::INFINITY;
                for j in search_start..=i {
                    let e = self.energies[j];
                    if e < self.threshold && e < min_energy {
                        min_energy = e;
                        split_frame = Some(j);
                    }
                }
                if let Some(split_frame) = split_frame {
                    self.cut_at(split_frame, out);
                }
            }

            fn split_search_start(&self, i: usize) -> usize {
                (i + 1)
                    .saturating_sub(self.quiet_search_frames)
                    .max(self.phrase_start_frame + 1)
            }

            fn cut_at(&mut self, split_frame: usize, out: &mut Vec<Phrase>) {
                out.push(Phrase {
                    samples: self.buffer
                        [self.emit_start_frame * self.frame_len..split_frame * self.frame_len]
                        .to_vec(),
                    continuation: self.continuation,
                    start_sample: self.stream_index(self.emit_start_frame),
                });

                self.drop_frames(split_frame);
                self.phrase_start_frame = 0;
                self.emit_start_frame = 0;
                self.continuation = true;
                self.speech_frames = self
                    .energies
                    .iter()
                    .filter(|&&e| e >= self.threshold)
                    .count();
                self.trailing_silence = self
                    .energies
                    .iter()
                    .rev()
                    .take_while(|&&e| e < self.threshold)
                    .count();
            }

            fn stream_index(&self, frame: usize) -> u64 {
                self.consumed + (frame * self.frame_len) as u64
            }

            fn drop_frames(&mut self, n: usize) {
                if n == 0 {
                    return;
                }
                self.consumed += (n * self.frame_len) as u64;
                self.buffer.drain(..n * self.frame_len);
                self.energies.drain(..n);
            }

            fn clear(&mut self) {
                self.consumed += self.buffer.len() as u64;
                self.buffer.clear();
                self.energies.clear();
                self.in_phrase = false;
                self.emit_start_frame = 0;
                self.phrase_start_frame = 0;
                self.speech_frames = 0;
                self.trailing_silence = 0;
                self.continuation = false;
            }
        }
    }

    /// Seeded audio: `runs` runs of up to `max_run` samples at one of
    /// `levels` each (alternating sign, so the RMS is the level, with a
    /// little jitter).
    fn seeded(seed: u64, runs: usize, levels: &[i16], max_run: usize) -> Vec<i16> {
        let mut state = seed;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out = Vec::new();
        for _ in 0..runs {
            let len = (next() % max_run as u64) as usize;
            let level = levels[(next() % levels.len() as u64) as usize];
            out.extend((0..len).map(|i| {
                let s = level.saturating_add((next() % 3) as i16 - 1);
                if i % 2 == 0 {
                    s
                } else {
                    -s
                }
            }));
        }
        out
    }

    /// The two splitters under comparison.
    trait Splits {
        fn feed(&mut self, samples: &[i16]) -> Vec<Phrase>;
        fn flush(&mut self) -> Option<Phrase>;
    }

    impl Splits for PhraseSplitter {
        fn feed(&mut self, samples: &[i16]) -> Vec<Phrase> {
            PhraseSplitter::feed(self, samples)
        }
        fn flush(&mut self) -> Option<Phrase> {
            PhraseSplitter::flush(self)
        }
    }

    impl Splits for v0_1_31::PhraseSplitter {
        fn feed(&mut self, samples: &[i16]) -> Vec<Phrase> {
            v0_1_31::PhraseSplitter::feed(self, samples)
        }
        fn flush(&mut self) -> Option<Phrase> {
            v0_1_31::PhraseSplitter::flush(self)
        }
    }

    /// Every phrase `splitter` emits for `audio` fed `chunk` samples at a
    /// time, the flush included.
    fn split_with(audio: &[i16], chunk: usize, splitter: &mut impl Splits) -> Vec<Phrase> {
        let mut phrases: Vec<Phrase> = audio.chunks(chunk).flat_map(|c| splitter.feed(c)).collect();
        phrases.extend(splitter.flush());
        phrases
    }

    #[test]
    fn rms_splits_match_0_1_31() {
        // Production frames at 16 kHz around 0.003 (98.3 of full scale),
        // with stretches long enough to reach both length caps.
        let levels = [0, 4, 50, 97, 99, 300, 300, 8000, 8000, 8000];
        let mut forced = 0;
        for (seed, silence_ms) in [(1, 400), (2, 900), (3, 100), (4, 2000)] {
            let audio = seeded(seed, 150, &levels, 4 * RATE);
            for chunk in [800, 1600, 4001] {
                let mut new = PhraseSplitter::new(RATE, VadConfig::close(Vad::Rms), silence_ms);
                let mut old = v0_1_31::PhraseSplitter::new(
                    RATE,
                    audio_silence_gate::SILENCE_RMS_THRESHOLD,
                    silence_ms,
                );
                let expected = split_with(&audio, chunk, &mut old);
                let got = split_with(&audio, chunk, &mut new);
                assert!(got == expected, "seed {seed}, chunk {chunk}");
                forced += expected.iter().filter(|p| p.continuation).count();
            }
        }
        assert!(forced > 0, "no phrase reached a cap: a weak comparison");
        // Tiny frames and caps, so cuts of every kind happen all the time.
        for seed in 10..30 {
            let audio = seeded(seed, 200, &[0, 50, 300, 330, 500, 1000], 120);
            let mut new = PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 6, 1, 3, 10);
            let mut old = v0_1_31::PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 6, 1, 3, 10);
            let expected = split_with(&audio, 37, &mut old);
            assert_eq!(split_with(&audio, 37, &mut new), expected, "seed {seed}");
        }
    }

    /// [`splitter`] with its speech flags read from `flags`.
    fn scripted(flags: &[bool]) -> PhraseSplitter {
        let mut s = splitter();
        s.source = SpeechSource::Script(flags.iter().copied().collect());
        s
    }

    #[test]
    fn silence_comes_from_the_detector_not_the_level() {
        // Loud throughout, but the detector hears speech only in frames 5 to
        // 9: one phrase, those frames plus a pad on each side.
        let mut flags = vec![false; 5];
        flags.extend([true; 5]);
        flags.extend([false; 10]);
        let mut s = scripted(&flags);
        let audio = frames(&[(SPEECH, 20)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());
        assert_eq!(phrases.len(), 1);
        assert_eq!(phrases[0].start_sample, (4 * FRAME) as u64);
        assert_eq!(phrases[0].samples.len(), 7 * FRAME);

        // Quiet audio the detector calls speech is a phrase all the same.
        let mut s = scripted(&[true; 6]);
        let mut phrases = s.feed(&frames(&[(5, 6)]));
        phrases.extend(s.flush());
        assert_eq!(phrases.len(), 1);
    }

    #[test]
    fn soft_cap_cuts_at_the_quietest_silent_frame() {
        // Soft cap 6 frames, search 3: at frame 5 the window is frames 3 to
        // 5. Frames 3 and 4 are silent to the detector, and 4 is the quieter
        // of them; frame 5 is quieter still but speech, so it is no cut.
        let mut s = capped_splitter(140);
        let flags = [true, true, true, false, false, true, true, true];
        s.source = SpeechSource::Script(flags.into_iter().collect());
        let audio = frames(&[
            (SPEECH, 3),
            (QUIET_SPEECH, 1),
            (QUIET_SPEECH / 2, 1),
            (5, 1),
            (SPEECH, 2),
        ]);
        let phrases = s.feed(&audio);
        assert_eq!(phrases.len(), 1, "{phrases:?}");
        assert_eq!(phrases[0].samples.len(), 4 * FRAME);
        let tail = s.flush().expect("the rest continues the phrase");
        assert!(tail.continuation);
        assert_eq!(rejoin(&[phrases[0].clone(), tail]), audio);
    }

    fn decision(start_sample: u64, speech: bool) -> FrameDecision {
        FrameDecision {
            start_sample,
            speech,
            score: if speech { 0.9 } else { 0.1 },
            dbfs: -30.0,
        }
    }

    #[test]
    fn a_16_ms_frame_counts_toward_the_frame_it_ends_in() {
        // 1600-sample frames: 16 ms frame 6 (1536..1792) ends in frame 1.
        let mut ahead = VecDeque::new();
        mark_frame(&mut ahead, 0, decision(5 * 256, true), 1600);
        mark_frame(&mut ahead, 0, decision(6 * 256, true), 1600);
        assert_eq!(ahead, [true, true]);
        // Not speech: nothing marked.
        let mut ahead = VecDeque::new();
        mark_frame(&mut ahead, 0, decision(6 * 256, false), 1600);
        assert!(ahead.is_empty());
        // Relative to the frames analyzed so far.
        let mut ahead = VecDeque::new();
        mark_frame(&mut ahead, 3, decision(25 * 256, true), 1600);
        assert_eq!(ahead, [false, true]);
        // A decision on a frame already analyzed is dropped, never wraps.
        let mut ahead = VecDeque::new();
        mark_frame(&mut ahead, 3, decision(256, true), 1600);
        assert!(ahead.is_empty());
    }

    /// Seeded Gaussian noise at about `level` normalized RMS.
    fn noise(n: usize, seed: u64, level: f64) -> Vec<i16> {
        let mut state = seed;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let (u, v) = (uniform(), uniform());
                let x = (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos();
                (x * level * i16::MAX as f64)
                    .round()
                    .clamp(-32768.0, 32767.0) as i16
            })
            .collect()
    }

    #[test]
    fn earshot_splits_no_phrase_out_of_noise() {
        // White noise at -40 dBFS: loud enough for the RMS test to call all
        // of it speech, never speech to earshot. Digital silence neither.
        let hiss = noise(10 * RATE, 7, 0.01);
        let silence = vec![0i16; 10 * RATE];
        for cfg in [
            VadConfig::close(Vad::Earshot),
            VadConfig::open(Vad::Earshot),
        ] {
            for audio in [&hiss, &silence] {
                let mut s = PhraseSplitter::new(RATE, cfg, 400);
                assert!(matches!(s.source, SpeechSource::Vad(_)));
                let phrases = split_with(audio, 800, &mut s);
                assert!(phrases.is_empty(), "{} phrase(s)", phrases.len());
            }
        }
        let mut rms = PhraseSplitter::new(RATE, VadConfig::close(Vad::Rms), 400);
        assert_eq!(split_with(&hiss, 800, &mut rms).len(), 1);
        // earshot runs at 16 kHz only: any other rate takes the RMS test.
        let other = PhraseSplitter::new(8_000, VadConfig::close(Vad::Earshot), 400);
        assert!(matches!(other.source, SpeechSource::Rms(_)));
    }
}
