//! What the Mic screen reads from a window of samples: its level and peak in
//! dBFS (decibels relative to full scale: 0 is the loudest a sample can be),
//! and a quality report.
//!
//! The one seam for the audio-quality work: `analyze` is all the screen
//! calls, and `quality` stays `None` (the quality line hidden) until that
//! work fills it in here.

use audio_silence_gate::{rms_energy, SILENCE_RMS_THRESHOLD};

/// The quietest level shown: what silence reads as.
pub const MIN_DBFS: f32 = -120.0;

/// One window's reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicReading {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    pub quality: Option<Quality>,
}

/// How usable the input is for speech. Filled by the audio-quality report;
/// `None` until then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    pub speech: bool,
    pub noise_dbfs: f32,
    pub snr_db: f32,
}

/// The reading of `window`, 16-bit samples at `rate` Hz.
pub fn analyze(window: &[i16], rate: u32) -> MicReading {
    // The quality report will need it; the level does not.
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
}
