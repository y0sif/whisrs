//! The VAD gate behind `scripts/check-vad.sh`: the shipped detectors
//! (`SpeechDetector`, `AutoStopDetector`, `speech_gate`, all on the `close`
//! profile) on synthetic speech in noise, with the RMS baseline beside them.
//!
//! Usage:
//!   cargo run --release -p audio-silence-gate --example vad_check -- \
//!       <fixture_dir> [--speech-dbfs -40] [--real a.wav b.wav ...]
//!
//! `<fixture_dir>` holds the WAVs of `scripts/gen-vad-fixture.sh`. Speech is
//! scaled to `--speech-dbfs` (active level) and the noise beds are mixed in
//! here, so each SNR is exact. `--real` prints the first 5 minutes of each
//! given 16 kHz mono track (his recordings) and is never gated.
//!
//! Exit code: 0 when every gated row passes, 1 when one fails, 2 on bad
//! input.

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use audio_silence_gate::{
    speech_gate, speech_mask, AutoStopDetector, GateReason, SpeechDetector, Vad, VadConfig,
    VAD_FRAME, VAD_SAMPLE_RATE,
};

const SR: usize = VAD_SAMPLE_RATE as usize;
const FRAME: usize = VAD_FRAME;
/// The 100 ms frame of whisrs's RMS decisions (phrase splits, track masks).
const RMS_FRAME: usize = 1600;
/// Capture chunk (`src/audio/device.rs`, 50 ms).
const CHUNK: usize = 800;
/// Clips are cut to a multiple of both frames.
const ALIGN: usize = 6400;
/// `[general] silence_timeout_ms` default.
const TIMEOUT_MS: u64 = 2000;
/// The daemon's batch gate minimum (`AUDIO_GATE_MIN_MS`).
const GATE_MIN_MS: u64 = 300;
/// Auto-stop must fire this long after the last word.
const STOP_WINDOW: (f64, f64) = (1.9, 2.6);
const BABBLE_STOP_WINDOW: (f64, f64) = (1.9, 3.0);
/// Noise rows run auto-stop over this many draws of the bed, 10 s apart:
/// one draw can pass or fail by luck at 0 dB SNR.
const DRAWS: usize = 6;
const DRAW_STEP_S: usize = 10;

fn close(vad: Vad) -> VadConfig {
    VadConfig::close(vad)
}

/// 16-bit PCM mono 16 kHz samples of a WAV, at most `max` of them. A data
/// chunk whose size a crash left unwritten is read to the end of the file.
fn read_wav(path: &str, max: Option<usize>) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{path}: not a RIFF WAVE file"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut pos = 12;
    let mut format_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            let (format, channels, rate, bits) = (
                u16_at(body),
                u16_at(body + 2),
                u32_at(body + 4),
                u16_at(body + 14),
            );
            if !(format == 1 || format == 0xFFFE)
                || channels != 1
                || rate != SR as u32
                || bits != 16
            {
                return Err(format!(
                    "{path}: need 16 kHz mono 16-bit PCM, got format {format}, {channels} ch, {rate} Hz, {bits} bit"
                ));
            }
            format_ok = true;
        } else if id == b"data" {
            if !format_ok {
                return Err(format!("{path}: data before fmt"));
            }
            let end = if size == 0 || body + size > bytes.len() {
                bytes.len()
            } else {
                body + size
            };
            let mut samples: Vec<i16> = bytes[body..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            if let Some(max) = max {
                samples.truncate(max);
            }
            return Ok(samples);
        }
        pos = body + size + (size & 1);
    }
    Err(format!("{path}: no data chunk"))
}

fn to_f(s: &[i16]) -> Vec<f64> {
    s.iter().map(|&x| x as f64 / i16::MAX as f64).collect()
}

fn to_i16(s: &[f64]) -> Vec<i16> {
    s.iter()
        .map(|&x| (x * i16::MAX as f64).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

fn rms_f(s: &[f64]) -> f64 {
    (s.iter().map(|x| x * x).sum::<f64>() / s.len().max(1) as f64).sqrt()
}

fn db_to_rms(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Reference speech labels on the 16 ms grid, from the noise-free source:
/// frame RMS at most 40 dB under the loudest frame, gaps under 192 ms
/// closed, islands under 48 ms dropped.
fn reference_labels(clean: &[i16]) -> Vec<bool> {
    let n = clean.len() / FRAME;
    let energy: Vec<f64> = (0..n)
        .map(|i| audio_silence_gate::rms_energy(&clean[i * FRAME..(i + 1) * FRAME]))
        .collect();
    let peak = energy.iter().cloned().fold(0.0, f64::max);
    let mut labels: Vec<bool> = energy.iter().map(|&e| e >= peak * 0.01).collect();
    let mut last: Option<usize> = None;
    for i in 0..n {
        if labels[i] {
            if let Some(l) = last {
                let gap = i - l - 1;
                if gap > 0 && gap < 12 {
                    labels[l + 1..i].fill(true);
                }
            }
            last = Some(i);
        }
    }
    let mut i = 0;
    while i < n {
        if !labels[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && labels[i] {
            i += 1;
        }
        if i - start < 3 {
            labels[start..i].fill(false);
        }
    }
    labels
}

/// RMS over the labelled speech frames.
fn active_rms(clean: &[f64], labels: &[bool]) -> f64 {
    let (mut acc, mut n) = (0.0, 0usize);
    for (i, _) in labels.iter().enumerate().filter(|(_, &l)| l) {
        for &x in &clean[i * FRAME..(i + 1) * FRAME] {
            acc += x * x;
            n += 1;
        }
    }
    (acc / n.max(1) as f64).sqrt()
}

/// Per-16 ms speech flags of the shipped earshot detector, fed in capture
/// chunks.
fn earshot_flags(samples: &[i16]) -> Vec<bool> {
    let mut detector = SpeechDetector::new(close(Vad::Earshot), VAD_SAMPLE_RATE);
    let mut flags = Vec::with_capacity(samples.len() / FRAME);
    for chunk in samples.chunks(CHUNK) {
        detector.feed(chunk, |d| flags.push(d.speech));
    }
    flags
}

/// The RMS rule's 100 ms decisions (whisrs's mask) on the 16 ms grid: each
/// 16 ms frame takes the decision of the 100 ms frame its centre lies in.
fn rms_flags(samples: &[i16]) -> Vec<bool> {
    let mask = speech_mask(samples, &close(Vad::Rms), RMS_FRAME);
    (0..samples.len() / FRAME)
        .map(|i| mask[((i * FRAME + FRAME / 2) / RMS_FRAME).min(mask.len() - 1)])
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Prf {
    precision: f64,
    recall: f64,
    f1: f64,
}

fn prf(pred: &[bool], reference: &[bool]) -> Prf {
    let (mut tp, mut fp, mut fn_) = (0f64, 0f64, 0f64);
    for (&p, &r) in pred.iter().zip(reference) {
        match (p, r) {
            (true, true) => tp += 1.0,
            (true, false) => fp += 1.0,
            (false, true) => fn_ += 1.0,
            (false, false) => {}
        }
    }
    let precision = if tp + fp > 0.0 { tp / (tp + fp) } else { 0.0 };
    let recall = if tp + fn_ > 0.0 { tp / (tp + fn_) } else { 0.0 };
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    Prf {
        precision,
        recall,
        f1,
    }
}

fn flagged_share(flags: &[bool]) -> f64 {
    flags.iter().filter(|&&f| f).count() as f64 / flags.len().max(1) as f64
}

/// When auto-stop armed and fired, in seconds from the clip start.
#[derive(Clone, Copy)]
struct Stop {
    armed_s: Option<f64>,
    fired_s: Option<f64>,
}

fn auto_stop(samples: &[i16], cfg: VadConfig) -> Stop {
    let mut stop = AutoStopDetector::with_vad(cfg, TIMEOUT_MS, VAD_SAMPLE_RATE);
    let mut armed_s = None;
    for (c, chunk) in samples.chunks(CHUNK).enumerate() {
        let fired = stop.feed(chunk);
        let t = ((c * CHUNK + chunk.len()) as f64) / SR as f64;
        if armed_s.is_none() && stop.has_speech() {
            armed_s = Some(t);
        }
        if fired {
            return Stop {
                armed_s,
                fired_s: Some(t),
            };
        }
    }
    Stop {
        armed_s,
        fired_s: None,
    }
}

impl Stop {
    /// Seconds after `speech_end` it fired.
    fn after(&self, speech_end: f64) -> Option<f64> {
        self.fired_s.map(|t| t - speech_end)
    }

    fn within(&self, speech_end: f64, window: (f64, f64)) -> bool {
        self.after(speech_end)
            .is_some_and(|d| d >= window.0 && d <= window.1)
    }

    fn label(&self, speech_end: f64) -> String {
        match (self.fired_s, self.armed_s) {
            (Some(t), _) if t < speech_end => format!("EARLY {:.2} s", t - speech_end),
            (Some(t), _) => format!("+{:.2} s", t - speech_end),
            (None, None) => "never (not armed)".into(),
            (None, Some(_)) => "never".into(),
        }
    }
}

fn gate_label(reason: Option<GateReason>) -> &'static str {
    reason.map_or("passes", GateReason::as_str)
}

struct Clips {
    clean: Vec<f64>,
    clean_labels: Vec<bool>,
    auto: Vec<f64>,
    auto_start: f64,
    auto_end: f64,
    yes: Vec<f64>,
    yes_end: f64,
    beds: Vec<(&'static str, Vec<f64>)>,
    speech_rms: f64,
}

/// Seconds of the first and past the last labelled frame.
fn speech_span(labels: &[bool]) -> (f64, f64) {
    let first = labels.iter().position(|&l| l).unwrap_or(0);
    let last = labels.iter().rposition(|&l| l).map_or(0, |i| i + 1);
    (
        (first * FRAME) as f64 / SR as f64,
        (last * FRAME) as f64 / SR as f64,
    )
}

fn load(dir: &str, speech_dbfs: f64) -> Result<Clips, String> {
    let read = |name: &str| -> Result<Vec<i16>, String> {
        let mut s = read_wav(&format!("{dir}/{name}.wav"), None)?;
        s.truncate(s.len() / ALIGN * ALIGN);
        if s.is_empty() {
            return Err(format!("{dir}/{name}.wav: too short"));
        }
        Ok(s)
    };
    let speech_rms = db_to_rms(speech_dbfs);
    // Speech scaled to the target active level.
    let scale = |raw: &[i16]| -> (Vec<f64>, Vec<bool>) {
        let labels = reference_labels(raw);
        let f = to_f(raw);
        let gain = speech_rms / active_rms(&f, &labels);
        (f.iter().map(|x| x * gain).collect(), labels)
    };
    let (clean, clean_labels) = scale(&read("clean")?);
    let (auto, auto_labels) = scale(&read("autostop")?);
    let (yes, yes_labels) = scale(&read("yes")?);
    let (auto_start, auto_end) = speech_span(&auto_labels);
    let (_, yes_end) = speech_span(&yes_labels);
    let mut beds = Vec::new();
    for name in ["pink", "brown", "babble"] {
        let bed = to_f(&read(name)?);
        // The draws of the auto-stop clip and the clean clip end before the
        // last 10 s, the noise-only clip.
        let needed = ((DRAWS - 1) * DRAW_STEP_S * SR + auto.len()).max(clean.len()) + 10 * SR;
        if bed.len() < needed {
            return Err(format!("{dir}/{name}.wav: too short for the clips"));
        }
        beds.push((name, bed));
    }
    Ok(Clips {
        clean,
        clean_labels,
        auto,
        auto_start,
        auto_end,
        yes,
        yes_end,
        beds,
        speech_rms,
    })
}

/// One measured condition: the clips mixed with a bed at an SNR.
struct Measured {
    earshot: Prf,
    rms: Prf,
    noise_fpr: Option<(f64, f64)>,
    stop_earshot: Stop,
    stop_rms: Stop,
    gate_earshot: Option<GateReason>,
    gate_rms: Option<GateReason>,
}

fn measure(clips: &Clips, clean: &[f64], auto: &[f64], noise_only: Option<&[f64]>) -> Measured {
    let clean = to_i16(clean);
    let auto = to_i16(auto);
    let noise_fpr = noise_only.map(|n| {
        let n = to_i16(n);
        (
            flagged_share(&earshot_flags(&n)),
            flagged_share(&rms_flags(&n)),
        )
    });
    Measured {
        earshot: prf(&earshot_flags(&clean), &clips.clean_labels),
        rms: prf(&rms_flags(&clean), &clips.clean_labels),
        noise_fpr,
        stop_earshot: auto_stop(&auto, close(Vad::Earshot)),
        stop_rms: auto_stop(&auto, close(Vad::Rms)),
        gate_earshot: speech_gate(&auto, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Earshot)).reason,
        gate_rms: speech_gate(&auto, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Rms)).reason,
    }
}

/// Auto-stop on the auto-stop clip over [`DRAWS`] draws of `bed` at `gain`:
/// whether every draw fired inside `window`, and a label.
fn stops_over_draws(clips: &Clips, bed: &[f64], gain: f64, window: (f64, f64)) -> (bool, String) {
    let end = clips.auto_end;
    let stops: Vec<Stop> = (0..DRAWS)
        .map(|k| {
            let auto = to_i16(&mix(&clips.auto, bed, k * DRAW_STEP_S * SR, gain, 0));
            auto_stop(&auto, close(Vad::Earshot))
        })
        .collect();
    let all_in = stops.iter().all(|s| s.within(end, window));
    let fired: Vec<f64> = stops.iter().filter_map(|s| s.after(end)).collect();
    let label = if fired.len() == stops.len() && fired.iter().all(|&d| d >= 0.0) {
        let lo = fired.iter().cloned().fold(f64::MAX, f64::min);
        let hi = fired.iter().cloned().fold(f64::MIN, f64::max);
        format!("stop +{lo:.2} to +{hi:.2} s ({DRAWS} draws)")
    } else {
        let each: Vec<String> = stops.iter().map(|s| s.label(end)).collect();
        format!("stop {} ({DRAWS} draws)", each.join(", "))
    };
    (all_in, label)
}

/// `speech` plus `bed[offset..]` scaled to `noise_rms`, from sample `from`.
fn mix(speech: &[f64], bed: &[f64], offset: usize, gain: f64, from: usize) -> Vec<f64> {
    speech
        .iter()
        .enumerate()
        .map(|(i, x)| {
            if i < from {
                *x
            } else {
                x + gain * bed[offset + i]
            }
        })
        .collect()
}

struct Report {
    md: String,
    failed: Vec<String>,
}

impl Report {
    fn row(&mut self, name: &str, earshot: String, rms: String, rule: &str, pass: Option<bool>) {
        let verdict = match pass {
            Some(true) => "PASS",
            Some(false) => {
                self.failed.push(name.to_string());
                "FAIL"
            }
            None => "printed",
        };
        let _ = writeln!(
            self.md,
            "| {name} | {earshot} | {rms} | {rule} | {verdict} |"
        );
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

fn prf_label(m: &Prf) -> String {
    format!("P/R/F1 {:.3}/{:.3}/{:.3}", m.precision, m.recall, m.f1)
}

fn run(clips: &Clips, real: &[String]) -> Report {
    let mut report = Report {
        md: String::new(),
        failed: Vec::new(),
    };
    let r = &mut report;
    let _ = writeln!(
        r.md,
        "speech active level {:.1} dBFS; clean clip {:.1} s; auto-stop clip speech {:.2} to {:.2} s of {:.1} s; timeout {TIMEOUT_MS} ms; profile close\n",
        20.0 * clips.speech_rms.log10(),
        clips.clean.len() as f64 / SR as f64,
        clips.auto_start,
        clips.auto_end,
        clips.auto.len() as f64 / SR as f64,
    );
    let _ = writeln!(r.md, "| row | earshot | RMS 0.003 | pass when | result |");
    let _ = writeln!(r.md, "|---|---|---|---|---|");
    let end = clips.auto_end;
    let mut cpu_audio: Vec<i16> = Vec::new();

    // Clean.
    let m = measure(clips, &clips.clean, &clips.auto, None);
    cpu_audio.extend(to_i16(&clips.clean));
    r.row(
        "clean",
        format!(
            "{}; stop {}",
            prf_label(&m.earshot),
            m.stop_earshot.label(end)
        ),
        format!("{}; stop {}", prf_label(&m.rms), m.stop_rms.label(end)),
        "F1 >= 0.95; stop +1.9 to +2.6 s",
        Some(m.earshot.f1 >= 0.95 && m.stop_earshot.within(end, STOP_WINDOW)),
    );

    // Quiet speakers.
    let quiet = |db: f64| -> (Vec<f64>, Vec<f64>) {
        let a = db_to_rms(db);
        (
            clips.clean.iter().map(|x| x * a).collect(),
            clips.auto.iter().map(|x| x * a).collect(),
        )
    };
    let (c60, a60) = quiet(-20.0);
    let m = measure(clips, &c60, &a60, None);
    r.row(
        "quiet, speech -20 dB (-60 dBFS)",
        format!(
            "{}; stop {}; gate {}",
            prf_label(&m.earshot),
            m.stop_earshot.label(end),
            gate_label(m.gate_earshot)
        ),
        format!(
            "{}; stop {}; gate {}",
            prf_label(&m.rms),
            m.stop_rms.label(end),
            gate_label(m.gate_rms)
        ),
        "recall >= 0.80; stop +1.9 to +2.6 s; gate passes",
        Some(
            m.earshot.recall >= 0.80
                && m.stop_earshot.within(end, STOP_WINDOW)
                && m.gate_earshot.is_none(),
        ),
    );
    let (c70, a70) = quiet(-30.0);
    let m = measure(clips, &c70, &a70, None);
    r.row(
        "quiet, speech -30 dB (-70 dBFS)",
        format!(
            "gate {}; stop {}",
            gate_label(m.gate_earshot),
            m.stop_earshot.label(end)
        ),
        format!(
            "gate {}; stop {}",
            gate_label(m.gate_rms),
            m.stop_rms.label(end)
        ),
        "gate too quiet",
        Some(m.gate_earshot == Some(GateReason::TooQuiet)),
    );

    // Noise beds.
    let ten_s = 10 * SR;
    for (name, bed) in &clips.beds {
        let bed_rms = rms_f(bed);
        let snrs: &[f64] = if *name == "babble" {
            &[20.0, 10.0, 5.0, 0.0]
        } else {
            &[10.0, 5.0, 0.0]
        };
        for &snr in snrs {
            let noise_rms = clips.speech_rms / db_to_rms(snr);
            let gain = noise_rms / bed_rms;
            let clean_mix = mix(&clips.clean, bed, 0, gain, 0);
            let auto_mix = mix(&clips.auto, bed, clips.clean.len(), gain, 0);
            let noise_only: Vec<f64> = bed[bed.len() - ten_s..].iter().map(|x| x * gain).collect();
            let m = measure(clips, &clean_mix, &auto_mix, Some(&noise_only));
            cpu_audio.extend(to_i16(&clean_mix));
            let (fpr_e, fpr_r) = m.noise_fpr.unwrap_or_default();
            let window = if *name == "babble" {
                BABBLE_STOP_WINDOW
            } else {
                STOP_WINDOW
            };
            let (stops_in, stops) = stops_over_draws(clips, bed, gain, window);
            let name = format!(
                "{name} {snr:+.0} dB SNR ({:.0} dBFS)",
                20.0 * noise_rms.log10()
            );
            let earshot = format!(
                "{}; noise-only flagged {}; {stops}",
                prf_label(&m.earshot),
                pct(fpr_e),
            );
            let rms = format!(
                "{}; noise-only flagged {}; stop {}",
                prf_label(&m.rms),
                pct(fpr_r),
                m.stop_rms.label(end)
            );
            if name.starts_with("babble") {
                if snr >= 20.0 {
                    r.row(
                        &name,
                        earshot,
                        rms,
                        "stop +1.9 to +3.0 s in every draw",
                        Some(stops_in),
                    );
                } else {
                    // A known limit without a speaker model.
                    r.row(&name, earshot, rms, "-", None);
                }
            } else {
                r.row(
                    &name,
                    earshot,
                    rms,
                    "noise-only flagged <= 2%; stop +1.9 to +2.6 s in every draw",
                    Some(fpr_e <= 0.02 && stops_in),
                );
            }
        }
    }

    // Pink noise switching on 1 s into the dead air before the dictation.
    {
        let (_, pink) = &clips.beds[0];
        let gain = clips.speech_rms / rms_f(pink);
        let onset = mix(&clips.auto, pink, 0, gain, SR);
        let auto = to_i16(&onset);
        let e = auto_stop(&auto, close(Vad::Earshot));
        let rms = auto_stop(&auto, close(Vad::Rms));
        let armed = |s: &Stop| match s.armed_s {
            Some(t) => format!("armed {t:.2} s"),
            None => "not armed".into(),
        };
        let early =
            e.fired_s.is_some_and(|t| t < end) || e.armed_s.is_some_and(|t| t < clips.auto_start);
        r.row(
            "pink 0 dB SNR switching on at 1 s (speech from 2 s)",
            format!("{}; stop {}", armed(&e), e.label(end)),
            format!("{}; stop {}", armed(&rms), rms.label(end)),
            "not armed before the speech, never fires early",
            Some(!early),
        );
    }

    // A lone "yes".
    {
        let yes = to_i16(&clips.yes);
        let e = auto_stop(&yes, close(Vad::Earshot));
        let rms = auto_stop(&yes, close(Vad::Rms));
        let gate_e = speech_gate(&yes, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Earshot)).reason;
        let gate_r = speech_gate(&yes, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Rms)).reason;
        r.row(
            "lone \"yes\"",
            format!(
                "gate {}; stop {}",
                gate_label(gate_e),
                e.label(clips.yes_end)
            ),
            format!(
                "gate {}; stop {}",
                gate_label(gate_r),
                rms.label(clips.yes_end)
            ),
            "gate passes; arms and fires",
            Some(
                gate_e.is_none()
                    && e.armed_s.is_some()
                    && e.after(clips.yes_end).is_some_and(|d| d >= 0.0),
            ),
        );
    }

    // Pink noise only, 10 s at -40 dBFS.
    {
        let (_, pink) = &clips.beds[0];
        let gain = db_to_rms(-40.0) / rms_f(pink);
        let noise = to_i16(&pink[..ten_s].iter().map(|x| x * gain).collect::<Vec<_>>());
        let e = auto_stop(&noise, close(Vad::Earshot));
        let rms = auto_stop(&noise, close(Vad::Rms));
        let gate_e = speech_gate(&noise, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Earshot)).reason;
        let gate_r = speech_gate(&noise, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Rms)).reason;
        let armed = |s: &Stop| {
            if s.armed_s.is_some() {
                "armed"
            } else {
                "not armed"
            }
        };
        r.row(
            "pink only, -40 dBFS, 10 s",
            format!("gate {}; {}", gate_label(gate_e), armed(&e)),
            format!("gate {}; {}", gate_label(gate_r), armed(&rms)),
            "gate without speech; never arms",
            Some(gate_e == Some(GateReason::NoSpeech) && e.armed_s.is_none()),
        );
    }

    // CPU: the whole detector, as whisrs feeds it.
    {
        let dur = cpu_audio.len() as f64 / SR as f64;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let mut detector = SpeechDetector::new(close(Vad::Earshot), VAD_SAMPLE_RATE);
            let mut speech = 0usize;
            let start = Instant::now();
            for chunk in cpu_audio.chunks(CHUNK) {
                detector.feed(chunk, |d| speech += d.speech as usize);
            }
            std::hint::black_box(speech);
            best = best.min(start.elapsed().as_secs_f64());
        }
        let realtime = dur / best;
        let frames = cpu_audio.len() / FRAME;
        r.row(
            "CPU",
            format!(
                "{:.0}x real time ({:.1} us per 16 ms frame, {dur:.0} s of audio)",
                realtime,
                best * 1e6 / frames as f64
            ),
            "-".into(),
            "faster than 100x real time",
            Some(realtime > 100.0),
        );
    }

    if !real.is_empty() {
        let _ = writeln!(
            r.md,
            "\n| real track (first 5 min) | dur s | speech % RMS / close / open | speech dBFS | noise dBFS | SNR dB | clipped | DC | gate (close) |"
        );
        let _ = writeln!(r.md, "|---|---|---|---|---|---|---|---|---|");
        for (i, path) in real.iter().enumerate() {
            let samples = match read_wav(path, Some(300 * SR)) {
                Ok(s) => s,
                Err(e) => {
                    let _ = writeln!(r.md, "| real{} | {e} | | | | | | | |", i + 1);
                    continue;
                }
            };
            let share = |cfg: VadConfig| {
                let mut detector = SpeechDetector::new(cfg, VAD_SAMPLE_RATE);
                let mut flags = Vec::new();
                for chunk in samples.chunks(CHUNK) {
                    detector.feed(chunk, |d| flags.push(d.speech));
                }
                (flagged_share(&flags), detector.levels())
            };
            let rms_share = flagged_share(&rms_flags(&samples));
            let (close_share, _) = share(close(Vad::Earshot));
            let (open_share, levels) = share(VadConfig::open(Vad::Earshot));
            let gate = speech_gate(&samples, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Earshot));
            let db = |x: Option<f32>| x.map_or("-".to_string(), |v| format!("{v:.1}"));
            let _ = writeln!(
                r.md,
                "| real{} | {:.0} | {:.0} / {:.0} / {:.0} | {} | {} | {} | {} | {} | {} |",
                i + 1,
                samples.len() as f64 / SR as f64,
                rms_share * 100.0,
                close_share * 100.0,
                open_share * 100.0,
                db(levels.speech_dbfs),
                db(levels.noise_dbfs),
                db(levels.snr_db),
                pct(levels.clipped_share as f64),
                pct(levels.dc_offset as f64),
                gate_label(gate.reason),
            );
        }
    }
    report
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first().cloned() else {
        eprintln!("usage: vad_check <fixture_dir> [--speech-dbfs -40] [--real a.wav ...]");
        return ExitCode::from(2);
    };
    let mut speech_dbfs = -40.0;
    let mut real = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--speech-dbfs" => {
                let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) else {
                    eprintln!("--speech-dbfs needs a number");
                    return ExitCode::from(2);
                };
                speech_dbfs = v;
                i += 2;
            }
            "--real" => {
                real = args[i + 1..].to_vec();
                break;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let clips = match load(&dir, speech_dbfs) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let report = run(&clips, &real);
    print!("{}", report.md);
    if report.failed.is_empty() {
        println!("\nall gated rows pass");
        ExitCode::SUCCESS
    } else {
        println!("\nFAILED: {}", report.failed.join("; "));
        ExitCode::from(1)
    }
}
