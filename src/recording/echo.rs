//! Speaker echo removal for the recording mic track.
//!
//! Without headphones the far side of a call plays through the speakers and
//! the mic records it a second time. [`cancel_echo`] takes that echo out of
//! the mic track before it is transcribed, with the system track (what the
//! speakers played) as the reference. It is a residual echo suppressor: it
//! learns how loud the speaker-to-mic path is in each frequency bin, predicts
//! the echo in every frame from the system audio, and turns the mic down in
//! the bins where that prediction dominates. The user speaking while the far
//! side is quiet passes through unchanged; speech over the far side loses
//! the bins the echo covers.
//!
//! The tracks are read from disk in four passes, so memory stays at one
//! block of audio however long the recording is:
//!
//! 1. [`estimate_lags`]: how far the mic lags the system track, by GCC-PHAT
//!    over [`LAG_BLOCK`] blocks where the far side is active. Tracks from two
//!    sound cards drift apart, so the lag is followed block by block.
//! 2. [`far_threshold`]: the system frame energy above which a frame counts
//!    as far-end dominant.
//! 3. [`echo_path`]: per bin, the median mic-to-system power ratio over those
//!    frames, which is the echo path's power gain.
//! 4. [`suppress`]: a gain per bin and frame, inverse STFT and overlap-add,
//!    written out as the mic is read, so the output has the mic's length.
//!    The same pass marks where the user speaks (the near-end mask), so
//!    only those stretches need transcribing.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use tracing::debug;

use super::process::open_track;
use super::speech;

const RATE: usize = super::SAMPLE_RATE as usize;

/// STFT frame (32 ms) and hop (50% overlap).
const FRAME: usize = 512;
const HOP: usize = FRAME / 2;
const BINS: usize = FRAME / 2 + 1;

/// The room keeps ringing after the speakers: the far-end power of the last
/// `SMEAR_FRAMES` frames counts too, each frame of age scaled by
/// `SMEAR_DECAY` (about 200 ms of reverb tail).
const SMEAR_FRAMES: usize = 12;
const SMEAR_DECAY: f32 = 0.7;

/// Frames whose system energy is above this quantile learn the echo path.
const FAR_QUANTILE: f64 = 0.7;

/// How far the predicted echo is over-subtracted: a bin keeps any of its
/// power only where the mic is more than this many times the prediction.
const OVERSUBTRACTION: f32 = 3.0;

/// Lag estimation block. Short enough that 100 ppm of clock drift smears
/// the correlation peak over only 16 samples.
const LAG_BLOCK: usize = 10 * RATE;

/// Search window around the lag the track offsets imply: the mic may hear
/// the speakers up to `MAX_DELAY` after the system track has them (output
/// latency, Bluetooth speakers), or up to `MAX_LEAD` before (the offsets are
/// only as exact as the capture start times). Once lags are measured, the
/// window follows them instead, so drift never walks the echo out of it.
const MAX_LEAD: usize = RATE / 4;
const MAX_DELAY: usize = RATE / 2;

/// A correlation peak is an echo when it stands this many standard
/// deviations above the correlation at the other lags. Measured on a real
/// recording: blocks without far-end speech peak at 4 to 8, blocks with it at
/// 15 to 39.
const MIN_PEAK_Z: f64 = 12.0;

/// Median smoothing window over the measured lags, in blocks.
const LAG_MEDIAN: usize = 5;

/// Confident blocks needed before the search window follows the measured
/// lag: their median, so one stray peak cannot drag the window away.
const LAG_LOCK: usize = LAG_MEDIAN / 2 + 1;

/// Keeps a silent bin from dividing by zero; far below any audible power of
/// samples scaled to -1..1.
const POWER_FLOOR: f32 = 1e-12;

/// Same, for the GCC-PHAT whitening.
const PHAT_FLOOR: f32 = 1e-9;

/// The speech band (300 to 3400 Hz) in STFT bins, where the near-end
/// detector compares the mic with the predicted echo.
const SPEECH_BINS: std::ops::Range<usize> = 300 * FRAME / RATE..3400 * FRAME / RATE;

/// How far the mic must be above the predicted echo (the prediction itself,
/// without the over-subtraction) in the speech band for a frame to count as
/// the user speaking.
const NEAR_END_MARGIN_DB: f64 = 10.0;

/// How loud the cancelled output must be in the speech band for the same,
/// in dBFS (mean power per sample), so room noise does not count.
const NEAR_END_FLOOR_DB: f64 = -60.0;

/// A system frame quieter than this mean square is not far-end audio
/// whatever its rank, so a recording where the far side rarely talks does not
/// learn the echo path from its noise floor (and the user's voice over it).
/// It gates both the lag search and the echo-path estimate.
///
/// An energy floor (RMS 0.003 of full scale, about -50 dBFS), not the speech
/// detector: the question is whether the system track carries enough
/// energy for a lag and an echo-path estimate, and music and effects leak
/// into the mic as echo too, so a speech detector would skip them and the
/// canceller would learn less. A local value, so a change to the speech
/// gate's own threshold cannot move it.
const ACTIVE_POWER: f64 = 0.003 * 0.003;

/// What [`cancel_echo`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum EchoOutcome {
    /// The echo-cancelled mic was written.
    Cancelled(Cancelled),
    /// Nothing to cancel, and nothing was written.
    Skipped(SkipReason),
}

/// A finished cancellation.
#[derive(Clone, PartialEq)]
pub struct Cancelled {
    pub report: EchoReport,
    /// Per [`speech::FRAME`] of the mic (the last one possibly partial):
    /// whether the user is speaking in it rather than the speakers echoing.
    pub near_end: Vec<bool>,
}

impl fmt::Debug for Cancelled {
    // The mask has a flag per 100 ms; a count reads better in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active = self.near_end.iter().filter(|&&a| a).count();
        f.debug_struct("Cancelled")
            .field("report", &self.report)
            .field(
                "near_end",
                &format_args!("{active} of {} frames", self.near_end.len()),
            )
            .finish()
    }
}

/// Why [`cancel_echo`] left the mic alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The mic track has no samples.
    EmptyMic,
    /// The system track never rises above silence while the mic records.
    SilentSystem,
    /// The far side plays but the mic does not pick it up: headphones, or
    /// speakers too quiet to matter.
    NoEcho,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SkipReason::EmptyMic => "the mic track is empty",
            SkipReason::SilentSystem => "the system track is silent",
            SkipReason::NoEcho => "the mic does not pick up the system audio",
        })
    }
}

/// What a cancellation measured, for the log.
#[derive(Debug, Clone, PartialEq)]
pub struct EchoReport {
    /// Median lag of the mic behind the system track, in samples.
    pub lag: i64,
    /// How far the lag moved over the recording (clock drift), in samples.
    pub lag_drift: i64,
    /// Blocks whose lag was measured.
    pub aligned_blocks: usize,
    /// Median power gain of the echo path over the bins, in dB.
    pub echo_path_db: f32,
}

/// Write `mic` with the echo of `system` removed to `out` (created 0600,
/// replacing any file there): a 16 kHz mono 16-bit WAV with exactly the
/// mic's sample count, so offsets and timestamps carry over unchanged.
///
/// `timeline_lag` is where the track offsets put the system track: mic
/// sample `u` and system sample `u - timeline_lag` were captured at the
/// same moment. The real lag is searched from [`MAX_LEAD`] before that to
/// [`MAX_DELAY`] after. When there is nothing to cancel, `out` is not
/// created. Blocking; reads each track four times.
pub fn cancel_echo(
    mic: &Path,
    system: &Path,
    timeline_lag: i64,
    out: &Path,
) -> anyhow::Result<EchoOutcome> {
    let mic_len = open_track(mic)?.duration() as usize;
    if mic_len == 0 {
        return Ok(EchoOutcome::Skipped(SkipReason::EmptyMic));
    }
    let lags = match estimate_lags(mic, system, timeline_lag, mic_len)? {
        Ok(lags) => lags,
        Err(reason) => return Ok(EchoOutcome::Skipped(reason)),
    };
    let pair = Pair {
        mic,
        system,
        mic_len,
        lags,
    };
    let mut stft = Stft::new();
    let threshold = far_threshold(&pair, &mut stft)?;
    let Some(path) = echo_path(&pair, threshold, &mut stft)? else {
        return Ok(EchoOutcome::Skipped(SkipReason::SilentSystem));
    };
    let near_end = suppress(&pair, &path, &mut stft, out)?;

    let mut lag_values: Vec<f64> = pair.lags.points.iter().map(|p| p.1).collect();
    lag_values.sort_by(f64::total_cmp);
    let mut path_db: Vec<f32> = path.iter().map(|&p| db(p)).collect();
    path_db.sort_by(f32::total_cmp);
    Ok(EchoOutcome::Cancelled(Cancelled {
        report: EchoReport {
            lag: lag_values[lag_values.len() / 2].round() as i64,
            lag_drift: (lag_values[lag_values.len() - 1] - lag_values[0]).round() as i64,
            aligned_blocks: lag_values.len(),
            echo_path_db: path_db[BINS / 2],
        },
        near_end,
    }))
}

/// The two tracks and how they line up.
struct Pair<'a> {
    mic: &'a Path,
    system: &'a Path,
    mic_len: usize,
    lags: LagTrack,
}

impl Pair<'_> {
    fn frames(&self) -> anyhow::Result<Frames<'_>> {
        Ok(Frames {
            mic: SampleStream::open(self.mic)?,
            system: SampleStream::open(self.system)?,
            lags: &self.lags,
            next: 0,
            count: self.mic_len.div_ceil(HOP) + 1,
            system_start: i64::MIN,
        })
    }
}

/// The lag of the mic behind the system track through the recording: mic
/// sample `u` holds the echo of system sample `u - lag`.
#[derive(Debug, Clone, PartialEq)]
struct LagTrack {
    /// `(mic sample, lag)` at the centre of every block with a confident
    /// peak, median-smoothed, in order. Two numbers per block, so a
    /// three-hour recording holds about a thousand.
    points: Vec<(f64, f64)>,
}

impl LagTrack {
    /// The lag at mic sample `pos`: linear between measured blocks, so drift
    /// is followed through a stretch where the far side is silent, and held
    /// before the first block and after the last.
    fn at(&self, pos: f64) -> f64 {
        let i = self.points.partition_point(|&(p, _)| p <= pos);
        match (i.checked_sub(1).map(|j| self.points[j]), self.points.get(i)) {
            (Some((p0, l0)), Some(&(p1, l1))) => l0 + (l1 - l0) * (pos - p0) / (p1 - p0),
            (Some((_, lag)), None) | (None, Some(&(_, lag))) => lag,
            (None, None) => 0.0,
        }
    }
}

/// Pass 1: the lag in every block where the system track is active and the
/// mic clearly holds its echo. Low-confidence peaks are dropped rather than
/// trusted, and the median smoothing outvotes a stray confident one. Each
/// block is searched around the median of the last [`LAG_MEDIAN`] confident
/// lags once there are [`LAG_LOCK`] of them, else around `timeline_lag`.
fn estimate_lags(
    mic: &Path,
    system: &Path,
    timeline_lag: i64,
    mic_len: usize,
) -> anyhow::Result<Result<LagTrack, SkipReason>> {
    let mut mic_stream = SampleStream::open(mic)?;
    let mut system_stream = SampleStream::open(system)?;
    let mut gcc = Gcc::new();
    let mut mic_block = vec![0f32; LAG_BLOCK];
    let mut system_block = vec![0f32; LAG_BLOCK];
    let mut active = false;
    let mut found = Vec::new();
    let mut recent = VecDeque::with_capacity(LAG_MEDIAN);
    for block in 0..mic_len.div_ceil(LAG_BLOCK) {
        let start = block * LAG_BLOCK;
        let search = if recent.len() >= LAG_LOCK {
            let mut lags: Vec<i64> = recent.iter().copied().collect();
            lags.sort_unstable();
            lags[(lags.len() - 1) / 2]
        } else {
            timeline_lag
        };
        mic_stream.read(start as i64, &mut mic_block)?;
        // Moves by at most the window per block, far less than the block, so
        // the read never steps backwards.
        system_stream.read(start as i64 - search, &mut system_block)?;
        if mean_square(&system_block) < ACTIVE_POWER {
            continue;
        }
        active = true;
        let (offset, z) = gcc.peak(&mic_block, &system_block)?;
        let lag = search + offset;
        debug!("echo lag block {block}: {lag} samples, peak z {z:.1}");
        if z >= MIN_PEAK_Z {
            let centre = start + LAG_BLOCK.min(mic_len - start) / 2;
            found.push((centre as f64, lag as f64));
            if recent.len() == LAG_MEDIAN {
                recent.pop_front();
            }
            recent.push_back(lag);
        }
    }
    if !active {
        return Ok(Err(SkipReason::SilentSystem));
    }
    if found.is_empty() {
        return Ok(Err(SkipReason::NoEcho));
    }
    Ok(Ok(LagTrack {
        points: median_smoothed(&found),
    }))
}

/// Each lag replaced by the median of the [`LAG_MEDIAN`] around it.
fn median_smoothed(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let half = LAG_MEDIAN / 2;
    (0..points.len())
        .map(|i| {
            let window = &points[i.saturating_sub(half)..(i + half + 1).min(points.len())];
            let mut lags: Vec<f64> = window.iter().map(|p| p.1).collect();
            lags.sort_by(f64::total_cmp);
            (points[i].0, lags[(lags.len() - 1) / 2])
        })
        .collect()
}

/// GCC-PHAT: the cross-correlation of two blocks with every frequency
/// whitened to unit magnitude, so each band votes equally and the peak is
/// about one sample wide even for low-pitched speech.
struct Gcc {
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    time: Vec<f32>,
    mic: Vec<Complex<f32>>,
    system: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Gcc {
    fn new() -> Self {
        // Long enough that the correlation does not wrap within the window.
        let len = (LAG_BLOCK + MAX_LEAD.max(MAX_DELAY)).next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(len);
        let ifft = planner.plan_fft_inverse(len);
        let scratch = vec![Complex::default(); fft.get_scratch_len().max(ifft.get_scratch_len())];
        Self {
            time: fft.make_input_vec(),
            mic: fft.make_output_vec(),
            system: fft.make_output_vec(),
            scratch,
            fft,
            ifft,
        }
    }

    /// The offset `d` in `-MAX_LEAD..=MAX_DELAY` at which `mic[t]` best
    /// matches `system[t - d]` (either polarity), and how many standard
    /// deviations that peak stands from the correlation's mean there.
    fn peak(&mut self, mic: &[f32], system: &[f32]) -> anyhow::Result<(i64, f64)> {
        self.time.fill(0.0);
        self.time[..mic.len()].copy_from_slice(mic);
        self.fft
            .process_with_scratch(&mut self.time, &mut self.mic, &mut self.scratch)?;
        self.time.fill(0.0);
        self.time[..system.len()].copy_from_slice(system);
        self.fft
            .process_with_scratch(&mut self.time, &mut self.system, &mut self.scratch)?;
        for (m, s) in self.mic.iter_mut().zip(&self.system) {
            let cross = *m * s.conj();
            *m = cross / (cross.norm() + PHAT_FLOOR);
        }
        zero_edge_phases(&mut self.mic);
        self.ifft
            .process_with_scratch(&mut self.mic, &mut self.time, &mut self.scratch)?;

        let n = self.time.len() as i64;
        let at = |d: i64| f64::from(self.time[d.rem_euclid(n) as usize]);
        let window = -(MAX_LEAD as i64)..=MAX_DELAY as i64;
        let count = (MAX_LEAD + MAX_DELAY + 1) as f64;
        let mean = window.clone().map(at).sum::<f64>() / count;
        let var = window.clone().map(|d| (at(d) - mean).powi(2)).sum::<f64>() / count;
        let (best, deviation) = window
            .map(|d| (d, (at(d) - mean).abs()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0, 0.0));
        let z = if var > 0.0 {
            deviation / var.sqrt()
        } else {
            0.0
        };
        Ok((best, z))
    }
}

/// A real signal's spectrum has real DC and Nyquist bins; keep rounding
/// from making them otherwise before an inverse transform.
fn zero_edge_phases(spectrum: &mut [Complex<f32>]) {
    if let Some(first) = spectrum.first_mut() {
        first.im = 0.0;
    }
    if let Some(last) = spectrum.last_mut() {
        last.im = 0.0;
    }
}

/// Pass 2: the system frame energy (dB) above which a frame is far-end
/// dominant: its [`FAR_QUANTILE`] over every frame.
fn far_threshold(pair: &Pair<'_>, stft: &mut Stft) -> anyhow::Result<f32> {
    let mut frames = pair.frames()?;
    let mut hist = Histogram::new(-300.0, 100.0, 0.1);
    let (mut mic, mut system) = ([0f32; FRAME], [0f32; FRAME]);
    let mut spectrum = vec![Complex::default(); BINS];
    let mut power = [0f32; BINS];
    while frames.read_next(&mut mic, &mut system)? {
        stft.forward(&system, &mut spectrum)?;
        power_of(&spectrum, &mut power);
        hist.add(frame_db(&power));
    }
    let threshold = hist.quantile(FAR_QUANTILE).unwrap_or(f32::INFINITY);
    debug!("echo: far-end frames are above {threshold:.1} dB");
    Ok(threshold)
}

/// Pass 3: the echo path's power gain per bin, the median of mic power over
/// smeared system power across the far-end dominant frames, or `None` when
/// there are none. Histograms of the ratio in dB keep this O(bins) however
/// many frames go in.
fn echo_path(pair: &Pair<'_>, threshold: f32, stft: &mut Stft) -> anyhow::Result<Option<Vec<f32>>> {
    let mut frames = pair.frames()?;
    let mut smear = Smear::new();
    let mut hists: Vec<Histogram> = (0..BINS)
        .map(|_| Histogram::new(-150.0, 150.0, 0.5))
        .collect();
    let (mut mic, mut system) = ([0f32; FRAME], [0f32; FRAME]);
    let mut spectrum = vec![Complex::default(); BINS];
    let (mut far_power, mut mic_power) = ([0f32; BINS], [0f32; BINS]);
    let mut far_frames = 0usize;
    while frames.read_next(&mut mic, &mut system)? {
        stft.forward(&system, &mut spectrum)?;
        power_of(&spectrum, &mut far_power);
        let smeared = smear.push(&far_power);
        if frame_db(&far_power) <= threshold || mean_square(&system) < ACTIVE_POWER {
            continue;
        }
        stft.forward(&mic, &mut spectrum)?;
        power_of(&spectrum, &mut mic_power);
        for ((hist, &y), &x) in hists.iter_mut().zip(&mic_power).zip(smeared) {
            hist.add(db(y / (x + POWER_FLOOR)));
        }
        far_frames += 1;
    }
    debug!("echo: learned the echo path from {far_frames} far-end frames");
    if far_frames == 0 {
        return Ok(None);
    }
    Ok(Some(
        hists
            .iter()
            .map(|h| undb(h.quantile(0.5).unwrap_or(-150.0)))
            .collect(),
    ))
}

/// Pass 4: turn each mic bin down by the predicted echo, `G = 1 -
/// OVERSUBTRACTION * echo / mic` clamped to `0..=1`, and write the result
/// to `out` as the frames complete. Returns the near-end mask: a
/// [`speech::FRAME`] is the user speaking when any STFT frame overlapping
/// it has the mic [`NEAR_END_MARGIN_DB`] over the predicted echo in the
/// speech band and the output there at [`NEAR_END_FLOOR_DB`] or louder.
fn suppress(
    pair: &Pair<'_>,
    path: &[f32],
    stft: &mut Stft,
    out: &Path,
) -> anyhow::Result<Vec<bool>> {
    let file = create_private(out)?;
    let mut writer = hound::WavWriter::new(BufWriter::new(file.try_clone()?), track_spec())
        .with_context(|| format!("failed to start WAV {}", out.display()))?;
    // Every sample lies in two frames: offset `j` of one and `j + HOP` of the
    // one before, so this undoes the analysis and synthesis windows.
    let norm: Vec<f32> = (0..HOP)
        .map(|j| 1.0 / (stft.window[j].powi(2) + stft.window[j + HOP].powi(2)))
        .collect();
    // In-band spectral power to mean power per sample (Parseval, one-sided).
    let per_sample = FRAME as f64 * stft.window.iter().map(|w| f64::from(w * w)).sum::<f64>() / 2.0;
    let mut near_end = vec![false; speech::frame_count(pair.mic_len)];

    let mut frames = pair.frames()?;
    let mut smear = Smear::new();
    let (mut mic, mut system) = ([0f32; FRAME], [0f32; FRAME]);
    let mut spectrum = vec![Complex::default(); BINS];
    let mut far_power = [0f32; BINS];
    let mut synth = [0f32; FRAME];
    let mut acc = [0f32; FRAME];
    let mut written = 0usize;
    let mut index = 0usize;
    while frames.read_next(&mut mic, &mut system)? {
        stft.forward(&system, &mut spectrum)?;
        power_of(&spectrum, &mut far_power);
        let smeared = smear.push(&far_power);
        stft.forward(&mic, &mut spectrum)?;
        let (mut mic_band, mut echo_band, mut out_band) = (0f64, 0f64, 0f64);
        for (bin, ((y, &x), &h)) in spectrum.iter_mut().zip(smeared).zip(path).enumerate() {
            let power = y.norm_sqr();
            let echo = h * x;
            let gain = (1.0 - OVERSUBTRACTION * echo / (power + POWER_FLOOR)).clamp(0.0, 1.0);
            if SPEECH_BINS.contains(&bin) {
                mic_band += f64::from(power);
                echo_band += f64::from(echo);
                out_band += f64::from(power * gain * gain);
            }
            *y *= gain;
        }
        let margin = 10.0 * ((mic_band + 1e-12) / (echo_band + 1e-12)).log10();
        let level = 10.0 * (out_band / per_sample + 1e-12).log10();
        if margin >= NEAR_END_MARGIN_DB && level >= NEAR_END_FLOOR_DB {
            // Frame `index` covers mic samples `(index - 1) * HOP` to `+ FRAME`.
            let start = (index * HOP).saturating_sub(HOP);
            let end = ((index * HOP + HOP).min(pair.mic_len)).max(1) - 1;
            let last = near_end.len() - 1;
            near_end[(start / speech::FRAME).min(last)..=(end / speech::FRAME).min(last)]
                .fill(true);
        }
        stft.inverse(&mut spectrum, &mut synth)?;
        for (a, s) in acc.iter_mut().zip(&synth) {
            *a += s;
        }
        // The hop this frame starts at now has both of its frames (the first
        // frame starts a hop before the track and completes nothing).
        if index > 0 {
            let n = HOP.min(pair.mic_len - written);
            for (a, k) in acc[..n].iter().zip(&norm) {
                writer.write_sample(to_i16(a * k))?;
            }
            written += n;
        }
        acc.copy_within(HOP.., 0);
        acc[HOP..].fill(0.0);
        index += 1;
    }
    ensure!(
        written == pair.mic_len,
        "wrote {written} of {} samples to {}",
        pair.mic_len,
        out.display()
    );
    writer
        .finalize()
        .with_context(|| format!("failed to finish {}", out.display()))?;
    file.sync_all()?;
    Ok(near_end)
}

/// Mic frames on the STFT grid, each with the system frame its echo came
/// from. Frame `i` covers mic samples `(i - 1) * HOP` to `+ FRAME`: the
/// first starts a hop before the track, so every mic sample lies in exactly
/// two frames.
struct Frames<'a> {
    mic: SampleStream,
    system: SampleStream,
    lags: &'a LagTrack,
    next: usize,
    count: usize,
    system_start: i64,
}

impl Frames<'_> {
    /// Read the next frame pair, or return false after the last.
    fn read_next(&mut self, mic: &mut [f32], system: &mut [f32]) -> anyhow::Result<bool> {
        if self.next == self.count {
            return Ok(false);
        }
        let start = self.next as i64 * HOP as i64 - HOP as i64;
        let lag = self.lags.at((start + FRAME as i64 / 2) as f64).round() as i64;
        // The lag moves far less than a hop per frame; never step backwards.
        self.system_start = (start - lag).max(self.system_start);
        self.mic.read(start, mic)?;
        self.system.read(self.system_start, system)?;
        self.next += 1;
        Ok(true)
    }
}

/// One track read front to back, serving windows at non-decreasing starts
/// as samples in -1..1, with zeros outside the track. Holds only the
/// samples from the latest start on.
struct SampleStream {
    path: PathBuf,
    samples: hound::WavIntoSamples<BufReader<File>, i16>,
    /// Track index of `buf[0]`.
    start: i64,
    buf: VecDeque<f32>,
}

impl SampleStream {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            samples: open_track(path)?.into_samples(),
            start: 0,
            buf: VecDeque::new(),
        })
    }

    fn pull(&mut self) -> anyhow::Result<Option<f32>> {
        let sample = self
            .samples
            .next()
            .transpose()
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        Ok(sample.map(|s| f32::from(s) / 32768.0))
    }

    /// Fill `out` with samples `from..from + out.len()`.
    fn read(&mut self, from: i64, out: &mut [f32]) -> anyhow::Result<()> {
        let first = from.max(0);
        ensure!(first >= self.start, "track read went backwards");
        let dropped = usize::try_from(first - self.start)?.min(self.buf.len());
        self.buf.drain(..dropped);
        self.start += dropped as i64;
        while self.start < first {
            if self.pull()?.is_none() {
                break;
            }
            self.start += 1;
        }
        let end = from + out.len() as i64;
        while self.start + (self.buf.len() as i64) < end {
            match self.pull()? {
                Some(sample) => self.buf.push_back(sample),
                None => break,
            }
        }
        for (i, o) in out.iter_mut().enumerate() {
            let at = usize::try_from(from + i as i64 - self.start).ok();
            *o = at.and_then(|at| self.buf.get(at)).copied().unwrap_or(0.0);
        }
        Ok(())
    }
}

/// 512-point STFT with a periodic Hann window.
struct Stft {
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    time: Vec<f32>,
    scratch: Vec<Complex<f32>>,
}

impl Stft {
    fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FRAME);
        let ifft = planner.plan_fft_inverse(FRAME);
        let window = (0..FRAME)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FRAME as f32).cos())
            .collect();
        let scratch = vec![Complex::default(); fft.get_scratch_len().max(ifft.get_scratch_len())];
        Self {
            window,
            time: fft.make_input_vec(),
            scratch,
            fft,
            ifft,
        }
    }

    /// The spectrum of one windowed frame.
    fn forward(&mut self, frame: &[f32], spectrum: &mut [Complex<f32>]) -> anyhow::Result<()> {
        for ((t, x), w) in self.time.iter_mut().zip(frame).zip(&self.window) {
            *t = x * w;
        }
        self.fft
            .process_with_scratch(&mut self.time, spectrum, &mut self.scratch)?;
        Ok(())
    }

    /// A spectrum back to a windowed frame, ready to overlap-add.
    fn inverse(&mut self, spectrum: &mut [Complex<f32>], frame: &mut [f32]) -> anyhow::Result<()> {
        zero_edge_phases(spectrum);
        self.ifft
            .process_with_scratch(spectrum, frame, &mut self.scratch)?;
        let scale = 1.0 / FRAME as f32;
        for (x, w) in frame.iter_mut().zip(&self.window) {
            *x *= w * scale;
        }
        Ok(())
    }
}

/// The far-end power still ringing in the room: per bin, the largest of
/// the last [`SMEAR_FRAMES`] frames' power, scaled by [`SMEAR_DECAY`] per
/// frame of age.
struct Smear {
    /// Newest first.
    history: VecDeque<[f32; BINS]>,
    out: [f32; BINS],
}

impl Smear {
    fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(SMEAR_FRAMES),
            out: [0.0; BINS],
        }
    }

    fn push(&mut self, power: &[f32; BINS]) -> &[f32; BINS] {
        if self.history.len() == SMEAR_FRAMES {
            self.history.pop_back();
        }
        self.history.push_front(*power);
        self.out.fill(0.0);
        let mut weight = 1.0;
        for frame in &self.history {
            for (o, p) in self.out.iter_mut().zip(frame) {
                *o = o.max(p * weight);
            }
            weight *= SMEAR_DECAY;
        }
        &self.out
    }
}

/// Counts in fixed-width bins, with values outside the range counted in the
/// end bins: quantiles of any number of values in O(bins) memory.
struct Histogram {
    lo: f32,
    step: f32,
    counts: Vec<u32>,
    total: u64,
}

impl Histogram {
    fn new(lo: f32, hi: f32, step: f32) -> Self {
        Self {
            lo,
            step,
            counts: vec![0; ((hi - lo) / step).round() as usize],
            total: 0,
        }
    }

    fn add(&mut self, value: f32) {
        let last = self.counts.len() - 1;
        // NaN and -inf land in the first bin, +inf in the last.
        let bin = (((value - self.lo) / self.step).floor().max(0.0) as usize).min(last);
        self.counts[bin] += 1;
        self.total += 1;
    }

    /// The `q` quantile (0 to 1), linear within its bin; `None` when empty.
    fn quantile(&self, q: f64) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        let target = q * self.total as f64;
        let mut below = 0u64;
        for (i, &count) in self.counts.iter().enumerate() {
            let count = u64::from(count);
            if count > 0 && (below + count) as f64 >= target {
                let within = ((target - below as f64) / count as f64) as f32;
                return Some(self.lo + (i as f32 + within) * self.step);
            }
            below += count;
        }
        Some(self.lo + self.counts.len() as f32 * self.step)
    }
}

fn power_of(spectrum: &[Complex<f32>], power: &mut [f32]) {
    for (p, c) in power.iter_mut().zip(spectrum) {
        *p = c.norm_sqr();
    }
}

/// A frame's total power in dB.
fn frame_db(power: &[f32]) -> f32 {
    db(power.iter().sum())
}

fn db(power: f32) -> f32 {
    10.0 * power.max(1e-30).log10()
}

fn undb(db: f32) -> f32 {
    10f32.powf(db / 10.0)
}

fn mean_square(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / samples.len() as f64
}

fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// The format of every recording track: 16 kHz mono 16-bit PCM.
fn track_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: super::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

/// Create (or truncate) `path` with mode 0600.
fn create_private(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // `mode` only applies to a new file.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Synthetic recording audio for the tests here and in `process`.
#[cfg(test)]
pub(super) mod fixtures {
    use super::RATE;
    use std::path::Path;

    /// Deterministic xorshift noise, so the fixtures need no extra crate.
    pub struct Rng(pub u64);

    impl Rng {
        /// Uniform in -1..1.
        pub fn uniform(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }
    }

    /// Sample index of `secs`.
    pub fn at(secs: f64) -> usize {
        (secs * RATE as f64) as usize
    }

    /// Speech-like audio: in each `(start, end)` span (seconds), low-passed
    /// noise under a 4 Hz syllable envelope, scaled so the spans have RMS
    /// `level`. Silence elsewhere.
    pub fn speech(len: usize, spans: &[(f64, f64)], level: f32, seed: u64) -> Vec<f32> {
        let mut rng = Rng(seed);
        let mut out = vec![0.0f32; len];
        let mut low = 0.0f32;
        let mut active = 0usize;
        for &(start, end) in spans {
            let (from, to) = (at(start).min(len), at(end).min(len));
            let phase = rng.uniform() * std::f32::consts::PI;
            for (i, o) in out[from..to].iter_mut().enumerate() {
                low += 0.5 * (rng.uniform() - low);
                let t = i as f32 / RATE as f32;
                *o = low * (0.5 - 0.5 * (std::f32::consts::TAU * 4.0 * t + phase).cos());
            }
            active += to - from;
        }
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / active.max(1) as f32).sqrt();
        if rms > 0.0 {
            out.iter_mut().for_each(|s| *s *= level / rms);
        }
        out
    }

    /// The far end as the mic hears it: `delay` samples late, through a
    /// direct path and 30 reflections decaying over 100 ms, about 22 dB down.
    pub fn echo(far: &[f32], delay: usize, seed: u64) -> Vec<f32> {
        let mut rng = Rng(seed);
        let mut taps = vec![(0usize, 1.0f32)];
        for _ in 0..30 {
            let d = 1 + (rng.uniform().abs() * 1599.0) as usize;
            taps.push((d, 0.6 * rng.uniform() * (-(d as f32) / 400.0).exp()));
        }
        let gain = 10f32.powf(-24.0 / 20.0);
        (0..far.len())
            .map(|t| {
                gain * taps
                    .iter()
                    .filter_map(|&(d, g)| t.checked_sub(delay + d).map(|i| g * far[i]))
                    .sum::<f32>()
            })
            .collect()
    }

    /// Add uniform noise of RMS `level`.
    pub fn add_noise(samples: &mut [f32], level: f32, seed: u64) {
        let mut rng = Rng(seed);
        let amp = level * 3f32.sqrt();
        samples.iter_mut().for_each(|s| *s += amp * rng.uniform());
    }

    /// Element-wise sum.
    pub fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    /// The 16-bit samples a WAV would hold.
    pub fn quantize(samples: &[f32]) -> Vec<i16> {
        samples.iter().map(|&s| super::to_i16(s)).collect()
    }

    pub fn write(path: &Path, samples: &[f32]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data = crate::audio::wav::encode_wav(&quantize(samples)).unwrap();
        std::fs::write(path, data).unwrap();
    }

    pub fn read(path: &Path) -> Vec<f32> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(|s| f32::from(s.unwrap()) / 32768.0)
            .collect()
    }

    /// A 12 s call over speakers: the far side talks in two spans, the mic
    /// hears it 1000 samples late, and the user talks in between.
    /// Returns `(mic, system)`.
    pub fn speaker_call() -> (Vec<f32>, Vec<f32>) {
        let len = at(12.0);
        let mut system = speech(len, &[(0.5, 5.0), (7.5, 11.5)], 0.1, 1);
        let near = speech(len, &[(5.6, 7.0)], 0.01, 2);
        let mut mic = mix(&echo(&system, 1000, 3), &near);
        add_noise(&mut mic, 3e-4, 4);
        add_noise(&mut system, 1e-4, 5);
        (mic, system)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    /// Energy ratio of `after` over `before` in dB within `span` seconds.
    fn change_db(before: &[f32], after: &[f32], span: (f64, f64)) -> f64 {
        let energy = |x: &[f32]| {
            x[at(span.0)..at(span.1)]
                .iter()
                .map(|&s| f64::from(s).powi(2))
                .sum::<f64>()
                + 1e-12
        };
        10.0 * (energy(after) / energy(before)).log10()
    }

    /// Write the tracks, cancel, and return the outcome and the output.
    fn run(mic: &[f32], system: &[f32], timeline_lag: i64) -> (EchoOutcome, Option<Vec<f32>>) {
        let tmp = tempfile::tempdir().unwrap();
        let (mic_path, system_path, out) = (
            tmp.path().join("mic.wav"),
            tmp.path().join("system.wav"),
            tmp.path().join("out.wav"),
        );
        write(&mic_path, mic);
        write(&system_path, system);
        let outcome = cancel_echo(&mic_path, &system_path, timeline_lag, &out).unwrap();
        let output = out.exists().then(|| {
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            read(&out)
        });
        (outcome, output)
    }

    fn report(outcome: &EchoOutcome) -> &EchoReport {
        match outcome {
            EchoOutcome::Cancelled(done) => &done.report,
            other => panic!("expected a cancellation, got {other:?}"),
        }
    }

    /// Spans of [`double_talk_call`], in seconds, clear of the edges.
    const ECHO_ONLY: [(f64, f64); 3] = [(2.0, 8.0), (28.0, 36.0), (55.5, 58.5)];
    const USER_ONLY: [(f64, f64); 2] = [(9.7, 13.3), (38.1, 42.9)];
    const DOUBLE_TALK: [(f64, f64); 2] = [(16.5, 19.5), (46.5, 49.5)];
    /// Neither side talks: only the two noise floors.
    const QUIET: (f64, f64) = (23.0, 26.5);

    /// A minute of call: the far side talks in five spans; the user talks
    /// alone in two (at 0.01 RMS, about the echo's level) and over the far
    /// side in two more (at `over_far`). The mic hears the speakers 1000
    /// samples late. Returns `(mic, system, near end)`.
    fn double_talk_call(over_far: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let len = at(60.0);
        let far_spans = [
            (1.0, 9.0),
            (14.0, 22.0),
            (27.0, 37.0),
            (44.0, 52.0),
            (55.0, 59.0),
        ];
        let mut system = speech(len, &far_spans, 0.1, 1);
        let near = mix(
            &speech(len, &[(9.6, 13.4), (38.0, 43.0)], 0.01, 2),
            &speech(len, &[(16.0, 20.0), (46.0, 50.0)], over_far, 6),
        );
        let mut mic = mix(&echo(&system, 1000, 3), &near);
        add_noise(&mut mic, 3e-4, 4);
        add_noise(&mut system, 1e-4, 5);
        (read_back(&mic), system, read_back(&near))
    }

    /// The track offsets say 900 samples where the echo is 1000 late.
    #[test]
    fn removes_the_echo_and_keeps_the_user() {
        let (mic, system, near) = double_talk_call(0.01);
        let (outcome, out) = run(&mic, &system, 900);
        let report = report(&outcome);
        assert!((report.lag - 1000).abs() <= 2, "{report:?}");
        let out = out.unwrap();
        assert_eq!(out.len(), mic.len());

        for span in ECHO_ONLY {
            let change = change_db(&mic, &out, span);
            assert!(change <= -15.0, "echo only {span:?}: {change:.1} dB");
        }
        for span in USER_ONLY {
            let change = change_db(&mic, &out, span);
            assert!(change.abs() < 1.0, "user only {span:?}: {change:.1} dB");
        }
        // Over the far side the user loses the bins the echo covers, but is
        // not wiped out with it.
        for span in DOUBLE_TALK {
            let kept = change_db(&near, &out, span);
            assert!(kept > -10.0, "double talk {span:?}: {kept:.1} dB");
        }
    }

    /// The near-end mask and the regions sent from it: the user alone, or
    /// speaking up over the far side, and never the echo or the noise floor.
    /// (Over the far side at the echo's own level the user is not told
    /// apart from it: the detector wants the mic 10 dB over the predicted
    /// echo.)
    #[test]
    fn the_near_end_mask_finds_the_user_not_the_echo() {
        let (mic, system, _) = double_talk_call(0.02);
        let (outcome, _) = run(&mic, &system, 900);
        let EchoOutcome::Cancelled(done) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(done.near_end.len(), speech::frame_count(mic.len()));
        let regions = speech::regions(&done.near_end, mic.len());
        // (share of frames active, share of the span sent)
        let measure = |span: (f64, f64)| {
            let frames = &done.near_end[at(span.0) / speech::FRAME..at(span.1) / speech::FRAME];
            let active = frames.iter().filter(|&&a| a).count() as f64 / frames.len() as f64;
            let samples = at(span.0)..at(span.1);
            let sent: usize = regions
                .iter()
                .map(|r| {
                    r.end
                        .min(samples.end)
                        .saturating_sub(r.start.max(samples.start))
                })
                .sum();
            (active, sent as f64 / samples.len() as f64)
        };
        for span in ECHO_ONLY.into_iter().chain([QUIET]) {
            let (active, sent) = measure(span);
            assert!(
                active < 0.02 && sent == 0.0,
                "{span:?}: {active:.2} {sent:.2}"
            );
        }
        for span in USER_ONLY {
            let (active, sent) = measure(span);
            assert!(
                active > 0.95 && sent == 1.0,
                "{span:?}: {active:.2} {sent:.2}"
            );
        }
        for span in DOUBLE_TALK {
            let (active, sent) = measure(span);
            assert!(
                active > 0.6 && sent == 1.0,
                "{span:?}: {active:.2} {sent:.2}"
            );
        }
    }

    /// An echo 20 dB louder than usual leaves a residual above the level
    /// floor, and is still not taken for the user: the margin over the
    /// predicted echo decides.
    #[test]
    fn a_loud_echo_is_not_the_user() {
        let len = at(30.0);
        let mut system = speech(len, &[(1.0, 13.0), (16.0, 29.0)], 0.1, 21);
        let mut mic: Vec<f32> = echo(&system, 1000, 22).iter().map(|s| s * 10.0).collect();
        add_noise(&mut mic, 3e-4, 23);
        add_noise(&mut system, 1e-4, 24);
        let (outcome, out) = run(&mic, &system, 0);
        let EchoOutcome::Cancelled(done) = outcome else {
            panic!("{outcome:?}");
        };
        let out = out.unwrap();
        let level = 10.0
            * (out[at(2.0)..at(12.0)]
                .iter()
                .map(|&v| f64::from(v).powi(2))
                .sum::<f64>()
                / at(10.0) as f64)
                .log10();
        assert!(level > NEAR_END_FLOOR_DB, "residual {level:.1} dB");
        assert!(done.near_end.iter().all(|&a| !a), "echo taken for the user");
    }

    /// The samples as the WAV round trip leaves them.
    fn read_back(samples: &[f32]) -> Vec<f32> {
        quantize(samples)
            .into_iter()
            .map(|s| f32::from(s) / 32768.0)
            .collect()
    }

    /// A USB mic and laptop speakers run on separate clocks: the system
    /// track is resampled by 100 ppm, so the echo slides 288 samples over
    /// three minutes. The lag is followed and the end is still cancelled.
    /// One lag for the whole call would also clear this bar at 100 ppm, and
    /// a fixture cannot drift much faster (from about 300 ppm a block's peak
    /// smears below [`MIN_PEAK_Z`]), so `frames_follow_a_sloped_lag_track`
    /// pins the per-frame lag instead.
    #[test]
    fn follows_clock_drift() {
        let len = at(180.0);
        let spans: Vec<(f64, f64)> = (0..22)
            .map(|i| (1.0 + 8.0 * i as f64, 7.0 + 8.0 * i as f64))
            .collect();
        let far = speech(len, &spans, 0.1, 6);
        let mut mic = echo(&far, 800, 7);
        add_noise(&mut mic, 3e-4, 8);
        let system = resampled(&far, 1e-4);
        let mic = read_back(&mic);

        let (outcome, out) = run(&mic, &system, 0);
        let report = report(&outcome);
        // 288 samples end to end; the edge medians each give up a block.
        assert!(
            (200..=330).contains(&report.lag_drift),
            "drift followed: {report:?}"
        );
        let out = out.unwrap();
        let last = spans[spans.len() - 1];
        let change = change_db(&mic, &out, (last.0 + 0.5, last.1 - 0.5));
        assert!(change <= -12.0, "end of the call: {change:.1} dB");
    }

    /// `far` as a system track whose clock runs `drift` fast: system sample
    /// `v` is the far end at `v * (1 + drift)` on the mic's clock.
    fn resampled(far: &[f32], drift: f64) -> Vec<f32> {
        (0..far.len())
            .map(|v| {
                let x = v as f64 * (1.0 + drift);
                let (i, frac) = (x as usize, x.fract() as f32);
                match (far.get(i), far.get(i + 1)) {
                    (Some(&a), Some(&b)) => a + (b - a) * frac,
                    (Some(&a), None) => a,
                    _ => 0.0,
                }
            })
            .collect()
    }

    /// The offsets put the echo 3900 samples before where it is, near the
    /// edge of the first search window, and the lag shrinks by 100 ppm, so
    /// the echo leaves that window after about a minute. The search follows
    /// the measured lag and finds it to the end of the call.
    #[test]
    fn the_lag_search_follows_drift_out_of_the_first_window() {
        let len = at(180.0);
        let spans: Vec<(f64, f64)> = (0..22)
            .map(|i| (1.0 + 8.0 * i as f64, 7.0 + 8.0 * i as f64))
            .collect();
        let far = speech(len, &spans, 0.1, 17);
        let mut mic = echo(&far, 1000, 18);
        add_noise(&mut mic, 3e-4, 19);
        let drift = -1e-4;
        let tmp = tempfile::tempdir().unwrap();
        let (mic_path, system_path) = (tmp.path().join("mic.wav"), tmp.path().join("system.wav"));
        write(&mic_path, &mic);
        write(&system_path, &resampled(&far, drift));

        let lags = estimate_lags(&mic_path, &system_path, 1000 + 3900, len)
            .unwrap()
            .unwrap();
        // Mic sample `u` holds the far end of system sample `u - lag`.
        let truth = |u: f64| u - (u - 1000.0) / (1.0 + drift);
        let last = lags.points.last().unwrap().0;
        assert!(last > at(170.0) as f64, "measured only up to sample {last}");
        for &(pos, lag) in &lags.points {
            let want = truth(pos);
            assert!((lag - want).abs() <= 40.0, "at {pos}: {lag} vs {want:.0}");
        }
    }

    /// Each system frame starts where the lag track puts the echo at that
    /// frame, not at one lag for the whole call.
    #[test]
    fn frames_follow_a_sloped_lag_track() {
        let tmp = tempfile::tempdir().unwrap();
        let (mic, system) = (tmp.path().join("mic.wav"), tmp.path().join("system.wav"));
        let len = 48_000;
        write(&mic, &vec![0.0; len]);
        // Every system sample encodes its own index.
        let code = |i: i64| (i % 20_000) as f32 / 32768.0;
        write(&system, &(0..60_000).map(code).collect::<Vec<f32>>());
        // 500 samples at the start, 5300 at the end: 0.1 per sample.
        let pair = Pair {
            mic: &mic,
            system: &system,
            mic_len: len,
            lags: LagTrack {
                points: vec![(0.0, 500.0), (len as f64, 5300.0)],
            },
        };
        let mut frames = pair.frames().unwrap();
        let (mut mic_frame, mut system_frame) = ([0f32; FRAME], [0f32; FRAME]);
        let mut checked = 0;
        while frames.read_next(&mut mic_frame, &mut system_frame).unwrap() {
            let start = (frames.next as i64 - 2) * HOP as i64;
            let centre = (start + FRAME as i64 / 2) as f64;
            let lag = (500.0 + 0.1 * centre.clamp(0.0, len as f64)).round() as i64;
            let from = start - lag;
            if from >= 0 {
                assert_eq!(system_frame[0], code(from), "frame at {start}");
                assert_eq!(system_frame[FRAME - 1], code(from + FRAME as i64 - 1));
                checked += 1;
            }
        }
        assert!(checked > 150, "{checked} frames checked");
    }

    /// The far side rarely talks while the user talks almost all the time,
    /// over a system track with a noise floor. Most of the loudest 30% of
    /// system frames are then its noise under the user's voice, which must
    /// not be learned as the echo path.
    #[test]
    fn a_mostly_quiet_far_side_keeps_the_user() {
        let len = at(60.0);
        let far_spans = [(2.0, 5.0), (40.0, 43.0)];
        let near_spans = [(5.5, 39.5), (43.5, 59.5)];
        let mut system = speech(len, &far_spans, 0.1, 9);
        // Two voices, so the syllable gaps of one are filled by the other.
        let near = mix(
            &speech(len, &near_spans, 0.007, 10),
            &speech(len, &near_spans, 0.007, 11),
        );
        let mut mic = mix(&echo(&system, 1000, 12), &near);
        add_noise(&mut mic, 3e-4, 13);
        add_noise(&mut system, 1e-4, 14);
        let mic = read_back(&mic);

        let (outcome, out) = run(&mic, &system, 1000);
        // The echo is about 22 dB down; learned from the user's voice over
        // the noise floor it comes out near +20 dB and wipes the weak bins
        // of everything the user says.
        let path_db = report(&outcome).echo_path_db;
        assert!(path_db < -15.0, "echo path {path_db:.1} dB");
        let out = out.unwrap();
        for span in [(6.0, 39.0), (44.0, 59.0)] {
            let change = change_db(&mic, &out, span);
            assert!(change.abs() < 1.0, "user only {span:?}: {change:.1} dB");
        }
        for span in [(2.5, 4.5), (40.5, 42.5)] {
            let change = change_db(&mic, &out, span);
            assert!(change <= -15.0, "echo only {span:?}: {change:.1} dB");
        }
    }

    #[test]
    fn a_silent_system_track_is_skipped() {
        let (mic, _) = speaker_call();
        let (outcome, out) = run(&mic, &vec![0.0; mic.len()], 0);
        assert_eq!(outcome, EchoOutcome::Skipped(SkipReason::SilentSystem));
        assert!(out.is_none(), "nothing written");

        // A system track that ends before the mic starts hearing anything.
        let (outcome, _) = run(&mic, &[], 0);
        assert_eq!(outcome, EchoOutcome::Skipped(SkipReason::SilentSystem));
    }

    /// With headphones the system track plays but the mic never hears it.
    #[test]
    fn headphones_are_skipped() {
        let len = at(25.0);
        let system = speech(len, &[(1.0, 12.0), (14.0, 24.0)], 0.1, 14);
        let mut mic = speech(len, &[(3.0, 6.0), (15.0, 18.0)], 0.01, 15);
        add_noise(&mut mic, 3e-4, 16);
        let (outcome, out) = run(&mic, &system, 0);
        assert_eq!(outcome, EchoOutcome::Skipped(SkipReason::NoEcho));
        assert!(out.is_none());
    }

    #[test]
    fn short_tracks_work() {
        // Shorter than one lag block.
        let (mic, system) = speaker_call();
        let (mic, system) = (read_back(&mic[..at(4.0)]), &system[..at(4.0)]);
        let (outcome, out) = run(&mic, system, 0);
        report(&outcome);
        let out = out.unwrap();
        assert_eq!(out.len(), mic.len());
        let change = change_db(&mic, &out, (1.0, 3.5));
        assert!(change <= -15.0, "{change:.1} dB");

        // Shorter than a frame: no error, and the length still holds.
        for len in [100, 1] {
            let (outcome, out) = run(&mic[..len], &system[..len], 0);
            if let Some(out) = out {
                assert_eq!(out.len(), len, "{outcome:?}");
            }
        }
        let (outcome, out) = run(&[], system, 0);
        assert_eq!(outcome, EchoOutcome::Skipped(SkipReason::EmptyMic));
        assert!(out.is_none());
    }

    #[test]
    fn untouched_audio_round_trips_exactly() {
        // Where the system track is digital silence the gain is exactly 1,
        // and the user's samples come back bit for bit, not merely close.
        let len = at(12.0);
        let system = speech(len, &[(0.5, 5.0), (7.5, 11.5)], 0.1, 1);
        let mut mic = mix(
            &echo(&system, 1000, 3),
            &speech(len, &[(5.6, 7.0)], 0.01, 2),
        );
        add_noise(&mut mic, 3e-4, 4);
        let (outcome, out) = run(&mic, &system, 0);
        report(&outcome);
        let (mic, out) = (quantize(&mic), quantize(&out.unwrap()));
        assert!(
            mic[at(5.8)..at(6.8)] == out[at(5.8)..at(6.8)],
            "user audio changed"
        );
    }

    #[test]
    fn lag_track_interpolates_and_holds() {
        let track = LagTrack {
            points: vec![(100.0, 10.0), (300.0, 30.0)],
        };
        assert_eq!(track.at(0.0), 10.0);
        assert_eq!(track.at(200.0), 20.0);
        assert_eq!(track.at(1e9), 30.0);
        // One outlier among agreeing blocks is outvoted.
        let smoothed =
            median_smoothed(&[(0.0, 5.0), (1.0, 5.0), (2.0, 90.0), (3.0, 5.0), (4.0, 6.0)]);
        assert!(smoothed.iter().all(|&(_, lag)| lag <= 6.0), "{smoothed:?}");
    }

    #[test]
    fn histogram_quantiles() {
        let mut hist = Histogram::new(0.0, 100.0, 0.5);
        for v in 0..100 {
            hist.add(v as f32 + 0.25);
        }
        let median = hist.quantile(0.5).unwrap();
        assert!((median - 50.0).abs() <= 0.5, "{median}");
        // Out of range values are clamped, never lost.
        hist.add(f32::NEG_INFINITY);
        hist.add(f32::NAN);
        hist.add(1e9);
        assert_eq!(hist.total, 103);
        assert!(Histogram::new(0.0, 1.0, 0.5).quantile(0.5).is_none());
    }

    /// Real-data check, run by hand on copies of a real recording:
    ///
    /// ```text
    /// WHISRS_ECHO_MIC=mic.wav WHISRS_ECHO_SYSTEM=system.wav \
    /// WHISRS_ECHO_OUT=out.wav WHISRS_ECHO_LAG=<(system - mic offset_ms) * 16> \
    /// cargo test --release --lib echo_on_a_real_recording -- --ignored --nocapture
    /// ```
    ///
    /// Prints the energy metrics of the Python prototype: the median 10 ms
    /// frame level of the mic before and after, where the aligned system
    /// track is loud (above -30 dB) and where it is silent (below -60 dB)
    /// while the mic is not (above -45 dB).
    #[test]
    #[ignore = "needs a real recording, see the doc comment"]
    fn echo_on_a_real_recording() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let (mic, system, out) = (
            PathBuf::from(var("WHISRS_ECHO_MIC")),
            PathBuf::from(var("WHISRS_ECHO_SYSTEM")),
            PathBuf::from(var("WHISRS_ECHO_OUT")),
        );
        let timeline_lag: i64 = var("WHISRS_ECHO_LAG").parse().unwrap();
        let started = std::time::Instant::now();
        let outcome = cancel_echo(&mic, &system, timeline_lag, &out).unwrap();
        println!("{outcome:?} in {:.2?}", started.elapsed());
        let report = report(&outcome);

        // 10 ms frame levels, streamed so the check does not hold the tracks.
        let frames = |path: &Path, shift: i64, count: usize| {
            let mut stream = SampleStream::open(path).unwrap();
            let mut frame = [0f32; 160];
            (0..count)
                .map(|k| {
                    stream.read(k as i64 * 160 - shift, &mut frame).unwrap();
                    10.0 * (mean_square(&frame) + 1e-10).log10()
                })
                .collect::<Vec<f64>>()
        };
        let count = open_track(&mic)
            .unwrap()
            .duration()
            .min(open_track(&system).unwrap().duration()) as usize
            / 160;
        let em = frames(&mic, 0, count);
        let eo = frames(&out, 0, count);
        let es = frames(&system, report.lag, count);
        let median = |pick: &dyn Fn(usize) -> bool, of: &[f64]| {
            let mut v: Vec<f64> = (0..count).filter(|&i| pick(i)).map(|i| of[i]).collect();
            v.sort_by(f64::total_cmp);
            (v.get(v.len() / 2).copied().unwrap_or(f64::NAN), v.len())
        };
        let far = |i: usize| es[i] > -30.0;
        let near = |i: usize| es[i] < -60.0 && em[i] > -45.0;
        let double = |i: usize| es[i] > -30.0 && em[i] > es[i] - 16.0;
        for (name, pick) in [
            ("far-end active", &far as &dyn Fn(usize) -> bool),
            ("near-end only", &near),
            ("double-talk-ish", &double),
        ] {
            let ((before, n), (after, _)) = (median(pick, &em), median(pick, &eo));
            println!("{name} frames ({n}): mic {before:.1} dB -> out {after:.1} dB");
        }
    }
}
