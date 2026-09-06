//! Silence-delimited phrase segmentation for local streaming ASR.
//!
//! [`PhraseSplitter`] is a pure, incremental splitter: feed it i16 PCM samples
//! and it yields complete phrases — runs of speech delimited by a configurable
//! stretch of silence. Each returned phrase is a contiguous slice of the input
//! audio (plus a little silence padding on both sides), so a decoder can
//! transcribe each phrase exactly once with no overlap and no deduplication.
//!
//! Rules:
//! - RMS energy is tracked per fixed-size frame (100 ms in production).
//! - A phrase ends once `split_silence_frames` consecutive silent frames
//!   follow speech.
//! - Phrases with fewer than `min_speech_frames` frames containing speech are
//!   discarded as noise — unless the phrase continues a forced cut, in which
//!   case a single speech frame is enough to keep it (speech was flowing at
//!   the cut, so a short tail is real speech, not a noise blip). A
//!   continuation with zero speech frames is still discarded.
//! - Emitted phrases include up to `pad_frames` of surrounding silence.
//! - A phrase that reaches `max_phrase_frames` (the soft cap) without a
//!   silence split is cut at the quietest *sub-threshold* frame in the recent
//!   lookback window. If every recent frame is voiced, the cut is deferred —
//!   the search re-runs each frame — until the first sub-threshold frame
//!   arrives or the phrase reaches `hard_max_phrase_frames` (the hard
//!   ceiling, sized to stay inside whisper's 30 s context window), where it
//!   is cut at the quietest recent frame even if voiced, so continuous
//!   pause-free speech still emits. No padding is added at a forced boundary
//!   (the audio is contiguous, and padding would duplicate samples).
//! - Each emitted [`Phrase`] records whether it *starts* at a forced cut
//!   (`continuation`), so a decoder can treat it as mid-sentence text.
//! - [`PhraseSplitter::flush`] emits the trailing in-progress phrase at end of
//!   stream (if it carries enough speech) exactly once.

use audio_silence_gate::rms_energy;

/// Analysis frame length in milliseconds.
const FRAME_MS: usize = 100;
/// Silence padding kept around each phrase, in milliseconds.
const PAD_MS: usize = 100;
/// Minimum speech (frames containing speech, ~ms) required to keep a phrase.
const MIN_PHRASE_SPEECH_MS: usize = 250;
/// Soft cap on phrase length: past this, cut at the first sub-threshold
/// frame found in the lookback window.
const MAX_PHRASE_SECS: usize = 20;
/// Hard ceiling on phrase length: past this, cut at the quietest recent
/// frame even if it is voiced. Sized to stay inside whisper's 30 s context
/// window.
const HARD_MAX_PHRASE_SECS: usize = 28;
/// How far back to look for the quietest frame when force-splitting.
const QUIET_SEARCH_MS: usize = 2000;

/// A completed phrase of contiguous audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// The phrase samples (contiguous input audio plus silence padding).
    pub samples: Vec<i16>,
    /// True when this phrase starts at a forced cut boundary (mid-speech),
    /// i.e. it continues the previous phrase rather than starting fresh.
    pub continuation: bool,
}

/// Incremental silence-delimited phrase splitter over i16 PCM samples.
pub struct PhraseSplitter {
    // Configuration (all in frames unless noted).
    frame_len: usize,
    threshold: f64,
    split_silence_frames: usize,
    min_speech_frames: usize,
    max_phrase_frames: usize,
    pad_frames: usize,
    quiet_search_frames: usize,
    hard_max_phrase_frames: usize,

    // Rolling state. `buffer` holds samples not yet consumed by an emitted or
    // discarded phrase; `energies[i]` is the RMS of frame `i` of `buffer`.
    buffer: Vec<i16>,
    energies: Vec<f64>,
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
}

impl PhraseSplitter {
    /// Create a splitter with production defaults for the given sample rate.
    ///
    /// `threshold` is a normalized RMS silence threshold (0.0–1.0);
    /// `phrase_silence_ms` is how much continuous silence ends a phrase.
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

    /// Create a splitter with explicit frame-level parameters (used by tests).
    ///
    /// `max_phrase_frames` is the soft cap (cut only at a sub-threshold
    /// frame); `hard_max_phrase_frames` is the hard ceiling (cut at the
    /// quietest recent frame regardless), clamped to at least the soft cap.
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
        }
    }

    /// Feed samples; returns zero or more completed phrases in FIFO order.
    pub fn feed(&mut self, samples: &[i16]) -> Vec<Phrase> {
        self.buffer.extend_from_slice(samples);
        let mut out = Vec::new();
        // Analyze every complete frame not yet analyzed. `process_frame` may
        // drop consumed frames from the front, so recompute the index each
        // iteration.
        while (self.energies.len() + 1) * self.frame_len <= self.buffer.len() {
            let i = self.energies.len();
            let energy = rms_energy(&self.buffer[i * self.frame_len..(i + 1) * self.frame_len]);
            self.energies.push(energy);
            self.process_frame(i, energy, &mut out);
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
    fn process_frame(&mut self, i: usize, energy: f64, out: &mut Vec<Phrase>) {
        let silent = energy < self.threshold;

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
        // sub-threshold frame, deferring (re-checked every frame) until one
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

    /// Soft-cap split: cut at the quietest *sub-threshold* frame in the
    /// lookback window, if any. When every recent frame is voiced this does
    /// nothing — the search re-runs on each subsequent frame, so the first
    /// arriving sub-threshold frame becomes the cut point.
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

    /// Drop the first `n` analyzed frames (and their samples) from the front.
    fn drop_frames(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.buffer.drain(..n * self.frame_len);
        self.energies.drain(..n);
    }

    fn clear(&mut self) {
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
}
