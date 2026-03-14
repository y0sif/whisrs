//! Silence detection based on RMS energy.
//!
//! Used to detect when the user has stopped speaking, for auto-stop
//! and VAD-based chunk splitting.

/// Calculate the RMS (root mean square) energy of a slice of i16 samples.
///
/// Returns a value between 0.0 and 1.0 (normalized to the i16 range).
pub fn rms_energy(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_squares: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_squares / samples.len() as f64).sqrt();

    // Normalize to 0.0–1.0 range.
    rms / i16::MAX as f64
}

/// Check if a chunk of audio is below the silence threshold.
///
/// `threshold` is a normalized RMS value (0.0–1.0). Typical speech
/// produces RMS around 0.02–0.15; silence is usually below 0.005.
pub fn is_silent(samples: &[i16], threshold: f64) -> bool {
    rms_energy(samples) < threshold
}

/// Tracks consecutive silent frames and signals when silence has exceeded
/// a configured timeout, enabling auto-stop of recording.
pub struct AutoStopDetector {
    /// Silence threshold (normalized RMS, 0.0–1.0).
    threshold: f64,
    /// Number of consecutive silent samples accumulated.
    silent_samples: u64,
    /// Number of consecutive silent samples required to trigger auto-stop.
    timeout_samples: u64,
    /// Whether any speech has been detected yet (avoid auto-stop before
    /// the user starts speaking).
    speech_detected: bool,
}

impl AutoStopDetector {
    /// Create a new auto-stop detector.
    ///
    /// - `threshold`: RMS silence threshold (e.g. 0.005).
    /// - `timeout_ms`: Duration of continuous silence (in milliseconds) to trigger stop.
    /// - `sample_rate`: Audio sample rate (e.g. 16000).
    pub fn new(threshold: f64, timeout_ms: u64, sample_rate: u32) -> Self {
        let timeout_samples = (timeout_ms * sample_rate as u64) / 1000;
        Self {
            threshold,
            silent_samples: 0,
            timeout_samples,
            speech_detected: false,
        }
    }

    /// Feed a chunk of audio samples and return `true` if auto-stop
    /// should be triggered.
    pub fn feed(&mut self, samples: &[i16]) -> bool {
        if is_silent(samples, self.threshold) {
            self.silent_samples += samples.len() as u64;
        } else {
            self.silent_samples = 0;
            self.speech_detected = true;
        }

        // Only trigger auto-stop after speech has been detected.
        self.speech_detected && self.silent_samples >= self.timeout_samples
    }

    /// Reset the detector state.
    pub fn reset(&mut self) {
        self.silent_samples = 0;
        self.speech_detected = false;
    }

    /// Whether any speech has been detected since creation or last reset.
    pub fn has_speech(&self) -> bool {
        self.speech_detected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_zero_rms() {
        let silence = vec![0i16; 1600];
        assert_eq!(rms_energy(&silence), 0.0);
        assert!(is_silent(&silence, 0.01));
    }

    #[test]
    fn empty_slice_is_zero_rms() {
        assert_eq!(rms_energy(&[]), 0.0);
    }

    #[test]
    fn loud_signal_has_high_rms() {
        let loud: Vec<i16> = vec![i16::MAX; 1600];
        let rms = rms_energy(&loud);
        assert!(rms > 0.9, "loud signal RMS should be near 1.0, got {rms}");
        assert!(!is_silent(&loud, 0.01));
    }

    #[test]
    fn quiet_signal_is_detected() {
        // Low-level samples (~1% of max).
        let quiet: Vec<i16> = (0..1600).map(|i| ((i % 100) as i16) - 50).collect();
        let rms = rms_energy(&quiet);
        assert!(rms < 0.01, "quiet signal RMS should be low, got {rms}");
        assert!(is_silent(&quiet, 0.01));
    }

    #[test]
    fn medium_signal() {
        // ~50% amplitude sine-ish wave.
        let medium: Vec<i16> = (0..1600)
            .map(|i| ((i as f64 * 0.1).sin() * 16000.0) as i16)
            .collect();
        let rms = rms_energy(&medium);
        assert!(rms > 0.1, "medium signal should have noticeable RMS");
        assert!(!is_silent(&medium, 0.01));
    }

    // --- AutoStopDetector tests ---

    #[test]
    fn auto_stop_not_triggered_without_speech() {
        // 16kHz, 2000ms timeout, threshold 0.01.
        let mut detector = AutoStopDetector::new(0.01, 2000, 16_000);

        // Feed 3 seconds of silence — should NOT trigger because no speech detected.
        let silence = vec![0i16; 16_000]; // 1 second
        assert!(!detector.feed(&silence));
        assert!(!detector.feed(&silence));
        assert!(!detector.feed(&silence));
        assert!(!detector.has_speech());
    }

    #[test]
    fn auto_stop_triggered_after_speech_then_silence() {
        let mut detector = AutoStopDetector::new(0.01, 2000, 16_000);

        // Feed some loud audio (speech).
        let loud: Vec<i16> = vec![10000; 16_000]; // 1 second of speech
        assert!(!detector.feed(&loud));
        assert!(detector.has_speech());

        // Feed 1 second of silence — not enough yet (need 2000ms).
        let silence = vec![0i16; 16_000];
        assert!(!detector.feed(&silence));

        // Feed another second — now 2 seconds of silence, should trigger.
        assert!(detector.feed(&silence));
    }

    #[test]
    fn auto_stop_resets_on_speech() {
        let mut detector = AutoStopDetector::new(0.01, 2000, 16_000);

        let loud: Vec<i16> = vec![10000; 16_000];
        let silence = vec![0i16; 16_000];

        // Speech, then 1.5s silence.
        detector.feed(&loud);
        detector.feed(&silence);
        // Half second more silence.
        let half_sec = vec![0i16; 8_000];
        assert!(!detector.feed(&half_sec));

        // More speech — resets silence counter.
        assert!(!detector.feed(&loud));

        // 1.5s silence again — not enough.
        detector.feed(&silence);
        assert!(!detector.feed(&half_sec));

        // Another 0.5s — now 2s total since last speech.
        assert!(detector.feed(&half_sec));
    }

    #[test]
    fn auto_stop_reset() {
        let mut detector = AutoStopDetector::new(0.01, 2000, 16_000);

        let loud: Vec<i16> = vec![10000; 16_000];
        detector.feed(&loud);
        assert!(detector.has_speech());

        detector.reset();
        assert!(!detector.has_speech());
    }

    #[test]
    fn auto_stop_exact_threshold() {
        // Timeout of exactly 1600 samples (100ms at 16kHz).
        let mut detector = AutoStopDetector::new(0.01, 100, 16_000);

        let loud: Vec<i16> = vec![10000; 1600];
        detector.feed(&loud);

        // Feed exactly 1600 silent samples.
        let silence = vec![0i16; 1600];
        assert!(detector.feed(&silence));
    }
}
