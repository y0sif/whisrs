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
/// How much louder than the sentences the burst clip's "Oh!" plays, one row
/// each: a laugh or an interjection close to the mic.
const BURST_DB: [f64; 2] = [20.0, 26.0];

fn close(vad: Vad) -> VadConfig {
    VadConfig::close(vad)
}

fn open(vad: Vad) -> VadConfig {
    VadConfig::open(vad)
}

/// The maintainer's soft-voice take (16 kHz mono, his mic, room at -54
/// dBFS): about 15 s of soft talk, a pause, a sentence at normal volume
/// (about -32 dBFS), about 20 s of soft talk. Local only (`fixtures/` is
/// gitignored, and a real voice is never committed): the rows are skipped
/// without it, and for any other file of that name.
const REAL_SOFT: &str = "real-soft.wav";
const REAL_SOFT_SAMPLES: usize = 972_000;

/// Its soft speech, hand labelled in 100 ms windows (`[start, end)` window
/// indices). A whisper transcript of the whole take (medium.en) hears soft
/// talk without a break from 1.4 to 17.0 s and from 39.0 to 59.0 s; the
/// recording's own Deepgram segments agree where they exist ("I'm speaking
/// very soft." at 1.77 to 3.47 s, "Now I'm back to the soft speaking." at
/// 39.12 to 41.37 s). Inside those two passages a window is speech when its
/// 500 to 6000 Hz energy (Hann-windowed FFT) is at least 6 dB over the
/// median of the 18 to 23 s silence: the band where the voice stands clear
/// of the mic's rumble. 126 windows, 12.6 s.
#[rustfmt::skip]
const REAL_SOFT_LABELS: &[(usize, usize)] = &[
    (15, 20), (25, 26), (27, 31), (44, 48), (52, 54), (58, 61), (63, 64), (74, 77),
    (86, 87), (88, 89), (90, 92), (93, 98), (99, 101), (103, 105), (106, 107), (108, 110),
    (116, 121), (122, 123), (124, 129), (139, 140), (141, 142), (150, 153), (154, 155),
    (156, 158), (159, 161), (162, 164), (167, 170), (390, 396), (397, 402), (416, 418),
    (424, 425), (426, 428), (430, 432), (433, 435), (436, 437), (440, 442), (446, 447),
    (460, 462), (475, 476), (491, 493), (496, 497), (505, 507), (508, 509), (513, 516),
    (525, 526), (527, 529), (530, 531), (532, 533), (534, 535), (537, 540), (545, 548),
    (560, 562), (564, 565), (566, 569), (570, 572), (573, 575), (587, 590),
];

/// The take's silences (100 ms windows): no speech at all.
const REAL_SOFT_SILENCES: &[(usize, usize)] = &[(172, 232), (353, 389)];

/// Dictations cut from the take: its pieces (100 ms windows, played back to
/// back) and where its last soft word ends, in seconds into the cut. The
/// second passage ends with the take, so 3.5 s of the first silence follow
/// it. Gated.
type Cut = (&'static str, &'static [(usize, usize)], f64);
const REAL_SOFT_DICTATIONS: [Cut; 3] = [
    ("first passage from 9.0 s", &[(90, 233)], 8.0),
    ("second passage", &[(390, 607), (175, 210)], 20.0),
    (
        "normal sentence, then the second passage",
        &[(233, 274), (390, 607), (175, 210)],
        24.1,
    ),
];

/// The first passage from its start: printed, not gated. From 6.1 to 9.0 s
/// the only sound is one breathy 0.24 s syllable (7.38 s) that earshot
/// scores 0.20 to 0.27, its own score for his room, so auto-stop fires in
/// it at about 8.5 s (0.1.31's RMS test fired there too, at 7.85 s).
const REAL_SOFT_WHOLE_PASSAGE: Cut = ("first passage from 0 s", &[(0, 233)], 17.0);

/// The labels are 100 ms windows, so the stop window opens 0.1 s before
/// [`STOP_WINDOW`]'s.
const REAL_STOP_WINDOW: (f64, f64) = (1.8, 2.6);

/// Recall of the soft speech the open profile's mask must reach, and the
/// share of it inside the regions a recording sends.
const REAL_SOFT_RECALL: f64 = 0.70;
const REAL_SOFT_SENT: f64 = 0.90;

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

/// When a voice was first heard ([`AutoStopDetector::heard_voice`]) and
/// when auto-stop armed, in seconds from the clip start.
fn heard_and_armed(samples: &[i16], cfg: VadConfig) -> (Option<f64>, Option<f64>) {
    let mut stop = AutoStopDetector::with_vad(cfg, TIMEOUT_MS, VAD_SAMPLE_RATE);
    let (mut heard, mut armed) = (None, None);
    for (c, chunk) in samples.chunks(CHUNK).enumerate() {
        stop.feed(chunk);
        let t = ((c * CHUNK + chunk.len()) as f64) / SR as f64;
        if heard.is_none() && stop.heard_voice() {
            heard = Some(t);
        }
        if armed.is_none() && stop.has_speech() {
            armed = Some(t);
        }
    }
    (heard, armed)
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
    /// The burst clip's "Oh!" alone, and its sentences alone, each at the
    /// speech level: the rows mix them with the "Oh!" [`BURST_DB`] louder.
    burst_word: Vec<f64>,
    burst_rest: Vec<f64>,
    /// The sentences' labels (the "Oh!" is not among them).
    burst_labels: Vec<bool>,
    /// Labelled length of the "Oh!", in seconds.
    burst_s: f64,
    beds: Vec<(&'static str, Vec<f64>)>,
    speech_rms: f64,
}

/// `recording::speech::regions` on a 100 ms mask: each run of active
/// windows padded 300 ms, regions less than 1 s apart merged, regions with
/// under 300 ms of active windows dropped. `[start, end)` window ranges.
fn regions(active: &[bool]) -> Vec<(usize, usize)> {
    let mut merged: Vec<((usize, usize), usize)> = Vec::new();
    let mut i = 0;
    while i < active.len() {
        if !active[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < active.len() && active[i] {
            i += 1;
        }
        let padded = (start.saturating_sub(3), (i + 3).min(active.len()));
        match merged.last_mut() {
            Some((last, count)) if padded.0 < last.1 + 10 => {
                last.1 = padded.1;
                *count += i - start;
            }
            _ => merged.push((padded, i - start)),
        }
    }
    merged
        .into_iter()
        .filter(|(_, count)| count * 100 >= 300)
        .map(|(r, _)| r)
        .collect()
}

/// What a recording makes of the take's mic track under one rule: the
/// share of the labelled windows the mask flags, the share inside the sent
/// regions, the sent seconds and regions, and the flagged silence windows.
struct Sent {
    recall: f64,
    sent: f64,
    seconds: f64,
    regions: usize,
    false_windows: usize,
}

fn sent(mask: &[bool], labels: &[bool]) -> Sent {
    let total = labels.iter().filter(|&&l| l).count().max(1) as f64;
    let hit = labels.iter().zip(mask).filter(|(&l, &m)| l && m).count();
    let regions = regions(mask);
    let mut inside = vec![false; mask.len()];
    for &(a, b) in &regions {
        inside[a..b].fill(true);
    }
    let covered = labels.iter().zip(&inside).filter(|(&l, &m)| l && m).count();
    Sent {
        recall: hit as f64 / total,
        sent: covered as f64 / total,
        seconds: regions.iter().map(|r| r.1 - r.0).sum::<usize>() as f64 / 10.0,
        regions: regions.len(),
        false_windows: REAL_SOFT_SILENCES
            .iter()
            .map(|&(a, b)| mask[a..b].iter().filter(|&&m| m).count())
            .sum(),
    }
}

impl Sent {
    fn label(&self) -> String {
        format!(
            "soft recall {:.3}, sent {:.3} ({:.1} s in {} regions), silence windows flagged {}",
            self.recall, self.sent, self.seconds, self.regions, self.false_windows
        )
    }
}

/// The soft-voice rows, when `fixtures/vad/real-soft.wav` is there.
fn real_soft_rows(r: &mut Report, dir: &str) {
    let name = "real soft voice (local fixture)";
    let path = format!("{dir}/{REAL_SOFT}");
    if !std::path::Path::new(&path).exists() {
        let skipped = "skipped (local fixture missing)".to_string();
        r.row(name, skipped, "-".into(), "-", None);
        return;
    }
    let samples = match read_wav(&path, None) {
        Ok(s) if s.len() == REAL_SOFT_SAMPLES => s,
        Ok(s) => {
            let skipped = format!("skipped (not the labelled take: {} samples)", s.len());
            r.row(name, skipped, "-".into(), "-", None);
            return;
        }
        Err(e) => {
            r.row(name, format!("skipped ({e})"), "-".into(), "-", None);
            return;
        }
    };

    // The recording: the mic track's mask (open profile) and its regions.
    let mut labels = vec![false; samples.len().div_ceil(RMS_FRAME)];
    for &(a, b) in REAL_SOFT_LABELS {
        labels[a..b].fill(true);
    }
    let mask = |vad| speech_mask(&samples, &open(vad), RMS_FRAME);
    let e = sent(&mask(Vad::Earshot), &labels);
    let rms = sent(&mask(Vad::Rms), &labels);
    r.row(
        "real soft voice, recording (open)",
        e.label(),
        rms.label(),
        "soft recall >= 0.70; sent >= 0.90",
        Some(e.recall >= REAL_SOFT_RECALL && e.sent >= REAL_SOFT_SENT),
    );

    // Dictations: auto-stop (close profile) on cuts of the take.
    let dictate = |(label, pieces, last_word): Cut| {
        let clip: Vec<i16> = pieces
            .iter()
            .flat_map(|&(a, b)| samples[a * RMS_FRAME..b * RMS_FRAME].iter().copied())
            .collect();
        let stop = |vad| auto_stop(&clip, close(vad));
        let (e, rms) = (stop(Vad::Earshot), stop(Vad::Rms));
        let show = |s: &Stop| {
            let armed = s
                .armed_s
                .map_or("not armed".into(), |t| format!("armed {t:.2} s"));
            format!("{label}: {armed}, stop {}", s.label(last_word))
        };
        let pass = e.armed_s.is_some() && e.within(last_word, REAL_STOP_WINDOW);
        (show(&e), show(&rms), pass)
    };
    let cuts: Vec<_> = REAL_SOFT_DICTATIONS.into_iter().map(dictate).collect();
    let join = |pick: fn(&(String, String, bool)) -> &String| {
        cuts.iter()
            .map(pick)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")
    };
    r.row(
        "real soft voice, dictations (close)",
        join(|c| &c.0),
        join(|c| &c.1),
        "each arms; stop +1.8 to +2.6 s after its last soft word",
        Some(cuts.iter().all(|c| c.2)),
    );
    let (e, rms, _) = dictate(REAL_SOFT_WHOLE_PASSAGE);
    r.row("real soft voice, a known limit", e, rms, "-", None);
}

/// Key clicks over a quiet room: `seconds` of pink noise at -60 dBFS with a
/// key press and its release (80 ms later, half as loud) every 125 to 315
/// ms, each a burst of seeded noise peaking at `peak_dbfs` and decaying in
/// 3 ms.
fn key_clicks(pink: &[f64], seconds: usize, peak_dbfs: f64) -> Vec<i16> {
    let n = seconds * SR;
    let gain = db_to_rms(-60.0) / rms_f(pink);
    let mut out: Vec<f64> = pink[..n].iter().map(|x| x * gain).collect();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut uniform = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let peak = db_to_rms(peak_dbfs);
    let mut t = SR / 2;
    while t + 2 * SR / 10 < n {
        for (start, a) in [(t, peak), (t + 1300, peak / 2.0)] {
            for k in 0..240 {
                out[start + k] += a * (2.0 * uniform() - 1.0) * (-(k as f64) / 50.0).exp();
            }
        }
        t += 2000 + (uniform() * 3000.0) as usize;
    }
    to_i16(&out)
}

/// A noise of the [`noise_bursts`] rows: a room or a burst.
#[derive(Clone, Copy, PartialEq)]
enum Noise {
    Pink,
    Brown,
    White,
}

impl Noise {
    fn name(self) -> &'static str {
        match self {
            Noise::Pink => "pink",
            Noise::Brown => "brown",
            Noise::White => "white",
        }
    }

    /// `n` samples of it from `offset` on (any RMS).
    fn samples(self, clips: &Clips, offset: usize, n: usize) -> Vec<f64> {
        match self {
            Noise::Pink => clips.beds[0].1[offset..offset + n].to_vec(),
            Noise::Brown => clips.beds[1].1[offset..offset + n].to_vec(),
            Noise::White => {
                let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ offset as u64;
                (0..n)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        2.0 * ((state >> 11) as f64 / (1u64 << 53) as f64) - 1.0
                    })
                    .collect()
            }
        }
    }
}

/// One [`noise_bursts`] case: the room (its noise and level), the burst
/// noise, how far the burst alone stands over the room (full band), and
/// the burst length and the pause after it.
#[derive(Clone, Copy)]
struct Bursts {
    room: (Noise, f64),
    noise: Noise,
    over_db: f64,
    len_ms: usize,
    gap_ms: usize,
}

/// Bursts in each [`noise_bursts`] case.
const BURSTS: usize = 4;

/// Noise that comes and goes before anyone speaks (a fan cycling, dishes,
/// a tap): [`BURSTS`] bursts over the room, the first 1 s in, then 3 s of
/// the room. Edges are abrupt, the hardest case for earshot's onset score.
/// A burst of the room's own noise comes from later in the bed.
fn noise_bursts(clips: &Clips, case: Bursts) -> Vec<i16> {
    let (len, gap) = (case.len_ms * SR / 1000, case.gap_ms * SR / 1000);
    let n = SR + BURSTS * len + (BURSTS - 1) * gap + 3 * SR;
    let (room_noise, room_dbfs) = case.room;
    let room = room_noise.samples(clips, 0, n);
    let room_gain = db_to_rms(room_dbfs) / rms_f(&room);
    let mut out: Vec<f64> = room.iter().map(|x| x * room_gain).collect();
    let source = case.noise.samples(clips, n, n);
    let gain = db_to_rms(room_dbfs + case.over_db) / rms_f(&source);
    for b in 0..BURSTS {
        let start = SR + b * (len + gap);
        for i in start..start + len {
            out[i] += gain * source[i];
        }
    }
    to_i16(&out)
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
    let (burst_word, burst_rest, burst_labels, burst_s) = split_burst(&read("burst")?, speech_rms)
        .ok_or_else(|| format!("{dir}/burst.wav: no speech after the burst"))?;
    // A burst longer than a second merged with the speech after it, so the
    // rows would test something else.
    if burst_s > 1.0 {
        return Err(format!(
            "{dir}/burst.wav: the burst lasts {burst_s:.2} s, not isolated"
        ));
    }
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
        burst_word,
        burst_rest,
        burst_labels,
        burst_s,
        beds,
        speech_rms,
    })
}

/// A split burst clip: the "Oh!" alone, the sentences alone, the
/// sentences' labels, and the "Oh!"'s labelled length in seconds.
type Burst = (Vec<f64>, Vec<f64>, Vec<bool>, f64);

/// The burst clip split into its first labelled run (the "Oh!") and the
/// rest (the sentences), each alone (zeros elsewhere) at `speech_rms` as an
/// active level, cut halfway through the dead air between them.
fn split_burst(raw: &[i16], speech_rms: f64) -> Option<Burst> {
    let labels = reference_labels(raw);
    let start = labels.iter().position(|&l| l)?;
    let end = start + labels[start..].iter().position(|&l| !l)?;
    let next = end + labels[end..].iter().position(|&l| l)?;
    let split = (end + next) / 2 * FRAME;
    let mut rest_labels = labels.clone();
    rest_labels[..end].fill(false);
    let mut word_labels = vec![false; labels.len()];
    word_labels[start..end].fill(true);
    let f = to_f(raw);
    let word_gain = speech_rms / active_rms(&f, &word_labels);
    let rest_gain = speech_rms / active_rms(&f, &rest_labels);
    let part = |range: std::ops::Range<usize>, gain: f64| -> Vec<f64> {
        f.iter()
            .enumerate()
            .map(|(i, x)| if range.contains(&i) { x * gain } else { 0.0 })
            .collect()
    };
    Some((
        part(0..split, word_gain),
        part(split..f.len(), rest_gain),
        rest_labels,
        ((end - start) * FRAME) as f64 / SR as f64,
    ))
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

fn run(clips: &Clips, dir: &str, real: &[String]) -> Report {
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

    // Quieter still, the floor rejects nearly all of it: auto-stop arms
    // late or never, so "heard" (what keeps whisrs's 10 s no-speech
    // timeout off) must come from the voice itself, on its first words.
    {
        let mut labels = Vec::new();
        let mut rms_labels = Vec::new();
        let mut pass = true;
        for db in [-36.0, -38.0] {
            let (_, quiet_auto) = quiet(db);
            let auto = to_i16(&quiet_auto);
            let (heard, armed) = heard_and_armed(&auto, close(Vad::Earshot));
            let (rms_heard, rms_armed) = heard_and_armed(&auto, close(Vad::Rms));
            let gate =
                speech_gate(&auto, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(Vad::Earshot)).reason;
            let at = |t: Option<f64>| t.map_or("never".to_string(), |t| format!("{t:.2} s"));
            labels.push(format!(
                "{:.0} dBFS: heard {}, armed {}, gate {}",
                -40.0 + db,
                at(heard),
                at(armed),
                gate_label(gate)
            ));
            rms_labels.push(format!(
                "{:.0} dBFS: heard {}, armed {}",
                -40.0 + db,
                at(rms_heard),
                at(rms_armed)
            ));
            pass &= heard.is_some_and(|t| t <= clips.auto_start + 1.0)
                && gate == Some(GateReason::TooQuiet);
        }
        r.row(
            "too quiet to arm (speech -76 and -78 dBFS, from 2.05 s)",
            labels.join("; "),
            rms_labels.join("; "),
            "heard within 1 s of the first word; gate too quiet",
            Some(pass),
        );
    }

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

    // Pink noise switching from -60 to -40 dBFS 1 s into the dead air: the
    // background is known when the noise rises.
    {
        let (_, pink) = &clips.beds[0];
        let low = db_to_rms(-60.0) / rms_f(pink);
        let high = clips.speech_rms / rms_f(pink);
        let stepped: Vec<f64> = clips
            .auto
            .iter()
            .enumerate()
            .map(|(i, x)| x + pink[i] * if i < SR { low } else { high })
            .collect();
        let auto = to_i16(&stepped);
        let e = auto_stop(&auto, close(Vad::Earshot));
        let rms = auto_stop(&auto, close(Vad::Rms));
        let armed = |s: &Stop| match s.armed_s {
            Some(t) => format!("armed {t:.2} s"),
            None => "not armed".into(),
        };
        let early =
            e.fired_s.is_some_and(|t| t < end) || e.armed_s.is_some_and(|t| t < clips.auto_start);
        r.row(
            "pink -60 dBFS rising to 0 dB SNR at 1 s (speech from 2 s)",
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

    // A loud interjection, then the sentences: the level it raised must not
    // keep the sentences under the relative floor.
    for db in BURST_DB {
        let gain = db_to_rms(db);
        let mixed: Vec<f64> = clips
            .burst_word
            .iter()
            .zip(&clips.burst_rest)
            .map(|(word, rest)| word * gain + rest)
            .collect();
        let burst = to_i16(&mixed);
        let (_, sentences_end) = speech_span(&clips.burst_labels);
        let e = auto_stop(&burst, close(Vad::Earshot));
        let rms = auto_stop(&burst, close(Vad::Rms));
        let recall_e = prf(&earshot_flags(&burst), &clips.burst_labels).recall;
        let recall_r = prf(&rms_flags(&burst), &clips.burst_labels).recall;
        let early = e.fired_s.is_some_and(|t| t < sentences_end);
        r.row(
            &format!(
                "\"Oh!\" ({:.1} s) {db:+.0} dB, then two sentences",
                clips.burst_s
            ),
            format!(
                "sentence recall {recall_e:.3}; stop {}",
                e.label(sentences_end)
            ),
            format!(
                "sentence recall {recall_r:.3}; stop {}",
                rms.label(sentences_end)
            ),
            "stop never before the sentences end; sentence recall >= 0.80",
            Some(!early && recall_e >= 0.80),
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
        // Printed: heard noise keeps whisrs's no-speech timeout off.
        let heard = |cfg| match heard_and_armed(&noise, cfg).0 {
            Some(_) => "heard",
            None => "not heard",
        };
        r.row(
            "pink only, -40 dBFS, 10 s",
            format!(
                "gate {}; {}; {}",
                gate_label(gate_e),
                armed(&e),
                heard(close(Vad::Earshot))
            ),
            format!(
                "gate {}; {}; {}",
                gate_label(gate_r),
                armed(&rms),
                heard(close(Vad::Rms))
            ),
            "gate without speech; never arms",
            Some(gate_e == Some(GateReason::NoSpeech) && e.armed_s.is_none()),
        );
    }

    // Typing near the mic: never speech.
    {
        let (_, pink) = &clips.beds[0];
        let mut labels = Vec::new();
        let mut pass = true;
        let mut rms_labels = Vec::new();
        for peak in [-20.0, -30.0] {
            let clicks = key_clicks(pink, 12, peak);
            let e = auto_stop(&clicks, close(Vad::Earshot));
            let rms = auto_stop(&clicks, close(Vad::Rms));
            let gate = |vad| speech_gate(&clicks, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(vad)).reason;
            let (gate_e, gate_r) = (gate(Vad::Earshot), gate(Vad::Rms));
            let armed = |s: &Stop| {
                if s.armed_s.is_some() {
                    "armed"
                } else {
                    "not armed"
                }
            };
            let heard = match heard_and_armed(&clicks, close(Vad::Earshot)).0 {
                Some(t) => format!("heard {t:.2} s"),
                None => "not heard".into(),
            };
            labels.push(format!(
                "{peak:.0} dBFS: gate {}, {}, {heard}",
                gate_label(gate_e),
                armed(&e)
            ));
            rms_labels.push(format!(
                "{peak:.0} dBFS: gate {}, {}",
                gate_label(gate_r),
                armed(&rms)
            ));
            pass &= gate_e == Some(GateReason::NoSpeech) && e.armed_s.is_none();
        }
        r.row(
            "key clicks over pink -60 dBFS, 12 s",
            labels.join("; "),
            rms_labels.join("; "),
            "gate without speech; never arms",
            Some(pass),
        );
    }

    // Noise that comes and goes before the first word: soft evidence must
    // not add up across separate bursts. A rumble-heavy room (brown, like
    // the maintainer's) is where a broadband burst stands clear.
    {
        let rooms = [(Noise::Pink, -60.0), (Noise::Brown, -54.0)];
        // Burst and pause: 0.7 to 1.5 s between bursts, or between onsets.
        let shapes = [
            (300, 700),
            (400, 1000),
            (500, 1500),
            (300, 400),
            (400, 600),
            (500, 1000),
        ];
        // White bursts 12 dB over a rumble room: earshot itself scores them
        // 0.55 to 0.58, so they are speech to it (printed below).
        let limit =
            |c: &Bursts| c.room.0 == Noise::Brown && c.noise == Noise::White && c.over_db > 9.0;
        let mut cases = Vec::new();
        for room in rooms {
            for noise in [Noise::Pink, Noise::Brown, Noise::White] {
                for over_db in [6.0, 9.0, 12.0] {
                    for (len_ms, gap_ms) in shapes {
                        cases.push(Bursts {
                            room,
                            noise,
                            over_db,
                            len_ms,
                            gap_ms,
                        });
                    }
                }
            }
        }
        let (gated, limits): (Vec<Bursts>, Vec<Bursts>) =
            cases.into_iter().partition(|c| !limit(c));
        for (name, cases, pass) in [
            (
                format!(
                    "{BURSTS} noise bursts, no speech (pink, brown, white at +6/+9/+12 dB over \
                     pink -60 dBFS and brown -54 dBFS; 300 to 500 ms; 0.4 to 1.5 s pauses)"
                ),
                gated,
                true,
            ),
            (
                format!(
                    "{BURSTS} white bursts +12 dB over brown -54 dBFS (earshot scores them as speech)"
                ),
                limits,
                false,
            ),
        ] {
            let (mut armed, mut passed, mut rms_armed, mut rms_passed) = (0, 0, 0, 0);
            let mut heard = 0;
            let mut failures = Vec::new();
            for case in &cases {
                let clip = noise_bursts(clips, *case);
                let e = auto_stop(&clip, close(Vad::Earshot));
                let rms = auto_stop(&clip, close(Vad::Rms));
                let gate = |vad| speech_gate(&clip, VAD_SAMPLE_RATE, GATE_MIN_MS, &close(vad)).reason;
                let (gate_e, gate_r) = (gate(Vad::Earshot), gate(Vad::Rms));
                armed += e.armed_s.is_some() as usize;
                passed += (gate_e != Some(GateReason::NoSpeech)) as usize;
                heard += heard_and_armed(&clip, close(Vad::Earshot)).0.is_some() as usize;
                rms_armed += rms.armed_s.is_some() as usize;
                rms_passed += gate_r.is_none() as usize;
                if pass && (e.armed_s.is_some() || gate_e != Some(GateReason::NoSpeech)) {
                    let at = e.armed_s.map_or("-".to_string(), |t| format!("{t:.2} s"));
                    failures.push(format!(
                        "{} {:+.0} dB over {} {:.0} dBFS, {}/{} ms: armed {at}, gate {}",
                        case.noise.name(),
                        case.over_db,
                        case.room.0.name(),
                        case.room.1,
                        case.len_ms,
                        case.gap_ms,
                        gate_label(gate_e)
                    ));
                }
            }
            let n = cases.len();
            let mut earshot = format!(
                "armed in {armed} of {n}; gate not \"without speech\" in {passed} of {n}; \
                 heard in {heard} of {n}"
            );
            if !failures.is_empty() {
                let _ = write!(earshot, " ({})", failures.join("; "));
            }
            let rms = format!("armed in {rms_armed} of {n}; gate passes in {rms_passed} of {n}");
            if pass {
                r.row(
                    &name,
                    earshot,
                    rms,
                    "never arms; gate without speech",
                    Some(armed == 0 && passed == 0),
                );
            } else {
                r.row(&name, earshot, rms, "-", None);
            }
        }
    }

    real_soft_rows(r, dir);

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
    let report = run(&clips, &dir, &real);
    print!("{}", report.md);
    if report.failed.is_empty() {
        println!("\nall gated rows pass");
        ExitCode::SUCCESS
    } else {
        println!("\nFAILED: {}", report.failed.join("; "));
        ExitCode::from(1)
    }
}
