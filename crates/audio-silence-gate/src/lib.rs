//! Speech detection for audio capture: is anyone speaking, and how loud?
//!
//! - [`SpeechDetector`]: the earshot voice activity detector (a small learned
//!   model that scores every 16 ms of 16 kHz audio for speech) behind
//!   warm-up, level floors, onset confirmation, hysteresis and hangover.
//! - [`AutoStopDetector`]: stops a recording after a stretch of silence that
//!   follows speech.
//! - [`speech_gate`]: decides whether a recorded buffer is worth sending to a
//!   transcription backend, and why not.
//! - [`SpeechMask`]: per-frame speech flags over a long track, streamed.
//! - [`LevelMeter`]: speech level, background level, SNR, clipping and DC
//!   offset, run inside every detector.
//!
//! Each takes a [`VadConfig`]: [`VadConfig::close`] for one user near the mic,
//! [`VadConfig::open`] when any voice counts. [`Vad::Rms`] keeps the 0.1
//! loudness test ([`rms_energy`] against [`SILENCE_RMS_THRESHOLD`]), which the
//! functions below it ([`is_silent`], [`audio_gate_reason`],
//! [`AutoStopDetector::new`]) still use as they always did.

mod level;
mod vad;

pub use level::{LevelMeter, Levels};
pub use vad::{
    speech_gate, speech_mask, FrameDecision, GateOutcome, SpeechDetector, SpeechMask, Vad,
    VadConfig, VAD_FRAME, VAD_SAMPLE_RATE,
};

/// Normalized RMS threshold below which audio is considered silence.
///
/// Shared between [`AutoStopDetector`] and [`audio_gate_reason`] so the gate
/// agrees with auto-stop on what counts as silence — empirically, recordings
/// whose entire duration sits under this floor carry no usable speech.
pub const SILENCE_RMS_THRESHOLD: f64 = 0.003;

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

/// Reason a recorded buffer should be skipped instead of sent to a transcription
/// backend.
///
/// Returned by [`audio_gate_reason`] and [`speech_gate`]; carries a short
/// human-readable label that the daemon logs and surfaces in notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateReason {
    /// Buffer contains zero samples.
    Empty,
    /// Recording shorter than the minimum allowed duration.
    TooShort,
    /// Recording's RMS energy is below the configured silence threshold for
    /// its full duration ([`audio_gate_reason`], [`Vad::Rms`]), or its peak is
    /// digital silence ([`Vad::Earshot`]).
    Silent,
    /// Caller passed an invalid configuration (e.g. `sample_rate == 0`) so
    /// the gate cannot compute a meaningful verdict and conservatively
    /// rejects the buffer.
    Invalid,
    /// The model heard a voice, but under [`VadConfig::min_speech_dbfs`].
    TooQuiet,
    /// Sound, but under 160 ms of accepted speech.
    NoSpeech,
}

impl GateReason {
    /// Short human-readable label suitable for logs and notifications.
    pub fn as_str(self) -> &'static str {
        match self {
            GateReason::Empty => "empty",
            GateReason::TooShort => "too short",
            GateReason::Silent => "silent",
            GateReason::Invalid => "invalid",
            GateReason::TooQuiet => "too quiet",
            GateReason::NoSpeech => "without speech",
        }
    }
}

/// Decide whether an audio buffer is too empty/silent to bother sending to a
/// cloud transcription backend.
///
/// Returns `Some(reason)` when the buffer should be discarded outright, `None`
/// otherwise. Thresholds are intentionally permissive — the goal is to filter
/// out the obviously unusable cases (accidental hotkey taps, recordings that
/// captured only background hum) that would otherwise reach the API. Cloud
/// Whisper variants (whisper-1, gpt-4o-*-transcribe) hallucinate verbatim
/// chunks of the supplied `prompt` when the audio carries no speech, so this
/// gate is the first line of defence against prompt-echo output.
///
/// # Contract
///
/// `sample_rate` is expected to be the recording sample rate in Hz (e.g.
/// `16_000`) and must be greater than zero. If `sample_rate == 0` is passed
/// (a misconfiguration) the function does **not** panic: it returns
/// `Some(GateReason::Invalid)`, since there is no way to compute a
/// meaningful duration without a sample rate, and the safest default is to
/// treat the buffer as ungatable and skip it.
pub fn audio_gate_reason(
    samples: &[i16],
    sample_rate: u32,
    min_duration_ms: u64,
    threshold: f64,
) -> Option<GateReason> {
    duration_reason(samples, sample_rate, min_duration_ms)
        .or_else(|| is_silent(samples, threshold).then_some(GateReason::Silent))
}

/// The checks every gate runs before it looks at the audio: empty, no
/// sample rate, too short.
fn duration_reason(samples: &[i16], sample_rate: u32, min_duration_ms: u64) -> Option<GateReason> {
    if samples.is_empty() {
        return Some(GateReason::Empty);
    }
    if sample_rate == 0 {
        // Cannot compute duration without a sample rate; conservatively skip
        // the buffer rather than dividing by zero. This is a misconfiguration
        // distinct from a too-short recording, so report it as Invalid.
        return Some(GateReason::Invalid);
    }
    let duration_ms = (samples.len() as u64).saturating_mul(1000) / sample_rate as u64;
    if duration_ms < min_duration_ms {
        return Some(GateReason::TooShort);
    }
    None
}

/// Tracks the silence after speech and signals when it has lasted a
/// configured timeout, enabling auto-stop of recording.
pub struct AutoStopDetector {
    mode: StopMode,
    /// Consecutive silent samples that trigger auto-stop. `None` (a timeout
    /// of 0) never fires.
    timeout_samples: Option<u64>,
}

enum StopMode {
    /// The 0.1 rule: a whole chunk is speech when its RMS reaches
    /// `threshold`. The detector only meters.
    Rms {
        threshold: f64,
        silent_samples: u64,
        speech_detected: bool,
        meter: SpeechDetector,
    },
    /// The speech detector decides per 16 ms frame.
    Vad {
        detector: SpeechDetector,
        /// Samples since the last speech frame.
        silent_samples: u64,
        armed: bool,
    },
}

/// The timeout in samples; `None` for 0, which turns auto-stop off (#178).
fn timeout_samples(timeout_ms: u64, sample_rate: u32) -> Option<u64> {
    if timeout_ms == 0 {
        return None;
    }
    // Saturate to >=1 so that a misconfigured `sample_rate == 0` does not
    // yield 0, which would cause `feed()` to auto-stop on the first detected
    // speech chunk (since `silent_samples >= 0` is trivially true).
    Some(((timeout_ms.saturating_mul(sample_rate as u64)) / 1000).max(1))
}

impl AutoStopDetector {
    /// Create a new auto-stop detector on the RMS test (the 0.1 rule).
    ///
    /// - `threshold`: RMS silence threshold (e.g. 0.005).
    /// - `timeout_ms`: Duration of continuous silence (in milliseconds) to
    ///   trigger stop. `0` means never.
    /// - `sample_rate`: Audio sample rate in Hz (e.g. 16000).
    ///
    /// # Notes
    ///
    /// `sample_rate` is expected to be greater than zero. The constructor
    /// does **not** panic on `0`, but the timeout it enforces is then
    /// saturated to a single sample rather than the duration the caller
    /// intended: treat it as a misconfiguration.
    pub fn new(threshold: f64, timeout_ms: u64, sample_rate: u32) -> Self {
        let meter = SpeechDetector::new(
            VadConfig {
                rms_threshold: threshold,
                ..VadConfig::close(Vad::Rms)
            },
            sample_rate,
        );
        Self {
            mode: StopMode::Rms {
                threshold,
                silent_samples: 0,
                speech_detected: false,
                meter,
            },
            timeout_samples: timeout_samples(timeout_ms, sample_rate),
        }
    }

    /// Create an auto-stop detector on `cfg`'s speech test.
    ///
    /// With [`Vad::Rms`], or at any rate other than 16 kHz, this is exactly
    /// [`new(cfg.rms_threshold, ..)`](Self::new). With [`Vad::Earshot`] it
    /// counts the samples since the last speech frame of a
    /// [`SpeechDetector`] and fires when that count reaches the timeout,
    /// once armed: [`has_speech`](Self::has_speech) turns true after 160 ms
    /// of accepted speech in total (a frame accepted only through
    /// [`VadConfig::soft_threshold`] counting half, and only within its run
    /// of speech: a pause of 300 ms forgets the soft frames before it), so
    /// a short noise onset, or several apart, cannot arm it while a lone
    /// "yes" or soft talk does. A `timeout_ms` of 0 never fires.
    pub fn with_vad(cfg: VadConfig, timeout_ms: u64, sample_rate: u32) -> Self {
        if cfg.vad == Vad::Rms || sample_rate != VAD_SAMPLE_RATE {
            return Self::new(cfg.rms_threshold, timeout_ms, sample_rate);
        }
        Self::from_detector(SpeechDetector::new(cfg, sample_rate), timeout_ms)
    }

    /// Auto-stop over a 16 kHz `detector`.
    fn from_detector(detector: SpeechDetector, timeout_ms: u64) -> Self {
        Self {
            mode: StopMode::Vad {
                detector,
                silent_samples: 0,
                armed: false,
            },
            timeout_samples: timeout_samples(timeout_ms, VAD_SAMPLE_RATE),
        }
    }

    /// Feed a chunk of audio samples and return `true` if auto-stop
    /// should be triggered.
    pub fn feed(&mut self, samples: &[i16]) -> bool {
        let timeout = self.timeout_samples;
        let reached = |armed: bool, silent: u64| armed && timeout.is_some_and(|t| silent >= t);
        match &mut self.mode {
            StopMode::Rms {
                threshold,
                silent_samples,
                speech_detected,
                meter,
            } => {
                meter.feed(samples, |_| {});
                if is_silent(samples, *threshold) {
                    *silent_samples += samples.len() as u64;
                } else {
                    *silent_samples = 0;
                    *speech_detected = true;
                }

                // Only trigger auto-stop after speech has been detected.
                reached(*speech_detected, *silent_samples)
            }
            StopMode::Vad {
                detector,
                silent_samples,
                armed,
            } => {
                let frame_len = detector.frame_len() as u64;
                // Fires on the frame that reaches the timeout, even when
                // speech resumes later in the same chunk.
                let mut fired = None;
                detector.feed_steps(samples, |step| {
                    if step.decision.speech {
                        *silent_samples = 0;
                    } else {
                        *silent_samples += frame_len;
                    }
                    *armed |= step.enough;
                    fired = Some(fired == Some(true) || reached(*armed, *silent_samples));
                });
                fired.unwrap_or_else(|| reached(*armed, *silent_samples))
            }
        }
    }

    /// Reset the detector state.
    pub fn reset(&mut self) {
        match &mut self.mode {
            StopMode::Rms {
                silent_samples,
                speech_detected,
                meter,
                ..
            } => {
                *silent_samples = 0;
                *speech_detected = false;
                meter.reset();
            }
            StopMode::Vad {
                detector,
                silent_samples,
                armed,
            } => {
                detector.reset();
                *silent_samples = 0;
                *armed = false;
            }
        }
    }

    /// Whether any speech has been detected since creation or last reset
    /// (with earshot: 160 ms of it, soft frames counting half and only
    /// within their run, which is what arms auto-stop).
    pub fn has_speech(&self) -> bool {
        match &self.mode {
            StopMode::Rms {
                speech_detected, ..
            } => *speech_detected,
            StopMode::Vad { armed, .. } => *armed,
        }
    }

    /// Whether any voice has been heard since creation or last reset, armed
    /// or not ([`SpeechDetector::heard_voice`]): with earshot any frame
    /// voiced before the level floors and the arming count, so it comes
    /// before [`has_speech`](Self::has_speech) and also for a voice too
    /// quiet to arm; with the RMS test any 16 ms frame (or chunk) at or
    /// above the threshold.
    pub fn heard_voice(&self) -> bool {
        match &self.mode {
            StopMode::Rms {
                speech_detected,
                meter,
                ..
            } => *speech_detected || meter.heard_voice(),
            StopMode::Vad { detector, .. } => detector.heard_voice(),
        }
    }

    /// The meter's figures for the stream so far.
    pub fn levels(&self) -> Levels {
        match &self.mode {
            StopMode::Rms { meter, .. } => meter.levels(),
            StopMode::Vad { detector, .. } => detector.levels(),
        }
    }

    /// With earshot, when the stream so far is what [`speech_gate`] calls
    /// [`GateReason::TooQuiet`] (a voice the level floors rejected): its
    /// levels, `speech_dbfs` and `snr_db` that voice's, as the gate reports
    /// them. `None` otherwise, and always with the RMS test, which has no
    /// such verdict.
    pub fn too_quiet(&self) -> Option<Levels> {
        match &self.mode {
            StopMode::Rms { .. } => None,
            StopMode::Vad { detector, .. } => {
                let verdict = detector.verdict();
                (verdict.reason == Some(GateReason::TooQuiet)).then_some(verdict.levels)
            }
        }
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

    // --- audio_gate_reason tests ---

    #[test]
    fn gate_rejects_empty() {
        assert_eq!(
            audio_gate_reason(&[], 16_000, 300, 0.005),
            Some(GateReason::Empty)
        );
    }

    #[test]
    fn gate_rejects_too_short() {
        // 100ms at 16kHz = 1600 samples; threshold 300ms.
        let samples: Vec<i16> = vec![10_000; 1_600];
        assert_eq!(
            audio_gate_reason(&samples, 16_000, 300, 0.005),
            Some(GateReason::TooShort)
        );
    }

    #[test]
    fn gate_rejects_silent_long_recording() {
        // 1s of pure silence — long enough but below threshold.
        let samples = vec![0i16; 16_000];
        assert_eq!(
            audio_gate_reason(&samples, 16_000, 300, 0.005),
            Some(GateReason::Silent)
        );
    }

    #[test]
    fn gate_accepts_speech() {
        // ~50% amplitude tone, 1s — clearly above threshold.
        let samples: Vec<i16> = (0..16_000)
            .map(|i| ((i as f64 * 0.1).sin() * 16_000.0) as i16)
            .collect();
        assert_eq!(audio_gate_reason(&samples, 16_000, 300, 0.005), None);
    }

    #[test]
    fn gate_handles_zero_sample_rate_without_panic() {
        // Non-empty buffer with sample_rate = 0 must not panic (would have
        // divided by zero pre-fix). Reported as Invalid — a misconfiguration,
        // distinct from a duration verdict like TooShort.
        let samples: Vec<i16> = vec![10_000; 1_600];
        assert_eq!(
            audio_gate_reason(&samples, 0, 300, 0.005),
            Some(GateReason::Invalid)
        );

        // Empty buffer with sample_rate = 0 still hits the Empty path first.
        assert_eq!(
            audio_gate_reason(&[], 0, 300, 0.005),
            Some(GateReason::Empty)
        );
    }

    #[test]
    fn auto_stop_handles_zero_sample_rate() {
        // sample_rate = 0 must not yield timeout_samples = 0 (which would
        // make feed() auto-stop on the first speech chunk because
        // `silent_samples >= 0` is trivially true). Saturated to >=1.
        let mut detector = AutoStopDetector::new(0.01, 2000, 0);

        // Speech alone should NOT trigger auto-stop — speech_detected flips
        // but silent_samples is reset to 0, which is below the saturated
        // timeout of 1.
        let loud: Vec<i16> = vec![10_000; 1_600];
        assert!(!detector.feed(&loud));
        assert!(detector.has_speech());

        // A single silent sample is enough to trip the saturated timeout
        // of 1 once speech has been detected — this is the expected
        // misconfiguration behavior, documented on `new`.
        let silence = vec![0i16; 1];
        assert!(detector.feed(&silence));
    }

    #[test]
    fn auto_stop_handles_zero_timeout() {
        // timeout_ms = 0 turns auto-stop off (#178): speech then any amount
        // of silence never fires, in both constructors.
        let loud: Vec<i16> = vec![10_000; 1_600];
        let silence = vec![0i16; 16_000];
        for mut detector in [
            AutoStopDetector::new(0.01, 0, 16_000),
            AutoStopDetector::with_vad(VadConfig::close(Vad::Rms), 0, 16_000),
            AutoStopDetector::with_vad(VadConfig::close(Vad::Earshot), 0, 16_000),
        ] {
            detector.feed(&loud);
            for _ in 0..10 {
                assert!(!detector.feed(&silence));
            }
        }
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

    // --- Vad::Rms decides exactly as 0.1.31 did ---

    /// The 0.1.31 code, verbatim, as the reference.
    mod v0_1_31 {
        use super::super::{is_silent, rms_energy, GateReason};

        pub fn audio_gate_reason(
            samples: &[i16],
            sample_rate: u32,
            min_duration_ms: u64,
            threshold: f64,
        ) -> Option<GateReason> {
            if samples.is_empty() {
                return Some(GateReason::Empty);
            }
            if sample_rate == 0 {
                return Some(GateReason::Invalid);
            }
            let duration_ms = (samples.len() as u64).saturating_mul(1000) / sample_rate as u64;
            if duration_ms < min_duration_ms {
                return Some(GateReason::TooShort);
            }
            if is_silent(samples, threshold) {
                return Some(GateReason::Silent);
            }
            None
        }

        pub struct AutoStopDetector {
            threshold: f64,
            silent_samples: u64,
            timeout_samples: u64,
            speech_detected: bool,
        }

        impl AutoStopDetector {
            pub fn new(threshold: f64, timeout_ms: u64, sample_rate: u32) -> Self {
                let timeout_samples =
                    ((timeout_ms.saturating_mul(sample_rate as u64)) / 1000).max(1);
                Self {
                    threshold,
                    silent_samples: 0,
                    timeout_samples,
                    speech_detected: false,
                }
            }

            pub fn feed(&mut self, samples: &[i16]) -> bool {
                if is_silent(samples, self.threshold) {
                    self.silent_samples += samples.len() as u64;
                } else {
                    self.silent_samples = 0;
                    self.speech_detected = true;
                }
                self.speech_detected && self.silent_samples >= self.timeout_samples
            }

            pub fn has_speech(&self) -> bool {
                self.speech_detected
            }
        }

        /// `recording::process::scan_track`'s energies through
        /// `recording::speech::energy_mask`.
        pub fn energy_mask(samples: &[i16], frame: usize, threshold: f64) -> Vec<bool> {
            let mut buf = Vec::with_capacity(frame);
            let mut energies = Vec::new();
            for &sample in samples {
                buf.push(sample);
                if buf.len() == frame {
                    energies.push(rms_energy(&buf));
                    buf.clear();
                }
            }
            if !buf.is_empty() {
                energies.push(rms_energy(&buf));
            }
            energies.iter().map(|&e| e >= threshold).collect()
        }
    }

    /// Seeded audio around the 0.003 threshold: runs of random length at
    /// levels from digital silence to loud, cut into chunks of random size.
    fn chunks_around_threshold(seed: u64, count: usize) -> Vec<Vec<i16>> {
        let mut state = seed;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // 98.3 is 0.003 of full scale.
        const LEVELS: [i16; 8] = [0, 4, 50, 97, 98, 99, 300, 8000];
        (0..count)
            .map(|_| {
                let len = (next() % 2400) as usize;
                let level = LEVELS[(next() % LEVELS.len() as u64) as usize];
                (0..len)
                    .map(|i| {
                        let jitter = (next() % 3) as i16 - 1;
                        let s = level.saturating_add(jitter);
                        if i % 2 == 0 {
                            s
                        } else {
                            -s
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rms_auto_stop_matches_0_1_31() {
        for (seed, timeout_ms, sample_rate, threshold) in [
            (1, 1, 16_000, 0.003),
            (2, 50, 16_000, 0.003),
            (3, 100, 16_000, 0.01),
            (4, 500, 16_000, 0.003),
            (5, 300, 8_000, 0.003),
            (6, 300, 0, 0.003),
        ] {
            let chunks = chunks_around_threshold(seed, 400);
            let mut reference = v0_1_31::AutoStopDetector::new(threshold, timeout_ms, sample_rate);
            let rms = VadConfig {
                rms_threshold: threshold,
                ..VadConfig::close(Vad::Rms)
            };
            let mut detectors = vec![
                AutoStopDetector::new(threshold, timeout_ms, sample_rate),
                AutoStopDetector::with_vad(rms, timeout_ms, sample_rate),
            ];
            if sample_rate != VAD_SAMPLE_RATE {
                // earshot falls back to the same rule off 16 kHz.
                let earshot = VadConfig {
                    vad: Vad::Earshot,
                    ..rms
                };
                detectors.push(AutoStopDetector::with_vad(earshot, timeout_ms, sample_rate));
            }
            let mut fired = 0;
            for chunk in &chunks {
                let expected = reference.feed(chunk);
                fired += expected as usize;
                for detector in &mut detectors {
                    assert_eq!(detector.feed(chunk), expected, "seed {seed}");
                    assert_eq!(detector.has_speech(), reference.has_speech());
                }
            }
            assert!(fired > 0, "seed {seed} never fired: a weak comparison");
        }
    }

    #[test]
    fn rms_gate_matches_0_1_31() {
        let buffers = chunks_around_threshold(7, 600);
        for samples in &buffers {
            for sample_rate in [16_000, 8_000, 0] {
                for min_duration_ms in [0, 50, 300] {
                    let expected =
                        v0_1_31::audio_gate_reason(samples, sample_rate, min_duration_ms, 0.003);
                    assert_eq!(
                        audio_gate_reason(samples, sample_rate, min_duration_ms, 0.003),
                        expected
                    );
                    let rms = VadConfig::close(Vad::Rms);
                    assert_eq!(
                        speech_gate(samples, sample_rate, min_duration_ms, &rms).reason,
                        expected
                    );
                    if sample_rate != VAD_SAMPLE_RATE {
                        let earshot = VadConfig::close(Vad::Earshot);
                        assert_eq!(
                            speech_gate(samples, sample_rate, min_duration_ms, &earshot).reason,
                            expected
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rms_mask_matches_0_1_31() {
        for seed in 10..20 {
            let chunks = chunks_around_threshold(seed, 60);
            let track: Vec<i16> = chunks.concat();
            let expected = v0_1_31::energy_mask(&track, 1600, SILENCE_RMS_THRESHOLD);
            let rms = VadConfig::close(Vad::Rms);
            assert_eq!(speech_mask(&track, &rms, 1600), expected);
            // Streamed in the chunks' own sizes.
            let mut mask = SpeechMask::new(rms, 1600);
            for chunk in &chunks {
                mask.feed(chunk);
            }
            assert_eq!(mask.finish(), expected, "seed {seed}");
        }
    }
}
