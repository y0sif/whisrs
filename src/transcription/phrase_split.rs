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
//!   discarded as noise.
//! - Emitted phrases include up to `pad_frames` of surrounding silence.
//! - A phrase that reaches `max_phrase_frames` without a silence split is
//!   force-split at the quietest recent frame boundary, so continuous
//!   pause-free speech still emits. No padding is added at a forced boundary
//!   (the audio is contiguous, and padding would duplicate samples).
//! - [`PhraseSplitter::flush`] emits the trailing in-progress phrase at end of
//!   stream (if it carries enough speech) exactly once.

use audio_silence_gate::rms_energy;

/// Analysis frame length in milliseconds.
const FRAME_MS: usize = 100;
/// Silence padding kept around each phrase, in milliseconds.
const PAD_MS: usize = 100;
/// Minimum speech (frames containing speech, ~ms) required to keep a phrase.
const MIN_PHRASE_SPEECH_MS: usize = 250;
/// Hard cap on phrase length before a forced split.
const MAX_PHRASE_SECS: usize = 20;
/// How far back to look for the quietest frame when force-splitting.
const QUIET_SEARCH_MS: usize = 2000;

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
        )
    }

    /// Create a splitter with explicit frame-level parameters (used by tests).
    pub fn with_params(
        frame_len: usize,
        threshold: f64,
        split_silence_frames: usize,
        min_speech_frames: usize,
        max_phrase_frames: usize,
        pad_frames: usize,
        quiet_search_frames: usize,
    ) -> Self {
        Self {
            frame_len: frame_len.max(1),
            threshold,
            split_silence_frames: split_silence_frames.max(1),
            min_speech_frames: min_speech_frames.max(1),
            max_phrase_frames: max_phrase_frames.max(2),
            pad_frames,
            quiet_search_frames: quiet_search_frames.max(1),
            buffer: Vec::new(),
            energies: Vec::new(),
            in_phrase: false,
            emit_start_frame: 0,
            phrase_start_frame: 0,
            speech_frames: 0,
            trailing_silence: 0,
        }
    }

    /// Feed samples; returns zero or more completed phrases in FIFO order.
    pub fn feed(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
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
    pub fn flush(&mut self) -> Option<Vec<i16>> {
        // A partial (<1 frame) unanalyzed tail without an open phrase can hold
        // at most one frame of speech — below any sensible minimum. Discard.
        if !self.in_phrase || self.speech_frames < self.min_speech_frames {
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
        let phrase = self.buffer[self.emit_start_frame * self.frame_len..end_sample].to_vec();
        self.clear();
        Some(phrase)
    }

    /// Process the just-analyzed frame `i` (index into `energies`).
    fn process_frame(&mut self, i: usize, energy: f64, out: &mut Vec<Vec<i16>>) {
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

        // Forced split: phrase reached the hard cap without a silence gap.
        if i + 1 - self.phrase_start_frame >= self.max_phrase_frames {
            self.force_split(i, out);
        }
    }

    /// Natural end of phrase: enough consecutive silence after speech.
    fn end_phrase(&mut self, i: usize, out: &mut Vec<Vec<i16>>) {
        // One past the last frame that contained speech.
        let speech_end = i + 1 - self.trailing_silence;
        let end_frame = (speech_end + self.pad_frames).min(self.energies.len());

        if self.speech_frames >= self.min_speech_frames {
            out.push(
                self.buffer[self.emit_start_frame * self.frame_len..end_frame * self.frame_len]
                    .to_vec(),
            );
        }

        self.in_phrase = false;
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

    /// Forced split at the cap: cut at the quietest recent frame boundary so
    /// we are least likely to cut mid-word. The quietest frame starts the
    /// next phrase; no padding is duplicated across the boundary.
    fn force_split(&mut self, i: usize, out: &mut Vec<Vec<i16>>) {
        let search_start = (i + 1)
            .saturating_sub(self.quiet_search_frames)
            .max(self.phrase_start_frame + 1);
        let mut split_frame = search_start;
        let mut min_energy = f64::INFINITY;
        for j in search_start..=i {
            if self.energies[j] < min_energy {
                min_energy = self.energies[j];
                split_frame = j;
            }
        }

        out.push(
            self.buffer[self.emit_start_frame * self.frame_len..split_frame * self.frame_len]
                .to_vec(),
        );

        // The remainder continues as an open phrase starting at the split.
        self.drop_frames(split_frame);
        self.phrase_start_frame = 0;
        self.emit_start_frame = 0;
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 10;
    const SPEECH: i16 = 1000; // normalized RMS ~0.031, above the 0.01 threshold
    const QUIET_SPEECH: i16 = 500; // ~0.015 — voiced but the quietest around

    /// frame_len=10, threshold=0.01, split after 3 silent frames, min 2 speech
    /// frames, cap at 100 frames, 1 pad frame, search 5 frames for the
    /// quietest split point.
    fn splitter() -> PhraseSplitter {
        PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 100, 1, 5)
    }

    fn frames(pattern: &[(i16, usize)]) -> Vec<i16> {
        let mut out = Vec::new();
        for &(value, count) in pattern {
            out.extend(std::iter::repeat_n(value, count * FRAME));
        }
        out
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
        assert_eq!(phrases[0].len(), 7 * FRAME);
        // Phrase 2: 1 pad + 3 speech + 1 pad frames.
        assert_eq!(phrases[1].len(), 5 * FRAME);
    }

    #[test]
    fn padding_included_around_speech() {
        let mut s = splitter();
        let audio = frames(&[(0, 3), (SPEECH, 4), (0, 4)]);
        let phrases = s.feed(&audio);

        assert_eq!(phrases.len(), 1);
        let p = &phrases[0];
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
        assert_eq!(phrases[0].len(), 5 * FRAME);
        assert_eq!(phrases[0][0], SPEECH);
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
    fn cap_force_splits_at_quietest_frame() {
        // Cap at 6 frames; quiet-but-voiced frame at index 4 should become
        // the split point (start of the next phrase).
        let mut s = PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 6, 1, 3);
        let audio = frames(&[(SPEECH, 4), (QUIET_SPEECH, 1), (SPEECH, 3)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert_eq!(phrases.len(), 2);
        // First phrase: frames 0..4 (split before the quiet frame).
        assert_eq!(phrases[0].len(), 4 * FRAME);
        assert!(phrases[0].iter().all(|&x| x == SPEECH));
        // Continuation: quiet frame + remaining speech, flushed at end.
        assert_eq!(phrases[1].len(), 4 * FRAME);
        assert_eq!(phrases[1][0], QUIET_SPEECH);
    }

    #[test]
    fn continuous_speech_is_emitted_without_loss_or_duplication() {
        // 25 frames of pause-free speech with a cap of 6: every sample must be
        // emitted exactly once across forced splits + final flush.
        let mut s = PhraseSplitter::with_params(FRAME, 0.01, 3, 2, 6, 1, 3);
        let audio = frames(&[(SPEECH, 25)]);
        let mut phrases = s.feed(&audio);
        phrases.extend(s.flush());

        assert!(
            phrases.len() >= 4,
            "expected multiple forced splits, got {}",
            phrases.len()
        );
        let rejoined: Vec<i16> = phrases.into_iter().flatten().collect();
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
        assert_eq!(phrase.len(), 6 * FRAME);
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
        assert_eq!(phrase.len(), 6 * FRAME);
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
