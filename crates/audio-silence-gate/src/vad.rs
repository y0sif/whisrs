//! Speech detection: the earshot model behind warm-up, level floors, onset
//! confirmation, hysteresis and hangover, the RMS test beside it, and the
//! whole-buffer gate and per-frame mask built on the same detector.

use crate::level::{dbfs, FrameStats, LevelMeter, Levels, SILENT_PEAK};
use crate::{audio_gate_reason, duration_reason, rms_energy, GateReason, SILENCE_RMS_THRESHOLD};

/// The only sample rate the earshot model runs at. A detector built for any
/// other rate falls back to [`Vad::Rms`].
pub const VAD_SAMPLE_RATE: u32 = 16_000;

/// Samples in one earshot frame: 16 ms at [`VAD_SAMPLE_RATE`].
pub const VAD_FRAME: usize = 256;

/// Duration of one analysis frame.
const FRAME_MS: u64 = 16;

/// Frames after `new` or `reset` that are never speech (160 ms): an abrupt
/// noise onset scores about 0.5 for about 130 ms while the model settles.
pub(crate) const WARMUP_FRAMES: u64 = 10;

/// Consecutive frames at or above the threshold that start speech (32 ms),
/// so a one-frame blip (a click, a key) never does.
const ONSET_FRAMES: u32 = 2;

/// Frames under the release score that speech survives (96 ms), which
/// bridges the dips inside words.
const HANGOVER_FRAMES: u32 = 6;

/// Accepted speech that arms auto-stop and passes the gate (160 ms): more
/// than a noise onset scores, less than a lone "yes".
pub(crate) const MIN_SPEECH_MS: u64 = 160;

/// Accepted speech that must set the running level before the relative
/// floor hangs from it (300 ms).
const RELATIVE_FLOOR_AFTER_MS: u64 = 300;

/// Time constants of the running speech level, counted in speech time.
const LEVEL_ATTACK_MS: f32 = 50.0;
const LEVEL_RELEASE_MS: f32 = 5_000.0;

/// Pre-emphasis of the level the soft threshold measures,
/// `y[n] = x[n] - 0.95 x[n-1]`: -24 dB at 100 Hz, -8 dB at 1 kHz, 0 dB from
/// 3 kHz. A mic's rumble and a fan's low end carry most of a quiet room's
/// energy and little of a soft voice, so measured this way a soft voice
/// stands 6 dB or more over the room, while steady pink or brown noise
/// stands under 2 dB over its own quietest frame.
const EMPHASIS: f64 = 0.95;

/// Frames of sound the background is the quietest of (240 ms). A noise that
/// switches on is the background after this long, so it stands clear of it
/// for at most 240 ms, which at half weight (see [`Evidence`]) is 120 ms
/// of the 160 ms that arm auto-stop. Longer let noise steps arm it; shorter
/// lost the soft talk after a sentence at normal volume.
const BACKGROUND_FRAMES: usize = 15;

/// Consecutive frames a soft voice stands clear of the background (32 ms)
/// before the soft threshold applies, so a key click or a tap (one or two
/// frames) never does.
const CLEAR_FRAMES: u32 = 2;

/// Frames without a voiced frame (304 ms) after which the soft
/// frames of a run stop counting toward arming and the gate (see
/// [`Evidence`]). Noise that comes and goes (a fan cycling, dishes) stands
/// clear for up to 240 ms each time, and pauses between them are longer;
/// the pauses inside soft talk are shorter.
const SOFT_RUN_GAP_FRAMES: u32 = 19;

/// Which test decides speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Vad {
    /// The earshot model (16 kHz only), behind the guards of
    /// [`SpeechDetector`].
    #[default]
    Earshot,
    /// The 0.1.31 loudness test: a block is speech when its RMS reaches
    /// [`VadConfig::rms_threshold`].
    Rms,
}

/// Detector settings. Start from [`VadConfig::close`] or
/// [`VadConfig::open`]; the fields are public for tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    pub vad: Vad,
    /// earshot score at or above which speech starts (0.55). Lower values
    /// let noise hold speech open: after speech the model's context lifts
    /// the score of the same noise from about 0.15 to 0.37 (+10 dB SNR) and
    /// 0.46 (0 dB), with runs above 0.5. 0.6 costs quiet speakers recall.
    pub threshold: f32,
    /// earshot score under which speech may end (0.55, the threshold). A
    /// lower release is the band that post-speech noise sits in; the
    /// hysteresis is there for a custom `threshold` above it.
    pub release: f32,
    /// A frame quieter than this (dBFS) is never speech, whatever its score.
    pub min_speech_dbfs: f32,
    /// A frame more than this many dB under the running speech level is never
    /// speech, once 300 ms of speech has set that level. The floor never
    /// rises above [`rms_threshold`](Self::rms_threshold): a frame the RMS
    /// test calls speech is never under it, so a short loud burst (a laugh at
    /// the mic) cannot push the user's normal speech under it. `None` turns
    /// the relative floor off.
    pub relative_floor_db: Option<f32>,
    /// [`Vad::Rms`]: normalized RMS at or above which a block is speech. With
    /// [`Vad::Earshot`], the highest the relative floor goes.
    pub rms_threshold: f64,
    /// A frame at least this many dB over the background (the quietest of
    /// the last 240 ms of sound), both measured pre-emphasized, needs only
    /// [`soft_threshold`](Self::soft_threshold) to start or keep speech once
    /// it and the frame before it stand that clear. `None` turns it off.
    pub soft_snr_db: Option<f32>,
    /// earshot score enough for a frame [`soft_snr_db`](Self::soft_snr_db)
    /// over the background (0.25). earshot scores a soft voice 0.2 to 0.5,
    /// under [`threshold`](Self::threshold), while the noise it lifts after
    /// speech to 0.37 or more sits at the background, so it never qualifies.
    /// A frame voiced only through it counts half toward arming auto-stop
    /// and passing the gate, and only within its run of speech.
    pub soft_threshold: f32,
}

impl VadConfig {
    /// One user near the mic (dictation, command mode, agent turns): frames
    /// under -65 dBFS, or more than 18 dB under the user (and under the RMS
    /// test's -50.5 dBFS), are not speech, so a TV or a colleague behind them
    /// does not hold a recording open.
    ///
    /// The values are tuned with `scripts/check-vad.sh` (18 dB: 20 let
    /// babble 20 dB under the user hold auto-stop open in a quarter of the
    /// noise draws, 15 cost the user's own soft frames; the cap at the RMS
    /// threshold: a 0.4 s "Oh!" 26 dB over the dictation after it otherwise
    /// left the dictation all under the floor, and auto-stop cut it off; the
    /// soft threshold 0.25 at 6 dB: a soft voice earshot scores 0.2 to 0.5
    /// was not speech at 0.55, and 0.2 let a drum loop arm auto-stop).
    pub fn close(vad: Vad) -> Self {
        Self {
            vad,
            threshold: 0.55,
            release: 0.55,
            min_speech_dbfs: -65.0,
            relative_floor_db: Some(18.0),
            rms_threshold: SILENCE_RMS_THRESHOLD,
            soft_snr_db: Some(6.0),
            soft_threshold: 0.25,
        }
    }

    /// Any voice counts (recording tracks: a shared mic, the system track):
    /// only frames under -80 dBFS are rejected, since dropping a quiet voice
    /// there loses transcript.
    pub fn open(vad: Vad) -> Self {
        Self {
            min_speech_dbfs: -80.0,
            relative_floor_db: None,
            ..Self::close(vad)
        }
    }
}

/// A [`SpeechDetector`]'s verdict on one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameDecision {
    /// The frame's first sample, counted from `new` or the last `reset`.
    pub start_sample: u64,
    /// Whether the frame is speech, after every guard.
    pub speech: bool,
    /// The earshot score (0.0 to 1.0). With [`Vad::Rms`], 1.0 for a frame at
    /// or above the threshold and 0.0 otherwise.
    pub score: f32,
    /// The frame's RMS level in dBFS (-120 for digital silence).
    pub dbfs: f32,
}

/// A frame [`SpeechDetector::heard_voice`] counted: when, and on which
/// rule. The first and the last of a stream are kept
/// ([`SpeechDetector::first_heard`], [`SpeechDetector::last_heard`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeardFrame {
    /// The frame's first sample, counted from `new` or the last `reset`.
    pub start_sample: u64,
    /// Samples in the frame: it ends at `start_sample + len`.
    pub len: u64,
    /// The earshot score (1.0 for a frame the RMS test calls speech).
    pub score: f32,
    /// The frame's RMS level in dBFS.
    pub dbfs: f32,
    /// Voiced only through [`VadConfig::soft_threshold`], under the full
    /// threshold.
    pub soft: bool,
}

impl HeardFrame {
    /// The sample after the frame.
    pub fn end_sample(&self) -> u64 {
        self.start_sample + self.len
    }
}

/// A source of per-frame speech scores: earshot, or a script in tests.
trait FrameScorer: Send {
    fn score(&mut self, frame: &[i16]) -> f32;
    fn reset(&mut self);
}

impl FrameScorer for earshot::Detector {
    fn score(&mut self, frame: &[i16]) -> f32 {
        self.predict_i16(frame)
    }

    fn reset(&mut self) {
        earshot::Detector::reset(self);
    }
}

/// What [`Hysteresis::step`] made of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Silence,
    /// Speech on its own score.
    Voiced,
    /// Speech only through the hangover.
    Held,
}

/// The scores one frame must reach to start and to keep speech.
#[derive(Debug, Clone, Copy)]
struct Bar {
    threshold: f32,
    release: f32,
}

impl Bar {
    /// `cfg`'s scores, or for a frame clear of the background the soft
    /// threshold where it is lower.
    fn of(cfg: &VadConfig, clear: bool) -> Self {
        if clear {
            Self {
                threshold: cfg.threshold.min(cfg.soft_threshold),
                release: cfg.release.min(cfg.soft_threshold),
            }
        } else {
            Self {
                threshold: cfg.threshold,
                release: cfg.release,
            }
        }
    }

    /// Whether `score` falls short of the bar for the next frame of
    /// `hysteresis` (the release inside speech, the threshold outside).
    fn misses(&self, score: f32, hysteresis: &Hysteresis) -> bool {
        score
            < if hysteresis.in_speech {
                self.release
            } else {
                self.threshold
            }
    }
}

/// Onset confirmation, hysteresis and hangover over per-frame scores.
#[derive(Debug, Clone, Copy, Default)]
struct Hysteresis {
    in_speech: bool,
    /// Consecutive frames at or above the threshold, outside speech.
    onset: u32,
    /// Consecutive frames under the release score, inside speech.
    below: u32,
}

impl Hysteresis {
    /// `eligible`: the frame passed the level floors. `bar`: the threshold
    /// and release score for this frame.
    fn step(&mut self, eligible: bool, score: f32, bar: Bar) -> Verdict {
        if self.in_speech {
            if eligible && score >= bar.release {
                self.below = 0;
                return Verdict::Voiced;
            }
            self.below += 1;
            if self.below <= HANGOVER_FRAMES {
                return Verdict::Held;
            }
            self.in_speech = false;
            self.below = 0;
        }
        if eligible && score >= bar.threshold {
            self.onset += 1;
            if self.onset >= ONSET_FRAMES {
                self.in_speech = true;
                self.onset = 0;
                return Verdict::Voiced;
            }
        } else {
            self.onset = 0;
        }
        Verdict::Silence
    }
}

/// The background the soft threshold measures a frame against: the
/// quietest pre-emphasized frame of the last 240 ms of sound. Digital
/// silence is skipped, so a stream that starts muted takes its first sound
/// (a noise switching on) as the background once 240 ms of it are in. Every
/// frame of sound counts, speech too, and nothing the detector decides
/// feeds back into it.
#[derive(Debug, Clone, Copy)]
struct Background {
    recent: [f32; BACKGROUND_FRAMES],
    len: usize,
    next: usize,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            recent: [0.0; BACKGROUND_FRAMES],
            len: 0,
            next: 0,
        }
    }
}

impl Background {
    fn follow(&mut self, dbfs: f32) {
        self.recent[self.next] = dbfs;
        self.next = (self.next + 1) % BACKGROUND_FRAMES;
        self.len = (self.len + 1).min(BACKGROUND_FRAMES);
    }

    /// The background, once 240 ms of sound are in.
    fn level(&self) -> Option<f32> {
        (self.len == BACKGROUND_FRAMES)
            .then(|| self.recent.iter().copied().fold(f32::MAX, f32::min))
    }

    /// Whether a frame at `dbfs` (pre-emphasized) stands
    /// [`VadConfig::soft_snr_db`] or more over the background.
    fn cleared_by(&self, dbfs: f32, cfg: &VadConfig) -> bool {
        cfg.soft_snr_db
            .zip(self.level())
            .is_some_and(|(snr, level)| dbfs >= level + snr)
    }
}

/// The running speech level the relative floor hangs from. It follows
/// voiced frames only (fast attack, slow release), so silence and rejected
/// voices never move it.
#[derive(Debug, Clone, Copy, Default)]
struct SpeechLevel {
    dbfs: f32,
    frames: u64,
}

impl SpeechLevel {
    fn follow(&mut self, dbfs: f32) {
        if self.frames == 0 {
            self.dbfs = dbfs;
        } else {
            let ms = if dbfs > self.dbfs {
                LEVEL_ATTACK_MS
            } else {
                LEVEL_RELEASE_MS
            };
            self.dbfs += (1.0 - (-(FRAME_MS as f32) / ms).exp()) * (dbfs - self.dbfs);
        }
        self.frames += 1;
    }

    /// The level under which a frame is rejected, once enough speech set it:
    /// `relative_floor_db` under the level, but never above the RMS test's
    /// threshold. The level follows only accepted frames, so without that
    /// cap a loud burst could set it so high that no later frame passes to
    /// bring it down again.
    fn floor(&self, cfg: &VadConfig) -> Option<f32> {
        let db = cfg.relative_floor_db?;
        (self.frames * FRAME_MS >= RELATIVE_FLOOR_AFTER_MS)
            .then(|| (self.dbfs - db).min(dbfs(cfg.rms_threshold)))
    }
}

/// The speech evidence that arms auto-stop and passes the gate: 160 ms of
/// voiced frames (speech on their own score, not only through the
/// hangover), counted in half frames.
///
/// A frame voiced on the full threshold counts 2 and always stays counted.
/// A frame voiced only through [`VadConfig::soft_threshold`] counts 1, and
/// only while its run lasts: [`SOFT_RUN_GAP_FRAMES`] without a voiced frame
/// forget the soft frames gathered so far. A noise that switches on stands
/// clear of the background for up to 240 ms and earshot scores its onset
/// about 0.5, so at full weight one onset alone could arm auto-stop; and
/// summed over separate onsets (a fan cycling, dishes before the first
/// word) half weight did, over a room whose energy is mostly rumble.
#[derive(Debug, Clone, Copy, Default)]
struct Evidence {
    full_halves: u64,
    /// Soft frames of the current run.
    soft_halves: u32,
    /// Frames since the last voiced one.
    since_voiced: u32,
    /// 160 ms was reached at some point (it stays reached).
    reached: bool,
}

impl Evidence {
    /// Count one frame. `soft`: voiced only through the soft threshold.
    fn add(&mut self, voiced: bool, soft: bool) {
        if voiced {
            self.since_voiced = 0;
            if soft {
                self.soft_halves += 1;
            } else {
                self.full_halves += 2;
            }
        } else {
            self.since_voiced = self.since_voiced.saturating_add(1);
            if self.since_voiced >= SOFT_RUN_GAP_FRAMES {
                self.soft_halves = 0;
            }
        }
        let halves = self.full_halves + u64::from(self.soft_halves);
        self.reached |= halves >= 2 * (MIN_SPEECH_MS / FRAME_MS);
    }
}

/// One frame's decision plus what auto-stop needs beyond it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) decision: FrameDecision,
    /// The stream so far holds 160 ms of speech ([`Evidence`]): what arms
    /// auto-stop and passes the gate.
    pub(crate) enough: bool,
}

/// Streaming speech detector: feed it audio in chunks of any size and it
/// calls back once per completed 16 ms frame.
///
/// With [`Vad::Earshot`] at 16 kHz, per frame, in order:
///
/// 1. earshot scores every frame, quiet ones too, so its context stays
///    intact;
/// 2. the first 10 frames (160 ms) after `new` or `reset` are never speech;
/// 3. a frame under [`VadConfig::min_speech_dbfs`], or more than
///    [`VadConfig::relative_floor_db`] under the running speech level (and
///    under [`VadConfig::rms_threshold`]), is never speech;
/// 4. speech starts after 2 consecutive frames at or above
///    [`VadConfig::threshold`];
/// 5. once in speech, frames at or above [`VadConfig::release`] keep it;
/// 6. speech ends only after 6 frames (96 ms) under the release score;
/// 7. a frame that stands [`VadConfig::soft_snr_db`] over the background
///    (pre-emphasized, the quietest frame of the last 240 ms of sound), as
///    the frame before it did, needs only [`VadConfig::soft_threshold`] in 4
///    and 5. Voiced that way it counts half toward arming auto-stop and
///    passing the gate, and only until 304 ms pass with no voiced frame.
///
/// With [`Vad::Rms`], or at any rate other than 16 kHz (a documented
/// fallback, never a panic), a 16 ms frame is speech when its RMS reaches
/// [`VadConfig::rms_threshold`], with none of the guards above.
///
/// A [`LevelMeter`] runs on everything fed: see [`levels`](Self::levels).
pub struct SpeechDetector {
    cfg: VadConfig,
    frame_len: usize,
    /// `None`: the RMS test decides.
    scorer: Option<Box<dyn FrameScorer>>,
    /// Samples of the frame in progress: capture chunks (800 samples) are
    /// not a multiple of the frame.
    pending: Vec<i16>,
    frames: u64,
    hysteresis: Hysteresis,
    /// The same decision without the level floors: the voice the floors
    /// rejected is what makes a gate verdict "too quiet".
    unfloored: Hysteresis,
    level: SpeechLevel,
    background: Background,
    /// Consecutive frames clear of the background.
    clear_run: u32,
    evidence: Evidence,
    /// The same without the level floors.
    unfloored_evidence: Evidence,
    unfloored_frames: u64,
    unfloored_sum_squares: f64,
    /// The first and the last frame voiced before the level floors and the
    /// evidence count ([`heard_voice`](Self::heard_voice)).
    first_heard: Option<HeardFrame>,
    last_heard: Option<HeardFrame>,
    meter: LevelMeter,
}

impl SpeechDetector {
    /// A detector for a stream at `sample_rate` Hz. earshot runs only at
    /// [`VAD_SAMPLE_RATE`]; any other rate decides with [`Vad::Rms`].
    pub fn new(cfg: VadConfig, sample_rate: u32) -> Self {
        let scorer = (cfg.vad == Vad::Earshot && sample_rate == VAD_SAMPLE_RATE)
            .then(|| earshot::Detector::default_boxed() as Box<dyn FrameScorer>);
        Self::build(cfg, sample_rate, scorer)
    }

    fn build(cfg: VadConfig, sample_rate: u32, scorer: Option<Box<dyn FrameScorer>>) -> Self {
        let frame_len = if scorer.is_some() {
            VAD_FRAME
        } else {
            (sample_rate as u64 * FRAME_MS / 1000).max(1) as usize
        };
        Self {
            cfg,
            frame_len,
            scorer,
            pending: Vec::with_capacity(frame_len),
            frames: 0,
            hysteresis: Hysteresis::default(),
            unfloored: Hysteresis::default(),
            level: SpeechLevel::default(),
            background: Background::default(),
            clear_run: 0,
            evidence: Evidence::default(),
            unfloored_evidence: Evidence::default(),
            unfloored_frames: 0,
            unfloored_sum_squares: 0.0,
            first_heard: None,
            last_heard: None,
            meter: LevelMeter::new(sample_rate),
        }
    }

    /// Feed the next samples of the stream; `on_frame` runs once per frame
    /// they complete, in order. A partial frame waits for the next feed.
    pub fn feed(&mut self, samples: &[i16], mut on_frame: impl FnMut(FrameDecision)) {
        self.feed_steps(samples, |step| on_frame(step.decision));
    }

    pub(crate) fn feed_steps(&mut self, samples: &[i16], mut on_step: impl FnMut(Step)) {
        self.meter.add_samples(samples);
        let mut rest = samples;
        if !self.pending.is_empty() {
            let take = (self.frame_len - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() < self.frame_len {
                return;
            }
            let frame = std::mem::take(&mut self.pending);
            on_step(self.step(&frame));
            self.pending = frame;
            self.pending.clear();
        }
        let mut frames = rest.chunks_exact(self.frame_len);
        for frame in &mut frames {
            on_step(self.step(frame));
        }
        self.pending.extend_from_slice(frames.remainder());
    }

    fn step(&mut self, frame: &[i16]) -> Step {
        let stats = FrameStats::of(frame);
        let index = self.frames;
        self.frames += 1;
        let frame_dbfs = stats.dbfs();
        let mut soft = false;
        let (score, verdict) = match self.scorer.as_mut() {
            None => {
                let speech = stats.rms() >= self.cfg.rms_threshold;
                if speech {
                    self.hear(index, 1.0, frame_dbfs, false);
                    (1.0, Verdict::Voiced)
                } else {
                    (0.0, Verdict::Silence)
                }
            }
            Some(scorer) => {
                let score = scorer.score(frame);
                let emphasized = emphasized_dbfs(frame);
                self.clear_run = if self.background.cleared_by(emphasized, &self.cfg) {
                    self.clear_run.saturating_add(1)
                } else {
                    0
                };
                let clear = self.clear_run >= CLEAR_FRAMES;
                if stats.peak > SILENT_PEAK {
                    self.background.follow(emphasized);
                }
                if index < WARMUP_FRAMES {
                    self.unfloored_evidence.add(false, false);
                    (score, Verdict::Silence)
                } else {
                    let loud = frame_dbfs >= self.cfg.min_speech_dbfs;
                    let above_level = self
                        .level
                        .floor(&self.cfg)
                        .is_none_or(|floor| frame_dbfs >= floor);
                    let bar = Bar::of(&self.cfg, clear);
                    let full = Bar::of(&self.cfg, false);
                    soft = full.misses(score, &self.hysteresis);
                    let unfloored_soft = full.misses(score, &self.unfloored);
                    let verdict = self.hysteresis.step(loud && above_level, score, bar);
                    let unfloored = self.unfloored.step(true, score, bar) == Verdict::Voiced;
                    self.unfloored_evidence.add(unfloored, unfloored_soft);
                    if unfloored {
                        self.hear(index, score, frame_dbfs, unfloored_soft);
                        self.unfloored_frames += 1;
                        self.unfloored_sum_squares += stats.rms() * stats.rms();
                    }
                    if verdict == Verdict::Voiced {
                        self.level.follow(frame_dbfs);
                    }
                    (score, verdict)
                }
            }
        };
        let voiced = verdict == Verdict::Voiced;
        self.evidence.add(voiced, voiced && soft);
        let speech = verdict != Verdict::Silence;
        self.meter.record(stats, speech);
        Step {
            decision: FrameDecision {
                start_sample: index * self.frame_len as u64,
                speech,
                score,
                dbfs: frame_dbfs,
            },
            enough: self.evidence.reached,
        }
    }

    /// Forget the stream: earshot's context, the frame in progress, the
    /// running level and the meter. Warm-up starts again.
    pub fn reset(&mut self) {
        if let Some(scorer) = self.scorer.as_mut() {
            scorer.reset();
        }
        self.pending.clear();
        self.frames = 0;
        self.hysteresis = Hysteresis::default();
        self.unfloored = Hysteresis::default();
        self.level = SpeechLevel::default();
        self.background = Background::default();
        self.clear_run = 0;
        self.evidence = Evidence::default();
        self.unfloored_evidence = Evidence::default();
        self.unfloored_frames = 0;
        self.unfloored_sum_squares = 0.0;
        self.first_heard = None;
        self.last_heard = None;
        self.meter.reset();
    }

    /// Whether any frame so far was speech before the level floors and the
    /// evidence count that arms auto-stop: with [`Vad::Earshot`], a frame
    /// voiced on the full or the soft threshold (warm-up, onset
    /// confirmation and hysteresis still apply), a voice the floors reject
    /// as too quiet included; with [`Vad::Rms`], a frame at or above
    /// [`VadConfig::rms_threshold`]. The earliest sign that someone spoke:
    /// it comes before auto-stop arms, and for a voice too quiet to arm it
    /// at all. A noise onset can count too.
    pub fn heard_voice(&self) -> bool {
        self.first_heard.is_some()
    }

    /// The first frame [`heard_voice`](Self::heard_voice) counted.
    pub fn first_heard(&self) -> Option<HeardFrame> {
        self.first_heard
    }

    /// The latest frame [`heard_voice`](Self::heard_voice) counted: how
    /// long ago anything was heard.
    pub fn last_heard(&self) -> Option<HeardFrame> {
        self.last_heard
    }

    /// Keep frame `index` as heard.
    fn hear(&mut self, index: u64, score: f32, dbfs: f32, soft: bool) {
        let len = self.frame_len as u64;
        let frame = HeardFrame {
            start_sample: index * len,
            len,
            score,
            dbfs,
            soft,
        };
        self.first_heard.get_or_insert(frame);
        self.last_heard = Some(frame);
    }

    /// The meter's figures for the stream so far.
    pub fn levels(&self) -> Levels {
        self.meter.levels()
    }

    /// Samples per analysis frame.
    pub(crate) fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Whether earshot decides (else the RMS test does).
    pub(crate) fn uses_model(&self) -> bool {
        self.scorer.is_some()
    }

    /// The gate's verdict on what was fed so far, past its length and
    /// digital-silence checks: sent with 160 ms of accepted speech at a
    /// level over [`VadConfig::min_speech_dbfs`], else [`GateReason::TooQuiet`]
    /// when the voice the floors rejected would have been enough (its level
    /// in `speech_dbfs` and `snr_db`), else [`GateReason::NoSpeech`].
    pub(crate) fn verdict(&self) -> GateOutcome {
        let mut levels = self.levels();
        let heard_dbfs = self.unfloored_dbfs();
        let quiet = heard_dbfs.is_some_and(|level| level < self.cfg.min_speech_dbfs);
        let reason = if self.evidence.reached && !quiet {
            None
        } else if self.unfloored_evidence.reached {
            // A voice, but the floor rejected it, all of it or on average.
            levels.speech_dbfs = heard_dbfs;
            levels.snr_db = heard_dbfs.zip(levels.noise_dbfs).map(|(s, n)| s - n);
            Some(GateReason::TooQuiet)
        } else {
            Some(GateReason::NoSpeech)
        };
        GateOutcome {
            reason,
            levels,
            first_heard: self.first_heard,
            last_heard: self.last_heard,
        }
    }

    /// Power mean (dBFS) of the frames that would be voiced without the
    /// level floors.
    fn unfloored_dbfs(&self) -> Option<f32> {
        (self.unfloored_frames > 0)
            .then(|| dbfs((self.unfloored_sum_squares / self.unfloored_frames as f64).sqrt()))
    }
}

/// The level of `frame` in dBFS after the pre-emphasis ([`EMPHASIS`]),
/// within the frame: the step from the frame before is left out, so a
/// sound that stops leaves no edge in the frame after it.
fn emphasized_dbfs(frame: &[i16]) -> f32 {
    let sum_squares: f64 = frame
        .windows(2)
        .map(|pair| {
            let y = pair[1] as f64 - EMPHASIS * pair[0] as f64;
            y * y
        })
        .sum();
    let n = frame.len().saturating_sub(1).max(1) as f64;
    dbfs((sum_squares / n).sqrt() / i16::MAX as f64)
}

/// What [`speech_gate`] decided, with the levels it measured on the way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateOutcome {
    /// Why to skip the buffer; `None` sends it.
    pub reason: Option<GateReason>,
    /// The buffer's levels. With [`GateReason::TooQuiet`], `speech_dbfs`
    /// (and `snr_db`) describe the voice the level floor rejected.
    pub levels: Levels,
    /// The first and the latest frame the detector heard a voice in
    /// ([`SpeechDetector::first_heard`], [`SpeechDetector::last_heard`]);
    /// `None` when it did not run (digital silence, too short).
    pub first_heard: Option<HeardFrame>,
    pub last_heard: Option<HeardFrame>,
}

/// Decide whether a recorded buffer carries speech worth sending to a
/// transcription backend.
///
/// With [`Vad::Earshot`] at 16 kHz: `Empty`, `Invalid` and `TooShort` as
/// [`audio_gate_reason`] has them; `Silent` when the peak is at or under 4
/// (digital silence), checked before the model runs, so a muted mic never
/// pays for it; then the detector runs over the buffer and the buffer passes
/// with at least 160 ms of accepted speech at a speech level of at least
/// [`VadConfig::min_speech_dbfs`]. `TooQuiet` means the model heard a voice
/// the level floor rejected, `NoSpeech` that there was sound but no voice.
///
/// With [`Vad::Rms`], or any rate other than 16 kHz, `reason` is exactly what
/// [`audio_gate_reason`] returns with [`VadConfig::rms_threshold`].
pub fn speech_gate(
    samples: &[i16],
    sample_rate: u32,
    min_duration_ms: u64,
    cfg: &VadConfig,
) -> GateOutcome {
    if cfg.vad == Vad::Rms || sample_rate != VAD_SAMPLE_RATE {
        let reason = audio_gate_reason(samples, sample_rate, min_duration_ms, cfg.rms_threshold);
        let mut detector = SpeechDetector::new(
            VadConfig {
                vad: Vad::Rms,
                ..*cfg
            },
            sample_rate,
        );
        detector.feed(samples, |_| {});
        return GateOutcome {
            reason,
            levels: detector.levels(),
            first_heard: detector.first_heard(),
            last_heard: detector.last_heard(),
        };
    }
    gate_with(
        samples,
        min_duration_ms,
        SpeechDetector::new(*cfg, sample_rate),
    )
}

/// The earshot side of [`speech_gate`], on a 16 kHz `detector`.
fn gate_with(samples: &[i16], min_duration_ms: u64, mut detector: SpeechDetector) -> GateOutcome {
    let early = duration_reason(samples, VAD_SAMPLE_RATE, min_duration_ms).or_else(|| {
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        (peak <= SILENT_PEAK).then_some(GateReason::Silent)
    });
    if let Some(reason) = early {
        // Measured without the model: nothing here is speech.
        let mut meter = LevelMeter::new(VAD_SAMPLE_RATE);
        meter.add_samples(samples);
        for frame in samples.chunks_exact(VAD_FRAME) {
            meter.add_frame(frame, false);
        }
        return GateOutcome {
            reason: Some(reason),
            levels: meter.levels(),
            first_heard: None,
            last_heard: None,
        };
    }

    detector.feed(samples, |_| {});
    detector.verdict()
}

/// Per-frame speech flags for a 16 kHz stream: one flag per `frame_len`
/// samples (a `frame_len` of 0 counts as 1), the last frame possibly
/// partial. It streams, so a long track can be scanned without loading it.
///
/// With [`Vad::Earshot`] a frame is active when any 16 ms speech frame of a
/// [`SpeechDetector`] overlaps it. With [`Vad::Rms`] it is active when
/// `rms_energy(frame) >= rms_threshold`, the 0.1.31 rule bit for bit.
pub struct SpeechMask {
    frame_len: usize,
    total: u64,
    flags: Vec<bool>,
    /// [`Vad::Rms`]: the samples of the mask frame in progress.
    rms_frame: Vec<i16>,
    /// Decides with earshot; with [`Vad::Rms`] it only meters.
    detector: SpeechDetector,
}

impl SpeechMask {
    pub fn new(cfg: VadConfig, frame_len: usize) -> Self {
        Self::with_detector(SpeechDetector::new(cfg, VAD_SAMPLE_RATE), frame_len)
    }

    fn with_detector(detector: SpeechDetector, frame_len: usize) -> Self {
        let frame_len = frame_len.max(1);
        let rms_frame = if detector.uses_model() {
            Vec::new()
        } else {
            Vec::with_capacity(frame_len)
        };
        Self {
            frame_len,
            total: 0,
            flags: Vec::new(),
            rms_frame,
            detector,
        }
    }

    /// Feed the next samples of the stream.
    pub fn feed(&mut self, samples: &[i16]) {
        self.total += samples.len() as u64;
        let frame_len = self.frame_len;
        let flags = &mut self.flags;
        if self.detector.uses_model() {
            self.detector.feed(samples, |decision| {
                if !decision.speech {
                    return;
                }
                let first = (decision.start_sample / frame_len as u64) as usize;
                let last =
                    ((decision.start_sample + VAD_FRAME as u64 - 1) / frame_len as u64) as usize;
                if flags.len() <= last {
                    flags.resize(last + 1, false);
                }
                flags[first..=last].fill(true);
            });
            return;
        }
        self.detector.feed(samples, |_| {});
        let threshold = self.detector.cfg.rms_threshold;
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (frame_len - self.rms_frame.len()).min(rest.len());
            self.rms_frame.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.rms_frame.len() == frame_len {
                flags.push(rms_energy(&self.rms_frame) >= threshold);
                self.rms_frame.clear();
            }
        }
    }

    /// The meter's figures for the stream so far.
    pub fn levels(&self) -> Levels {
        self.detector.levels()
    }

    /// The flags: `total.div_ceil(frame_len)` of them.
    pub fn finish(mut self) -> Vec<bool> {
        if !self.rms_frame.is_empty() {
            let threshold = self.detector.cfg.rms_threshold;
            self.flags.push(rms_energy(&self.rms_frame) >= threshold);
        }
        self.flags
            .resize(self.total.div_ceil(self.frame_len as u64) as usize, false);
        self.flags
    }
}

/// [`SpeechMask`] over a buffer already in memory.
pub fn speech_mask(samples: &[i16], cfg: &VadConfig, frame_len: usize) -> Vec<bool> {
    let mut mask = SpeechMask::new(*cfg, frame_len);
    mask.feed(samples);
    mask.finish()
}

// Detectors run on tokio tasks and on recording writer threads.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<SpeechDetector>();
    assert_send::<SpeechMask>();
    assert_send::<crate::AutoStopDetector>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AutoStopDetector;

    /// Scores read from a script, one per frame, then 0.0. `reset` replays it.
    struct Script {
        scores: Vec<f32>,
        next: usize,
    }

    impl FrameScorer for Script {
        fn score(&mut self, _frame: &[i16]) -> f32 {
            let score = self.scores.get(self.next).copied().unwrap_or(0.0);
            self.next += 1;
            score
        }

        fn reset(&mut self) {
            self.next = 0;
        }
    }

    /// A scorer that must never run.
    struct Unreachable;

    impl FrameScorer for Unreachable {
        fn score(&mut self, _frame: &[i16]) -> f32 {
            panic!("the model ran");
        }

        fn reset(&mut self) {}
    }

    fn scripted(cfg: VadConfig, scores: Vec<f32>) -> SpeechDetector {
        SpeechDetector::build(
            cfg,
            VAD_SAMPLE_RATE,
            Some(Box::new(Script { scores, next: 0 })),
        )
    }

    fn close() -> VadConfig {
        VadConfig::close(Vad::Earshot)
    }

    fn open() -> VadConfig {
        VadConfig::open(Vad::Earshot)
    }

    /// `frames` frames of alternating `±a` at `level` dBFS (RMS `a`, no DC).
    fn at_level(level: f32, frames: usize) -> Vec<i16> {
        let a = (10f64.powf(level as f64 / 20.0) * i16::MAX as f64).round() as i16;
        (0..frames * VAD_FRAME)
            .map(|i| if i % 2 == 0 { a } else { -a })
            .collect()
    }

    fn run(detector: &mut SpeechDetector, samples: &[i16]) -> Vec<FrameDecision> {
        let mut out = Vec::new();
        detector.feed(samples, |d| out.push(d));
        out
    }

    fn flags(decisions: &[FrameDecision]) -> Vec<bool> {
        decisions.iter().map(|d| d.speech).collect()
    }

    /// Indices of the speech frames.
    fn speech_at(decisions: &[FrameDecision]) -> Vec<usize> {
        (0..decisions.len())
            .filter(|&i| decisions[i].speech)
            .collect()
    }

    fn scores(parts: &[(f32, usize)]) -> Vec<f32> {
        parts
            .iter()
            .flat_map(|&(score, n)| std::iter::repeat_n(score, n))
            .collect()
    }

    const WARM: usize = WARMUP_FRAMES as usize;

    #[test]
    fn warm_up_frames_are_never_speech() {
        let mut detector = scripted(close(), vec![1.0; 20]);
        let decisions = run(&mut detector, &at_level(-30.0, 20));
        // 10 warm-up frames, one onset frame, then speech.
        assert_eq!(speech_at(&decisions), (WARM + 1..20).collect::<Vec<_>>());
        assert_eq!(decisions[WARM + 1].start_sample, (WARM as u64 + 1) * 256);
    }

    #[test]
    fn onset_needs_two_frames_in_a_row() {
        let script = scores(&[
            (0.0, WARM + 2),
            (0.9, 1),
            (0.0, 2),
            (0.9, 1),
            (0.0, 1),
            (0.9, 3),
        ]);
        let mut detector = scripted(close(), script);
        let decisions = run(&mut detector, &at_level(-30.0, 40));
        // Lone blips at 12 and 15 never start speech; the pair at 17 and 18
        // does, on its second frame, then 19, then the 6-frame hangover.
        assert_eq!(speech_at(&decisions), (18..=25).collect::<Vec<_>>());
    }

    #[test]
    fn hysteresis_keeps_speech_above_release() {
        // The profiles release at the threshold; a band needs a custom one.
        let banded = VadConfig {
            threshold: 0.6,
            release: 0.4,
            ..close()
        };
        let mut detector = scripted(banded, scores(&[(0.0, WARM), (0.9, 2), (0.45, 30)]));
        let decisions = run(&mut detector, &at_level(-30.0, WARM + 32));
        assert_eq!(
            speech_at(&decisions),
            (WARM + 1..WARM + 32).collect::<Vec<_>>()
        );

        // Between release and threshold never starts speech.
        let mut detector = scripted(banded, scores(&[(0.0, WARM), (0.45, 30)]));
        let decisions = run(&mut detector, &at_level(-30.0, WARM + 30));
        assert!(speech_at(&decisions).is_empty());
    }

    #[test]
    fn hangover_bridges_six_frames() {
        // A 6-frame dip is bridged.
        let script = scores(&[(0.0, WARM), (0.9, 4), (0.39, 6), (0.9, 4)]);
        let mut detector = scripted(close(), script);
        let decisions = run(&mut detector, &at_level(-30.0, WARM + 14));
        assert_eq!(
            speech_at(&decisions),
            (WARM + 1..WARM + 14).collect::<Vec<_>>()
        );

        // A 7-frame dip ends speech on its seventh frame.
        let script = scores(&[(0.0, WARM), (0.9, 4), (0.1, 7), (0.9, 1)]);
        let mut detector = scripted(close(), script);
        let decisions = run(&mut detector, &at_level(-30.0, WARM + 12));
        assert_eq!(
            speech_at(&decisions),
            (WARM + 1..WARM + 10).collect::<Vec<_>>()
        );
    }

    #[test]
    fn absolute_floor_rejects_quiet_frames() {
        // -70 dBFS: under close's -65, over open's -80.
        let mut detector = scripted(close(), vec![1.0; 40]);
        assert!(speech_at(&run(&mut detector, &at_level(-70.0, 40))).is_empty());

        let mut detector = scripted(open(), vec![1.0; 40]);
        assert_eq!(run(&mut detector, &at_level(-70.0, 40)).len(), 40);
        let mut detector = scripted(open(), vec![1.0; 40]);
        assert_eq!(
            speech_at(&run(&mut detector, &at_level(-70.0, 40))).len(),
            40 - WARM - 1
        );

        let mut detector = scripted(close(), vec![1.0; 40]);
        assert_eq!(
            speech_at(&run(&mut detector, &at_level(-62.0, 40))).len(),
            40 - WARM - 1
        );
    }

    #[test]
    fn relative_floor_rejects_quiet_background_voices() {
        // The user at -30 dBFS for 480 ms, then a voice 25 dB under them.
        let mut samples = at_level(-30.0, WARM + 30);
        samples.extend(at_level(-55.0, 40));
        let mut detector = scripted(close(), vec![1.0; WARM + 70]);
        let decisions = run(&mut detector, &samples);
        let after = WARM + 30 + HANGOVER_FRAMES as usize;
        assert!(
            flags(&decisions[after..]).iter().all(|&s| !s),
            "the background voice must not be speech"
        );

        // 15 dB under the user is still the user (a soft passage).
        let mut samples = at_level(-30.0, WARM + 30);
        samples.extend(at_level(-45.0, 40));
        let mut detector = scripted(close(), vec![1.0; WARM + 70]);
        let decisions = run(&mut detector, &samples);
        assert!(flags(&decisions[WARM + 1..]).iter().all(|&s| s));

        // Open profile: no relative floor.
        let mut samples = at_level(-30.0, WARM + 30);
        samples.extend(at_level(-55.0, 40));
        let mut detector = scripted(open(), vec![1.0; WARM + 70]);
        let decisions = run(&mut detector, &samples);
        assert!(flags(&decisions[WARM + 1..]).iter().all(|&s| s));
    }

    /// The relative floor never rises above the RMS test's threshold
    /// (-50.5 dBFS): after a 400 ms burst at -15 dBFS (a laugh at the mic),
    /// the user's speech at -40, 25 dB under it, is speech; a voice at -55
    /// after it is still rejected.
    #[test]
    fn relative_floor_stays_under_the_rms_threshold() {
        let burst = 25;
        let mut samples = at_level(-15.0, WARM + burst);
        samples.extend(at_level(-40.0, 60));
        let mut detector = scripted(close(), vec![1.0; WARM + burst + 60]);
        let decisions = run(&mut detector, &samples);
        assert!(
            flags(&decisions[WARM + 1..]).iter().all(|&s| s),
            "the speech after the burst is speech"
        );

        let mut samples = at_level(-15.0, WARM + burst);
        samples.extend(at_level(-55.0, 40));
        let mut detector = scripted(close(), vec![1.0; WARM + burst + 40]);
        let decisions = run(&mut detector, &samples);
        let after = WARM + burst + HANGOVER_FRAMES as usize;
        assert!(flags(&decisions[after..]).iter().all(|&s| !s));
    }

    #[test]
    fn relative_floor_waits_for_300_ms_of_speech() {
        // 192 ms of the user (frames 11 to 22) does not set the level yet, so
        // the quieter voice after it is speech until 300 ms of speech in all
        // (frame 29), then rejected: frames 30 to 35 are the hangover.
        let mut samples = at_level(-30.0, WARM + 1 + 12);
        samples.extend(at_level(-55.0, 40));
        let mut detector = scripted(close(), vec![1.0; WARM + 53]);
        let decisions = run(&mut detector, &samples);
        assert_eq!(speech_at(&decisions), (WARM + 1..36).collect::<Vec<_>>());

        // A quiet voice alone (over the absolute floor) is speech.
        let mut detector = scripted(close(), vec![1.0; 40]);
        let decisions = run(&mut detector, &at_level(-55.0, 40));
        assert!(flags(&decisions[WARM + 1..]).iter().all(|&s| s));
    }

    fn concat(parts: &[(f32, usize)]) -> Vec<i16> {
        parts
            .iter()
            .flat_map(|&(level, frames)| at_level(level, frames))
            .collect()
    }

    /// Frames 32 to 39: the -40 dBFS burst after its first two frames (one
    /// to stand clear twice, one to confirm the onset); 40 to 45: the
    /// hangover.
    #[test]
    fn soft_threshold_applies_clear_of_the_background() {
        let burst = concat(&[(-60.0, 30), (-40.0, 10), (-60.0, 30)]);
        let mut detector = scripted(close(), vec![0.3; 70]);
        assert_eq!(
            speech_at(&run(&mut detector, &burst)),
            (32..46).collect::<Vec<_>>()
        );

        // The same score at the background's level is not speech.
        let mut detector = scripted(close(), vec![0.3; 70]);
        assert!(speech_at(&run(&mut detector, &at_level(-40.0, 70))).is_empty());

        // `soft_snr_db: None` turns the rule off.
        let off = VadConfig {
            soft_snr_db: None,
            ..close()
        };
        let mut detector = scripted(off, vec![0.3; 70]);
        assert!(speech_at(&run(&mut detector, &burst)).is_empty());
    }

    /// A level that rises and stays is the background 240 ms later: the
    /// soft frames stop at frame 44, the hangover holds 45 to 50, and the
    /// 13 soft frames do not arm auto-stop.
    #[test]
    fn a_level_step_stands_clear_for_240_ms() {
        let step = concat(&[(-60.0, 30), (-40.0, 60)]);
        let mut detector = scripted(close(), vec![0.3; 90]);
        assert_eq!(
            speech_at(&run(&mut detector, &step)),
            (32..51).collect::<Vec<_>>()
        );

        let mut stop = AutoStopDetector::from_detector(scripted(close(), vec![0.3; 90]), 2000);
        assert_eq!(fires_at(&mut stop, &step), None);
        assert!(!stop.has_speech());
    }

    #[test]
    fn clicks_of_one_or_two_frames_never_use_the_soft_threshold() {
        for frames in [1, 2] {
            let click = concat(&[(-60.0, 30), (-20.0, frames), (-60.0, 30)]);
            let mut detector = scripted(close(), vec![0.3; 70]);
            assert!(
                speech_at(&run(&mut detector, &click)).is_empty(),
                "a {frames}-frame click"
            );
        }
    }

    /// A stream that starts in digital silence takes its first sound as the
    /// background once 240 ms of it are in, so a noise switching on is
    /// never clear of it.
    #[test]
    fn the_background_waits_for_240_ms_of_sound() {
        let mut samples = vec![0i16; 30 * VAD_FRAME];
        samples.extend(at_level(-40.0, 40));
        let mut detector = scripted(close(), vec![0.3; 70]);
        assert!(speech_at(&run(&mut detector, &samples)).is_empty());
    }

    /// Two -40 dBFS bursts of `frames` frames over -60 dBFS, `pause` frames
    /// apart, each voiced only through the soft threshold from its third
    /// frame.
    fn soft_bursts(frames: usize, pause: usize) -> Vec<i16> {
        concat(&[
            (-60.0, 30),
            (-40.0, frames),
            (-60.0, pause),
            (-40.0, frames),
            (-60.0, 30),
        ])
    }

    fn frames(samples: &[i16]) -> usize {
        samples.len() / VAD_FRAME
    }

    /// The gate's reason, and whether auto-stop armed, on `samples` scored
    /// by `script`.
    fn evidence_of(samples: &[i16], script: Vec<f32>) -> (Option<GateReason>, bool) {
        let reason = gate(samples, scorer(script.clone())).reason;
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 2000);
        fires_at(&mut stop, samples);
        (reason, stop.has_speech())
    }

    /// `samples` scored 0.3 throughout: only frames clear of the background
    /// are voiced, and only through the soft threshold.
    fn soft_evidence(samples: &[i16]) -> (Option<GateReason>, bool) {
        evidence_of(samples, vec![0.3; frames(samples)])
    }

    /// 20 soft frames (320 ms) in one run of speech do what 10 strong ones
    /// do; 18 do not. A 192 ms pause (12 frames, the hangover 6 of them)
    /// keeps the run.
    #[test]
    fn soft_frames_count_half_toward_the_gate_and_arming() {
        assert_eq!(soft_evidence(&soft_bursts(12, 12)), (None, true));
        assert_eq!(
            soft_evidence(&soft_bursts(11, 12)),
            (Some(GateReason::NoSpeech), false)
        );
        // One burst on a strong score is enough: 11 voiced frames.
        let strong = scorer(scores(&[(0.0, 30), (0.9, 12)]));
        let one_burst = concat(&[(-60.0, 30), (-40.0, 12)]);
        assert_eq!(gate(&one_burst, strong).reason, None);
    }

    /// Soft frames separated by 304 ms with no voiced frame (19 frames, a
    /// 30-frame pause here) do not add up: separate noise onsets before the
    /// first word never arm auto-stop or pass the gate. Strong frames still
    /// add up across the same pause.
    #[test]
    fn soft_frames_count_only_within_their_run() {
        assert_eq!(
            soft_evidence(&soft_bursts(12, 30)),
            (Some(GateReason::NoSpeech), false)
        );
        // Four separate soft bursts, 40 soft frames in all.
        let four = concat(&[
            (-60.0, 30),
            (-40.0, 12),
            (-60.0, 30),
            (-40.0, 12),
            (-60.0, 30),
            (-40.0, 12),
            (-60.0, 30),
            (-40.0, 12),
            (-60.0, 30),
        ]);
        assert_eq!(soft_evidence(&four), (Some(GateReason::NoSpeech), false));
        // Strong bursts of 6 voiced frames each: 12 across the pause.
        let script = scores(&[(0.0, 30), (0.9, 7), (0.0, 30), (0.9, 7), (0.0, 30)]);
        assert_eq!(evidence_of(&soft_bursts(7, 30), script), (None, true));
        // A 15-frame pause keeps the run: 17 frames without a voiced one
        // (the hangover among them) before the next burst's first voiced
        // frame.
        assert_eq!(soft_evidence(&soft_bursts(12, 15)), (None, true));
    }

    /// A voice the level floor rejects is heard all the same, before
    /// anything is speech or armed, and so is a voice before it arms.
    /// Noise is not.
    #[test]
    fn heard_voice_counts_any_frame_before_the_floors_and_arming() {
        // -70 dBFS, under close's -65: no speech, never armed, gate too
        // quiet, but heard from the onset frame on.
        let quiet = at_level(-70.0, 60);
        let mut detector = scripted(close(), vec![1.0; 60]);
        let mut heard_at = None;
        let mut speech = 0;
        for (i, frame) in quiet.chunks(VAD_FRAME).enumerate() {
            detector.feed(frame, |d| speech += d.speech as usize);
            if heard_at.is_none() && detector.heard_voice() {
                heard_at = Some(i);
            }
        }
        assert_eq!((heard_at, speech), (Some(WARM + 1), 0));
        let mut stop = AutoStopDetector::from_detector(scripted(close(), vec![1.0; 60]), 2000);
        fires_at(&mut stop, &quiet);
        assert!(stop.heard_voice());
        assert!(!stop.has_speech());
        let outcome = gate(&quiet, scorer(vec![1.0; 60]));
        assert_eq!(outcome.reason, Some(GateReason::TooQuiet));
        // The gate reports the frames it heard, as the detector keeps them.
        let onset = ((WARM + 1) * VAD_FRAME) as u64;
        assert_eq!(outcome.first_heard.map(|f| f.start_sample), Some(onset));
        assert_eq!(
            outcome.last_heard.map(|f| f.start_sample),
            Some(59 * VAD_FRAME as u64)
        );

        // Three strong frames: heard on the second, never armed.
        let script = scores(&[(0.0, WARM), (0.9, 3)]);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 2000);
        let samples = at_level(-30.0, WARM + 40);
        assert!(!stop.feed(&samples[..(WARM + 1) * VAD_FRAME]));
        assert!(!stop.heard_voice(), "one frame is not an onset");
        stop.feed(&samples[(WARM + 1) * VAD_FRAME..]);
        assert!(stop.heard_voice());
        assert!(!stop.has_speech());
        // The onset frame first, the last voiced one last (the hangover
        // after it is not heard), each on the full threshold.
        let heard = |at: usize| HeardFrame {
            start_sample: (at * VAD_FRAME) as u64,
            len: VAD_FRAME as u64,
            score: 0.9,
            dbfs: -30.0,
            soft: false,
        };
        let round = |f: Option<HeardFrame>| {
            f.map(|f| HeardFrame {
                dbfs: f.dbfs.round(),
                ..f
            })
        };
        assert_eq!(round(stop.first_heard()), Some(heard(WARM + 1)));
        assert_eq!(round(stop.last_heard()), Some(heard(WARM + 2)));
        assert_eq!(
            heard(WARM + 2).end_sample(),
            ((WARM + 3) * VAD_FRAME) as u64
        );

        // Soft frames clear of the background count too, as soft.
        let mut detector = scripted(close(), vec![0.3; 70]);
        detector.feed(&soft_bursts(4, 30)[..40 * VAD_FRAME], |_| {});
        assert!(detector.heard_voice());
        assert!(detector.first_heard().is_some_and(|f| f.soft));
        assert!(detector.last_heard().is_some_and(|f| f.soft));

        // A score under both thresholds, and warm-up, never do.
        let mut detector = scripted(close(), vec![0.2; 60]);
        detector.feed(&at_level(-30.0, 60), |_| {});
        assert!(!detector.heard_voice());
        let mut detector = scripted(close(), vec![1.0; WARM]);
        detector.feed(&at_level(-30.0, WARM), |_| {});
        assert!(!detector.heard_voice());

        // Reset forgets it.
        let mut detector = scripted(close(), vec![1.0; 30]);
        detector.feed(&at_level(-30.0, 30), |_| {});
        assert!(detector.heard_voice());
        detector.reset();
        assert!(!detector.heard_voice());
        assert_eq!(
            (detector.first_heard(), detector.last_heard()),
            (None, None)
        );
    }

    /// A stream the gate would call too quiet says so, with the level of
    /// the voice the floor rejected; a loud one or noise does not.
    #[test]
    fn auto_stop_names_a_too_quiet_stream() {
        let quiet = at_level(-70.0, 60);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), vec![1.0; 60]), 2000);
        stop.feed(&quiet);
        let levels = stop.too_quiet().expect("too quiet");
        assert!(
            (levels.speech_dbfs.unwrap() + 70.0).abs() < 0.5,
            "{levels:?}"
        );
        assert_eq!(levels, gate(&quiet, scorer(vec![1.0; 60])).levels);

        let mut stop = AutoStopDetector::from_detector(scripted(close(), vec![1.0; 60]), 2000);
        stop.feed(&at_level(-30.0, 60));
        assert_eq!(stop.too_quiet(), None);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), vec![0.1; 60]), 2000);
        stop.feed(&at_level(-30.0, 60));
        assert_eq!(stop.too_quiet(), None);
        let mut stop = AutoStopDetector::with_vad(VadConfig::close(Vad::Rms), 2000, 16_000);
        stop.feed(&at_level(-70.0, 60));
        assert_eq!(stop.too_quiet(), None);
    }

    /// With the RMS test any 16 ms frame at or over the threshold is heard,
    /// even in a 50 ms chunk whose own RMS is under it (so it does not arm).
    #[test]
    fn heard_voice_with_rms_is_any_frame_over_the_threshold() {
        // -48 dBFS (RMS 0.004 >= 0.003) for one frame, then digital silence:
        // the chunk's RMS is 0.0023.
        let mut chunk = at_level(-48.0, 1);
        chunk.resize(800, 0);
        for cfg in [close(), open()] {
            let mut stop = AutoStopDetector::with_vad(
                VadConfig {
                    vad: Vad::Rms,
                    ..cfg
                },
                2000,
                VAD_SAMPLE_RATE,
            );
            stop.feed(&vec![0; 800]);
            assert!(!stop.heard_voice());
            stop.feed(&chunk);
            assert!(stop.heard_voice());
            assert!(!stop.has_speech());
            // Frame 3 (samples 768 to 1024) holds 224 of the loud samples,
            // enough on its own, scored 1.0.
            let heard = stop.first_heard().unwrap();
            assert_eq!((heard.start_sample, heard.score), (768, 1.0));
            assert_eq!(stop.last_heard(), Some(heard));
        }
        let mut stop = AutoStopDetector::new(SILENCE_RMS_THRESHOLD, 2000, VAD_SAMPLE_RATE);
        stop.feed(&at_level(-30.0, 4));
        assert!(stop.has_speech() && stop.heard_voice());
        stop.reset();
        assert!(!stop.heard_voice());
    }

    /// Frames of varying level with varying scores, plus a partial frame.
    fn varied() -> (Vec<i16>, Vec<f32>) {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut samples = Vec::new();
        let mut script = Vec::new();
        for _ in 0..300 {
            let level = -75.0 + (next() % 60) as f32;
            samples.extend(at_level(level, 1));
            script.push((next() % 1000) as f32 / 1000.0);
        }
        samples.extend(at_level(-30.0, 1).into_iter().take(100));
        (samples, script)
    }

    #[test]
    fn chunk_sizes_do_not_change_decisions() {
        let (samples, script) = varied();
        let mut whole = scripted(close(), script.clone());
        let expected = run(&mut whole, &samples);
        assert_eq!(expected.len(), 300);
        assert!(expected.iter().any(|d| d.speech) && expected.iter().any(|d| !d.speech));
        for size in [1, 255, 256, 257, 800] {
            let mut detector = scripted(close(), script.clone());
            let mut got = Vec::new();
            for chunk in samples.chunks(size) {
                detector.feed(chunk, |d| got.push(d));
            }
            assert_eq!(got, expected, "chunk size {size}");
            assert_eq!(detector.levels(), whole.levels(), "chunk size {size}");
        }
    }

    #[test]
    fn reset_starts_over() {
        let (samples, script) = varied();
        let mut detector = scripted(close(), script);
        let first = run(&mut detector, &samples);
        let first_levels = detector.levels();
        detector.reset();
        assert_eq!(detector.levels().duration_ms, 0);
        // Same input after reset: same decisions, from sample 0, warm-up
        // included, and the partial frame was dropped.
        assert_eq!(run(&mut detector, &samples), first);
        assert_eq!(detector.levels(), first_levels);
    }

    /// Feed `samples` frame by frame; the index of the frame on which
    /// auto-stop first fires.
    fn fires_at(stop: &mut AutoStopDetector, samples: &[i16]) -> Option<usize> {
        samples.chunks(VAD_FRAME).position(|frame| stop.feed(frame))
    }

    #[test]
    fn auto_stop_arms_after_160_ms_of_speech() {
        let silence_after = |strong: usize| {
            let script = scores(&[(0.0, WARM), (0.9, strong)]);
            let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 160);
            let samples = at_level(-30.0, WARM + strong + 200);
            (fires_at(&mut stop, &samples), stop.has_speech())
        };
        // 10 strong frames: one onset frame and 144 ms voiced.
        assert_eq!(silence_after(10), (None, false));
        // 11 strong frames: 160 ms voiced.
        let (fired, armed) = silence_after(11);
        assert!(armed);
        assert!(fired.is_some());

        // A 130 ms noise onset: 8 strong frames, 7 voiced, and the hangover
        // after them does not count.
        assert_eq!(silence_after(8), (None, false));

        // Arming counts speech in total: two 112 ms bursts.
        let script = scores(&[(0.0, WARM), (0.9, 8), (0.0, 20), (0.9, 8)]);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 160);
        let samples = at_level(-30.0, WARM + 36);
        assert_eq!(fires_at(&mut stop, &samples), None);
        assert!(stop.has_speech());
    }

    #[test]
    fn auto_stop_fires_when_the_timeout_is_reached() {
        // Speech on frames 11 to 29, the hangover to 35, then silence from
        // 36: 2 s (125 frames) of it is reached on frame 160.
        let script = scores(&[(0.0, WARM), (0.9, 20)]);
        let samples = at_level(-30.0, 400);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script.clone()), 2000);
        assert_eq!(fires_at(&mut stop, &samples), Some(160));
        // It stays fired while the silence lasts.
        assert!(stop.feed(&at_level(-30.0, 1)));

        // In 800-sample chunks it fires in the chunk that completes frame 160.
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 2000);
        let chunk = samples.chunks(800).position(|c| stop.feed(c));
        assert_eq!(chunk, Some((161 * VAD_FRAME - 1) / 800));
    }

    #[test]
    fn auto_stop_never_fires_with_zero_timeout() {
        let script = scores(&[(0.0, WARM), (0.9, 40)]);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 0);
        assert_eq!(fires_at(&mut stop, &at_level(-30.0, 2000)), None);
        assert!(stop.has_speech());
    }

    #[test]
    fn auto_stop_reset_disarms() {
        let script = scores(&[(0.0, WARM), (0.9, 20)]);
        let mut stop = AutoStopDetector::from_detector(scripted(close(), script), 2000);
        assert!(fires_at(&mut stop, &at_level(-30.0, 200)).is_some());
        stop.reset();
        assert!(!stop.has_speech());
        assert_eq!(stop.levels().duration_ms, 0);
    }

    fn gate(samples: &[i16], scorer: Box<dyn FrameScorer>) -> GateOutcome {
        gate_with(
            samples,
            300,
            SpeechDetector::build(close(), VAD_SAMPLE_RATE, Some(scorer)),
        )
    }

    fn scorer(scores: Vec<f32>) -> Box<dyn FrameScorer> {
        Box::new(Script { scores, next: 0 })
    }

    #[test]
    fn gate_reasons() {
        let cfg = close();
        assert_eq!(
            speech_gate(&[], 16_000, 300, &cfg).reason,
            Some(GateReason::Empty)
        );
        assert_eq!(
            speech_gate(&at_level(-30.0, 60), 0, 300, &cfg).reason,
            Some(GateReason::Invalid)
        );
        assert_eq!(
            gate(&at_level(-30.0, 6), Box::new(Unreachable)).reason,
            Some(GateReason::TooShort)
        );

        // Digital silence: decided from the peak, without the model.
        let muted = gate(&at_level(-80.0, 62), Box::new(Unreachable));
        assert_eq!(muted.reason, Some(GateReason::Silent));
        assert_eq!(muted.levels.duration_ms, 992);
        assert!(muted.levels.peak_dbfs < -78.0);

        // A voice at -30 dBFS: sent.
        let speech = gate(&at_level(-30.0, 62), scorer(vec![1.0; 62]));
        assert_eq!(speech.reason, None);
        assert!((speech.levels.speech_dbfs.unwrap() + 30.0).abs() < 0.1);

        // The same voice at -70 dBFS: too quiet, with its level.
        let quiet = gate(&at_level(-70.0, 62), scorer(vec![1.0; 62]));
        assert_eq!(quiet.reason, Some(GateReason::TooQuiet));
        assert!((quiet.levels.speech_dbfs.unwrap() + 70.0).abs() < 0.5);

        // Loud sound without a voice.
        let noise = gate(&at_level(-30.0, 62), scorer(vec![0.1; 62]));
        assert_eq!(noise.reason, Some(GateReason::NoSpeech));
        assert_eq!(noise.levels.speech_dbfs, None);
        assert!((noise.levels.noise_dbfs.unwrap() + 30.0).abs() < 0.2);

        // 144 ms of voice is not enough; 160 ms is.
        let short = scores(&[(0.0, WARM), (1.0, 10)]);
        assert_eq!(
            gate(&at_level(-30.0, 62), scorer(short)).reason,
            Some(GateReason::NoSpeech)
        );
        let enough = scores(&[(0.0, WARM), (1.0, 11)]);
        assert_eq!(gate(&at_level(-30.0, 62), scorer(enough)).reason, None);
    }

    #[test]
    fn gate_labels() {
        assert_eq!(GateReason::TooQuiet.as_str(), "too quiet");
        assert_eq!(GateReason::NoSpeech.as_str(), "without speech");
    }

    #[test]
    fn mask_marks_frames_a_speech_frame_overlaps() {
        // 5100 samples: three 100 ms frames and a partial one of 300.
        // Speech starts on 16 ms frame 17 (4352..4608, mask frame 2) and the
        // hangover holds frame 18 (4608..4864, mask frames 2 and 3).
        let script = scores(&[(0.0, 16), (0.9, 2)]);
        let detector = scripted(close(), script);
        let mut mask = SpeechMask::with_detector(detector, 1600);
        let samples = at_level(-30.0, 20);
        mask.feed(&samples[..2000]);
        mask.feed(&samples[2000..5100]);
        // Hangover frames 18 to 23; only 18 is complete in 5100 samples.
        assert_eq!(mask.finish(), vec![false, false, true, true]);

        let mask = SpeechMask::with_detector(scripted(close(), vec![]), 1600);
        assert_eq!(mask.finish(), Vec::<bool>::new());
        let mut mask = SpeechMask::with_detector(scripted(close(), vec![]), 1600);
        mask.feed(&samples[..5100]);
        assert_eq!(mask.finish(), vec![false; 4]);
    }

    #[test]
    fn rms_mask_keeps_a_partial_last_frame() {
        let mut samples = vec![0i16; 1600];
        samples.extend(at_level(-30.0, 7).into_iter().take(1600));
        samples.extend(at_level(-30.0, 1).into_iter().take(150));
        let cfg = VadConfig::close(Vad::Rms);
        assert_eq!(speech_mask(&samples, &cfg, 1600), vec![false, true, true]);
    }

    /// Seeded Gaussian noise (Box-Muller over xorshift).
    fn gaussian(n: usize, seed: u64) -> Vec<f64> {
        let mut state = seed;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let (u, v) = (uniform(), uniform());
                (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
            })
            .collect()
    }

    /// `signal` scaled to an RMS of `level` dBFS.
    fn scaled(signal: &[f64], level: f64) -> Vec<i16> {
        let rms = (signal.iter().map(|x| x * x).sum::<f64>() / signal.len() as f64).sqrt();
        let gain = 10f64.powf(level / 20.0) * i16::MAX as f64 / rms;
        signal
            .iter()
            .map(|x| (x * gain).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    #[test]
    fn earshot_never_calls_noise_speech() {
        let n = 10 * VAD_SAMPLE_RATE as usize;
        let white = scaled(&gaussian(n, 7), -40.0);
        let mut acc = 0.0;
        let brown_raw: Vec<f64> = gaussian(n, 11)
            .into_iter()
            .map(|x| {
                acc = 0.995 * acc + x;
                acc
            })
            .collect();
        let brown = scaled(&brown_raw, -40.0);
        let silence = vec![0i16; n];
        for (name, samples) in [("silence", &silence), ("white", &white), ("brown", &brown)] {
            for cfg in [close(), open()] {
                let mut detector = SpeechDetector::new(cfg, VAD_SAMPLE_RATE);
                assert!(detector.uses_model());
                let mut speech = 0;
                for chunk in samples.chunks(800) {
                    detector.feed(chunk, |d| speech += d.speech as usize);
                }
                assert_eq!(speech, 0, "{name} was speech in {speech} frames");

                let mut stop = AutoStopDetector::with_vad(cfg, 2000, VAD_SAMPLE_RATE);
                assert!(!samples.chunks(800).any(|c| stop.feed(c)));
                assert!(!stop.has_speech(), "{name} armed auto-stop");
            }
            let expected = if name == "silence" {
                GateReason::Silent
            } else {
                GateReason::NoSpeech
            };
            assert_eq!(
                speech_gate(samples, VAD_SAMPLE_RATE, 300, &close()).reason,
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn other_rates_fall_back_to_rms() {
        let detector = SpeechDetector::new(close(), 48_000);
        assert!(!detector.uses_model());
        assert_eq!(detector.frame_len(), 768);
        // A rate of 0 does not panic.
        let mut detector = SpeechDetector::new(close(), 0);
        detector.feed(&[1, 2, 3], |_| {});
        assert_eq!(detector.levels().duration_ms, 0);
    }
}
