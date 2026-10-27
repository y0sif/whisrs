//! Audio quality: verdicts on how a stream sounds (too quiet, noisy,
//! clipping, a DC offset, strong speaker echo), and the level each skip
//! toast names.
//!
//! The figures come from the gate crate's level meter ([`Levels`]); the
//! thresholds and the wording are whisrs policy and live here. Dictation uses
//! them as well as recordings, so this module is in every build. [`analyze`]
//! is the seam a mic check calls.

use audio_silence_gate::{GateReason, Levels, SpeechDetector, Vad, VadConfig};

use super::capture::SAMPLE_RATE;

/// Speech under this level (dBFS) is too quiet. A normal speaking level
/// sits near -40 dBFS.
pub const TOO_QUIET_DBFS: f32 = -55.0;

/// Speech less than this far above the background (dB) is noisy: ASR copes
/// to about 10 dB and degrades below about 6 dB.
pub const NOISY_SNR_DB: f32 = 10.0;

/// A clipped share of the samples (0.0 to 1.0) at or above this is
/// clipping: 0.1%.
pub const CLIPPING_SHARE: f32 = 0.001;

/// A DC offset (share of full scale, either sign) at or above this is
/// reported: 1%.
pub const DC_OFFSET_SHARE: f32 = 0.01;

/// A speaker echo path into the mic above this (dB) is strong. Real echo
/// paths ran -12.9 to -30.4 dB, so this flags only the loudest.
pub const STRONG_ECHO_DB: f32 = -15.0;

/// Peak level (dBFS) at or under which a stream is digital silence: between
/// the peaks of 4 (-78.3 dBFS, the gate's digital silence) and 5 (-76.3).
const DIGITAL_SILENCE_DBFS: f32 = -77.3;

/// What a stream measured: its levels by the detector that decided what was
/// speech, and the speaker echo path when echo removal ran on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityReport {
    pub vad: Vad,
    pub levels: Levels,
    /// The speaker echo's path into the mic, in dB: recordings only, when
    /// echo removal ran.
    pub echo_path_db: Option<f32>,
}

impl QualityReport {
    pub fn new(vad: Vad, levels: Levels) -> Self {
        Self {
            vad,
            levels,
            echo_path_db: None,
        }
    }

    /// One verdict per check the figures allow, ok or warn: the speech level
    /// and the SNR need speech (and a background), the echo needs echo
    /// removal. `system-silent` is never here: it is about a system track
    /// while the mic hears speech, which a recording decides live.
    pub fn verdicts(&self) -> Vec<Verdict> {
        let levels = &self.levels;
        let mut out = Vec::new();
        if let Some(speech) = levels.speech_dbfs {
            out.push(Verdict::of(Check::TooQuiet, speech));
        }
        if let Some(snr) = levels.snr_db {
            out.push(Verdict::of(Check::Noisy, snr));
        }
        out.push(Verdict::of(Check::Clipping, levels.clipped_share));
        out.push(Verdict::of(Check::DcOffset, levels.dc_offset));
        if let Some(echo) = self.echo_path_db {
            out.push(Verdict::of(Check::StrongEcho, echo));
        }
        out
    }

    /// The verdicts that warn.
    pub fn warnings(&self) -> Vec<Verdict> {
        self.verdicts().into_iter().filter(|v| v.warn).collect()
    }
}

/// Measure 16 kHz `samples` with `vad`, the open profile (any voice counts,
/// as on a recording track). Pure: the same samples give the same report.
pub fn analyze(samples: &[i16], vad: Vad) -> QualityReport {
    let mut meter = QualityMeter::new(vad);
    meter.feed(samples);
    meter.report()
}

/// [`analyze`] over a stream: feed it chunks of any size.
pub struct QualityMeter {
    vad: Vad,
    detector: SpeechDetector,
    /// Samples in speech frames so far.
    speech_samples: u64,
}

impl QualityMeter {
    pub fn new(vad: Vad) -> Self {
        Self {
            vad,
            detector: SpeechDetector::new(VadConfig::open(vad), SAMPLE_RATE),
            speech_samples: 0,
        }
    }

    pub fn feed(&mut self, samples: &[i16]) {
        let speech = &mut self.speech_samples;
        self.detector.feed(samples, |frame| {
            if frame.speech {
                *speech += audio_silence_gate::VAD_FRAME as u64;
            }
        });
    }

    /// Speech heard so far, in samples.
    pub fn speech_samples(&self) -> u64 {
        self.speech_samples
    }

    pub fn report(&self) -> QualityReport {
        QualityReport::new(self.vad, self.detector.levels())
    }
}

/// What a [`Verdict`] checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Check {
    TooQuiet,
    Noisy,
    Clipping,
    DcOffset,
    StrongEcho,
    SystemSilent,
}

impl Check {
    pub const ALL: [Check; 6] = [
        Check::TooQuiet,
        Check::Noisy,
        Check::Clipping,
        Check::DcOffset,
        Check::StrongEcho,
        Check::SystemSilent,
    ];

    /// The check's code, as `recording.json` stores a warning.
    pub fn code(self) -> &'static str {
        match self {
            Check::TooQuiet => "too-quiet",
            Check::Noisy => "noisy",
            Check::Clipping => "clipping",
            Check::DcOffset => "dc-offset",
            Check::StrongEcho => "strong-echo",
            Check::SystemSilent => "system-silent",
        }
    }

    /// The check a [`code`](Self::code) names.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// One check's outcome, ok or warn, with the figure it was decided on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    pub check: Check,
    /// Whether it warns; ok otherwise.
    pub warn: bool,
    /// The figure: dBFS for `too-quiet`, dB for `noisy` and `strong-echo`, a
    /// share (0.0 to 1.0) for `clipping` and `dc-offset` (signed), 0 for
    /// `system-silent`, which has none.
    pub value: f32,
}

impl Verdict {
    /// `check` decided on `value` by the thresholds above. `system-silent`
    /// has no figure: whoever raises it decided it, so it always warns.
    pub fn of(check: Check, value: f32) -> Self {
        let warn = match check {
            Check::TooQuiet => value < TOO_QUIET_DBFS,
            Check::Noisy => value < NOISY_SNR_DB,
            Check::Clipping => value >= CLIPPING_SHARE,
            Check::DcOffset => value.abs() >= DC_OFFSET_SHARE,
            Check::StrongEcho => value > STRONG_ECHO_DB,
            Check::SystemSilent => true,
        };
        Self { check, warn, value }
    }

    pub fn code(&self) -> &'static str {
        self.check.code()
    }

    /// What a warning tells the user, with its advice; an ok verdict gives
    /// the figure alone.
    pub fn message(&self) -> String {
        let v = self.value;
        match (self.check, self.warn) {
            (Check::TooQuiet, true) => {
                format!("too quiet: speech at {v:.0} dB. Move closer or raise the mic gain.")
            }
            (Check::TooQuiet, false) => format!("speech at {v:.0} dB"),
            (Check::Noisy, true) => format!(
                "noisy: speech only {v:.0} dB above the background. A noise filter helps \
                 (see troubleshooting)."
            ),
            (Check::Noisy, false) => format!("speech {v:.0} dB above the background"),
            (Check::Clipping, true) => format!(
                "clipping: {}% of the audio hit the limit. Lower the mic gain.",
                clip_percent(v)
            ),
            (Check::Clipping, false) => format!("{}% of the audio hit the limit", clip_percent(v)),
            (Check::DcOffset, _) => format!("DC offset of {:.0}% on the signal", v.abs() * 100.0),
            (Check::StrongEcho, true) => {
                format!("strong speaker echo ({v:.0} dB). Headphones give a cleaner mic track.")
            }
            (Check::StrongEcho, false) => format!("speaker echo at {v:.0} dB"),
            (Check::SystemSilent, _) => {
                "nothing played on the system track while your mic heard speech".to_string()
            }
        }
    }

    /// A warning in short, as the transcript's front matter lists it:
    /// "noisy (speech 7 dB above the background)".
    pub fn detail(&self) -> String {
        let v = self.value;
        match self.check {
            Check::TooQuiet => format!("too quiet (speech at {v:.0} dB)"),
            Check::Noisy => format!("noisy (speech {v:.0} dB above the background)"),
            Check::Clipping => format!("clipping ({}% of the audio)", clip_percent(v)),
            Check::DcOffset => format!("DC offset ({:.0}%)", v.abs() * 100.0),
            Check::StrongEcho => format!("strong speaker echo ({v:.0} dB)"),
            Check::SystemSilent => {
                "system silent (nothing played while the mic heard speech)".to_string()
            }
        }
    }
}

/// A clipped share as a percentage with one decimal: "0.4".
fn clip_percent(share: f32) -> String {
    format!("{:.1}", share * 100.0)
}

/// A gate's reason with the level that explains it, as a skip toast names
/// it: "silent (peak -82 dB)", "too quiet (speech at -68 dB)", "without
/// speech (background at -48 dB)", "too short (0.2 s)". An empty or invalid
/// buffer has no level to give.
pub fn skip_label(reason: GateReason, levels: &Levels) -> String {
    let label = reason.as_str();
    let peak = || format!("{label} (peak {:.0} dB)", levels.peak_dbfs);
    match reason {
        GateReason::Silent => peak(),
        GateReason::TooQuiet => match levels.speech_dbfs {
            Some(speech) => format!("{label} (speech at {speech:.0} dB)"),
            None => peak(),
        },
        GateReason::NoSpeech => match levels.noise_dbfs {
            Some(noise) => format!("{label} (background at {noise:.0} dB)"),
            None => peak(),
        },
        GateReason::TooShort => {
            format!("{label} ({:.1} s)", levels.duration_ms as f64 / 1000.0)
        }
        _ => label.to_string(),
    }
}

/// The advice a skip toast ends with: silent audio is most often a muted
/// mic.
pub fn skip_hint(reason: GateReason) -> Option<&'static str> {
    (reason == GateReason::Silent).then_some("Is the mic muted?")
}

/// Why a stream that produced no text had nothing to transcribe, as the gate
/// would name it, from what its auto-stop detector measured: `None` when the
/// detector heard speech (`heard_speech`, [`AutoStopDetector::has_speech`]),
/// since then the audio is not why. With `vad` = rms the only reason is
/// "silent", as with the gate.
///
/// [`AutoStopDetector::has_speech`]: audio_silence_gate::AutoStopDetector::has_speech
pub fn stream_skip_reason(vad: Vad, levels: &Levels, heard_speech: bool) -> Option<GateReason> {
    if heard_speech {
        return None;
    }
    Some(if levels.duration_ms == 0 {
        GateReason::Empty
    } else if vad == Vad::Rms || levels.peak_dbfs <= DIGITAL_SILENCE_DBFS {
        GateReason::Silent
    } else {
        GateReason::NoSpeech
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a square wave at `amplitude`, with no DC.
    fn square(amplitude: i16, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    fn levels() -> Levels {
        Levels {
            speech_dbfs: Some(-41.0),
            noise_dbfs: Some(-62.0),
            snr_db: Some(21.0),
            clipped_share: 0.0,
            dc_offset: 0.0,
            speech_share: 0.4,
            peak_dbfs: -12.0,
            duration_ms: 10_000,
        }
    }

    fn verdict(report: &QualityReport, check: Check) -> Option<Verdict> {
        report.verdicts().into_iter().find(|v| v.check == check)
    }

    #[test]
    fn a_clean_report_is_all_ok() {
        let mut report = QualityReport::new(Vad::Earshot, levels());
        report.echo_path_db = Some(-25.0);
        let verdicts = report.verdicts();
        let checks: Vec<Check> = verdicts.iter().map(|v| v.check).collect();
        assert_eq!(
            checks,
            [
                Check::TooQuiet,
                Check::Noisy,
                Check::Clipping,
                Check::DcOffset,
                Check::StrongEcho
            ]
        );
        assert!(verdicts.iter().all(|v| !v.warn), "{verdicts:?}");
        assert!(report.warnings().is_empty());
    }

    #[test]
    fn each_verdict_warns_past_its_threshold() {
        let cases = [
            (Check::TooQuiet, -55.0, -55.1, "too-quiet"),
            (Check::Noisy, 10.0, 9.9, "noisy"),
            (Check::Clipping, 0.000_99, 0.001, "clipping"),
            (Check::DcOffset, 0.009, -0.01, "dc-offset"),
            (Check::StrongEcho, -15.0, -14.9, "strong-echo"),
        ];
        for (check, ok, warn, code) in cases {
            assert!(!Verdict::of(check, ok).warn, "{code} at {ok}");
            assert!(Verdict::of(check, warn).warn, "{code} at {warn}");
            assert_eq!(check.code(), code);
            assert_eq!(Check::from_code(code), Some(check));
        }
        assert!(Verdict::of(Check::SystemSilent, 0.0).warn);
        assert_eq!(Check::from_code("system-silent"), Some(Check::SystemSilent));
        assert_eq!(Check::from_code("loud"), None);
    }

    #[test]
    fn a_report_warns_per_figure() {
        let mut quiet = levels();
        quiet.speech_dbfs = Some(-58.0);
        let report = QualityReport::new(Vad::Earshot, quiet);
        assert_eq!(
            report
                .warnings()
                .iter()
                .map(|v| v.code())
                .collect::<Vec<_>>(),
            ["too-quiet"]
        );

        let mut noisy = levels();
        noisy.snr_db = Some(7.0);
        noisy.clipped_share = 0.004;
        noisy.dc_offset = 0.02;
        let mut report = QualityReport::new(Vad::Earshot, noisy);
        report.echo_path_db = Some(-12.0);
        assert_eq!(
            report
                .warnings()
                .iter()
                .map(|v| v.code())
                .collect::<Vec<_>>(),
            ["noisy", "clipping", "dc-offset", "strong-echo"]
        );

        // No speech: nothing to judge the level or the SNR by; no echo
        // removal: no echo verdict.
        let mut silent = levels();
        silent.speech_dbfs = None;
        silent.snr_db = None;
        let report = QualityReport::new(Vad::Earshot, silent);
        assert_eq!(verdict(&report, Check::TooQuiet), None);
        assert_eq!(verdict(&report, Check::Noisy), None);
        assert_eq!(verdict(&report, Check::StrongEcho), None);
        assert!(verdict(&report, Check::Clipping).is_some());
    }

    #[test]
    fn messages_follow_the_table() {
        let message = |check, value| Verdict::of(check, value).message();
        assert_eq!(
            message(Check::TooQuiet, -58.2),
            "too quiet: speech at -58 dB. Move closer or raise the mic gain."
        );
        assert_eq!(
            message(Check::Noisy, 7.0),
            "noisy: speech only 7 dB above the background. A noise filter helps \
             (see troubleshooting)."
        );
        assert_eq!(
            message(Check::Clipping, 0.004),
            "clipping: 0.4% of the audio hit the limit. Lower the mic gain."
        );
        assert_eq!(
            message(Check::DcOffset, 0.02),
            "DC offset of 2% on the signal"
        );
        assert_eq!(
            message(Check::StrongEcho, -12.0),
            "strong speaker echo (-12 dB). Headphones give a cleaner mic track."
        );
        assert_eq!(
            message(Check::SystemSilent, 0.0),
            "nothing played on the system track while your mic heard speech"
        );
        assert_eq!(message(Check::TooQuiet, -41.0), "speech at -41 dB");
        assert_eq!(
            Verdict::of(Check::Noisy, 7.0).detail(),
            "noisy (speech 7 dB above the background)"
        );
    }

    /// Clipping counts runs of 3 or more samples at the rail; a run of 2 is
    /// a loud transient.
    #[test]
    fn analyze_counts_clip_runs_of_three_not_two() {
        let mut twos = square(1000, 16_000);
        let mut threes = twos.clone();
        for run in 0..20 {
            let at = 500 + run * 700;
            twos[at..at + 2].fill(i16::MAX);
            threes[at..at + 3].fill(i16::MAX);
        }
        let report = analyze(&twos, Vad::Rms);
        assert_eq!(report.levels.clipped_share, 0.0);
        assert!(!verdict(&report, Check::Clipping).unwrap().warn);

        let report = analyze(&threes, Vad::Rms);
        let share = report.levels.clipped_share;
        assert!((share - 60.0 / 16_000.0).abs() < 1e-6, "{share}");
        assert!(verdict(&report, Check::Clipping).unwrap().warn);
        // The same at the bottom rail.
        let bottom: Vec<i16> = threes
            .iter()
            .map(|&s| if s == i16::MAX { i16::MIN } else { s })
            .collect();
        assert!(
            verdict(&analyze(&bottom, Vad::Rms), Check::Clipping)
                .unwrap()
                .warn
        );
    }

    /// Speech just over the RMS test, a background 7 dB under it: noisy.
    #[test]
    fn analyze_measures_a_noisy_stream() {
        let mut samples = Vec::new();
        for block in 0..200 {
            // One 16 ms frame each: speech at about -45 dBFS, background at
            // about -52 dBFS.
            let amplitude = if block % 2 == 0 { 184 } else { 82 };
            samples.extend(square(amplitude, 256));
        }
        let report = analyze(&samples, Vad::Rms);
        let snr = report.levels.snr_db.unwrap();
        assert!((snr - 7.0).abs() < 0.5, "{snr}");
        assert!(verdict(&report, Check::Noisy).unwrap().warn);
        assert!(!verdict(&report, Check::TooQuiet).unwrap().warn);
        assert_eq!(analyze(&samples, Vad::Rms), report, "pure");
    }

    #[test]
    fn analyze_finds_a_dc_offset() {
        let samples: Vec<i16> = square(1000, 16_000).iter().map(|s| s + 655).collect();
        let report = analyze(&samples, Vad::Earshot);
        let dc = verdict(&report, Check::DcOffset).unwrap();
        assert!(dc.warn);
        assert_eq!(dc.message(), "DC offset of 2% on the signal");
    }

    /// Digital silence: no speech, no background, nothing to judge but the
    /// rails.
    #[test]
    fn analyze_digital_silence() {
        let report = analyze(&vec![0i16; 32_000], Vad::Earshot);
        assert_eq!(report.levels.speech_dbfs, None);
        assert_eq!(report.levels.noise_dbfs, None);
        assert_eq!(report.levels.duration_ms, 2_000);
        assert!(report.warnings().is_empty());
    }

    #[test]
    fn skip_labels_name_a_level() {
        let mut l = levels();
        l.peak_dbfs = -82.4;
        assert_eq!(skip_label(GateReason::Silent, &l), "silent (peak -82 dB)");
        l.speech_dbfs = Some(-68.0);
        assert_eq!(
            skip_label(GateReason::TooQuiet, &l),
            "too quiet (speech at -68 dB)"
        );
        l.noise_dbfs = Some(-48.2);
        assert_eq!(
            skip_label(GateReason::NoSpeech, &l),
            "without speech (background at -48 dB)"
        );
        // A background of digital silence only: the peak instead.
        l.noise_dbfs = None;
        assert_eq!(
            skip_label(GateReason::NoSpeech, &l),
            "without speech (peak -82 dB)"
        );
        l.duration_ms = 210;
        assert_eq!(skip_label(GateReason::TooShort, &l), "too short (0.2 s)");
        assert_eq!(skip_label(GateReason::Empty, &l), "empty");
        assert_eq!(skip_hint(GateReason::Silent), Some("Is the mic muted?"));
        assert_eq!(skip_hint(GateReason::NoSpeech), None);
    }

    #[test]
    fn a_stream_without_text_is_named_like_the_gate() {
        let mut l = levels();
        assert_eq!(stream_skip_reason(Vad::Earshot, &l, true), None);
        assert_eq!(
            stream_skip_reason(Vad::Earshot, &l, false),
            Some(GateReason::NoSpeech)
        );
        assert_eq!(
            stream_skip_reason(Vad::Rms, &l, false),
            Some(GateReason::Silent)
        );
        // A peak of 4 is digital silence, 5 is not.
        l.peak_dbfs = analyze(&[4, -4], Vad::Earshot).levels.peak_dbfs;
        assert_eq!(
            stream_skip_reason(Vad::Earshot, &l, false),
            Some(GateReason::Silent)
        );
        l.peak_dbfs = analyze(&[5, -5], Vad::Earshot).levels.peak_dbfs;
        assert_eq!(
            stream_skip_reason(Vad::Earshot, &l, false),
            Some(GateReason::NoSpeech)
        );
        l.duration_ms = 0;
        assert_eq!(
            stream_skip_reason(Vad::Earshot, &l, false),
            Some(GateReason::Empty)
        );
    }
}
