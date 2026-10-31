//! Splitting long recording tracks into backend-sized chunks.
//!
//! A backend takes at most [`TranscriptionBackend::max_segment_chunk_secs`]
//! of audio per request, so a long track is cut into consecutive chunks.
//! Each cut lands in the quietest stretch near the cap, so a word is not
//! split across two requests.
//!
//! [`TranscriptionBackend::max_segment_chunk_secs`]:
//!     crate::transcription::TranscriptionBackend::max_segment_chunk_secs

use std::ops::Range;

use audio_silence_gate::rms_energy;

/// Analysis frame length in milliseconds.
const FRAME_MS: usize = 100;

/// How far before the cap to look for a quiet place to cut.
const LOOKBACK_SECS: usize = 60;

/// Length in samples of one analysis frame at `sample_rate`.
pub fn frame_len(sample_rate: usize) -> usize {
    (sample_rate * FRAME_MS / 1000).max(1)
}

/// Split `samples` into contiguous ranges of at most `max_chunk_secs` each,
/// with the RMS test deciding speech: a 100 ms frame is speech when its RMS
/// reaches `threshold`.
///
/// The ranges cover every sample exactly once, in order. When a cut is needed
/// it goes in the middle of the longest run of 100 ms frames whose RMS is
/// below `threshold`, searched over the last [`LOOKBACK_SECS`] (or half the
/// cap, if shorter) before the cap; with no quiet frame there, it goes at the
/// quietest frame. Empty input yields no ranges.
pub fn split_at_silence(
    samples: &[i16],
    sample_rate: usize,
    max_chunk_secs: usize,
    threshold: f64,
) -> Vec<Range<usize>> {
    let frame = frame_len(sample_rate);
    let energies: Vec<f64> = samples.chunks_exact(frame).map(rms_energy).collect();
    let speech: Vec<bool> = energies.iter().map(|&e| e >= threshold).collect();
    split_by_energies(
        &energies,
        &speech,
        samples.len(),
        sample_rate,
        max_chunk_secs,
    )
}

/// [`split_at_silence`] from precomputed frames, so a long track can be split
/// without holding its samples: `energies[i]` is the RMS ([`rms_energy`]) of
/// samples `i * frame_len .. (i + 1) * frame_len` for every whole frame of
/// the `total_len`-sample track, and `speech[i]` whether that frame holds
/// speech (the track's speech mask, `[audio] vad`).
///
/// A cut goes in the middle of the longest run of frames without speech in
/// the lookback, else at the quietest frame by energy. A frame `speech` has
/// no flag for counts as speech.
pub fn split_by_energies(
    energies: &[f64],
    speech: &[bool],
    total_len: usize,
    sample_rate: usize,
    max_chunk_secs: usize,
) -> Vec<Range<usize>> {
    let frame = frame_len(sample_rate);
    let max_len = (max_chunk_secs * sample_rate).max(frame);
    // Whole frames only, and at least one, so every cut makes progress.
    let lookback_frames = ((LOOKBACK_SECS * sample_rate).min(max_len / 2) / frame).max(1);

    let mut ranges = Vec::new();
    let mut start = 0;
    while start < total_len {
        if total_len - start <= max_len {
            ranges.push(start..total_len);
            break;
        }
        // Every cut is a whole number of frames from 0, so `start` is
        // frame-aligned and the frames up to the cap are all whole.
        let base = start / frame;
        let cap_frames = max_len / frame;
        let first = cap_frames.saturating_sub(lookback_frames).max(1);
        let window = base + first..base + cap_frames;
        let flags = speech.get(window.clone()).unwrap_or(&[]);
        let cut_frame = first + pick_cut(&energies[window], flags);
        let cut = start + cut_frame * frame;
        ranges.push(start..cut);
        start = cut;
    }
    ranges
}

/// Index into `energies` of the frame to cut at: the middle of the longest
/// run of frames without speech (the latest one on a tie), else the quietest
/// frame (again the latest on a tie). `speech[i]` flags frame `i`; a frame
/// past its end counts as speech.
fn pick_cut(energies: &[f64], speech: &[bool]) -> usize {
    let mut best: Option<(usize, usize)> = None; // (run start, run length)
    let mut run_start = None;
    for i in 0..energies.len() {
        if !speech.get(i).copied().unwrap_or(true) {
            let s = *run_start.get_or_insert(i);
            let len = i + 1 - s;
            if best.is_none_or(|(_, best_len)| len >= best_len) {
                best = Some((s, len));
            }
        } else {
            run_start = None;
        }
    }
    if let Some((s, len)) = best {
        return s + len / 2;
    }
    // Latest quietest frame on a tie, so uniform audio still fills the cap.
    energies
        .iter()
        .enumerate()
        .rev()
        .min_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = 1000; // 100-sample frames keep the fixtures small
    const LOUD: i16 = 3000;

    fn audio(pattern: &[(i16, usize)]) -> Vec<i16> {
        let mut out = Vec::new();
        for &(value, secs) in pattern {
            // Alternate sign so the RMS is |value|, like real audio.
            out.extend((0..secs * RATE).map(|i| if i % 2 == 0 { value } else { -value }));
        }
        out
    }

    fn assert_full_coverage(ranges: &[Range<usize>], len: usize) {
        assert_eq!(ranges.first().map(|r| r.start), Some(0));
        assert_eq!(ranges.last().map(|r| r.end), Some(len));
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "ranges must be contiguous");
        }
        assert!(ranges.iter().all(|r| r.start < r.end));
    }

    #[test]
    fn short_input_is_one_range() {
        let samples = audio(&[(LOUD, 5)]);
        assert_eq!(split_at_silence(&samples, RATE, 10, 0.01), vec![0..5000]);
        assert!(split_at_silence(&[], RATE, 10, 0.01).is_empty());
    }

    #[test]
    fn cap_is_respected_and_coverage_is_full() {
        let samples = audio(&[(LOUD, 95)]);
        let ranges = split_at_silence(&samples, RATE, 10, 0.01);
        assert_full_coverage(&ranges, samples.len());
        assert!(ranges.iter().all(|r| r.len() <= 10 * RATE));
        // Uniform audio: cuts go right at the cap, not at the lookback start.
        assert_eq!(ranges.len(), 10);
    }

    #[test]
    fn cuts_land_in_the_longest_silence() {
        // 20 s cap: speech 0-12 s, 1 s gap, speech, 3 s gap at 15-18 s, speech.
        let samples = audio(&[(LOUD, 12), (0, 1), (LOUD, 2), (0, 3), (LOUD, 10)]);
        let ranges = split_at_silence(&samples, RATE, 20, 0.01);
        assert_full_coverage(&ranges, samples.len());
        // Middle of the 3 s gap.
        assert_eq!(ranges[0].end, 16_500);
        let cut = ranges[0].end;
        assert!(samples[cut - 50..cut + 50].iter().all(|&s| s == 0));
    }

    #[test]
    fn no_silence_cuts_at_the_quietest_frame() {
        // Loud throughout except one quieter (still voiced) second at 7 s.
        let samples = audio(&[(LOUD, 7), (LOUD / 4, 1), (LOUD, 7)]);
        let ranges = split_at_silence(&samples, RATE, 10, 0.01);
        assert_full_coverage(&ranges, samples.len());
        assert!(
            (7000..8000).contains(&ranges[0].end),
            "cut at {} is not in the quiet second",
            ranges[0].end
        );
    }

    /// The 0.1.31 cut rule: the longest sub-threshold run, else the
    /// quietest frame, the latest on a tie.
    fn reference_pick_cut(energies: &[f64], threshold: f64) -> usize {
        let mut best: Option<(usize, usize)> = None;
        let mut run_start = None;
        for (i, &e) in energies.iter().enumerate() {
            if e < threshold {
                let s = *run_start.get_or_insert(i);
                let len = i + 1 - s;
                if best.is_none_or(|(_, best_len)| len >= best_len) {
                    best = Some((s, len));
                }
            } else {
                run_start = None;
            }
        }
        if let Some((s, len)) = best {
            return s + len / 2;
        }
        energies
            .iter()
            .enumerate()
            .rev()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map_or(0, |(i, _)| i)
    }

    /// The implementation before frame energies were precomputed (0.1.31's
    /// cut rule included), kept as the reference [`split_by_energies`] must
    /// match on an RMS mask.
    fn reference_split(
        samples: &[i16],
        sample_rate: usize,
        max_chunk_secs: usize,
        threshold: f64,
    ) -> Vec<Range<usize>> {
        let frame = (sample_rate * FRAME_MS / 1000).max(1);
        let max_len = (max_chunk_secs * sample_rate).max(frame);
        let lookback_frames = ((LOOKBACK_SECS * sample_rate).min(max_len / 2) / frame).max(1);
        let mut ranges = Vec::new();
        let mut start = 0;
        while start < samples.len() {
            if samples.len() - start <= max_len {
                ranges.push(start..samples.len());
                break;
            }
            let cap_frames = max_len / frame;
            let first = cap_frames.saturating_sub(lookback_frames).max(1);
            let energies: Vec<f64> = (first..cap_frames)
                .map(|f| {
                    let from = start + f * frame;
                    rms_energy(&samples[from..from + frame])
                })
                .collect();
            let cut = start + (first + reference_pick_cut(&energies, threshold)) * frame;
            ranges.push(start..cut);
            start = cut;
        }
        ranges
    }

    #[test]
    fn energies_split_matches_the_reference() {
        // Speech-like bursts of varying loudness and gaps, plus a partial
        // trailing frame, at a short cap so there are many cuts.
        let mut samples = Vec::new();
        for i in 0..400usize {
            let amp = [LOUD, 0, LOUD / 3, 0, LOUD / 8][i % 5];
            let secs = 1 + (i * 7) % 4;
            samples.extend((0..secs * RATE).map(|j| if j % 2 == 0 { amp } else { -amp }));
        }
        samples.extend([5i16; 37]);
        for cap in [7, 10, 30, 120] {
            let expected = reference_split(&samples, RATE, cap, 0.01);
            assert_eq!(split_at_silence(&samples, RATE, cap, 0.01), expected);
            let energies: Vec<f64> = samples
                .chunks_exact(frame_len(RATE))
                .map(rms_energy)
                .collect();
            let speech: Vec<bool> = energies.iter().map(|&e| e >= 0.01).collect();
            assert_eq!(
                split_by_energies(&energies, &speech, samples.len(), RATE, cap),
                expected
            );
        }
    }

    #[test]
    fn cuts_follow_the_speech_mask_and_the_energies_break_ties() {
        // Loud throughout, at a 10 s cap (lookback 5 s: frames 50 to 99).
        let energies = vec![0.1; 150];
        let len = 150 * frame_len(RATE);
        // The mask hears no speech at frames 60 to 69: the cut is in the
        // middle of that run, whatever the level.
        let mut speech = vec![true; 150];
        speech[60..70].fill(false);
        let ranges = split_by_energies(&energies, &speech, len, RATE, 10);
        assert_eq!(ranges[0].end, 65 * frame_len(RATE));
        // Speech throughout: the quietest frame by energy.
        let mut quiet = energies.clone();
        quiet[80] = 0.05;
        let ranges = split_by_energies(&quiet, &[true; 150], len, RATE, 10);
        assert_eq!(ranges[0].end, 80 * frame_len(RATE));
        // Frames without a flag count as speech.
        let ranges = split_by_energies(&quiet, &[], len, RATE, 10);
        assert_eq!(ranges[0].end, 80 * frame_len(RATE));
    }

    #[test]
    fn lookback_is_bounded_to_the_last_minute() {
        // A long silence early in a 10-minute cap must not be chosen over a
        // shorter one within the last minute: chunks stay near the cap.
        let samples = audio(&[(LOUD, 100), (0, 30), (LOUD, 440), (0, 2), (LOUD, 100)]);
        let ranges = split_at_silence(&samples, RATE, 600, 0.01);
        assert_full_coverage(&ranges, samples.len());
        assert_eq!(ranges[0].end, 571_000);
    }
}
