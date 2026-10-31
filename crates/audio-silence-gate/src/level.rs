//! Level meter: how loud the speech and the background are, and whether the
//! signal clips or carries a DC offset.
//!
//! Every [`SpeechDetector`](crate::SpeechDetector), and so every auto-stop,
//! gate and mask pass, runs one. The figures are measurements only: what
//! counts as too quiet or too noisy is left to the caller.

use crate::vad::WARMUP_FRAMES;

/// Level reported for digital silence, in dBFS. The quietest nonzero `i16`
/// signal sits near -90 dBFS, so this never collides with a real level.
pub(crate) const FLOOR_DBFS: f32 = -120.0;

/// A frame or buffer whose peak is at or under this is digital silence (a
/// muted mic, an idle system track), not a quiet room.
pub(crate) const SILENT_PEAK: u16 = 4;

/// A sample at or beyond this magnitude sits at the rail.
const RAIL: u16 = i16::MAX as u16;

/// A run of this many samples or more at the same rail counts as clipping.
/// A lone full-scale peak (or two) is a loud transient, not distortion.
const CLIP_RUN: u32 = 3;

/// Width of one noise histogram bin, in dB.
const BIN_DB: f32 = 0.25;

/// Noise histogram bins, covering [`FLOOR_DBFS`] to 0 dBFS.
const BINS: usize = (-FLOOR_DBFS / BIN_DB) as usize;

/// dBFS of a normalized RMS level (the [`rms_energy`](crate::rms_energy)
/// scale, 0.0 to 1.0), [`FLOOR_DBFS`] for silence.
pub(crate) fn dbfs(rms: f64) -> f32 {
    if rms <= 0.0 {
        return FLOOR_DBFS;
    }
    ((20.0 * rms.log10()) as f32).max(FLOOR_DBFS)
}

/// The sums one analysis frame contributes, computed once per frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameStats {
    sum_squares: f64,
    len: usize,
    pub(crate) peak: u16,
}

impl FrameStats {
    pub(crate) fn of(frame: &[i16]) -> Self {
        let sum_squares = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let peak = frame.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        Self {
            sum_squares,
            len: frame.len(),
            peak,
        }
    }

    /// Normalized RMS, computed exactly as [`rms_energy`](crate::rms_energy)
    /// computes it.
    pub(crate) fn rms(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        (self.sum_squares / self.len as f64).sqrt() / i16::MAX as f64
    }

    pub(crate) fn dbfs(&self) -> f32 {
        dbfs(self.rms())
    }
}

/// What a [`LevelMeter`] measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    /// Power mean of the speech frames, in dBFS: the speaker's loudness.
    /// `None` without speech.
    pub speech_dbfs: Option<f32>,
    /// Median of the non-speech frames, in dBFS: the background. The warm-up
    /// frames and digital silence are skipped, so an idle system track has
    /// `None`.
    pub noise_dbfs: Option<f32>,
    /// `speech_dbfs` minus `noise_dbfs`, when both exist.
    pub snr_db: Option<f32>,
    /// Share of samples (0.0 to 1.0) in runs of 3 or more at the same rail
    /// (`|s| >= 32767`). Single full-scale peaks do not count.
    pub clipped_share: f32,
    /// Mean sample as a share of full scale (signed): a constant shift some
    /// mics add.
    pub dc_offset: f32,
    /// Share of analysis frames (0.0 to 1.0) that were speech.
    pub speech_share: f32,
    /// Highest sample magnitude, in dBFS (-120 for digital silence).
    pub peak_dbfs: f32,
    /// Duration of everything measured, in milliseconds.
    pub duration_ms: u64,
}

/// Running level figures for one stream.
///
/// Feed every sample to [`add_samples`](Self::add_samples) (peak, DC offset,
/// clipping, duration) and every analysis frame, with its speech decision,
/// to [`add_frame`](Self::add_frame) (speech and noise levels).
#[derive(Debug, Clone)]
pub struct LevelMeter {
    sample_rate: u32,
    sample_count: u64,
    sample_sum: i64,
    peak: u16,
    clipped: u64,
    /// Length and sign of the run of rail samples the stream ends in.
    rail_run: u32,
    rail_positive: bool,
    frames: u64,
    speech_frames: u64,
    speech_sum_squares: f64,
    speech_samples: u64,
    noise_frames: u64,
    noise_bins: Vec<u32>,
}

impl LevelMeter {
    /// A meter for a stream at `sample_rate` Hz (only the duration uses it;
    /// `0` reports a duration of 0).
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            sample_count: 0,
            sample_sum: 0,
            peak: 0,
            clipped: 0,
            rail_run: 0,
            rail_positive: false,
            frames: 0,
            speech_frames: 0,
            speech_sum_squares: 0.0,
            speech_samples: 0,
            noise_frames: 0,
            noise_bins: vec![0; BINS],
        }
    }

    /// Count samples in stream order: peak, DC offset, clipping, duration.
    pub fn add_samples(&mut self, samples: &[i16]) {
        for &s in samples {
            let magnitude = s.unsigned_abs();
            self.peak = self.peak.max(magnitude);
            self.sample_sum += s as i64;
            if magnitude >= RAIL {
                let positive = s > 0;
                if self.rail_run > 0 && positive == self.rail_positive {
                    self.rail_run += 1;
                } else {
                    self.rail_run = 1;
                    self.rail_positive = positive;
                }
                // The run's first samples count once it is long enough.
                match self.rail_run.cmp(&CLIP_RUN) {
                    std::cmp::Ordering::Equal => self.clipped += CLIP_RUN as u64,
                    std::cmp::Ordering::Greater => self.clipped += 1,
                    std::cmp::Ordering::Less => {}
                }
            } else {
                self.rail_run = 0;
            }
        }
        self.sample_count += samples.len() as u64;
    }

    /// Count one analysis frame and whether it was speech: speech frames set
    /// the speech level, the others the noise floor (except the first
    /// frames, while a detector warms up, and digital silence).
    pub fn add_frame(&mut self, frame: &[i16], speech: bool) {
        self.record(FrameStats::of(frame), speech);
    }

    pub(crate) fn record(&mut self, stats: FrameStats, speech: bool) {
        let index = self.frames;
        self.frames += 1;
        if speech {
            self.speech_frames += 1;
            self.speech_sum_squares += stats.sum_squares;
            self.speech_samples += stats.len as u64;
        } else if index >= WARMUP_FRAMES && stats.peak > SILENT_PEAK {
            let bin = ((stats.dbfs() - FLOOR_DBFS) / BIN_DB) as usize;
            self.noise_bins[bin.min(BINS - 1)] += 1;
            self.noise_frames += 1;
        }
    }

    /// The figures so far.
    pub fn levels(&self) -> Levels {
        let speech_dbfs = (self.speech_samples > 0).then(|| {
            dbfs((self.speech_sum_squares / self.speech_samples as f64).sqrt() / i16::MAX as f64)
        });
        let noise_dbfs = self.noise_median();
        let snr_db = match (speech_dbfs, noise_dbfs) {
            (Some(speech), Some(noise)) => Some(speech - noise),
            _ => None,
        };
        let share = |part: u64, whole: u64| {
            if whole == 0 {
                0.0
            } else {
                (part as f64 / whole as f64) as f32
            }
        };
        let dc_offset = if self.sample_count == 0 {
            0.0
        } else {
            (self.sample_sum as f64 / self.sample_count as f64 / i16::MAX as f64) as f32
        };
        let duration_ms = if self.sample_rate == 0 {
            0
        } else {
            self.sample_count.saturating_mul(1000) / self.sample_rate as u64
        };
        Levels {
            speech_dbfs,
            noise_dbfs,
            snr_db,
            clipped_share: share(self.clipped, self.sample_count),
            dc_offset,
            speech_share: share(self.speech_frames, self.frames),
            peak_dbfs: dbfs(self.peak.min(RAIL) as f64 / i16::MAX as f64),
            duration_ms,
        }
    }

    /// Forget everything measured.
    pub fn reset(&mut self) {
        *self = Self::new(self.sample_rate);
    }

    /// The lower median of the noise frames, as its bin's centre.
    fn noise_median(&self) -> Option<f32> {
        if self.noise_frames == 0 {
            return None;
        }
        let middle = self.noise_frames.div_ceil(2);
        let mut seen = 0u64;
        for (bin, &count) in self.noise_bins.iter().enumerate() {
            seen += count as u64;
            if seen >= middle {
                return Some(FLOOR_DBFS + (bin as f32 + 0.5) * BIN_DB);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of alternating `±amplitude`: RMS `amplitude`, no DC.
    fn square(amplitude: i16, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn two_sample_rail_runs_are_not_clipping() {
        let mut meter = LevelMeter::new(16_000);
        // Two runs of 2 at the top rail, one of 2 at the bottom rail, and a
        // top-then-bottom pair, which is two runs of 1.
        meter.add_samples(&[0, 32767, 32767, 0, -32768, -32767, 100, 32767, -32768, 0]);
        assert_eq!(meter.levels().clipped_share, 0.0);
    }

    #[test]
    fn three_sample_rail_runs_are_clipping() {
        let mut meter = LevelMeter::new(16_000);
        // A run of 3 at the top, a run of 4 at the bottom: 7 of 20 samples.
        let mut samples = vec![0i16; 20];
        samples[2..5].fill(32767);
        samples[10..14].fill(-32768);
        meter.add_samples(&samples);
        assert!((meter.levels().clipped_share - 7.0 / 20.0).abs() < 1e-6);
    }

    #[test]
    fn rail_runs_carry_across_chunks() {
        let mut meter = LevelMeter::new(16_000);
        meter.add_samples(&[0, 32767, 32767]);
        meter.add_samples(&[32767, 0]);
        // One run of 3 split 2 + 1 over two chunks.
        assert!((meter.levels().clipped_share - 3.0 / 5.0).abs() < 1e-6);
    }

    #[test]
    fn dc_offset_is_the_mean_sample() {
        let mut meter = LevelMeter::new(16_000);
        // Mean +3277: 10% of full scale.
        let samples: Vec<i16> = square(1000, 1600).iter().map(|s| s + 3277).collect();
        meter.add_samples(&samples);
        assert!((meter.levels().dc_offset - 0.1).abs() < 1e-3);

        let mut meter = LevelMeter::new(16_000);
        meter.add_samples(&square(1000, 1600));
        assert_eq!(meter.levels().dc_offset, 0.0);
    }

    #[test]
    fn snr_of_a_tone_over_noise() {
        let mut meter = LevelMeter::new(16_000);
        // Warm-up frames are loud non-speech: they must not move the noise
        // floor.
        for _ in 0..WARMUP_FRAMES {
            let frame = square(10_000, 256);
            meter.add_samples(&frame);
            meter.add_frame(&frame, false);
        }
        // A tone at -30 dBFS as speech, a hiss at -60 dBFS as background.
        let tone: Vec<i16> = (0..256)
            .map(|i| ((i as f64 * 0.3).sin() * 1036.0 * std::f64::consts::SQRT_2) as i16)
            .collect();
        let mut state = 1u32;
        for frame_index in 0..100 {
            if frame_index % 2 == 0 {
                meter.add_samples(&tone);
                meter.add_frame(&tone, true);
            } else {
                let hiss: Vec<i16> = (0..256)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        // Uniform in [-57, 57]: RMS about 33 (-60 dBFS).
                        (state % 115) as i16 - 57
                    })
                    .collect();
                meter.add_samples(&hiss);
                meter.add_frame(&hiss, false);
            }
        }
        let levels = meter.levels();
        let speech = levels.speech_dbfs.unwrap();
        let noise = levels.noise_dbfs.unwrap();
        assert!((speech + 30.0).abs() < 0.5, "speech {speech}");
        assert!((noise + 60.0).abs() < 1.0, "noise {noise}");
        assert!((levels.snr_db.unwrap() - 30.0).abs() < 1.5);
        assert!((levels.speech_share - 50.0 / 110.0).abs() < 1e-6);
    }

    #[test]
    fn digital_silence_is_no_noise_floor() {
        let mut meter = LevelMeter::new(16_000);
        for _ in 0..50 {
            let frame = square(SILENT_PEAK as i16, 256);
            meter.add_samples(&frame);
            meter.add_frame(&frame, false);
        }
        let levels = meter.levels();
        assert_eq!(levels.noise_dbfs, None);
        assert_eq!(levels.speech_dbfs, None);
        assert_eq!(levels.snr_db, None);
        assert_eq!(levels.duration_ms, 800);
    }

    #[test]
    fn peak_and_reset() {
        let mut meter = LevelMeter::new(16_000);
        meter.add_samples(&[0, 3277, -100]);
        assert!((meter.levels().peak_dbfs + 20.0).abs() < 0.01);
        meter.reset();
        let levels = meter.levels();
        assert_eq!(levels.peak_dbfs, FLOOR_DBFS);
        assert_eq!(levels.duration_ms, 0);
        assert_eq!(levels.speech_share, 0.0);
    }
}
