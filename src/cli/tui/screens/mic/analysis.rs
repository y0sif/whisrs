//! What the Mic screen reads from a window of samples: its level and peak in
//! dBFS (decibels relative to full scale: 0 is the loudest a sample can be),
//! and a quality report of the latest seconds.
//!
//! [`Analyzer`] is all the meter calls: [`analyze`] for each window's level,
//! and `whisrs::audio::quality::analyze` over the last [`QUALITY_SPAN`]
//! samples, measured again every [`QUALITY_EVERY`], for the quality line.

use std::collections::VecDeque;

use audio_silence_gate::{rms_energy, Vad, SILENCE_RMS_THRESHOLD};
use whisrs::audio::quality::{self, QualityReport, Verdict};

/// What the quality line is measured over: the last 10 s at 16 kHz.
pub const QUALITY_SPAN: usize = 16_000 * 10;

/// How often it is measured again: every half second of audio.
pub const QUALITY_EVERY: usize = 8_000;

/// The quietest level shown: what silence reads as.
pub const MIN_DBFS: f32 = -120.0;

/// One window's reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicReading {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    pub quality: Option<Quality>,
}

/// How usable the input is for speech, over the last [`QUALITY_SPAN`], by
/// the detector `[audio] vad` names: the quality report's figures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    /// The speaker's level; `None` while nothing was speech.
    pub speech_dbfs: Option<f32>,
    /// The background; `None` while there is none to measure.
    pub noise_dbfs: Option<f32>,
    pub snr_db: Option<f32>,
    /// The first thing the report warns about: too quiet, noisy, clipping.
    pub warning: Option<Verdict>,
}

impl Quality {
    fn of(report: &QualityReport) -> Quality {
        Quality {
            speech_dbfs: report.levels.speech_dbfs,
            noise_dbfs: report.levels.noise_dbfs,
            snr_db: report.levels.snr_db,
            warning: report.warnings().first().copied(),
        }
    }
}

/// A stream's readings: each window's level, and the quality of the latest
/// seconds once [`QUALITY_EVERY`] of audio came.
pub struct Analyzer {
    vad: Vad,
    recent: VecDeque<i16>,
    since: usize,
    quality: Option<Quality>,
}

impl Analyzer {
    pub fn new(vad: Vad) -> Analyzer {
        Analyzer {
            vad,
            recent: VecDeque::with_capacity(QUALITY_SPAN),
            since: 0,
            quality: None,
        }
    }

    /// The reading of `window`, the stream's next 16-bit samples at `rate`
    /// Hz. The quality report runs at 16 kHz only, as the capture gives it.
    pub fn analyze(&mut self, window: &[i16], rate: u32) -> MicReading {
        let mut reading = analyze(window, rate);
        if rate != 16_000 {
            return reading;
        }
        self.recent.extend(window);
        let over = self.recent.len().saturating_sub(QUALITY_SPAN);
        self.recent.drain(..over);
        self.since += window.len();
        if self.since >= QUALITY_EVERY {
            self.since = 0;
            let report = quality::analyze(self.recent.make_contiguous(), self.vad);
            self.quality = Some(Quality::of(&report));
        }
        reading.quality = self.quality;
        reading
    }
}

/// The level of `window`, 16-bit samples at `rate` Hz; no quality, which
/// needs more than one window ([`Analyzer`]).
pub fn analyze(window: &[i16], rate: u32) -> MicReading {
    // The level does not depend on it.
    let _ = rate;
    let peak = window.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    MicReading {
        rms_dbfs: dbfs(rms_energy(window)),
        peak_dbfs: dbfs(f64::from(peak) / f64::from(i16::MAX)),
        quality: None,
    }
}

/// Where the silence gate sits: 20 log10 of its RMS threshold, about -50.
pub fn gate_dbfs() -> f32 {
    dbfs(SILENCE_RMS_THRESHOLD)
}

/// A level from 0 to 1 in dBFS, [`MIN_DBFS`] at the quietest.
fn dbfs(level: f64) -> f32 {
    if level <= 0.0 {
        return MIN_DBFS;
    }
    ((20.0 * level.log10()) as f32).max(MIN_DBFS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_read_in_dbfs() {
        let silence = analyze(&[0; 800], 16_000);
        assert_eq!(silence.rms_dbfs, MIN_DBFS);
        assert_eq!(silence.peak_dbfs, MIN_DBFS);
        assert_eq!(silence.quality, None);
        assert_eq!(analyze(&[], 16_000).rms_dbfs, MIN_DBFS);

        let full: Vec<i16> = (0..800)
            .map(|i| if i % 2 == 0 { i16::MAX } else { -i16::MAX })
            .collect();
        let loud = analyze(&full, 16_000);
        assert!(loud.rms_dbfs.abs() < 0.01, "{}", loud.rms_dbfs);
        assert!(loud.peak_dbfs.abs() < 0.01, "{}", loud.peak_dbfs);

        // A tenth of full scale is -20 dBFS.
        let tenth = analyze(&[3277; 800], 16_000);
        assert!((tenth.rms_dbfs + 20.0).abs() < 0.01, "{}", tenth.rms_dbfs);
    }

    #[test]
    fn the_gate_is_about_minus_50() {
        assert!((gate_dbfs() + 50.46).abs() < 0.01, "{}", gate_dbfs());
        // Just over the threshold reads over the gate.
        let level = (SILENCE_RMS_THRESHOLD * f64::from(i16::MAX) * 1.1) as i16;
        assert!(analyze(&[level; 800], 16_000).rms_dbfs > gate_dbfs());
    }

    /// A square wave of `amplitude`, 533 samples (one window), in runs of 8.
    fn tone(amplitude: i16) -> Vec<i16> {
        (0..533)
            .map(|i| if i % 16 < 8 { amplitude } else { -amplitude })
            .collect()
    }

    /// The quality line waits for half a second of audio, then measures the
    /// latest seconds: a quiet background, then a tone the RMS test calls
    /// speech, give a speech level, a background and the SNR between them.
    #[test]
    fn quality_follows_the_latest_seconds() {
        let mut analyzer = Analyzer::new(Vad::Rms);
        let quiet = vec![20i16; 533];
        assert_eq!(analyzer.analyze(&quiet, 16_000).quality, None);
        for _ in 0..30 {
            analyzer.analyze(&quiet, 16_000);
        }
        let mut last = analyzer.analyze(&tone(6_000), 16_000);
        for _ in 0..30 {
            last = analyzer.analyze(&tone(6_000), 16_000);
        }
        let quality = last.quality.expect("measured by now");
        let speech = quality.speech_dbfs.expect("the tone is speech");
        assert!((speech + 14.7).abs() < 1.0, "{speech}");
        assert!(quality.snr_db.is_some_and(|snr| snr > 30.0), "{quality:?}");
        assert_eq!(quality.warning, None);

        // Only the last 10 s are kept.
        for _ in 0..400 {
            analyzer.analyze(&tone(6_000), 16_000);
        }
        assert_eq!(analyzer.recent.len(), QUALITY_SPAN);

        // At another rate there is the level alone.
        let mut other = Analyzer::new(Vad::Rms);
        for _ in 0..40 {
            assert_eq!(other.analyze(&tone(6_000), 48_000).quality, None);
        }
    }

    /// A tone at full scale clips, and the line says so.
    #[test]
    fn clipping_is_warned_about() {
        let mut analyzer = Analyzer::new(Vad::Rms);
        let mut last = analyzer.analyze(&tone(i16::MAX), 16_000);
        for _ in 0..20 {
            last = analyzer.analyze(&tone(i16::MAX), 16_000);
        }
        let warning = last.quality.and_then(|q| q.warning).expect("a warning");
        assert_eq!(warning.check, whisrs::audio::quality::Check::Clipping);
        assert!(
            warning.message().starts_with("clipping:"),
            "{}",
            warning.message()
        );
    }
}
