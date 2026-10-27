//! Track writer for the recording feature: writes captured audio chunks to
//! WAV files.
//!
//! One writer per track (mic, system). The header is rewritten every
//! `flush_every`, so a crash leaves a file that [`super::wav_repair`] can fix
//! with at most that much audio missing from the header's length. A track's
//! writer also measures its quality ([`QualityMeter`]) on the chunks it
//! writes anyway; the mic's watches for a muted mic ([`SilenceDetector`])
//! and raises the live quality warnings ([`LiveQuality`]). The system
//! track's tells the mic's when it plays ([`SystemSignal`]).

use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use audio_silence_gate::{LevelMeter, Vad};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::audio::capture::SAMPLE_RATE;
use crate::audio::quality::{Check, QualityMeter, QualityReport, Verdict, OWN_SPEECH_FOR_LEVEL};
use crate::audio::AudioChunk;

/// Loudest sample of a chunk that is still digital silence: a muted mic,
/// not a quiet room. On real recordings the quietest 100 ms of a room peak at
/// 14 to 50, and no stretch at or below this lasted more than 0.2 s (the
/// first chunk of a stream).
pub const SILENCE_PEAK: u16 = 4;

/// How long the mic may send only digital silence before the user is warned.
pub const SILENCE_WARN_AFTER: Duration = Duration::from_secs(3 * 60);

/// Counts how long a track has sent only digital silence ([`SILENCE_PEAK`]),
/// firing once when that reaches `after`, and again only after real audio
/// came back in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceDetector {
    after_samples: u64,
    silent_samples: u64,
    armed: bool,
}

impl SilenceDetector {
    pub fn new(after: Duration) -> Self {
        Self {
            after_samples: (after.as_secs_f64() * f64::from(SAMPLE_RATE)) as u64,
            silent_samples: 0,
            armed: true,
        }
    }

    /// Take the next chunk; true when it completes `after` of silence.
    pub fn feed(&mut self, chunk: &[i16]) -> bool {
        if chunk.iter().any(|s| s.unsigned_abs() > SILENCE_PEAK) {
            self.silent_samples = 0;
            self.armed = true;
            return false;
        }
        self.silent_samples += chunk.len() as u64;
        if self.armed && self.silent_samples >= self.after_samples {
            self.armed = false;
            return true;
        }
        false
    }
}

/// How often the mic's writer looks at its live quality: every 30 s of
/// audio.
pub const LIVE_CHECK_EVERY: Duration = Duration::from_secs(30);

/// Speech the mic must have heard before the live checks start (30 s): less
/// is too little to judge a mic or a room by. The speech level and the room
/// count only the user's own speech ([`SystemSignal::playing`]).
pub const LIVE_SPEECH_BEFORE: Duration = OWN_SPEECH_FOR_LEVEL;

/// Mic speech after which a system track that has sent only digital silence
/// is warned about (2 minutes).
pub const SYSTEM_SILENT_AFTER: Duration = Duration::from_secs(2 * 60);

/// How long after the system track last carried sound the mic is still
/// taken to hear the speakers (300 ms): their echo's tail, and the two
/// writers taking their chunks at slightly different times.
pub const PLAYBACK_HANGOVER: Duration = Duration::from_millis(300);

fn samples_in(duration: Duration) -> u64 {
    (duration.as_secs_f64() * f64::from(SAMPLE_RATE)) as u64
}

/// What the system track's writer tells the mic's, across their threads:
/// whether the system track has carried sound yet (`system-silent` waits for
/// it) and when it last did, so the mic's speech level and room leave out
/// what it records while the speakers play: a lecture's echo is not the
/// user's voice. Set from the start ([`no_system_track`](Self::no_system_track))
/// when no system track is recorded; then nothing is left out.
#[derive(Debug)]
pub struct SystemSignal {
    heard: AtomicBool,
    epoch: Instant,
    /// When the system writer last took a chunk above digital silence, in
    /// ms after `epoch`; [`NEVER`] before the first.
    last_sound_ms: AtomicU64,
}

/// [`SystemSignal::last_sound_ms`] before any sound.
const NEVER: u64 = u64::MAX;

impl Default for SystemSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemSignal {
    pub fn new() -> Self {
        Self {
            heard: AtomicBool::new(false),
            epoch: Instant::now(),
            last_sound_ms: AtomicU64::new(NEVER),
        }
    }

    /// No system track is recorded (none configured, or it failed to
    /// open): `system-silent` never fires.
    pub fn no_system_track(&self) {
        self.heard.store(true, Ordering::Relaxed);
    }

    /// Whether the system track has carried sound (or there is none).
    pub fn heard(&self) -> bool {
        self.heard.load(Ordering::Relaxed)
    }

    /// The system writer took a chunk above digital silence, now.
    pub fn sound(&self) {
        let now = self.epoch.elapsed().as_millis() as u64;
        self.last_sound_ms.store(now, Ordering::Relaxed);
        self.heard.store(true, Ordering::Relaxed);
    }

    /// Whether the system track carried sound within [`PLAYBACK_HANGOVER`].
    pub fn playing(&self) -> bool {
        self.playing_at(Instant::now())
    }

    fn playing_at(&self, now: Instant) -> bool {
        let last = self.last_sound_ms.load(Ordering::Relaxed);
        let since_epoch = now.saturating_duration_since(self.epoch).as_millis() as u64;
        last != NEVER && since_epoch <= last.saturating_add(PLAYBACK_HANGOVER.as_millis() as u64)
    }
}

/// The mic's live quality warnings, each raised at most once per recording:
/// every [`LIVE_CHECK_EVERY`] of audio, once [`LIVE_SPEECH_BEFORE`] of
/// speech was heard, clipping (over the last check's 30 s), and once that
/// much of the user's own speech was, a mic too quiet and a noisy room (over
/// the recording so far); and `system-silent` once the mic has heard
/// [`SYSTEM_SILENT_AFTER`] of speech while the system track sent only
/// digital silence ([`SystemSignal::heard`] still false).
///
/// The user's own speech is what the mic records while the system track
/// is not playing ([`SystemSignal::playing`]): the speakers' echo is left
/// out of the speech level, the room and the SNR, stored ones too
/// ([`QualityMeter::feed_marked`]). Clipping and the DC offset count
/// everything.
pub struct LiveQuality {
    meter: QualityMeter,
    /// Clipping since the last check.
    window: LevelMeter,
    since_check: u64,
    raised: Vec<Check>,
    system: Arc<SystemSignal>,
}

impl LiveQuality {
    pub fn new(meter: QualityMeter, system: Arc<SystemSignal>) -> Self {
        Self {
            meter,
            window: LevelMeter::new(SAMPLE_RATE),
            since_check: 0,
            raised: Vec::new(),
            system,
        }
    }

    /// Take the next chunk; the warnings it raises, none twice.
    pub fn feed(&mut self, chunk: &[i16]) -> Vec<Verdict> {
        self.meter.feed_marked(chunk, !self.system.playing());
        self.window.add_samples(chunk);
        self.since_check += chunk.len() as u64;
        let speech = self.meter.speech_samples();
        let mut found = Vec::new();
        if speech >= samples_in(SYSTEM_SILENT_AFTER) && !self.system.heard() {
            found.push(Verdict::of(Check::SystemSilent, 0.0));
        }
        if self.since_check >= samples_in(LIVE_CHECK_EVERY) {
            self.since_check = 0;
            let clipped = self.window.levels().clipped_share;
            self.window.reset();
            if speech >= samples_in(LIVE_SPEECH_BEFORE) {
                let own = self.meter.own_speech_samples() >= samples_in(LIVE_SPEECH_BEFORE);
                found.extend(check_warnings(&self.meter.report(), clipped, own));
            }
        }
        found.retain(|v| !self.raised.contains(&v.check));
        self.raised.extend(found.iter().map(|v| v.check));
        found
    }

    /// The warnings raised so far.
    pub fn raised(&self) -> &[Check] {
        &self.raised
    }

    /// The track's quality so far.
    pub fn report(&self) -> QualityReport {
        self.meter.report()
    }
}

/// What one live check warns about: with `levels` (enough of the user's
/// own speech), a mic too quiet or a noisy room by `report` (the recording
/// so far); clipping by `clipped` (the share over the last
/// [`LIVE_CHECK_EVERY`]).
fn check_warnings(report: &QualityReport, clipped: f32, levels: bool) -> Vec<Verdict> {
    let mut found: Vec<Verdict> = report
        .warnings()
        .into_iter()
        .filter(|v| levels && matches!(v.check, Check::TooQuiet | Check::Noisy))
        .collect();
    let clipping = Verdict::of(Check::Clipping, clipped);
    if clipping.warn {
        found.push(clipping);
    }
    found
}

/// What the mic's writer raises, on the writer's thread, so neither callback
/// may block: `on_silence` once the mic has sent only digital silence for
/// `silence_after` (and again only after real audio came back in between),
/// `on_warning` for each live quality warning ([`LiveQuality`]).
pub struct MicAlarm {
    pub silence_after: Duration,
    pub on_silence: Box<dyn FnMut() + Send>,
    pub on_warning: Box<dyn FnMut(Verdict) + Send>,
    /// What the system track's writer says: whether it carried sound yet
    /// (`system-silent` is raised only while it has not, and never without
    /// a system track) and whether it plays now (the speakers' echo is left
    /// out of the mic's speech level and room).
    pub system: Arc<SystemSignal>,
}

/// What a track writer recorded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackStats {
    /// Samples written (16 kHz mono).
    pub samples: u64,
    /// When the first non-empty chunk arrived, to align the tracks.
    pub first_chunk_at: Option<Instant>,
    /// The track's quality, measured on the chunks as written. `None` from a
    /// writer that does not measure ([`spawn_track_writer`]) or that wrote
    /// nothing.
    pub quality: Option<QualityReport>,
    /// The live warnings the mic's writer raised.
    pub raised: Vec<Check>,
}

/// Whether a track writer failed because the disk is full (or the user's
/// quota is): `ENOSPC` or `EDQUOT`, as the plain I/O error or inside
/// hound's, which does not expose it as a `source`.
pub fn is_disk_full(err: &anyhow::Error) -> bool {
    let full = |io: &std::io::Error| {
        matches!(io.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
            || io.kind() == std::io::ErrorKind::StorageFull
    };
    err.chain().any(|cause| {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return full(io);
        }
        matches!(cause.downcast_ref::<hound::Error>(), Some(hound::Error::IoError(io)) if full(io))
    })
}

/// The format of every recording track: 16 kHz mono 16-bit PCM.
fn track_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

/// Write the chunks from `rx` to a WAV file at `path` (created 0600) until
/// the channel closes, then finalize it. Runs on the blocking pool. Measures
/// nothing: [`spawn_mic_writer`] and [`spawn_system_writer`] do.
pub fn spawn_track_writer(
    rx: mpsc::UnboundedReceiver<AudioChunk>,
    path: PathBuf,
    flush_every: Duration,
) -> JoinHandle<anyhow::Result<TrackStats>> {
    tokio::task::spawn_blocking(move || write_track(rx, &path, flush_every, Monitor::None))
}

/// [`spawn_track_writer`] for the mic, which also measures its quality with
/// `vad`, raises `alarm.on_silence` when the mic sends only digital silence
/// and `alarm.on_warning` for each live quality warning. Never used for the
/// system track: a paused video is silent too.
pub fn spawn_mic_writer(
    rx: mpsc::UnboundedReceiver<AudioChunk>,
    path: PathBuf,
    flush_every: Duration,
    vad: Vad,
    alarm: MicAlarm,
) -> JoinHandle<anyhow::Result<TrackStats>> {
    let monitor = Monitor::Mic {
        silence: SilenceDetector::new(alarm.silence_after),
        live: Box::new(LiveQuality::new(
            QualityMeter::new(vad),
            Arc::clone(&alarm.system),
        )),
        alarm,
    };
    tokio::task::spawn_blocking(move || write_track(rx, &path, flush_every, monitor))
}

/// [`spawn_track_writer`] for the system track, which also measures its
/// quality with `vad` and tells `system` of every chunk that is more than
/// digital silence ([`SystemSignal::sound`]).
pub fn spawn_system_writer(
    rx: mpsc::UnboundedReceiver<AudioChunk>,
    path: PathBuf,
    flush_every: Duration,
    vad: Vad,
    system: Arc<SystemSignal>,
) -> JoinHandle<anyhow::Result<TrackStats>> {
    let monitor = Monitor::System {
        meter: Box::new(QualityMeter::new(vad)),
        system,
    };
    tokio::task::spawn_blocking(move || write_track(rx, &path, flush_every, monitor))
}

/// What a track writer watches besides writing.
enum Monitor {
    None,
    Mic {
        silence: SilenceDetector,
        live: Box<LiveQuality>,
        alarm: MicAlarm,
    },
    System {
        meter: Box<QualityMeter>,
        system: Arc<SystemSignal>,
    },
}

impl Monitor {
    fn feed(&mut self, chunk: &[i16]) {
        match self {
            Monitor::None => {}
            Monitor::Mic {
                silence,
                live,
                alarm,
            } => {
                if silence.feed(chunk) {
                    (alarm.on_silence)();
                }
                for warning in live.feed(chunk) {
                    (alarm.on_warning)(warning);
                }
            }
            Monitor::System { meter, system } => {
                meter.feed(chunk);
                if chunk.iter().any(|s| s.unsigned_abs() > SILENCE_PEAK) {
                    system.sound();
                }
            }
        }
    }

    /// Put what it measured into `stats`; nothing for a track with no audio.
    fn finish(self, stats: &mut TrackStats) {
        if stats.samples == 0 {
            return;
        }
        match self {
            Monitor::None => {}
            Monitor::Mic { live, .. } => {
                stats.quality = Some(live.report());
                stats.raised = live.raised().to_vec();
            }
            Monitor::System { meter, .. } => stats.quality = Some(meter.report()),
        }
    }
}

fn create_track_file(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // `mode` only applies to a new file.
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(file)
}

fn write_track(
    mut rx: mpsc::UnboundedReceiver<AudioChunk>,
    path: &Path,
    flush_every: Duration,
    mut monitor: Monitor,
) -> anyhow::Result<TrackStats> {
    let file = create_track_file(path)?;
    let mut writer = hound::WavWriter::new(BufWriter::new(file), track_spec())
        .with_context(|| format!("failed to start WAV {}", path.display()))?;
    let mut stats = TrackStats::default();
    let mut last_flush = Instant::now();

    while let Some(chunk) = rx.blocking_recv() {
        if chunk.is_empty() {
            continue;
        }
        monitor.feed(&chunk);
        stats.first_chunk_at.get_or_insert_with(Instant::now);
        for &sample in &chunk {
            writer
                .write_sample(sample)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        stats.samples += chunk.len() as u64;
        if last_flush.elapsed() >= flush_every {
            // Rewrites the header lengths, so the file is valid up to here.
            writer
                .flush()
                .with_context(|| format!("failed to flush {}", path.display()))?;
            last_flush = Instant::now();
        }
    }

    writer
        .finalize()
        .with_context(|| format!("failed to finalize {}", path.display()))?;
    debug!("wrote {} samples to {}", stats.samples, path.display());
    monitor.finish(&mut stats);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn read_samples(path: &Path) -> Vec<i16> {
        let mut reader = hound::WavReader::open(path).unwrap();
        assert_eq!(reader.spec(), track_spec());
        reader.samples::<i16>().map(|s| s.unwrap()).collect()
    }

    #[tokio::test]
    async fn writes_all_chunks_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.wav");
        let (tx, rx) = mpsc::unbounded_channel();
        let task = spawn_track_writer(rx, path.clone(), Duration::from_secs(2));

        let mut expected = Vec::new();
        for i in 0..10i16 {
            let chunk: Vec<i16> = (0..160).map(|j| i * 1000 + j).collect();
            expected.extend_from_slice(&chunk);
            tx.send(chunk).unwrap();
        }
        // An empty chunk neither counts nor sets the first-chunk time.
        tx.send(Vec::new()).unwrap();
        drop(tx);

        let stats = task.await.unwrap().unwrap();
        assert_eq!(stats.samples, 1600);
        assert!(stats.first_chunk_at.is_some());
        assert_eq!(read_samples(&path), expected);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn empty_track_is_a_valid_empty_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system.wav");
        let (tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
        tx.send(Vec::new()).unwrap();
        drop(tx);
        let stats = spawn_track_writer(rx, path.clone(), Duration::from_secs(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats, TrackStats::default());
        assert!(read_samples(&path).is_empty());
    }

    #[tokio::test]
    async fn flush_leaves_a_valid_header_mid_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.wav");
        let (tx, rx) = mpsc::unbounded_channel();
        // Flush after every chunk.
        let task = spawn_track_writer(rx, path.clone(), Duration::ZERO);

        tx.send(vec![7i16; 1600]).unwrap();
        // Wait until the flushed header reports the chunk, without closing
        // the channel: this is what a crash mid-recording leaves on disk.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(reader) = hound::WavReader::open(&path) {
                if reader.len() == 1600 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "header never flushed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(read_samples(&path), vec![7i16; 1600]);
        let repaired = crate::recording::wav_repair::repair_wav(&path).unwrap();
        assert_eq!(repaired.samples, 1600);
        assert!(!repaired.changed);

        tx.send(vec![1i16; 400]).unwrap();
        drop(tx);
        let stats = task.await.unwrap().unwrap();
        assert_eq!(stats.samples, 2000);
        assert_eq!(read_samples(&path).len(), 2000);
    }

    #[tokio::test]
    async fn unflushed_tail_is_repairable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.wav");
        let (tx, rx) = mpsc::unbounded_channel();
        // Never flush on cadence: only the header from creation is on disk
        // until the BufWriter spills.
        let task = spawn_track_writer(rx, path.clone(), Duration::from_secs(3600));
        // Enough to overflow the 8 KiB BufWriter several times.
        tx.send(vec![3i16; 16_000]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) < 16_000 {
            assert!(Instant::now() < deadline, "data never reached the file");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Simulate a crash: copy the file as it is now, before finalize.
        let crashed = dir.path().join("crashed.wav");
        std::fs::copy(&path, &crashed).unwrap();
        let repaired = crate::recording::wav_repair::repair_wav(&crashed).unwrap();
        assert!(repaired.changed);
        assert!(repaired.samples > 0 && repaired.samples <= 16_000);
        assert_eq!(read_samples(&crashed).len() as u64, repaired.samples);

        drop(tx);
        assert_eq!(task.await.unwrap().unwrap().samples, 16_000);
    }

    #[test]
    fn a_full_disk_is_recognized() {
        let enospc = || std::io::Error::from_raw_os_error(libc::ENOSPC);
        let write =
            anyhow::Error::from(hound::Error::IoError(enospc())).context("failed to write mic.wav");
        assert!(is_disk_full(&write));
        let finalize = anyhow::Error::from(enospc()).context("failed to finalize mic.wav");
        assert!(is_disk_full(&finalize));
        let quota = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::EDQUOT));
        assert!(is_disk_full(&quota));
        let other = anyhow::Error::from(hound::Error::IoError(std::io::Error::from_raw_os_error(
            libc::EIO,
        )));
        assert!(!is_disk_full(&other));
        assert!(!is_disk_full(&anyhow::anyhow!("no space left")));
    }

    /// A second of `samples` in 100 ms chunks.
    fn second_of(sample: impl Fn(usize) -> i16) -> Vec<Vec<i16>> {
        (0..10)
            .map(|c| (0..1600).map(|i| sample(c * 1600 + i)).collect())
            .collect()
    }

    /// Feed `secs` seconds of `sample` and count how often it fired.
    fn fires(detector: &mut SilenceDetector, secs: usize, sample: impl Fn(usize) -> i16) -> usize {
        let mut fired = 0;
        for _ in 0..secs {
            for chunk in second_of(&sample) {
                fired += usize::from(detector.feed(&chunk));
            }
        }
        fired
    }

    #[test]
    fn digital_silence_fires_once_after_three_minutes() {
        let mut detector = SilenceDetector::new(SILENCE_WARN_AFTER);
        assert_eq!(fires(&mut detector, 179, |_| 0), 0, "not before 3 minutes");
        assert_eq!(fires(&mut detector, 1, |_| 0), 1, "at 3 minutes");
        assert_eq!(fires(&mut detector, 600, |_| 0), 0, "once, not every chunk");
        // Dither at the threshold is still silence.
        let mut dither = SilenceDetector::new(SILENCE_WARN_AFTER);
        let at_peak = |i: usize| if i.is_multiple_of(2) { 4 } else { -4 };
        assert_eq!(fires(&mut dither, 180, at_peak), 1);
    }

    #[test]
    fn a_quiet_room_never_fires() {
        let mut detector = SilenceDetector::new(SILENCE_WARN_AFTER);
        // A noise floor peaking at 14, the quietest real room measured.
        let floor = |i: usize| ((i * 7919) % 29) as i16 - 14;
        assert_eq!(fires(&mut detector, 600, floor), 0);
        // Silence with a click now and then never adds up to 3 minutes.
        let clicks = |i: usize| {
            if i.is_multiple_of(16_000 * 120) {
                2000
            } else {
                0
            }
        };
        assert_eq!(fires(&mut detector, 600, clicks), 0);
    }

    #[test]
    fn real_audio_rearms_the_warning() {
        let mut detector = SilenceDetector::new(SILENCE_WARN_AFTER);
        assert_eq!(fires(&mut detector, 200, |_| 0), 1);
        assert_eq!(fires(&mut detector, 1, |i| (i % 100) as i16 * 300), 0);
        assert_eq!(fires(&mut detector, 179, |_| 0), 0);
        assert_eq!(fires(&mut detector, 1, |_| 0), 1, "fires again");
    }

    #[tokio::test]
    async fn the_mic_writer_raises_the_alarm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.wav");
        let (tx, rx) = mpsc::unbounded_channel();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&fired);
        let alarm = MicAlarm {
            silence_after: Duration::from_millis(200),
            on_silence: Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
            on_warning: Box::new(|v| panic!("no live warning in 1.3 s: {v:?}")),
            system: Arc::new(SystemSignal::new()),
        };
        let task = spawn_mic_writer(rx, path.clone(), Duration::from_secs(2), Vad::Rms, alarm);
        for chunk in [
            vec![0i16; 1600],
            vec![0; 1600],
            vec![0; 1600],
            vec![900; 1600],
        ] {
            tx.send(chunk).unwrap();
        }
        for _ in 0..4 {
            tx.send(vec![1i16; 1600]).unwrap();
        }
        drop(tx);
        let stats = task.await.unwrap().unwrap();
        assert_eq!(stats.samples, 8 * 1600, "every chunk is still written");
        assert_eq!(fired.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(read_samples(&path).len(), 8 * 1600);
    }

    /// `secs` seconds of 100 ms chunks whose 16 ms frames alternate speech
    /// to the RMS test (-45 dBFS) and a background 7 dB under it, so half the
    /// audio is speech and the room is noisy. `clip` puts runs of 3 samples
    /// at the rail in every chunk: 0.19% of the audio.
    fn noisy_room(start: usize, secs: usize, clip: bool) -> Vec<Vec<i16>> {
        (0..secs * 10)
            .map(|c| {
                let mut chunk: Vec<i16> = (0..1600)
                    .map(|i| {
                        let n = start * 16_000 + c * 1600 + i;
                        let amplitude = if (n / 256).is_multiple_of(2) { 184 } else { 82 };
                        if n.is_multiple_of(2) {
                            amplitude
                        } else {
                            -amplitude
                        }
                    })
                    .collect();
                if clip {
                    chunk[100..103].fill(i16::MAX);
                }
                chunk
            })
            .collect()
    }

    /// Feed `chunks`, returning the codes raised.
    fn raised(live: &mut LiveQuality, chunks: &[Vec<i16>]) -> Vec<&'static str> {
        chunks
            .iter()
            .flat_map(|chunk| live.feed(chunk))
            .map(|v| v.code())
            .collect()
    }

    /// Each live warning fires once per recording: nothing before 30 s of
    /// speech, the noisy room at the first check after it, clipping at the
    /// first check over a clipping 30 s, and the system track after 2
    /// minutes of mic speech while it sent only digital silence.
    #[test]
    fn live_warnings_fire_once() {
        let system = Arc::new(SystemSignal::new());
        let mut live = LiveQuality::new(QualityMeter::new(Vad::Rms), system);
        // 30 s of audio is 15 s of speech: the first check stays quiet.
        assert!(raised(&mut live, &noisy_room(0, 30, false)).is_empty());
        // 30 s of speech by the second check.
        assert_eq!(raised(&mut live, &noisy_room(30, 30, false)), ["noisy"]);
        assert!(raised(&mut live, &noisy_room(60, 178, false)).is_empty());
        // 2 minutes of speech at 240 s, none of it on the system track.
        assert_eq!(
            raised(&mut live, &noisy_room(238, 4, false)),
            ["system-silent"]
        );
        // The check at 270 s covers 28 s of clipping.
        assert_eq!(raised(&mut live, &noisy_room(242, 30, true)), ["clipping"]);
        assert!(raised(&mut live, &noisy_room(272, 128, true)).is_empty());
        assert_eq!(
            live.raised(),
            [Check::Noisy, Check::SystemSilent, Check::Clipping]
        );
    }

    /// A system track that carried sound never gets the warning, nor does a
    /// recording without one.
    #[test]
    fn a_system_track_that_played_is_never_silent() {
        let system = Arc::new(SystemSignal::new());
        let mut live = LiveQuality::new(QualityMeter::new(Vad::Rms), Arc::clone(&system));
        assert!(raised(&mut live, &noisy_room(0, 60, false)).contains(&"noisy"));
        system.sound();
        assert!(raised(&mut live, &noisy_room(60, 300, false)).is_empty());

        let none = Arc::new(SystemSignal::new());
        none.no_system_track();
        let mut live = LiveQuality::new(QualityMeter::new(Vad::Rms), none);
        assert_eq!(raised(&mut live, &noisy_room(0, 360, false)), ["noisy"]);
    }

    /// The signal plays for [`PLAYBACK_HANGOVER`] after the last sound, and
    /// never without a system track.
    #[test]
    fn the_system_signal_plays_for_the_hangover() {
        let system = SystemSignal::new();
        assert!(!system.heard());
        assert!(!system.playing());
        system.sound();
        assert!(system.heard());
        let now = Instant::now();
        assert!(system.playing_at(now));
        assert!(system.playing_at(now + Duration::from_millis(200)));
        assert!(!system.playing_at(now + Duration::from_millis(400)));

        let none = SystemSignal::new();
        none.no_system_track();
        assert!(none.heard(), "no system-silent without a system track");
        assert!(!none.playing(), "nothing is left out");
    }

    /// A meter on which a square wave at amplitude 52 (-56 dBFS) is speech
    /// and one at 8 (-72 dBFS) is not: the RMS test at -66 dBFS, so a quiet
    /// voice can be "too quiet" without earshot.
    fn quiet_meter() -> QualityMeter {
        QualityMeter::with_config(audio_silence_gate::VadConfig {
            rms_threshold: 0.0005,
            ..audio_silence_gate::VadConfig::open(Vad::Rms)
        })
    }

    /// `secs` seconds of 100 ms chunks whose 16 ms frames alternate a voice
    /// at -56 dBFS and a room at -72 dBFS: too quiet, 16 dB above the room.
    fn quiet_voice(secs: usize) -> Vec<Vec<i16>> {
        (0..secs * 10)
            .map(|c| {
                (0..1600)
                    .map(|i| {
                        let n = c * 1600 + i;
                        let amplitude = if (n / 256).is_multiple_of(2) { 52 } else { 8 };
                        if n.is_multiple_of(2) {
                            amplitude
                        } else {
                            -amplitude
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// Feed `chunks` while the system track plays (a sound before each),
    /// returning the codes raised.
    fn raised_while_playing(
        live: &mut LiveQuality,
        system: &SystemSignal,
        chunks: &[Vec<i16>],
    ) -> Vec<&'static str> {
        chunks
            .iter()
            .flat_map(|chunk| {
                system.sound();
                live.feed(chunk)
            })
            .map(|v| v.code())
            .collect()
    }

    /// A lecture on the speakers while the user is silent: its echo reaches
    /// the mic at -56 dBFS, which is not the user's speech, so neither the
    /// live check nor the stored verdict calls the mic too quiet.
    #[test]
    fn speaker_echo_is_not_a_quiet_mic() {
        let system = Arc::new(SystemSignal::new());
        let mut live = LiveQuality::new(quiet_meter(), Arc::clone(&system));
        assert!(raised_while_playing(&mut live, &system, &quiet_voice(180)).is_empty());
        let report = live.report();
        assert_eq!(report.levels.speech_dbfs, None, "{report:?}");
        assert_eq!(report.levels.snr_db, None);
        assert!(report.warnings().is_empty(), "{report:?}");
        // The echo is still speech to the detector: only the levels leave it
        // out.
        assert!(report.levels.speech_share > 0.4);
    }

    /// The same quiet voice while the system track is silent is the user's:
    /// it warns live and in the stored verdict, as without a system track.
    /// Speech the speakers played over afterwards changes neither.
    #[test]
    fn a_quiet_mic_still_warns_while_the_system_is_silent() {
        let system = Arc::new(SystemSignal::new());
        let mut live = LiveQuality::new(quiet_meter(), Arc::clone(&system));
        // 30 s of speech by the check at 60 s.
        assert_eq!(raised(&mut live, &quiet_voice(60)), ["too-quiet"]);
        let report = live.report();
        let speech = report.levels.speech_dbfs.unwrap();
        assert!((speech + 56.0).abs() < 0.5, "{speech}");
        assert!(report.warnings().iter().any(|v| v.check == Check::TooQuiet));

        // Louder speech over the speakers is left out of the level.
        let loud: Vec<Vec<i16>> = noisy_room(60, 60, false);
        assert!(raised_while_playing(&mut live, &system, &loud).is_empty());
        let report = live.report();
        assert_eq!(report.levels.speech_dbfs, Some(speech));
        assert!(report.warnings().iter().any(|v| v.check == Check::TooQuiet));
    }

    /// Too little of the user's own speech for a stored verdict once the
    /// speakers played: none, rather than one from 10 s. Without a system
    /// track the same 10 s still gives one, as before.
    #[test]
    fn a_stored_level_needs_30_seconds_of_own_speech() {
        let system = Arc::new(SystemSignal::new());
        let mut live = LiveQuality::new(quiet_meter(), Arc::clone(&system));
        assert!(raised(&mut live, &quiet_voice(20)).is_empty());
        assert!(raised_while_playing(&mut live, &system, &quiet_voice(10)).is_empty());
        let report = live.report();
        assert_eq!(report.levels.speech_dbfs, None);
        assert!(report.levels.noise_dbfs.is_some(), "the room is measured");
        assert!(report.warnings().is_empty());

        let none = Arc::new(SystemSignal::new());
        none.no_system_track();
        let mut live = LiveQuality::new(quiet_meter(), none);
        assert!(raised(&mut live, &quiet_voice(20)).is_empty());
        let codes: Vec<&str> = live.report().warnings().iter().map(|v| v.code()).collect();
        assert_eq!(codes, ["too-quiet"]);
    }

    /// A check warns about a mic too quiet and a noisy room over the whole
    /// recording, clipping over its own 30 s.
    #[test]
    fn a_check_warns_by_the_report_and_the_window() {
        let levels = audio_silence_gate::Levels {
            speech_dbfs: Some(-58.0),
            noise_dbfs: Some(-70.0),
            snr_db: Some(12.0),
            clipped_share: 0.01,
            dc_offset: 0.05,
            speech_share: 0.5,
            peak_dbfs: -20.0,
            duration_ms: 60_000,
        };
        let report = QualityReport::new(Vad::Earshot, levels);
        let codes = |clipped, levels| -> Vec<&'static str> {
            check_warnings(&report, clipped, levels)
                .iter()
                .map(|v| v.code())
                .collect()
        };
        // The whole recording's clipping and DC offset are not live checks.
        assert_eq!(codes(0.0, true), ["too-quiet"]);
        assert_eq!(codes(0.002, true), ["too-quiet", "clipping"]);
        // Too little own speech: clipping alone.
        assert_eq!(codes(0.002, false), ["clipping"]);
    }

    /// The writers measure the track as written; the system's sets the flag
    /// once it carries more than digital silence.
    #[tokio::test]
    async fn the_writers_measure_their_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let signal = Arc::new(SystemSignal::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let system = spawn_system_writer(
            rx,
            dir.path().join("system.wav"),
            Duration::from_secs(2),
            Vad::Rms,
            Arc::clone(&signal),
        );
        tx.send(vec![SILENCE_PEAK as i16; 1600]).unwrap();
        tx.send(vec![0; 1600]).unwrap();
        drop(tx);
        let stats = system.await.unwrap().unwrap();
        assert!(!signal.heard(), "digital silence");
        assert!(!signal.playing());
        let quality = stats.quality.unwrap();
        assert_eq!(quality.levels.speech_dbfs, None);
        assert_eq!(quality.levels.noise_dbfs, None);

        let (tx, rx) = mpsc::unbounded_channel();
        let system = spawn_system_writer(
            rx,
            dir.path().join("system.wav"),
            Duration::from_secs(2),
            Vad::Rms,
            Arc::clone(&signal),
        );
        for chunk in noisy_room(0, 2, false) {
            tx.send(chunk).unwrap();
        }
        drop(tx);
        let stats = system.await.unwrap().unwrap();
        assert!(signal.heard());
        assert!(stats.quality.unwrap().levels.speech_dbfs.is_some());
        assert!(stats.raised.is_empty());

        // No audio, no report.
        let (tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
        drop(tx);
        let system = spawn_system_writer(
            rx,
            dir.path().join("empty.wav"),
            Duration::from_secs(2),
            Vad::Rms,
            signal,
        );
        assert_eq!(system.await.unwrap().unwrap().quality, None);
    }

    #[tokio::test]
    async fn unwritable_path_is_an_error() {
        let (_tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
        let err = spawn_track_writer(
            rx,
            PathBuf::from("/nonexistent-whisrs-dir/mic.wav"),
            Duration::from_secs(2),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(err.to_string().contains("failed to create"), "{err:#}");
    }
}
