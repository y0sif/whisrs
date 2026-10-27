//! The recording feature in the daemon: sessions, the processing queue,
//! crash recovery and graceful shutdown.
//!
//! A recording lives entirely outside the dictation state machine and its
//! lock, and a long transcription runs on its own task so IPC never waits
//! for it. While a recording is in progress, dictation and voice commands
//! refuse to start ([`refuse_while_recording`]); a dictation already
//! recording finishes as usual. Status goes out on a separate watch channel
//! that only the tray reads, so a recording never animates the overlay or
//! fires the dictation hooks.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use audio_silence_gate::Vad;
use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::device::default_monitor_source;
use whisrs::audio::quality::{Check, Verdict};
use whisrs::recording::naming::USER_NAME_LOST;
use whisrs::recording::process::process_recording;
use whisrs::recording::recorder::{
    is_disk_full, spawn_mic_writer, spawn_system_writer, MicAlarm, TrackStats, SILENCE_WARN_AFTER,
};
use whisrs::recording::speakers::separation_failure;
use whisrs::recording::summary::{self, SUMMARY_FAILED, SUMMARY_TIMEOUT};
use whisrs::recording::wav_repair::repair_wav;
use whisrs::recording::{
    store, EndReason, NetworkWait, RecordingAction, RecordingMeta, RecordingOptions,
    RecordingPhase, RecordingState, RecordingStatus, SummaryRequest, TrackKind, TrackMeta,
    TrackQuality, DEFAULT_OTHERS_LABEL, NETWORK_RETRY_EVERY, NETWORK_RETRY_FOR, NETWORK_RETRY_MAX,
};
use whisrs::transcription::retry::is_network_unavailable;
use whisrs::{Config, RecordingConfig, Response};

use crate::context::DaemonContext;
use crate::notify::{send_notification, truncate_preview};

/// How often a track writer rewrites its WAV header, bounding what a crash
/// can leave outside the header's length.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

/// How long a stop waits for each track writer to finalize its file.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How much longer a writer that missed its timeout gets once its capture
/// (the sender side of its channel) has been dropped.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long shutdown waits for a recording to be finalized, so a SIGTERM
/// from systemd is answered well inside its own timeout.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Grace after [`SHUTDOWN_TIMEOUT`]; recovery repairs whatever is left.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// How long shutdown waits for the session lock. A start holds it while
/// it opens two devices (up to ~5 s each), and must either finish or be
/// left with a `recording.json` that recovery picks up.
const SHUTDOWN_LOCK_TIMEOUT: Duration = Duration::from_secs(12);

/// How long shutdown waits for the aborted processing worker to unwind.
const WORKER_ABORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Size of the header the track writer (`hound`, 16-bit mono PCM) writes
/// before any sample; a track no longer than this holds no audio.
const WAV_HEADER_LEN: u64 = 44;

/// Processing attempts after which recovery stops requeueing a recording: a
/// recording whose processing crashes the daemon would otherwise be retried
/// on every restart, and systemd restarts it forever.
const MAX_ATTEMPTS: u32 = 3;

/// Longest error text put in a notification.
const NOTIFY_ERROR_MAX: usize = 200;

/// What one minute of recording takes on disk: two 16 kHz mono 16-bit tracks
/// (1.92 MB a minute each) plus the echo-cancelled copy of the mic that
/// processing writes into `.cache/`. Chunk results there are a few kB.
const DISK_BYTES_PER_MINUTE: u64 = 3 * 32_000 * 60;

/// Free space a recording needs to start: about 10 minutes of it
/// ([`DISK_BYTES_PER_MINUTE`]) plus headroom to process them.
const DISK_MIN_TO_START: u64 = 100_000_000;

/// How often a recording in progress checks the free space.
const DISK_CHECK_EVERY: Duration = Duration::from_secs(60);

/// Below this many minutes of room a recording warns, once.
const DISK_WARN_MINUTES: u64 = 30;

/// Below this many minutes of room a recording stops, as at its time limit,
/// so its tracks are finalized and transcribed before a write fails.
const DISK_STOP_MINUTES: u64 = 2;

/// How the free space is read: [`store::free_space`], or a stand-in in tests.
pub(crate) type FreeSpaceFn = fn(&Path) -> std::io::Result<u64>;

/// Longest wait for one read of the free space. A hung network mount can
/// block `statvfs` for minutes: the check is skipped instead, so neither a
/// start nor a recording's time limit waits on it.
const FREE_SPACE_TIMEOUT: Duration = Duration::from_secs(5);

/// What [`open_tracks`] gives a start: the captures, their metas, and why the
/// system audio could not be opened.
type OpenedTracks = (Vec<TrackCapture>, Vec<TrackMeta>, Option<String>);

/// [`open_tracks`], or a stand-in.
type OpenTracksFn =
    fn(&Path, &str, Option<Result<SystemDevice>>, Vad, MicAlarm) -> Result<OpenedTracks>;

/// How a start reaches the audio devices: [`resolve_system`] and
/// [`open_tracks`], or stand-ins in tests, which must never open the host's
/// mic or ask its sound server.
#[derive(Clone, Copy)]
struct Devices {
    resolve_system: fn(SystemSource) -> Option<Result<SystemDevice>>,
    open_tracks: OpenTracksFn,
}

/// The devices every [`RecordingRuntime`] starts with. In test builds these
/// are stand-ins that panic, so no test context (this module's, the
/// pipeline's, or one added later) can open the host's mic or ask its sound
/// server through a start.
#[cfg(not(test))]
const DEVICES: Devices = Devices {
    resolve_system,
    open_tracks,
};

#[cfg(test)]
const DEVICES: Devices = Devices {
    resolve_system: |_| panic!("a test start looked up the system audio device"),
    open_tracks: |_, _, _, _, _| panic!("a test start opened the audio devices"),
};

/// How often [`run_network_retries`] looks for a recording waiting for the
/// network whose next attempt is due ([`NETWORK_RETRY_EVERY`] after its last).
const NETWORK_RETRY_TICK: Duration = Duration::from_secs(60);

/// Start of the error a stop returns when it saved the recording but could
/// not queue it. [`stop`] notifies it itself, since a compositor-bound
/// `whisrs recording toggle` shows nobody its stderr.
const NOT_TRANSCRIBED: &str = "recording stopped but not transcribed";

/// Whether a recording error was already notified where it happened, so the
/// hotkey and tray dispatcher does not toast it a second time.
pub(crate) fn error_already_notified(message: &str) -> bool {
    message.starts_with(NOT_TRANSCRIBED)
}

/// A start that opens the mic to type what the user says, which a recording
/// in progress refuses.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DictationStart<'a> {
    /// `toggle`: the CLI, its hotkey, or a tray left-click.
    Dictation,
    /// `command`.
    CommandMode,
    /// A named `[[llm_commands]]` entry.
    LlmCommand(&'a str),
    /// An agent's voice turn (`[[agents]]`).
    Agent(&'a str),
}

/// Refuse `start` while a recording is in progress, returning the error to
/// send back.
///
/// The mic is shared with the recording: the transcript would record the
/// dictation as the user's speech, and in a call the others would hear it.
/// Only starts are refused: a dictation already recording when the recording
/// started keeps its own capture and stops, cancels and types as usual.
///
/// Notified like any other failure (`notify_error`), since a hotkey or tray
/// press shows nobody the response and no overlay appears for a start that
/// did not happen.
pub(crate) fn refuse_while_recording(
    context: &DaemonContext,
    start: DictationStart<'_>,
) -> Option<Response> {
    if !context.recording.is_capturing() {
        return None;
    }
    let (what, subject) = match start {
        DictationStart::Dictation => ("dictation".to_string(), "Dictation".to_string()),
        DictationStart::CommandMode => ("command mode".to_string(), "Command mode".to_string()),
        DictationStart::LlmCommand(name) => (format!("llm-command '{name}'"), format!("'{name}'")),
        DictationStart::Agent(name) => (format!("agent '{name}'"), format!("Agent '{name}'")),
    };
    info!("{what} not started: a recording is in progress");
    let message = format!("A recording is in progress. {subject} is off until you stop it.");
    if context.notify_error() {
        send_notification("whisrs", &message);
    }
    Some(error_response(message))
}

/// What the processing worker does with a queued recording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Job {
    /// Transcribe it ([`process_recording`]).
    #[default]
    Transcribe,
    /// Summarize its transcript (`whisrs recording summarize`), without
    /// transcribing it again.
    Summarize,
}

/// A recording that is capturing, processing, queued or waiting for the
/// network, as shown in status.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActiveRecording {
    id: String,
    title: Option<String>,
    started_at: DateTime<Local>,
    dir: PathBuf,
    /// Queued or processed as an automatic retry of a recording waiting for
    /// the network: it does not count towards [`MAX_ATTEMPTS`].
    network_retry: bool,
    job: Job,
}

impl ActiveRecording {
    fn from_meta(dir: &Path, meta: &RecordingMeta) -> Self {
        Self {
            id: meta.id.clone(),
            title: meta.title.clone(),
            started_at: meta.started_at,
            dir: dir.to_path_buf(),
            network_retry: false,
            job: Job::Transcribe,
        }
    }

    fn from_header(dir: &Path, header: &store::RecordingHeader) -> Self {
        Self {
            id: header.id.clone(),
            title: header.title.clone(),
            started_at: header.started_at,
            dir: dir.to_path_buf(),
            network_retry: false,
            job: Job::Transcribe,
        }
    }
}

/// What the daemon is doing with recordings, the source of [`RecordingStatus`].
#[derive(Debug, Default)]
struct RecordingView {
    capturing: Option<ActiveRecording>,
    processing: Option<ActiveRecording>,
    queued: Vec<ActiveRecording>,
    /// Waiting for the network ([`RecordingState::Waiting`]) and neither
    /// queued nor processing: what [`retry_waiting`] looks at.
    waiting: Vec<ActiveRecording>,
    /// Ids of finished recordings whose files are being rewritten (a
    /// rename, the retention sweep): not shown, but neither queued nor
    /// rewritten by anyone else meanwhile ([`RecordingRuntime::hold`]).
    held: Vec<String>,
}

impl RecordingView {
    /// Capturing wins over processing: it is what the user can still act
    /// on. With nothing capturing or processing yet, the next queued recording
    /// is shown, so a stop reports the recording it just queued.
    fn status(&self) -> RecordingStatus {
        let (phase, active) = match (&self.capturing, &self.processing) {
            (Some(rec), _) => (RecordingPhase::Recording, Some(rec)),
            (None, Some(proc)) => (RecordingPhase::Processing, Some(proc)),
            (None, None) => match self.queued.first() {
                Some(next) => (RecordingPhase::Processing, Some(next)),
                None => (RecordingPhase::Idle, None),
            },
        };
        RecordingStatus {
            phase,
            id: active.map(|a| a.id.clone()),
            title: active.and_then(|a| a.title.clone()),
            started_at: active.map(|a| a.started_at),
            dir: active.map(|a| a.dir.clone()),
            queued: self.queued.iter().map(|q| q.id.clone()).collect(),
            waiting: self.waiting.iter().map(|w| w.id.clone()).collect(),
            summarizing: phase == RecordingPhase::Processing
                && active.is_some_and(|a| a.job == Job::Summarize),
        }
    }

    fn is_pending(&self, id: &str) -> bool {
        self.processing.as_ref().is_some_and(|p| p.id == id)
            || self.queued.iter().any(|q| q.id == id)
            || self.held.iter().any(|h| h == id)
    }

    /// Whether a hold on `id` must wait: it is held, or queued or being
    /// processed for a transcription. A summary job does not count: it
    /// reads a snapshot and holds the recording only to write its result
    /// ([`summarize_one`]), so a rename or the sweep goes on meanwhile.
    fn blocks_a_hold(&self, id: &str) -> bool {
        let transcribing = |r: &ActiveRecording| r.id == id && r.job == Job::Transcribe;
        self.processing.as_ref().is_some_and(transcribing)
            || self.queued.iter().any(transcribing)
            || self.held.iter().any(|h| h == id)
    }
}

/// One recording waiting for the processing worker.
#[derive(Debug, Clone)]
pub(crate) struct QueuedRecording {
    recording: ActiveRecording,
}

/// One track being recorded.
struct TrackCapture {
    kind: TrackKind,
    /// `None` only in tests, which feed the writer's channel directly.
    handle: Option<AudioCaptureHandle>,
    writer: JoinHandle<Result<TrackStats>>,
    /// Set when the writer ended before the recording was stopped: the device
    /// stopped delivering audio (unplugged, server restart) or a write failed.
    ended_early: Arc<AtomicBool>,
}

impl TrackCapture {
    fn new(
        kind: TrackKind,
        handle: Option<AudioCaptureHandle>,
        writer: JoinHandle<Result<TrackStats>>,
    ) -> Self {
        Self {
            kind,
            handle,
            writer,
            ended_early: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// What a track is called in notifications and notes.
fn track_label(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Mic => "microphone",
        TrackKind::System => "system audio",
    }
}

/// Wrap a track's writer so that its ending before `stopping` is set is
/// noticed: logged, flagged for the meta, and notified once. A write that
/// hit a full disk says so.
fn watch_writer(track: &mut TrackCapture, id: &str, stopping: &Arc<AtomicBool>, notify: bool) {
    let placeholder = tokio::spawn(async { Ok(TrackStats::default()) });
    let writer = std::mem::replace(&mut track.writer, placeholder);
    let kind = track.kind;
    let early = Arc::clone(&track.ended_early);
    let stopping = Arc::clone(stopping);
    let id = id.to_string();
    track.writer = tokio::spawn(async move {
        let result = match writer.await {
            Ok(result) => result,
            Err(e) => Err(anyhow::anyhow!("writer task failed: {e}")),
        };
        if !stopping.load(Ordering::Acquire) {
            early.store(true, Ordering::Release);
            match &result {
                Ok(_) => warn!(
                    "recording {id}: the {} stopped delivering audio before the recording was stopped",
                    track_label(kind)
                ),
                Err(e) => warn!(
                    "recording {id}: the {} track stopped early: {e:#}",
                    track_label(kind)
                ),
            }
            if notify {
                let body = match &result {
                    Err(e) if is_disk_full(e) => format!(
                        "Recording: the disk is full, the {} is no longer being saved",
                        track_label(kind)
                    ),
                    _ => format!("Recording: {} stopped delivering audio", track_label(kind)),
                };
                send_notification("whisrs", &body);
            }
        }
        result
    });
}

/// The recording in progress.
pub(crate) struct RecordingSession {
    dir: PathBuf,
    meta: RecordingMeta,
    /// Taken just before the captures start; track offsets are relative to it.
    started: Instant,
    tracks: Vec<TrackCapture>,
    /// The watch ([`spawn_watch`]: time limit and free space), aborted when
    /// the recording stops another way.
    watch: Option<JoinHandle<()>>,
    /// Set before the captures are stopped, so a writer ending after it is
    /// a normal end and one ending before it is a device that died.
    stopping: Arc<AtomicBool>,
}

/// Recording state shared by every connection. Deliberately separate from
/// `DaemonState`: dictation start holds that lock for seconds while a device
/// opens, and a recording start must not wait on it (nor it on a recording).
pub(crate) struct RecordingRuntime {
    session: tokio::sync::Mutex<Option<RecordingSession>>,
    view: std::sync::Mutex<RecordingView>,
    status_tx: watch::Sender<RecordingStatus>,
    queue_tx: mpsc::UnboundedSender<QueuedRecording>,
    /// Set by [`shutdown`], so a start queued behind it on the session lock
    /// does not open devices while the daemon exits.
    shutting_down: AtomicBool,
    /// The [`run_worker`] task, aborted by [`shutdown`] so an interrupted
    /// transcription does not count as a crashed attempt.
    worker: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// The folder of a start in progress, so a shutdown that cannot wait for
    /// the start can still tell whether it recorded anything.
    starting: std::sync::Mutex<Option<PathBuf>>,
    /// Reads the free space of the recordings folder.
    free_space: FreeSpaceFn,
    devices: Devices,
}

impl RecordingRuntime {
    /// The runtime, the status receiver for the tray, and the queue receiver
    /// for [`run_worker`].
    pub(crate) fn new() -> (
        Self,
        watch::Receiver<RecordingStatus>,
        mpsc::UnboundedReceiver<QueuedRecording>,
    ) {
        let (status_tx, status_rx) = watch::channel(RecordingStatus::default());
        let (queue_tx, queue_rx) = mpsc::unbounded_channel();
        let runtime = Self {
            session: tokio::sync::Mutex::new(None),
            view: std::sync::Mutex::new(RecordingView::default()),
            status_tx,
            queue_tx,
            shutting_down: AtomicBool::new(false),
            worker: std::sync::Mutex::new(None),
            starting: std::sync::Mutex::new(None),
            free_space: store::free_space,
            devices: DEVICES,
        };
        (runtime, status_rx, queue_rx)
    }

    /// Hand over the [`run_worker`] task, for [`shutdown`] to abort.
    pub(crate) fn set_worker(&self, worker: JoinHandle<()>) {
        *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(worker);
    }

    fn status(&self) -> RecordingStatus {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).status()
    }

    /// The recording status, for a task that waits for a recording to stop
    /// (agent replies are held while one captures).
    pub(crate) fn subscribe(&self) -> watch::Receiver<RecordingStatus> {
        self.status_tx.subscribe()
    }

    /// Whether a recording is in progress. Reads the view, never the session
    /// lock, so it answers at once while a start or stop holds that lock for
    /// seconds opening or closing devices. A start counts once its devices
    /// are open, a stop until its tracks are finalized.
    pub(crate) fn is_capturing(&self) -> bool {
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capturing
            .is_some()
    }

    /// Change the view and broadcast the new status. `send_replace` updates
    /// the value even with no receiver (tray disabled), so `status` stays
    /// truthful.
    fn update(&self, f: impl FnOnce(&mut RecordingView)) {
        let status = {
            let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut view);
            view.status()
        };
        self.status_tx.send_replace(status);
    }

    /// Take recording `id` off the view as capturing, unless another recording
    /// is shown by now: a stop finalizes its tracks (up to 15 s) after
    /// releasing the session lock, so a recording may start meanwhile, and an
    /// unconditional clear would show it, and refuse dictation, as idle.
    fn clear_capturing(&self, id: &str) {
        self.update(|v| {
            if v.capturing.as_ref().is_some_and(|r| r.id == id) {
                v.capturing = None;
            }
        });
    }

    /// Queue a recording for processing, refusing one that is already queued
    /// or being processed.
    fn enqueue(&self, recording: ActiveRecording) -> Result<()> {
        self.reserve(&recording)?;
        self.dispatch(recording)
    }

    /// Claim a place in the queue, refusing a recording that is already queued
    /// or being processed. The check and the claim are one step, so two
    /// retries of one recording cannot both pass. Follow with [`Self::dispatch`]
    /// or [`Self::unreserve`].
    fn reserve(&self, recording: &ActiveRecording) -> Result<()> {
        let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if view.held.contains(&recording.id) {
            bail!(
                "recording {} is busy (being renamed or cleaned up); try again",
                recording.id
            );
        }
        if view.is_pending(&recording.id) {
            bail!(
                "recording {} is already queued, being transcribed or being summarized",
                recording.id
            );
        }
        view.queued.push(recording.clone());
        Ok(())
    }

    fn unreserve(&self, id: &str) {
        self.update(|v| v.queued.retain(|q| q.id != id));
    }

    /// Claim recording `id` while its files are rewritten, like
    /// [`Self::reserve`] but without showing it as queued: the tray and
    /// `status` see nothing. Refuses one that is held, or queued or being
    /// processed for a transcription; a summary job does not stop it
    /// ([`RecordingView::blocks_a_hold`]). Follow with [`Self::release`].
    fn hold(&self, id: &str) -> Result<()> {
        let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if view.blocks_a_hold(id) {
            bail!("recording {id} is queued, being transcribed or being rewritten");
        }
        view.held.push(id.to_string());
        Ok(())
    }

    fn release(&self, id: &str) {
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .held
            .retain(|h| h != id);
    }

    /// Hand a reserved recording to the worker. A recording waiting for the
    /// network is no longer shown as waiting once it is queued.
    fn dispatch(&self, recording: ActiveRecording) -> Result<()> {
        let id = recording.id.clone();
        if self.queue_tx.send(QueuedRecording { recording }).is_err() {
            self.unreserve(&id);
            bail!("the recording processing worker is not running");
        }
        self.update(|v| v.waiting.retain(|w| w.id != id));
        Ok(())
    }

    /// Show recording `recording` as waiting for the network.
    fn show_waiting(&self, recording: ActiveRecording) {
        self.update(|v| {
            v.waiting.retain(|w| w.id != recording.id);
            v.waiting.push(ActiveRecording {
                network_retry: false,
                ..recording
            });
        });
    }

    fn hide_waiting(&self, id: &str) {
        self.update(|v| v.waiting.retain(|w| w.id != id));
    }
}

/// Clears [`RecordingRuntime::starting`] however a start returns.
struct StartingGuard<'a>(&'a RecordingRuntime);

impl<'a> StartingGuard<'a> {
    fn new(runtime: &'a RecordingRuntime, dir: &Path) -> Self {
        *runtime.starting.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
        Self(runtime)
    }
}

impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        *self.0.starting.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Recording notifications are progress the user asked for, so they follow
/// `[general] notify` alone: the overlay never shows recordings, so unlike
/// dictation toasts they are not suppressed when it is enabled.
fn notify(context: &DaemonContext, body: &str) {
    if context.notify {
        send_notification("whisrs", body);
    }
}

fn recordings_root(config: &Config) -> PathBuf {
    store::recordings_dir_from(RecordingConfig::effective(config).dir.as_deref())
}

/// The mic track's device: `[recording] mic_device`, else `[audio] device`.
fn mic_device(recording: &RecordingConfig, audio_device: &str) -> String {
    recording
        .mic_device
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(audio_device)
        .to_string()
}

/// Where the system track comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemSource {
    /// The monitor of the default output device, looked up at start and
    /// followed when the default output changes.
    DefaultMonitor,
    /// `"none"`: record the mic only.
    Off,
    /// A specific source, opened strictly.
    Named(String),
}

fn system_source(setting: Option<&str>) -> SystemSource {
    match setting.map(str::trim) {
        None | Some("") => SystemSource::DefaultMonitor,
        Some(s) if s.eq_ignore_ascii_case("default") => SystemSource::DefaultMonitor,
        Some(s) if s.eq_ignore_ascii_case("none") => SystemSource::Off,
        Some(s) => SystemSource::Named(s.to_string()),
    }
}

/// A track's first chunk relative to the moment the captures started.
fn offset_ms(started: Instant, first_chunk_at: Option<Instant>) -> i64 {
    first_chunk_at
        .map(|t| t.saturating_duration_since(started).as_millis() as i64)
        .unwrap_or(0)
}

fn track_meta(kind: TrackKind, device: String) -> TrackMeta {
    TrackMeta {
        kind,
        file: format!("audio/{}.wav", kind.file_stem()),
        device,
        offset_ms: 0,
        samples: 0,
        quality: None,
    }
}

fn recording_response(context: &DaemonContext) -> Response {
    Response::Recording {
        recording: context.recording.status(),
    }
}

fn error_response(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

/// Handle `Command::Recording`.
pub(crate) async fn handle_recording(
    context: Arc<DaemonContext>,
    action: RecordingAction,
    title: Option<String>,
    id: Option<String>,
    options: RecordingOptions,
) -> Response {
    // Checked here and not only by the CLI, since any client can write to
    // the socket: a bad value is refused before a device opens or a
    // recording on disk is touched.
    let options = match options.validated() {
        Ok(options) => options,
        Err(message) => {
            warn!("recording {action:?} refused: {message}");
            return error_response(message);
        }
    };
    let result = match action {
        RecordingAction::Start => start(&context, title, &options).await,
        RecordingAction::Stop => stop(&context, None, EndReason::Stopped).await,
        RecordingAction::Toggle => {
            let recording = context.recording.session.lock().await.is_some();
            if recording {
                stop(&context, None, EndReason::Stopped).await
            } else {
                start(&context, title, &options).await
            }
        }
        RecordingAction::Cancel => cancel(&context).await,
        RecordingAction::Status => Ok(()),
        RecordingAction::Retry => retry(&context, id.as_deref().unwrap_or("last"), &options).await,
        RecordingAction::Copy => copy(&context, id.as_deref().unwrap_or("last")).await,
        RecordingAction::Name => name(&context, id.as_deref().unwrap_or("last"), &options).await,
        RecordingAction::Summarize => summarize(&context, id.as_deref().unwrap_or("last")).await,
    };
    match result {
        Ok(()) => recording_response(&context),
        Err(e) => {
            warn!("recording {action:?} failed: {e:#}");
            error_response(format!("{e:#}"))
        }
    }
}

/// The system track's source as resolved at start.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemDevice {
    name: String,
    /// The default output's monitor: the capture moves when the default
    /// output changes. A source named in the config is never followed.
    follow_default: bool,
}

/// Find the system track's source. Blocking. `None` when the recording
/// takes the mic only.
fn resolve_system(system: SystemSource) -> Option<Result<SystemDevice>> {
    match system {
        SystemSource::Off => None,
        SystemSource::DefaultMonitor => Some(default_monitor_source().map(|name| SystemDevice {
            name,
            follow_default: true,
        })),
        SystemSource::Named(name) => Some(Ok(SystemDevice {
            name,
            follow_default: false,
        })),
    }
}

/// The alarms the mic's writer raises. When the mic has sent only digital
/// silence for [`SILENCE_WARN_AFTER`] (likely muted or the wrong device),
/// and again only after real audio came back in between. And each live
/// quality warning, at most once per recording ([`live_warning_toast`]);
/// `system_heard` is the flag the system track's writer sets.
fn mic_alarm(id: &str, notify: bool, system_heard: Arc<AtomicBool>) -> MicAlarm {
    let silence_id = id.to_string();
    let warning_id = id.to_string();
    let minutes = SILENCE_WARN_AFTER.as_secs() / 60;
    MicAlarm {
        silence_after: SILENCE_WARN_AFTER,
        on_silence: Box::new(move || {
            warn!(
                "recording {silence_id}: the microphone has sent only silence for {minutes} minutes"
            );
            if notify {
                send_notification(
                    "whisrs",
                    &format!(
                        "Recording: your mic has sent only silence for {minutes} minutes. \
                         Is it muted?"
                    ),
                );
            }
        }),
        on_warning: Box::new(move |verdict| {
            warn!("recording {warning_id}: {}", verdict.message());
            if notify {
                send_notification("whisrs", &live_warning_toast(&verdict));
            }
        }),
        system_heard,
    }
}

/// The toast for a live recording warning. A system track that has played
/// nothing may be a solo memo, so that one only points at the setting.
fn live_warning_toast(verdict: &Verdict) -> String {
    match verdict.check {
        Check::SystemSilent => "Recording: nothing has played on the system track yet. If this \
                                is a call, check [recording] system_device."
            .to_string(),
        _ => format!("Recording: {}", verdict.message()),
    }
}

/// Open the mic (required) and the system source (best effort), each with a
/// writer started as soon as its capture is up so the first-chunk times are
/// not skewed by the other device's start. Both writers measure their
/// track with `vad`; the mic's raises `alarm` when the mic is muted and for
/// each live quality warning. Without a system track, `alarm.system_heard`
/// is set here, so no system-silent warning comes. Blocking: runs on the
/// blocking pool, where the runtime handle is available for the writers.
// Reached only through `DEVICES`, which test builds replace with stand-ins.
#[cfg_attr(test, allow(dead_code))]
fn open_tracks(
    dir: &Path,
    mic: &str,
    system: Option<Result<SystemDevice>>,
    vad: Vad,
    alarm: MicAlarm,
) -> Result<OpenedTracks> {
    let mut captures = Vec::new();
    let mut metas = Vec::new();

    let mut handle = AudioCaptureHandle::start_strict(mic)
        .with_context(|| format!("failed to open the microphone ({mic})"))?;
    let rx = handle
        .take_receiver()
        .context("mic capture has no receiver")?;
    let system_heard = Arc::clone(&alarm.system_heard);
    let writer = spawn_mic_writer(
        rx,
        store::audio_path(dir, TrackKind::Mic),
        FLUSH_EVERY,
        vad,
        alarm,
    );
    captures.push(TrackCapture::new(TrackKind::Mic, Some(handle), writer));
    metas.push(track_meta(TrackKind::Mic, mic.to_string()));

    let Some(system) = system else {
        system_heard.store(true, Ordering::Relaxed);
        return Ok((captures, metas, None));
    };
    // The strict start's errors already name the device.
    let opened = system.and_then(
        |SystemDevice {
             name,
             follow_default,
         }| {
            let handle = if follow_default {
                AudioCaptureHandle::start_following_monitor(&name)
            } else {
                AudioCaptureHandle::start_strict(&name)
            };
            handle.map(|h| (name, h))
        },
    );
    match opened {
        Ok((name, mut handle)) => {
            let rx = handle
                .take_receiver()
                .context("system capture has no receiver")?;
            let writer = spawn_system_writer(
                rx,
                store::audio_path(dir, TrackKind::System),
                FLUSH_EVERY,
                vad,
                system_heard,
            );
            captures.push(TrackCapture::new(TrackKind::System, Some(handle), writer));
            metas.push(track_meta(TrackKind::System, name));
            Ok((captures, metas, None))
        }
        Err(e) => {
            system_heard.store(true, Ordering::Relaxed);
            Ok((captures, metas, Some(format!("{e:#}"))))
        }
    }
}

/// The meta of a recording that is starting: each track's language from
/// `options` ([`RecordingOptions::language_for`]), else from the config
/// ([`RecordingConfig::language_for`]), the label of the system track,
/// whether the mic is shared and how many voices to look for, from
/// `options` else the config, and who the recording is with.
fn new_meta(
    id: String,
    title: Option<String>,
    started_at: DateTime<Local>,
    options: &RecordingOptions,
    config: &Config,
) -> RecordingMeta {
    let recording = RecordingConfig::effective(config);
    let language = |kind| {
        options.language_for(kind).map_or_else(
            || recording.language_for(kind, &config.general.language),
            str::to_string,
        )
    };
    let mut meta = RecordingMeta::new(id, title, started_at);
    meta.mic_language = Some(language(TrackKind::Mic));
    meta.system_language = Some(language(TrackKind::System));
    meta.others_label = Some(
        options
            .others
            .clone()
            .unwrap_or_else(|| DEFAULT_OTHERS_LABEL.to_string()),
    );
    meta.shared_mic = Some(options.shared_mic.unwrap_or(recording.shared_mic));
    meta.num_speakers = Some(options.num_speakers.unwrap_or(recording.num_speakers));
    meta.participants = options.participants.clone().unwrap_or_default();
    meta
}

/// Start a recording. `options` are already validated.
async fn start(
    context: &Arc<DaemonContext>,
    title: Option<String>,
    options: &RecordingOptions,
) -> Result<()> {
    let mut slot = context.recording.session.lock().await;
    if context.recording.shutting_down.load(Ordering::Acquire) {
        bail!("the daemon is shutting down");
    }
    if let Some(session) = slot.as_ref() {
        bail!("a recording is already in progress ({})", session.meta.id);
    }
    // Agent replies are held from here, not only once the devices are open,
    // and one playing is stopped and plays after the recording: it would
    // land on the system track.
    let _replies = crate::agent::hold_for_recording(context);

    let recording_cfg = RecordingConfig::effective(&context.config);
    let root = recordings_root(&context.config);
    let title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    check_space_to_start(context.recording.free_space, &root, FREE_SPACE_TIMEOUT).await?;
    let started_at = Local::now();
    let (id, dir) = store::create(&root, started_at, title.as_deref())?;
    let _starting = StartingGuard::new(&context.recording, &dir);

    let mic = mic_device(&recording_cfg, &context.config.audio.device);
    let system = system_source(recording_cfg.system_device.as_deref());
    let devices = context.recording.devices;
    let system = tokio::task::spawn_blocking(move || (devices.resolve_system)(system))
        .await
        .unwrap_or_else(|e| Some(Err(anyhow::anyhow!("system source lookup panicked: {e}"))));

    // Written before any audio exists: a crash or SIGTERM while the devices
    // open leaves a recording recovery can find, never an orphan audio folder.
    let mut meta = new_meta(id.clone(), title, started_at, options, &context.config);
    meta.tracks = vec![track_meta(TrackKind::Mic, mic.clone())];
    if let Some(Ok(system)) = &system {
        meta.tracks
            .push(track_meta(TrackKind::System, system.name.clone()));
    }
    if let Err(e) = store::write_meta(&dir, &meta) {
        remove_recording_dir(&dir).await;
        return Err(e);
    }

    let started = Instant::now();
    let open_dir = dir.clone();
    let alarm = mic_alarm(&id, context.notify, Arc::new(AtomicBool::new(false)));
    let vad = context.config.audio.vad();
    let opened = tokio::task::spawn_blocking(move || {
        (devices.open_tracks)(&open_dir, &mic, system, vad, alarm)
    })
    .await
    .context("audio start panicked")
    .and_then(|r| r);
    let (mut tracks, track_metas, system_error) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            remove_recording_dir(&dir).await;
            return Err(e);
        }
    };
    let stopping = Arc::new(AtomicBool::new(false));
    for track in &mut tracks {
        watch_writer(track, &id, &stopping, context.notify);
    }

    meta.tracks = track_metas;
    if let Some(e) = &system_error {
        warn!("recording {id}: system audio unavailable, recording the mic only: {e}");
        meta.errors.push(format!(
            "System audio unavailable, recorded the mic only: {e}"
        ));
    }
    if let Err(e) = store::write_meta(&dir, &meta) {
        drop(finish_tracks(tracks, &stopping, STOP_TIMEOUT, STOP_GRACE).await);
        remove_recording_dir(&dir).await;
        return Err(e);
    }

    let watch = spawn_watch(
        context,
        id.clone(),
        dir.clone(),
        recording_cfg.max_duration_minutes.max(1),
        DISK_CHECK_EVERY,
    );

    let active = ActiveRecording::from_meta(&dir, &meta);
    info!(
        "recording {id} writing to {} (mic in {}, system audio in {})",
        dir.display(),
        meta.mic_language.as_deref().unwrap_or_default(),
        meta.system_language.as_deref().unwrap_or_default()
    );
    *slot = Some(RecordingSession {
        dir,
        meta,
        started,
        tracks,
        watch: Some(watch),
        stopping,
    });
    // Shown before the session lock is released, so a stop that takes this
    // recording right after cannot have its clear overwritten by this. When
    // both are held, the session lock is taken first; the view lock is never
    // held across an await.
    context.recording.update(|v| v.capturing = Some(active));
    drop(slot);

    notify(context, "Recording your mic and system audio");
    if let Some(e) = system_error {
        notify(
            context,
            &format!(
                "System audio unavailable, recording your mic only: {}",
                truncate_preview(&e, NOTIFY_ERROR_MAX)
            ),
        );
    }
    Ok(())
}

/// Refuse a start when the recordings folder has less than
/// [`DISK_MIN_TO_START`] free. A free space that cannot be read, or not
/// within `timeout` ([`FREE_SPACE_TIMEOUT`]), does not stop a recording: the
/// checks while it runs read it again.
async fn check_space_to_start(
    free_space: FreeSpaceFn,
    root: &Path,
    timeout: Duration,
) -> Result<()> {
    let at = root.to_path_buf();
    let read = tokio::task::spawn_blocking(move || free_space(&at));
    let Ok(read) = tokio::time::timeout(timeout, read).await else {
        warn!(
            "reading the free space in {} took over {} s; starting without the check",
            root.display(),
            timeout.as_secs()
        );
        return Ok(());
    };
    match read {
        Ok(Ok(free)) if free < DISK_MIN_TO_START => bail!(
            "not enough free disk space to record: {} MB free in {}, and a recording \
             needs {} MB (about 10 minutes plus room to transcribe them)",
            free / 1_000_000,
            root.display(),
            DISK_MIN_TO_START / 1_000_000
        ),
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => {
            warn!("could not read the free space in {}: {e}", root.display());
            Ok(())
        }
        Err(e) => {
            warn!("free space check panicked: {e}");
            Ok(())
        }
    }
}

/// What the free space allows a recording in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskRoom {
    Enough,
    /// Less than [`DISK_WARN_MINUTES`] fit: warn once.
    Low(u64),
    /// Less than [`DISK_STOP_MINUTES`] fit: stop now.
    Full(u64),
}

/// How many more minutes of recording fit in `free` bytes after `recorded`
/// of it: the echo-cancelled copy of what is already recorded (one track,
/// written by processing) is held back first.
fn minutes_that_fit(free: u64, recorded: Duration) -> u64 {
    let mic_so_far = recorded.as_secs().saturating_mul(32_000);
    free.saturating_sub(mic_so_far) / DISK_BYTES_PER_MINUTE
}

fn disk_room(free: u64, recorded: Duration) -> DiskRoom {
    match minutes_that_fit(free, recorded) {
        left if left < DISK_STOP_MINUTES => DiskRoom::Full(left),
        left if left < DISK_WARN_MINUTES => DiskRoom::Low(left),
        _ => DiskRoom::Enough,
    }
}

/// Reads the free space for a recording's watch on the blocking pool,
/// waiting at most [`FREE_SPACE_TIMEOUT`] and never past the time limit. A
/// read that timed out (a hung network mount) is kept and waited on again
/// at the next check rather than started anew, so a mount that never
/// answers ties up one blocking thread, not one a minute.
struct FreeSpaceReader {
    read: FreeSpaceFn,
    dir: PathBuf,
    /// [`FREE_SPACE_TIMEOUT`], shorter in tests.
    timeout: Duration,
    pending: Option<JoinHandle<std::io::Result<u64>>>,
    warned: bool,
}

impl FreeSpaceReader {
    fn new(read: FreeSpaceFn, dir: PathBuf) -> Self {
        Self {
            read,
            dir,
            timeout: FREE_SPACE_TIMEOUT,
            pending: None,
            warned: false,
        }
    }

    /// The free space, or `None` when it cannot be read by `limit` or
    /// within its timeout (logged at WARN the first time).
    async fn free(&mut self, id: &str, limit: tokio::time::Instant) -> Option<u64> {
        let mut read = self.pending.take().unwrap_or_else(|| {
            let (read, at) = (self.read, self.dir.clone());
            tokio::task::spawn_blocking(move || read(&at))
        });
        let deadline = limit.min(tokio::time::Instant::now() + self.timeout);
        match tokio::time::timeout_at(deadline, &mut read).await {
            Ok(Ok(Ok(free))) => Some(free),
            Ok(Ok(Err(e))) => {
                debug!("could not read the free space for recording {id}: {e}");
                None
            }
            Ok(Err(_)) => None,
            Err(_) => {
                if !self.warned {
                    self.warned = true;
                    warn!(
                        "recording {id}: reading the free space in {} takes over {} s; \
                         skipping the disk checks until it answers",
                        self.dir.display(),
                        self.timeout.as_secs()
                    );
                }
                self.pending = Some(read);
                None
            }
        }
    }
}

/// Watch a recording in progress: stop it at its `minutes` limit, and every
/// `check_every` read the free space in `dir`, warning once when it runs
/// low and stopping it the way the time limit does when it is almost full.
/// A free-space read never holds up the time limit ([`FreeSpaceReader`]).
fn spawn_watch(
    context: &Arc<DaemonContext>,
    id: String,
    dir: PathBuf,
    minutes: u32,
    check_every: Duration,
) -> JoinHandle<()> {
    let ctx = Arc::clone(context);
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let limit = started + Duration::from_secs(u64::from(minutes) * 60);
        let mut warned = false;
        let mut reader = FreeSpaceReader::new(ctx.recording.free_space, dir);
        loop {
            let next_check = tokio::time::Instant::now() + check_every;
            if next_check >= limit {
                tokio::time::sleep_until(limit).await;
                info!("recording {id} reached the {minutes} minute limit, stopping");
                stop_from_watch(&ctx, &id, EndReason::MaxDuration).await;
                return;
            }
            tokio::time::sleep_until(next_check).await;
            let Some(free) = reader.free(&id, limit).await else {
                continue;
            };
            match disk_room(free, started.elapsed()) {
                DiskRoom::Enough => {}
                DiskRoom::Low(left) => {
                    if !warned {
                        warned = true;
                        warn!("recording {id}: the disk is filling up, about {left} minutes left");
                        notify(
                            &ctx,
                            &format!(
                                "Recording: the disk is filling up, room for about {left} \
                                 more minutes. It stops by itself before the disk is full."
                            ),
                        );
                    }
                }
                DiskRoom::Full(_) => {
                    warn!(
                        "recording {id}: only {} MB free on the disk, stopping",
                        free / 1_000_000
                    );
                    stop_from_watch(&ctx, &id, EndReason::DiskFull).await;
                    return;
                }
            }
        }
    })
}

/// Stop recording `id` from its watch, notifying a failure the stop did not
/// notify itself.
async fn stop_from_watch(context: &Arc<DaemonContext>, id: &str, reason: EndReason) {
    if let Err(e) = stop(context, Some(id), reason).await {
        let message = format!("{e:#}");
        warn!("failed to stop recording {id} ({reason:?}): {message}");
        if !error_already_notified(&message) {
            let when = match reason {
                EndReason::DiskFull => "with the disk almost full",
                _ => "at the time limit",
            };
            notify(
                context,
                &format!(
                    "Recording: failed to stop {when}: {}",
                    truncate_preview(&message, NOTIFY_ERROR_MAX)
                ),
            );
        }
    }
}

/// Result of finalizing one track.
#[derive(Debug)]
struct TrackOutcome {
    kind: TrackKind,
    stats: Result<TrackStats, String>,
    /// The writer was still running after its timeout and grace: its file
    /// may still change, so nothing may repair or read it yet.
    stuck: bool,
    /// The writer ended before the stop (see [`TrackCapture::ended_early`]).
    ended_early: bool,
}

fn joined(
    result: Result<Result<TrackStats>, tokio::task::JoinError>,
) -> Result<TrackStats, String> {
    match result {
        Ok(Ok(stats)) => Ok(stats),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("writer task failed: {e}")),
    }
}

/// Mark the recording as stopping, stop every capture, then wait (bounded)
/// for each writer to finalize its WAV. A writer that misses `limit` has its
/// capture dropped, which closes its channel, and gets `grace` more; one
/// still running after that is reported `stuck`. Capture threads are joined
/// on the blocking pool without waiting: they exit after the stop signal.
async fn finish_tracks(
    mut tracks: Vec<TrackCapture>,
    stopping: &AtomicBool,
    limit: Duration,
    grace: Duration,
) -> Vec<TrackOutcome> {
    stopping.store(true, Ordering::Release);
    for track in &mut tracks {
        if let Some(handle) = track.handle.as_mut() {
            handle.stop();
        }
    }
    let deadline = tokio::time::Instant::now() + limit;
    let mut outcomes = Vec::new();
    for track in tracks {
        let TrackCapture {
            kind,
            mut handle,
            mut writer,
            ended_early,
        } = track;
        let mut stuck = false;
        let stats = match tokio::time::timeout_at(deadline, &mut writer).await {
            Ok(result) => joined(result),
            Err(_) => {
                warn!(
                    "the {} track did not finish writing in {} s; closing its capture",
                    kind.file_stem(),
                    limit.as_secs()
                );
                if let Some(handle) = handle.take() {
                    tokio::task::spawn_blocking(move || drop(handle));
                }
                match tokio::time::timeout(grace, &mut writer).await {
                    Ok(result) => joined(result),
                    Err(_) => {
                        stuck = true;
                        Err("the track was still being written after the stop".to_string())
                    }
                }
            }
        };
        if let Some(handle) = handle {
            tokio::task::spawn_blocking(move || drop(handle));
        }
        outcomes.push(TrackOutcome {
            kind,
            stats,
            stuck,
            ended_early: ended_early.load(Ordering::Acquire),
        });
    }
    outcomes
}

/// Fold the writers' results into the meta: sample counts, offsets, each
/// track's quality, and a note for any track that did not finalize cleanly.
/// A `system-silent` warning the mic's writer raised goes on the system
/// track, which it is about.
fn apply_outcomes(meta: &mut RecordingMeta, started: Instant, outcomes: &[TrackOutcome]) {
    let system_silent = outcomes.iter().any(|o| {
        o.kind == TrackKind::Mic
            && o.stats
                .as_ref()
                .is_ok_and(|s| s.raised.contains(&Check::SystemSilent))
    });
    for outcome in outcomes {
        if outcome.ended_early {
            let note = format!(
                "The {} stopped delivering audio before the recording ended",
                track_label(outcome.kind)
            );
            if !meta.errors.contains(&note) {
                meta.errors.push(note);
            }
        }
        let Some(track) = meta.tracks.iter_mut().find(|t| t.kind == outcome.kind) else {
            continue;
        };
        match &outcome.stats {
            Ok(stats) => {
                track.samples = stats.samples;
                track.offset_ms = offset_ms(started, stats.first_chunk_at);
                track.quality = stats.quality.as_ref().map(TrackQuality::from_report);
                if let Some(quality) = track.quality.as_mut() {
                    if system_silent && track.kind == TrackKind::System {
                        quality.add_warning(Check::SystemSilent);
                    }
                }
            }
            Err(e) => {
                let note = format!("{} track did not finalize: {e}", outcome.kind.file_stem());
                warn!("recording {}: {note}", meta.id);
                meta.errors.push(note);
            }
        }
    }
}

/// Mark a recording as ended and waiting for processing.
fn end_recording(meta: &mut RecordingMeta, reason: EndReason, ended_at: DateTime<Local>) {
    meta.status = RecordingState::Processing;
    meta.ended_at = Some(ended_at);
    meta.end_reason = Some(reason);
}

/// Finalize a track that a writer could not: fix its header and read the
/// sample count off the file. A stuck writer's file is left alone: it may
/// still be written to.
fn repair_unfinished(dir: &Path, meta: &mut RecordingMeta, outcomes: &[TrackOutcome]) {
    for outcome in outcomes.iter().filter(|o| o.stats.is_err() && !o.stuck) {
        let path = store::audio_path(dir, outcome.kind);
        match repair_wav(&path) {
            Ok(rep) => {
                if let Some(t) = meta.tracks.iter_mut().find(|t| t.kind == outcome.kind) {
                    t.samples = rep.samples;
                }
            }
            Err(e) => warn!("could not repair {}: {e:#}", path.display()),
        }
    }
}

/// Stop the recording. `expected_id` (from the recording's watch) makes the
/// stop a no-op when a different recording is in progress by now.
async fn stop(
    context: &Arc<DaemonContext>,
    expected_id: Option<&str>,
    reason: EndReason,
) -> Result<()> {
    let session = {
        let mut slot = context.recording.session.lock().await;
        match slot.as_ref() {
            None if expected_id.is_some() => return Ok(()),
            None => bail!("no recording is in progress"),
            Some(s) if expected_id.is_some_and(|id| id != s.meta.id) => return Ok(()),
            Some(_) => slot.take().expect("checked above"),
        }
    };
    let RecordingSession {
        dir,
        mut meta,
        started,
        tracks,
        watch,
        stopping,
    } = session;
    // The watch is the caller on a time-limit or disk-full stop: aborting it
    // there would cancel this very stop at its next await.
    if let Some(watch) = watch {
        if !matches!(reason, EndReason::MaxDuration | EndReason::DiskFull) {
            watch.abort();
        }
    }

    let outcomes = finish_tracks(tracks, &stopping, STOP_TIMEOUT, STOP_GRACE).await;
    apply_outcomes(&mut meta, started, &outcomes);
    repair_unfinished(&dir, &mut meta, &outcomes);
    end_recording(&mut meta, reason, Local::now());
    let stuck: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.stuck)
        .map(|o| track_label(o.kind))
        .collect();
    if !stuck.is_empty() {
        // Processing now would read a file that is still being written.
        let message = format!(
            "{NOT_TRANSCRIBED}: the {} track was still being written {} s \
             after the stop; retry with: whisrs recording retry {}",
            stuck.join(" and "),
            (STOP_TIMEOUT + STOP_GRACE).as_secs(),
            meta.id
        );
        meta.status = RecordingState::Failed;
        meta.error = Some(message.clone());
        let written = store::write_meta(&dir, &meta);
        context.recording.clear_capturing(&meta.id);
        written?;
        error!("recording {}: {message}", meta.id);
        // Notified here for every caller (CLI, hotkey, tray, time limit);
        // they skip it via [`error_already_notified`], so one stop gives one
        // toast.
        notify(
            context,
            &format!(
                "Recording: {}",
                truncate_preview(&message, NOTIFY_ERROR_MAX)
            ),
        );
        bail!(message);
    }
    let written = store::write_meta(&dir, &meta);
    let active = ActiveRecording::from_meta(&dir, &meta);
    context.recording.clear_capturing(&meta.id);
    written?;
    info!("recording {} stopped ({reason:?})", meta.id);

    let queued = context.recording.enqueue(active);
    match reason {
        EndReason::MaxDuration => notify(
            context,
            &format!(
                "Recording stopped at the {} minute limit. Transcribing...",
                RecordingConfig::effective(&context.config).max_duration_minutes
            ),
        ),
        EndReason::DiskFull => notify(
            context,
            "Recording stopped: the disk is almost full. Transcribing...",
        ),
        _ => notify(context, "Recording stopped. Transcribing..."),
    }
    queued
}

/// Cancel the recording in progress and delete it. Recordings waiting for
/// the network are left alone: their automatic tries end by themselves
/// ([`NETWORK_RETRY_FOR`]).
async fn cancel(context: &Arc<DaemonContext>) -> Result<()> {
    let session = context
        .recording
        .session
        .lock()
        .await
        .take()
        .context("no recording is in progress")?;
    if let Some(watch) = &session.watch {
        watch.abort();
    }
    drop(finish_tracks(session.tracks, &session.stopping, STOP_TIMEOUT, STOP_GRACE).await);
    remove_recording_dir(&session.dir).await;
    context.recording.clear_capturing(&session.meta.id);
    info!("recording {} cancelled and deleted", session.meta.id);
    notify(context, "Recording cancelled and deleted");
    Ok(())
}

async fn remove_recording_dir(dir: &Path) {
    let dir = dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir).map(|_| dir))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r.map_err(anyhow::Error::from));
    if let Err(e) = result {
        warn!("failed to delete recording folder: {e:#}");
    }
}

/// Whether any of the recording's track files is still on disk.
fn has_audio(dir: &Path, meta: &RecordingMeta) -> bool {
    !meta.audio_deleted && meta.tracks.iter().any(|t| dir.join(&t.file).exists())
}

/// Queue a recording for transcription again. The languages,
/// `shared_mic` and `num_speakers` `options` give replace the stored ones
/// (and are saved); the rest keep theirs.
async fn retry(
    context: &Arc<DaemonContext>,
    query: &str,
    options: &RecordingOptions,
) -> Result<()> {
    let root = recordings_root(&context.config);
    let dir = store::resolve(&root, query)?;
    let meta = store::read_meta(&dir)?;
    let recording_id = context
        .recording
        .session
        .lock()
        .await
        .as_ref()
        .map(|s| s.meta.id.clone());
    if recording_id.as_deref() == Some(meta.id.as_str()) {
        bail!("recording {} is still in progress", meta.id);
    }
    if !has_audio(&dir, &meta) {
        bail!(
            "recording {} has no audio left to transcribe (deleted by audio_retention_days)",
            meta.id
        );
    }
    // Claimed before the meta is touched: a retry of a recording that is
    // queued or being processed must not rewrite it under the worker.
    let active = ActiveRecording::from_meta(&dir, &meta);
    context.recording.reserve(&active)?;
    // Repair both tracks first (cheap and idempotent: a crash leftover may
    // still have stale headers), then mark it `processing`. A manual retry
    // is a fresh start for the crash-loop guard.
    let repair_dir = dir.clone();
    let retry_options = options.clone();
    let prepared =
        tokio::task::spawn_blocking(move || recover_dir(&repair_dir, Some(&retry_options)))
            .await
            .context("recording repair panicked")
            .and_then(|r| r);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            context.recording.unreserve(&meta.id);
            return Err(e);
        }
    };
    context.recording.dispatch(active)?;
    info!(
        "recording {} queued for transcription again (mic in {}, system audio in {})",
        meta.id,
        prepared
            .mic_language
            .as_deref()
            .unwrap_or("the configured language"),
        prepared
            .system_language
            .as_deref()
            .unwrap_or("the configured language")
    );
    Ok(())
}

/// Name a speaker of a transcribed recording (`whisrs recording name`):
/// `options` give the speaker and the name (empty clears it). The
/// recording is held while its files are rewritten
/// ([`RecordingRuntime::hold`]), so a recording that is queued or being
/// transcribed is refused, and neither a retry nor the retention sweep
/// rewrites it under the rename.
async fn name(context: &Arc<DaemonContext>, query: &str, options: &RecordingOptions) -> Result<()> {
    let (Some(speaker), Some(name)) = (options.speaker.clone(), options.name.clone()) else {
        bail!("naming a speaker needs the speaker and its name (an empty name clears it)");
    };
    let root = recordings_root(&context.config);
    let dir = store::resolve(&root, query)?;
    let meta = store::read_meta(&dir)?;
    // Checked before the hold too, so one never holds back the stop of a
    // recording in progress.
    whisrs::recording::naming::ensure_transcribed(&meta)?;
    if context.recording.hold(&meta.id).is_err() {
        bail!(
            "recording {} is queued or being transcribed: name its speakers once its \
             transcript is ready",
            meta.id
        );
    }
    let ctx = Arc::clone(context);
    let rename_dir = dir.clone();
    let renamed = tokio::task::spawn_blocking(move || {
        whisrs::recording::naming::rename_speaker(&rename_dir, &ctx.config, &speaker, &name)
    })
    .await
    .context("speaker naming panicked")
    .and_then(|r| r);
    context.recording.release(&meta.id);
    let (id, renamed) = renamed?;
    let speaker = renamed.speakers.iter().find(|s| s.id == id);
    info!(
        "recording {}: speaker {id} is now {:?}",
        renamed.id,
        speaker.map_or("", |s| s.name.as_str())
    );
    Ok(())
}

/// Queue a transcribed recording for a summary (`whisrs recording
/// summarize`), whatever `[recording] summary` says ([`queue_summary`]).
/// One without a transcript, or a config without an `[llm]`, is refused
/// before anything is queued.
async fn summarize(context: &Arc<DaemonContext>, query: &str) -> Result<()> {
    let root = recordings_root(&context.config);
    let dir = store::resolve(&root, query)?;
    let meta = store::read_meta(&dir)?;
    summary::ensure_summarizable(&meta, &context.config)?;
    queue_summary(
        context,
        ActiveRecording::from_meta(&dir, &meta),
        SummaryRequest::Asked,
    )
    .await
}

/// Queue a summary job for `recording`, wanted by `request`. It goes
/// through the one processing queue, so it never runs beside a
/// transcription (waiting summaries run after waiting transcriptions,
/// [`next_job`]); a recording that is queued, being processed or held is
/// refused. Its `summary_pending` mark is set first, so a summary that a
/// shutdown or a crash interrupts is queued again at the next start
/// ([`resume_summaries`]).
async fn queue_summary(
    context: &Arc<DaemonContext>,
    recording: ActiveRecording,
    request: SummaryRequest,
) -> Result<()> {
    let active = ActiveRecording {
        job: Job::Summarize,
        ..recording
    };
    context.recording.reserve(&active)?;
    let mark_dir = active.dir.clone();
    let marked = tokio::task::spawn_blocking(move || summary::mark_pending(&mark_dir, request))
        .await
        .context("summary bookkeeping panicked")
        .and_then(|r| r);
    if let Err(e) = marked {
        context.recording.unreserve(&active.id);
        return Err(e);
    }
    let id = active.id.clone();
    context.recording.dispatch(active)?;
    info!("recording {id} queued for a summary");
    Ok(())
}

async fn copy(context: &Arc<DaemonContext>, query: &str) -> Result<()> {
    let root = recordings_root(&context.config);
    let dir = store::resolve(&root, query)?;
    let meta = store::read_meta(&dir)?;
    let path = store::transcript_path(&dir, &meta.id);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let state = serde_json::to_string(&meta.status).unwrap_or_default();
            let mut message = format!(
                "recording {} has no transcript yet (status {})",
                meta.id,
                state.trim_matches('"')
            );
            if meta.status == RecordingState::Failed {
                message.push_str(&format!(". Retry with: whisrs recording retry {}", meta.id));
            }
            bail!(message);
        }
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    tokio::task::spawn_blocking(move || xkb_type::default_clipboard().set_text(&text))
        .await
        .context("clipboard task panicked")?
        .context("failed to copy the transcript to the clipboard")?;
    notify(context, "Recording transcript copied to the clipboard");
    Ok(())
}

/// The processing worker: one recording at a time, in queue order, on its own
/// task so a long transcription never holds up IPC or dictation.
pub(crate) async fn run_worker(
    context: Arc<DaemonContext>,
    mut queue: mpsc::UnboundedReceiver<QueuedRecording>,
) {
    let mut pending = VecDeque::new();
    loop {
        if pending.is_empty() {
            match queue.recv().await {
                Some(item) => pending.push_back(item),
                None => return,
            }
        }
        while let Ok(item) = queue.try_recv() {
            pending.push_back(item);
        }
        if let Some(item) = next_job(&mut pending) {
            process_one(&context, item).await;
        }
    }
}

/// The job the worker runs next: the first transcription waiting, else the
/// first summary. Waiting summaries never hold up a waiting transcript, but
/// one worker runs one job at a time (a local LLM beside local whisper is
/// too much memory for a small machine): a summary already running delays
/// the next transcription until it ends, each LLM call up to
/// [`SUMMARY_TIMEOUT`].
fn next_job(pending: &mut VecDeque<QueuedRecording>) -> Option<QueuedRecording> {
    let at = pending
        .iter()
        .position(|item| item.recording.job == Job::Transcribe)
        .unwrap_or(0);
    pending.remove(at)
}

async fn process_one(context: &Arc<DaemonContext>, item: QueuedRecording) {
    let QueuedRecording { recording } = item;
    let id = recording.id.clone();
    let dir = recording.dir.clone();
    let network_retry = recording.network_retry;
    let job = recording.job;
    context.recording.update(|v| {
        v.queued.retain(|q| q.id != id);
        v.processing = Some(recording);
    });
    if job == Job::Summarize {
        summarize_one(context, &dir, &id).await;
        context.recording.update(|v| v.processing = None);
        sweep(context).await;
        return;
    }

    // Counted before the attempt, so one that takes the daemon down with it
    // still counts towards [`MAX_ATTEMPTS`] at the next start. A graceful
    // shutdown gives it back ([`stop_worker`]). An automatic network retry
    // counts towards its own cap ([`NETWORK_RETRY_MAX`]) instead.
    let counted = if network_retry {
        count_network_attempt(&dir)
    } else {
        count_attempt(&dir)
    };
    if let Err(e) = counted {
        error!("failed to record a processing attempt for recording {id}: {e:#}");
    }
    // A panic in a backend or in the processing itself must not leave the
    // recording shown as processing forever, nor kill the worker and with it
    // every later recording. Caught in this task (not a spawned one) so a
    // shutdown that aborts the worker still drops the work with it.
    let result = AssertUnwindSafe(process_recording(
        &dir,
        &context.config,
        Some(Arc::clone(&context.transcription_backend)),
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| {
        Err(anyhow::anyhow!(
            "processing crashed: {}",
            panic_message(&*panic)
        ))
    });
    let mut summarize_next = None;
    match result {
        Ok(meta) => {
            let md = store::transcript_path(&dir, &meta.id);
            info!("recording {id} transcript ready: {}", md.display());
            notify(context, &ready_message(&md, &meta));
            if RecordingConfig::effective(&context.config).summary {
                summarize_next = Some(meta);
            }
        }
        Err(e) if is_network_unavailable(&e) => {
            wait_for_network(context, &dir, &id, &format!("{e:#}")).await
        }
        Err(e) => {
            let message = format!("{e:#}");
            error!("recording {id} transcription failed: {message}");
            if let Err(write_err) = mark_failed(&dir, &message) {
                error!("failed to record the failure of recording {id}: {write_err:#}");
            }
            notify(
                context,
                &format!(
                    "Recording transcription failed: {}. Retry with: whisrs recording retry {id}",
                    truncate_preview(&message, NOTIFY_ERROR_MAX)
                ),
            );
        }
    }
    context.recording.update(|v| v.processing = None);
    // With `[recording] summary`, the summary follows as a job of its own:
    // the transcript is out and notified first, and a summary that fails,
    // panics or is interrupted never touches the transcript's status.
    if let Some(meta) = summarize_next {
        let recording = ActiveRecording::from_meta(&dir, &meta);
        if let Err(e) = queue_summary(context, recording, SummaryRequest::Auto).await {
            warn!("recording {id}: the summary was not queued: {e:#}");
        }
    }
    sweep(context).await;
}

/// How long a summary job waits to write its result while a rename or the
/// sweep holds the recording.
const SUMMARY_WRITE_WAIT: Duration = Duration::from_secs(30);

/// Run a summary job and notify how it went. The LLM works from a
/// snapshot of `recording.json` ([`summary::snapshot`], [`summary::make`])
/// without holding the recording, so `recording name` and the retention
/// sweep go on meanwhile; the result is written under a short hold
/// ([`summary::write_result`]), merged into the recording as it is then. A
/// recording deleted or transcribed again meanwhile drops the result. A
/// panic is caught like one in processing.
async fn summarize_one(context: &Arc<DaemonContext>, dir: &Path, id: &str) {
    let snap_dir = dir.to_path_buf();
    let snapshot = tokio::task::spawn_blocking(move || summary::snapshot(&snap_dir))
        .await
        .context("summary bookkeeping panicked")
        .and_then(|r| r);
    let snapshot = match snapshot {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            info!("recording {id} is gone; its summary is dropped");
            return;
        }
        Err(e) => {
            summary_failed(context, dir, id, &format!("{e:#}")).await;
            return;
        }
    };
    let made = AssertUnwindSafe(summary::make(&context.config, &snapshot, SUMMARY_TIMEOUT))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            Err(format!(
                "{SUMMARY_FAILED}the summary crashed: {}",
                panic_message(&*panic)
            ))
        });
    let Some(held) = hold_to_write(context, id).await else {
        warn!(
            "recording {id} stayed busy for {SUMMARY_WRITE_WAIT:?}; its summary is dropped, and \
             made again at the next start if it was asked for or summaries are still on"
        );
        return;
    };
    let write_dir = dir.to_path_buf();
    let written =
        tokio::task::spawn_blocking(move || summary::write_result(&write_dir, &snapshot, made))
            .await
            .context("summary bookkeeping panicked")
            .and_then(|r| r);
    drop(held);
    match written {
        Ok(Some((meta, Ok(())))) => {
            let md = store::transcript_path(dir, &meta.id);
            info!("recording {id} summary ready: {}", md.display());
            notify(
                context,
                &format!("Recording summary ready: {}", md.display()),
            );
        }
        Ok(Some((_, Err(note)))) => {
            let why = note.strip_prefix(SUMMARY_FAILED).unwrap_or(&note);
            notify(
                context,
                &format!(
                    "Recording summary failed: {}",
                    truncate_preview(why, NOTIFY_ERROR_MAX)
                ),
            );
        }
        Ok(None) => info!(
            "recording {id} was deleted or transcribed again during its summary; the summary \
             is dropped"
        ),
        Err(e) => summary_failed(context, dir, id, &format!("{e:#}")).await,
    }
}

/// A [`RecordingRuntime::hold`] on `id` for a summary job's write, waiting
/// up to [`SUMMARY_WRITE_WAIT`] for a rename or the sweep to let go.
async fn hold_to_write(context: &Arc<DaemonContext>, id: &str) -> Option<Held> {
    let deadline = tokio::time::Instant::now() + SUMMARY_WRITE_WAIT;
    loop {
        if context.recording.hold(id).is_ok() {
            return Some(Held {
                context: Arc::clone(context),
                id: id.to_string(),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A summary job that could not run or write: logged, its pending mark
/// cleared, and notified.
async fn summary_failed(context: &Arc<DaemonContext>, dir: &Path, id: &str, why: &str) {
    error!("recording {id} summary failed: {why}");
    let clear_dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || summary::clear_pending(&clear_dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("recording {id}: failed to clear its pending summary: {e:#}"),
        Err(e) => warn!("recording {id}: summary bookkeeping panicked: {e}"),
    }
    notify(
        context,
        &format!(
            "Recording summary failed: {}",
            truncate_preview(why, NOTIFY_ERROR_MAX)
        ),
    );
}

/// The notification for a finished transcript at `md`. When its voices
/// could not be told apart it says so and why: the transcript is fine, but
/// not what speaker separation promised, and this is the one place the
/// user hears of it (`recording.json` and the transcript's notes keep it).
fn ready_message(md: &Path, meta: &RecordingMeta) -> String {
    let lost = meta
        .errors
        .iter()
        .filter(|note| note.starts_with(USER_NAME_LOST))
        .count();
    let lost = match lost {
        0 => String::new(),
        1 => "; 1 speaker name you gave no longer matches a voice".to_string(),
        n => format!("; {n} speaker names you gave no longer match a voice"),
    };
    match separation_failure(meta) {
        Some(why) => format!(
            "Recording transcript ready, without speakers ({}){lost}: {}",
            truncate_preview(why, NOTIFY_ERROR_MAX),
            md.display()
        ),
        None => format!("Recording transcript ready{lost}: {}", md.display()),
    }
}

/// Mark recording `id` as waiting for the network after an attempt could
/// not reach the backend, and show it so. Notified only when it starts
/// waiting: the automatic attempts that fail after that stay quiet. One
/// that ran past its deadline waits again, quietly, and the next tick of
/// [`run_network_retries`] gives up on it ([`network_retry_due`]).
async fn wait_for_network(context: &Arc<DaemonContext>, dir: &Path, id: &str, message: &str) {
    match mark_waiting(dir, message, Local::now()) {
        Ok((meta, started)) => {
            warn!("recording {id} is waiting for the network: {message}");
            context
                .recording
                .show_waiting(ActiveRecording::from_meta(dir, &meta));
            if started {
                notify(
                    context,
                    &format!(
                        "Recording: no network. whisrs tries again every {} minutes \
                         for up to {} hours; whisrs recording retry {id} tries now",
                        NETWORK_RETRY_EVERY.as_secs() / 60,
                        NETWORK_RETRY_FOR.as_secs() / 3600
                    ),
                );
            }
        }
        Err(e) => error!("failed to record that recording {id} waits for the network: {e:#}"),
    }
}

/// Fail recording `id` that was waiting for the network, keeping its audio
/// for `whisrs recording retry`, and notify why. Blocking writes run off
/// the runtime.
async fn stopped_waiting(context: &Arc<DaemonContext>, dir: &Path, id: &str, why: &str) {
    let fail_dir = dir.to_path_buf();
    let reason = why.to_string();
    let failed = tokio::task::spawn_blocking(move || fail_wait(&fail_dir, &reason))
        .await
        .context("recording bookkeeping panicked")
        .and_then(|r| r);
    context.recording.hide_waiting(id);
    match failed {
        Ok(()) => {
            warn!("recording {id}: {why}");
            notify(
                context,
                &format!("Recording {why}. Retry with: whisrs recording retry {id}"),
            );
        }
        Err(e) => error!("failed to record the failure of recording {id}: {e:#}"),
    }
}

/// The text of a caught panic's payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn count_attempt(dir: &Path) -> Result<()> {
    let mut meta = store::read_meta(dir)?;
    meta.attempts = meta.attempts.saturating_add(1);
    store::write_meta(dir, &meta)
}

fn mark_failed(dir: &Path, message: &str) -> Result<()> {
    let mut meta = store::read_meta(dir)?;
    meta.status = RecordingState::Failed;
    meta.error = Some(message.to_string());
    store::write_meta(dir, &meta)
}

/// Count an automatic network retry ([`NetworkWait::attempts`]), never
/// [`RecordingMeta::attempts`].
fn count_network_attempt(dir: &Path) -> Result<()> {
    let mut meta = store::read_meta(dir)?;
    if let Some(wait) = meta.network_wait.as_mut() {
        wait.attempts = wait.attempts.saturating_add(1);
    }
    store::write_meta(dir, &meta)
}

/// Mark a recording as waiting for the network after an attempt failed for
/// lack of it at `now`, starting its [`NetworkWait`] (and with it the
/// [`NETWORK_RETRY_FOR`] window) if it has none: a recording's first failed
/// try, or one after a manual retry, which clears the old wait. The flag is
/// true when the wait started here, which is when it is notified.
fn mark_waiting(dir: &Path, message: &str, now: DateTime<Local>) -> Result<(RecordingMeta, bool)> {
    let mut meta = store::read_meta(dir)?;
    meta.status = RecordingState::Waiting;
    meta.error = Some(message.to_string());
    let started = meta.network_wait.is_none();
    let wait = meta.network_wait.get_or_insert(NetworkWait {
        since: now,
        attempts: 0,
        last_attempt_at: now,
    });
    wait.last_attempt_at = now;
    store::write_meta(dir, &meta)?;
    Ok((meta, started))
}

/// End a recording's wait for the network: `failed`, with `why` before the
/// last network error. Its audio and chunk cache stay for a retry.
fn fail_wait(dir: &Path, why: &str) -> Result<()> {
    let mut meta = store::read_meta(dir)?;
    meta.status = RecordingState::Failed;
    meta.error = Some(match meta.error.take() {
        Some(last) => format!("{why}: {last}"),
        None => why.to_string(),
    });
    store::write_meta(dir, &meta)
}

/// Why a recording stopped waiting for the network at its deadline, the one
/// sentence for it: stored as "{why}: {last error}" and notified as
/// "Recording {why}." ([`stopped_waiting`]).
fn network_window_over() -> String {
    format!(
        "gave up waiting for the network {} hours after its first failed try",
        NETWORK_RETRY_FOR.as_secs() / 3600
    )
}

/// What [`retry_waiting`] does with a recording waiting for the network.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NetworkRetry {
    /// Queue an automatic attempt now.
    Due,
    /// Its last attempt was less than [`NETWORK_RETRY_EVERY`] ago.
    NotYet,
    /// Past [`RecordingMeta::network_deadline`] or [`NETWORK_RETRY_MAX`]:
    /// fail it, for the reason given.
    GiveUp(String),
}

/// Whether a recording waiting for the network gets an automatic attempt at
/// `now`. `startup` makes it due whenever its last attempt was: the network
/// may well be back after a reboot.
fn network_retry_due(meta: &RecordingMeta, now: DateTime<Local>, startup: bool) -> NetworkRetry {
    let Some(wait) = &meta.network_wait else {
        return NetworkRetry::Due;
    };
    if meta
        .network_deadline()
        .is_some_and(|deadline| now >= deadline)
    {
        return NetworkRetry::GiveUp(network_window_over());
    }
    if wait.attempts >= NETWORK_RETRY_MAX {
        return NetworkRetry::GiveUp(format!(
            "gave up waiting for the network after {} automatic attempts",
            wait.attempts
        ));
    }
    let every = chrono::Duration::from_std(NETWORK_RETRY_EVERY).unwrap_or(chrono::Duration::MAX);
    if startup || now.signed_duration_since(wait.last_attempt_at) >= every {
        NetworkRetry::Due
    } else {
        NetworkRetry::NotYet
    }
}

/// Queue an automatic attempt of every recording waiting for the network
/// that is due ([`network_retry_due`]), and fail the ones out of time or
/// attempts. At `startup` the waiting recordings are read from disk, later
/// from the view. Each goes through the processing queue like any other
/// recording, so it never runs beside another one, and is reserved first, so
/// a manual retry and this never take it twice.
async fn retry_waiting(context: &Arc<DaemonContext>, now: DateTime<Local>, startup: bool) {
    let candidates: Vec<ActiveRecording> = if startup {
        let root = recordings_root(&context.config);
        tokio::task::spawn_blocking(move || store::waiting(&root))
            .await
            .unwrap_or_default()
            .iter()
            .map(|(dir, meta)| ActiveRecording::from_meta(dir, meta))
            .collect()
    } else {
        context
            .recording
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting
            .clone()
    };
    for recording in candidates {
        let id = recording.id.clone();
        let meta = match store::read_meta(&recording.dir) {
            Ok(meta) if meta.status == RecordingState::Waiting => meta,
            Ok(_) => {
                context.recording.hide_waiting(&id);
                continue;
            }
            Err(e) => {
                warn!("failed to read recording {id}: {e:#}");
                context.recording.hide_waiting(&id);
                continue;
            }
        };
        let attempt = ActiveRecording {
            network_retry: true,
            ..ActiveRecording::from_meta(&recording.dir, &meta)
        };
        match network_retry_due(&meta, now, startup) {
            NetworkRetry::NotYet => context.recording.show_waiting(attempt),
            NetworkRetry::GiveUp(why) => {
                if context.recording.reserve(&attempt).is_ok() {
                    stopped_waiting(context, &recording.dir, &id, &why).await;
                    context.recording.unreserve(&id);
                }
            }
            NetworkRetry::Due => {
                if context.recording.reserve(&attempt).is_err() {
                    continue;
                }
                info!("recording {id}: trying the network again");
                if let Err(e) = context.recording.dispatch(attempt) {
                    warn!("failed to queue recording {id}: {e:#}");
                }
            }
        }
    }
}

/// Every [`NETWORK_RETRY_TICK`], queue the recordings waiting for the network
/// whose next attempt is due. The first pass runs at startup, from
/// [`recover_on_startup`].
pub(crate) async fn run_network_retries(context: Arc<DaemonContext>) {
    loop {
        tokio::time::sleep(NETWORK_RETRY_TICK).await;
        retry_waiting(&context, Local::now(), false).await;
    }
}

/// A [`RecordingRuntime::hold`] on one recording, released when dropped.
struct Held {
    context: Arc<DaemonContext>,
    id: String,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.context.recording.release(&self.id);
    }
}

/// Delete the audio of finished recordings past `audio_retention_days`,
/// each held while its `recording.json` is rewritten, so a rename is never
/// lost to it; one that is busy is swept next time.
async fn sweep(context: &Arc<DaemonContext>) {
    let root = recordings_root(&context.config);
    let days = RecordingConfig::effective(&context.config).audio_retention_days;
    let ctx = Arc::clone(context);
    let swept = tokio::task::spawn_blocking(move || {
        store::sweep_retention_guarded(&root, days, Local::now(), |id| {
            ctx.recording.hold(id).ok().map(|()| Held {
                context: Arc::clone(&ctx),
                id: id.to_string(),
            })
        })
    })
    .await
    .unwrap_or_default();
    for dir in swept {
        info!("deleted recording audio past retention: {}", dir.display());
    }
}

/// Update an interrupted recording's meta from its repaired tracks: sample
/// counts from the files, and a recording that never stopped becomes a
/// `Recovered` one ending at `ended_fallback`. A recording already stopped
/// (`Processing`, e.g. ended by a shutdown) keeps its end.
fn recover_meta(
    meta: &mut RecordingMeta,
    repaired: &[(TrackKind, u64)],
    ended_fallback: DateTime<Local>,
) {
    for (kind, samples) in repaired {
        if let Some(track) = meta.tracks.iter_mut().find(|t| t.kind == *kind) {
            track.samples = *samples;
        }
    }
    if meta.status == RecordingState::Recording {
        meta.end_reason = Some(EndReason::Recovered);
        if meta.ended_at.is_none() {
            meta.ended_at = Some(ended_fallback);
        }
    }
    meta.status = RecordingState::Processing;
    meta.error = None;
    if meta.attempts >= MAX_ATTEMPTS {
        meta.status = RecordingState::Failed;
        meta.error = Some(format!(
            "processing was interrupted {} times; retry with whisrs recording retry {}",
            meta.attempts, meta.id
        ));
    }
}

/// Repair an interrupted recording's WAVs and update its meta. Blocking.
/// `retry` (a manual retry) zeroes the crash-loop count first and replaces
/// the settings it gives ([`RecordingMeta::apply_options`]).
fn recover_dir(dir: &Path, retry: Option<&RecordingOptions>) -> Result<RecordingMeta> {
    let mut meta = store::read_meta(dir)?;
    if let Some(options) = retry {
        meta.attempts = 0;
        meta.network_wait = None;
        meta.apply_options(options);
    }
    let mut repaired = Vec::new();
    let mut newest: Option<std::time::SystemTime> = None;
    if !meta.audio_deleted {
        for track in meta.tracks.clone() {
            let path = dir.join(&track.file);
            if !path.exists() {
                continue;
            }
            match repair_wav(&path) {
                Ok(outcome) => {
                    if outcome.changed {
                        info!("repaired {}", path.display());
                    }
                    repaired.push((track.kind, outcome.samples));
                }
                Err(e) => {
                    let note = format!("{} track could not be repaired: {e:#}", track.file);
                    warn!("recording {}: {note}", meta.id);
                    meta.errors.push(note);
                }
            }
            if let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                newest = Some(newest.map_or(modified, |n| n.max(modified)));
            }
        }
    }
    let ended = newest
        .map(DateTime::<Local>::from)
        .unwrap_or_else(Local::now);
    recover_meta(&mut meta, &repaired, ended);
    store::write_meta(dir, &meta)?;
    Ok(meta)
}

/// Recover one interrupted recording and queue it. It is reserved in the
/// queue before anything on disk is touched, like a retry: a retry that
/// already holds it makes this skip it, and a retry arriving meanwhile is
/// refused, so the two never repair or requeue it twice.
async fn recover_one(context: &Arc<DaemonContext>, dir: PathBuf) {
    let meta = match store::read_meta(&dir) {
        Ok(meta) => meta,
        Err(e) => {
            warn!("failed to recover recording {}: {e:#}", dir.display());
            return;
        }
    };
    let active = ActiveRecording::from_meta(&dir, &meta);
    if context.recording.reserve(&active).is_err() {
        info!(
            "recording {} is already queued (retried meanwhile); not recovering it",
            meta.id
        );
        return;
    }
    let id = meta.id;
    let repair_dir = dir.clone();
    let recovered = tokio::task::spawn_blocking(move || recover_interrupted(&repair_dir))
        .await
        .context("recording recovery panicked")
        .and_then(|r| r);
    let meta = match recovered {
        Ok(Some(meta)) => meta,
        Ok(None) => {
            // A retry finished it between the listing and the reservation.
            context.recording.unreserve(&id);
            return;
        }
        Err(e) => {
            context.recording.unreserve(&id);
            warn!("failed to recover recording {}: {e:#}", dir.display());
            return;
        }
    };
    if meta.status == RecordingState::Failed {
        context.recording.unreserve(&id);
        warn!(
            "not resuming recording {id}: {}",
            meta.error.as_deref().unwrap_or_default()
        );
        notify(
            context,
            &format!(
                "Recording transcription stopped after {} interrupted attempts. \
                 Retry with: whisrs recording retry {id}",
                meta.attempts
            ),
        );
        return;
    }
    info!(
        "resuming interrupted recording {id} ({:?})",
        meta.end_reason
    );
    if let Err(e) = context
        .recording
        .dispatch(ActiveRecording::from_meta(&dir, &meta))
    {
        warn!("failed to queue recording {id}: {e:#}");
    }
}

/// [`recover_dir`] for a recording still interrupted on disk. `None` when it
/// no longer is (a retry processed it after the startup listing), so a
/// finished recording is never flipped back to processing. Blocking.
fn recover_interrupted(dir: &Path) -> Result<Option<RecordingMeta>> {
    let meta = store::read_meta(dir)?;
    if !matches!(
        meta.status,
        RecordingState::Recording | RecordingState::Processing
    ) {
        return Ok(None);
    }
    recover_dir(dir, None).map(Some)
}

/// At startup: requeue every recording a crash, reboot or shutdown left
/// capturing or processing, then apply audio retention. Spawned, so the
/// IPC server comes up without waiting for it.
pub(crate) async fn recover_on_startup(context: Arc<DaemonContext>) {
    let root = recordings_root(&context.config);
    // Listed under the session lock: a start in progress finishes first and
    // its recording is skipped below, and one that begins after the listing
    // creates a folder the listing does not have. Recovering it would repair
    // and requeue a recording that is still being written.
    let (interrupted, recording) = {
        let slot = context.recording.session.lock().await;
        let recording = slot.as_ref().map(|s| s.dir.clone());
        let scan_root = root.clone();
        let interrupted = tokio::task::spawn_blocking(move || store::interrupted(&scan_root))
            .await
            .unwrap_or_default();
        (interrupted, recording)
    };
    // Oldest first, so they are processed in the order they happened.
    for dir in interrupted.into_iter().rev() {
        if recording.as_ref() == Some(&dir) {
            continue;
        }
        recover_one(&context, dir).await;
    }
    resume_summaries(&context).await;
    // The network may be back after a restart: try the waiting ones now.
    retry_waiting(&context, Local::now(), true).await;
    sweep(&context).await;
}

/// At startup: queue again every summary a shutdown or a crash
/// interrupted, or that was still queued (`summary_pending`), oldest
/// first, from the headers. One the user asked for always; an automatic
/// one only while `[recording] summary` is still on, else its mark is
/// cleared.
async fn resume_summaries(context: &Arc<DaemonContext>) {
    let root = recordings_root(&context.config);
    let pending: Vec<(PathBuf, store::RecordingHeader)> = tokio::task::spawn_blocking(move || {
        store::list_headers(&root)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .filter(|(_, h)| h.summary_pending.is_some() && h.status == RecordingState::Done)
            .collect()
    })
    .await
    .unwrap_or_default();
    let summary_on = RecordingConfig::effective(&context.config).summary;
    for (dir, header) in pending {
        let id = header.id.clone();
        let request = header.summary_pending.unwrap_or(SummaryRequest::Asked);
        if request == SummaryRequest::Auto && !summary_on {
            info!(
                "recording {id}: its automatic summary is not made: [recording] summary is \
                 off now"
            );
            let clear_dir = dir.clone();
            if let Ok(Err(e)) =
                tokio::task::spawn_blocking(move || summary::clear_pending(&clear_dir)).await
            {
                warn!("recording {id}: failed to clear its pending summary: {e:#}");
            }
            continue;
        }
        info!("recording {id}: resuming its interrupted summary");
        let recording = ActiveRecording::from_header(&dir, &header);
        if let Err(e) = queue_summary(context, recording, request).await {
            warn!("recording {id}: the summary was not queued again: {e:#}");
        }
    }
}

/// On SIGTERM/SIGINT: finalize a recording in progress (bounded) and leave it
/// `processing` with `end_reason: shutdown`, so the next start transcribes
/// it. A recording mid-processing is already `processing` on disk and is
/// resumed the same way.
///
/// A start in progress holds the session lock while its devices open; this
/// waits for it ([`SHUTDOWN_LOCK_TIMEOUT`]) and then stops what it started.
/// A start that was waiting behind it on the lock refuses to run.
pub(crate) async fn shutdown(context: &DaemonContext) {
    context
        .recording
        .shutting_down
        .store(true, Ordering::Release);
    stop_worker(&context.recording).await;
    let mut slot =
        match tokio::time::timeout(SHUTDOWN_LOCK_TIMEOUT, context.recording.session.lock()).await {
            Ok(slot) => slot,
            Err(_) => {
                abandon_start(&context.recording).await;
                return;
            }
        };
    let Some(session) = slot.take() else {
        return;
    };
    let RecordingSession {
        dir,
        mut meta,
        started,
        tracks,
        watch,
        stopping,
    } = session;
    if let Some(watch) = watch {
        watch.abort();
    }
    let outcomes = finish_tracks(tracks, &stopping, SHUTDOWN_TIMEOUT, SHUTDOWN_GRACE).await;
    apply_outcomes(&mut meta, started, &outcomes);
    end_recording(&mut meta, EndReason::Shutdown, Local::now());
    match store::write_meta(&dir, &meta) {
        Ok(()) => info!(
            "recording {} saved at shutdown; it will be transcribed on the next start",
            meta.id
        ),
        Err(e) => error!("failed to save recording {} at shutdown: {e:#}", meta.id),
    }
    context.recording.clear_capturing(&meta.id);
}

/// Abort the processing worker, so an interrupted transcription is resumed
/// on the next start without counting as a crashed attempt: the attempt it
/// counted up front ([`process_one`]) is given back once the task is gone.
async fn stop_worker(runtime: &RecordingRuntime) {
    let worker = runtime
        .worker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(worker) = worker else {
        return;
    };
    worker.abort();
    match tokio::time::timeout(WORKER_ABORT_TIMEOUT, worker).await {
        Err(_) => {
            // Still running: giving the attempt back could race its writes.
            warn!("the recording processing worker did not stop in time");
            return;
        }
        // Only an attempt this abort interrupted is given back. A worker
        // that panicked was not interrupted by the shutdown: its attempt is
        // the crash the crash-loop guard counts.
        Ok(Err(e)) if e.is_cancelled() => {}
        Ok(Err(e)) => {
            warn!("the recording processing worker had failed: {e}");
            return;
        }
        // The queue closed and the worker ended on its own: nothing was
        // interrupted.
        Ok(Ok(())) => return,
    }
    // Read only now, with the task gone, so the view is final.
    let processing = runtime
        .view
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .processing
        .clone();
    let Some(ActiveRecording {
        id,
        dir,
        network_retry,
        job,
        ..
    }) = processing
    else {
        return;
    };
    if job == Job::Summarize {
        // A summary counts no attempt and leaves the recording done; its
        // pending mark stays, so the next start queues it again.
        info!(
            "recording {id}: summary interrupted by shutdown; it is queued again at the next start"
        );
        return;
    }
    if network_retry {
        // It counted no attempt: put it back to waiting, so the next start
        // retries it as one more automatic attempt rather than an
        // interrupted one.
        match tokio::task::spawn_blocking(move || rewait(&dir)).await {
            Ok(Ok(())) => info!(
                "recording {id}: network retry interrupted by shutdown; it is retried on the next start"
            ),
            Ok(Err(e)) => warn!("recording {id}: failed to put it back to waiting: {e:#}"),
            Err(e) => warn!("recording {id}: attempt bookkeeping panicked: {e}"),
        }
        return;
    }
    match tokio::task::spawn_blocking(move || uncount_attempt(&dir)).await {
        Ok(Ok(true)) => info!(
            "recording {id}: transcription interrupted by shutdown; it resumes on the next start"
        ),
        Ok(Ok(false)) => {}
        Ok(Err(e)) => warn!("recording {id}: failed to give back the interrupted attempt: {e:#}"),
        Err(e) => warn!("recording {id}: attempt bookkeeping panicked: {e}"),
    }
}

/// Put a recording whose automatic network retry a shutdown interrupted back
/// to waiting. Only one still `processing` is touched. Blocking.
fn rewait(dir: &Path) -> Result<()> {
    let mut meta = store::read_meta(dir)?;
    if meta.status == RecordingState::Processing && meta.network_wait.is_some() {
        meta.status = RecordingState::Waiting;
        store::write_meta(dir, &meta)?;
    }
    Ok(())
}

/// Undo [`count_attempt`] for a recording a shutdown interrupted. Only a
/// recording still `processing` is touched: one that finished or failed
/// before the abort keeps its count. Blocking.
fn uncount_attempt(dir: &Path) -> Result<bool> {
    let mut meta = store::read_meta(dir)?;
    if meta.status != RecordingState::Processing || meta.attempts == 0 {
        return Ok(false);
    }
    meta.attempts = meta.attempts.saturating_sub(1);
    store::write_meta(dir, &meta)?;
    Ok(true)
}

/// Whether any track of the recording in `dir` holds a sample on disk.
fn has_audio_frames(dir: &Path) -> bool {
    [TrackKind::Mic, TrackKind::System].iter().any(|kind| {
        std::fs::metadata(store::audio_path(dir, *kind)).is_ok_and(|m| m.len() > WAV_HEADER_LEN)
    })
}

/// Shutdown could not wait for a start still opening its devices. With no
/// audio written yet there is nothing to transcribe, so its folder is
/// deleted instead of being left for recovery to fail on; with audio, the
/// `recording.json` the start wrote first lets the next start recover it.
async fn abandon_start(runtime: &RecordingRuntime) {
    let starting = runtime
        .starting
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match starting {
        Some(dir) if !has_audio_frames(&dir) => {
            warn!(
                "a recording start was still opening devices at shutdown; deleting {} (no audio yet)",
                dir.display()
            );
            remove_recording_dir(&dir).await;
        }
        _ => warn!(
            "a recording start is still in progress at shutdown; it will be recovered on the next start"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_mode::{handle_command_mode, handle_llm_command};
    use crate::context::{CommandModeContext, DaemonState, LlmCommandContext};
    use crate::dictation::{handle_cancel, handle_toggle};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use whisrs::recording::recorder::spawn_track_writer;
    use whisrs::recording::Segment;
    use whisrs::state::Action;
    use whisrs::transcription::{
        StreamText, Transcript, TranscriptionBackend, TranscriptionConfig,
    };
    use whisrs::State;

    fn active(id: &str) -> ActiveRecording {
        ActiveRecording {
            id: id.into(),
            title: Some(format!("{id} title")),
            started_at: Local::now(),
            dir: PathBuf::from(format!("/m/{id}")),
            network_retry: false,
            job: Job::Transcribe,
        }
    }

    #[test]
    fn status_idle_by_default() {
        assert_eq!(
            RecordingView::default().status(),
            RecordingStatus::default()
        );
    }

    #[test]
    fn status_recording_wins_over_processing() {
        let view = RecordingView {
            capturing: Some(active("rec")),
            processing: Some(active("proc")),
            queued: vec![active("q")],
            ..Default::default()
        };
        let status = view.status();
        assert_eq!(status.phase, RecordingPhase::Recording);
        assert_eq!(status.id.as_deref(), Some("rec"));
        assert_eq!(status.title.as_deref(), Some("rec title"));
        assert_eq!(status.queued, vec!["q".to_string()]);
    }

    #[test]
    fn status_processing_and_queue_only() {
        let view = RecordingView {
            capturing: None,
            processing: Some(active("proc")),
            queued: vec![],
            ..Default::default()
        };
        assert_eq!(view.status().phase, RecordingPhase::Processing);
        assert_eq!(view.status().id.as_deref(), Some("proc"));

        let queued_only = RecordingView {
            queued: vec![active("a"), active("b2")],
            ..Default::default()
        };
        assert_eq!(queued_only.status().phase, RecordingPhase::Processing);
        assert_eq!(queued_only.status().id.as_deref(), Some("a"));
        assert_eq!(queued_only.status().queued, vec!["a", "b2"]);
        assert!(queued_only.is_pending("a"));
        assert!(!queued_only.is_pending("b"));
    }

    #[test]
    fn device_selection() {
        let mut cfg = RecordingConfig::default();
        assert_eq!(mic_device(&cfg, "default"), "default");
        cfg.mic_device = Some("  ".into());
        assert_eq!(mic_device(&cfg, "hw:1"), "hw:1");
        cfg.mic_device = Some("my-mic".into());
        assert_eq!(mic_device(&cfg, "hw:1"), "my-mic");

        assert_eq!(system_source(None), SystemSource::DefaultMonitor);
        assert_eq!(system_source(Some("Default")), SystemSource::DefaultMonitor);
        assert_eq!(system_source(Some("none")), SystemSource::Off);
        assert_eq!(
            system_source(Some("x.monitor")),
            SystemSource::Named("x.monitor".into())
        );
        // A source named in the config is recorded as is, never followed;
        // only the default output's monitor is (resolved at start, which
        // needs a sound server, so it is covered end to end).
        let named = resolve_system(SystemSource::Named("x.monitor".into()));
        assert_eq!(
            named.unwrap().unwrap(),
            SystemDevice {
                name: "x.monitor".into(),
                follow_default: false
            }
        );
        assert!(resolve_system(SystemSource::Off).is_none());
    }

    #[test]
    fn offsets_are_relative_to_the_start() {
        let start = Instant::now();
        assert_eq!(offset_ms(start, None), 0);
        assert_eq!(
            offset_ms(start, Some(start + Duration::from_millis(250))),
            250
        );
        // A clock read before the start never goes negative.
        assert_eq!(offset_ms(start + Duration::from_secs(1), Some(start)), 0);
    }

    #[test]
    fn apply_outcomes_fills_tracks_and_notes_failures() {
        let start = Instant::now();
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.tracks = vec![
            track_meta(TrackKind::Mic, "mic".into()),
            track_meta(TrackKind::System, "sys".into()),
        ];
        let outcomes = vec![
            TrackOutcome {
                kind: TrackKind::Mic,
                stats: Ok(TrackStats {
                    samples: 32_000,
                    first_chunk_at: Some(start + Duration::from_millis(40)),
                    ..TrackStats::default()
                }),
                stuck: false,
                ended_early: false,
            },
            TrackOutcome {
                kind: TrackKind::System,
                stats: Err("boom".into()),
                stuck: false,
                ended_early: false,
            },
        ];
        apply_outcomes(&mut meta, start, &outcomes);
        assert_eq!(meta.tracks[0].samples, 32_000);
        assert_eq!(meta.tracks[0].offset_ms, 40);
        assert_eq!(meta.tracks[1].samples, 0);
        assert_eq!(meta.errors.len(), 1);
        assert!(meta.errors[0].contains("system"));

        end_recording(&mut meta, EndReason::Stopped, Local::now());
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.end_reason, Some(EndReason::Stopped));
        assert!(meta.ended_at.is_some());
    }

    /// Each writer's quality report lands on its track, and the mic's live
    /// system-silent warning on the system track.
    #[test]
    fn apply_outcomes_stores_each_tracks_quality() {
        let start = Instant::now();
        let report = |samples: &[i16]| {
            Some(whisrs::audio::quality::analyze(
                samples,
                audio_silence_gate::Vad::Rms,
            ))
        };
        let tone: Vec<i16> = (0..16_000)
            .map(|i| if i % 2 == 0 { 3000 } else { -3000 })
            .collect();
        let outcome = |kind, stats| TrackOutcome {
            kind,
            stats: Ok(stats),
            stuck: false,
            ended_early: false,
        };
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.tracks = vec![
            track_meta(TrackKind::Mic, "mic".into()),
            track_meta(TrackKind::System, "sys".into()),
        ];
        let outcomes = vec![
            outcome(
                TrackKind::Mic,
                TrackStats {
                    samples: 16_000,
                    quality: report(&tone),
                    raised: vec![Check::SystemSilent],
                    ..TrackStats::default()
                },
            ),
            outcome(
                TrackKind::System,
                TrackStats {
                    samples: 16_000,
                    quality: report(&[0; 16_000]),
                    ..TrackStats::default()
                },
            ),
        ];
        apply_outcomes(&mut meta, start, &outcomes);
        let mic = meta.tracks[0].quality.as_ref().unwrap();
        assert_eq!(mic.vad, "rms");
        assert!(mic.speech_dbfs.is_some());
        assert!(mic.warnings.is_empty(), "{:?}", mic.warnings);
        let system = meta.tracks[1].quality.as_ref().unwrap();
        assert_eq!(system.speech_dbfs, None);
        assert_eq!(system.warnings, ["system-silent"]);

        // Into recording.json and back.
        let json = serde_json::to_string(&meta).unwrap();
        let back: RecordingMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tracks, meta.tracks);
    }

    #[test]
    fn recover_meta_marks_recording_as_recovered() {
        let ended = Local::now();
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.tracks = vec![track_meta(TrackKind::Mic, "mic".into())];
        recover_meta(&mut meta, &[(TrackKind::Mic, 48_000)], ended);
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.end_reason, Some(EndReason::Recovered));
        assert_eq!(meta.ended_at, Some(ended));
        assert_eq!(meta.tracks[0].samples, 48_000);
    }

    #[test]
    fn recover_meta_keeps_a_shutdown_end() {
        let shutdown_at = Local::now() - chrono::Duration::minutes(5);
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.status = RecordingState::Processing;
        end_recording(&mut meta, EndReason::Shutdown, shutdown_at);
        recover_meta(&mut meta, &[], Local::now());
        assert_eq!(meta.end_reason, Some(EndReason::Shutdown));
        assert_eq!(meta.ended_at, Some(shutdown_at));
        assert_eq!(meta.status, RecordingState::Processing);
    }

    /// Every loud chunk becomes one segment with fixed text.
    struct FakeBackend {
        text: &'static str,
        fail: bool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TranscriptionBackend for FakeBackend {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            unreachable!("recordings ask for segments")
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> Result<Transcript> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                bail!("fake backend is down");
            }
            Ok(Transcript {
                segments: vec![Segment {
                    start_ms: 0,
                    end_ms: 1000,
                    text: self.text.to_string(),
                    speaker: None,
                    words: None,
                }],
            })
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    fn test_context(
        root: &Path,
        backend: Arc<dyn TranscriptionBackend>,
    ) -> (Arc<DaemonContext>, mpsc::UnboundedReceiver<QueuedRecording>) {
        test_context_with(root, backend, "")
    }

    /// [`test_context`] with `extra` appended to its config.
    fn test_context_with(
        root: &Path,
        backend: Arc<dyn TranscriptionBackend>,
        extra: &str,
    ) -> (Arc<DaemonContext>, mpsc::UnboundedReceiver<QueuedRecording>) {
        build_context(root, backend, extra, store::free_space)
    }

    /// Every runtime a test builds, the pipeline's `RecordingRuntime::new()`
    /// included, has the panicking stand-ins for devices ([`DEVICES`]). The
    /// lookup of `Off` touches nothing even on the real devices, so this
    /// check is safe to run against a build that lost them.
    #[test]
    fn test_runtimes_never_reach_the_real_devices() {
        let (runtime, _, _) = RecordingRuntime::new();
        let lookup = runtime.devices.resolve_system;
        let reached = std::panic::catch_unwind(|| lookup(SystemSource::Off));
        assert!(reached.is_err(), "a test runtime has the real devices");
    }

    fn build_context(
        root: &Path,
        backend: Arc<dyn TranscriptionBackend>,
        extra: &str,
        free_space: FreeSpaceFn,
    ) -> (Arc<DaemonContext>, mpsc::UnboundedReceiver<QueuedRecording>) {
        // `[audio] vad = "rms"`: the test tracks are tones, loud but no voice,
        // which earshot would find no speech in.
        let config: Config = toml::from_str(&format!(
            "[audio]\nvad = \"rms\"\n[general]\nbackend = \"groq\"\n[recording]\n\
             user_name = \"Tester\"\ndir = \"{}\"\n{extra}",
            root.display()
        ))
        .unwrap();
        let (mut recording, _status_rx, queue_rx) = RecordingRuntime::new();
        recording.free_space = free_space;
        let context = DaemonContext {
            config,
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            transcription_backend: backend,
            notify: false,
            state_tx: crate::context::StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            recording,
            agents: crate::agent::AgentRuntime::disabled(),
            agent_board: watch::channel(Vec::new()).0,
            agent_overlay_shown: std::sync::atomic::AtomicBool::new(false),
        };
        (Arc::new(context), queue_rx)
    }

    /// A recording a crash left with status `recording`: a mic WAV whose header was never
    /// finalized (sizes still zero) holding one second of loud audio.
    fn crashed_recording(root: &Path) -> PathBuf {
        let (id, dir) = store::create(root, Local::now(), Some("crash")).unwrap();
        let mut meta = RecordingMeta::new(id, Some("crash".into()), Local::now());
        meta.tracks = vec![track_meta(TrackKind::Mic, "default".into())];
        store::write_meta(&dir, &meta).unwrap();

        let samples: Vec<i16> = (0..16_000)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect();
        let mut bytes = whisrs::audio::wav::encode_wav(&samples).unwrap();
        // Zero the RIFF and data sizes, as an unflushed writer leaves them.
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        let data = bytes.windows(4).position(|w| w == b"data").unwrap();
        bytes[data + 4..data + 8].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(store::audio_path(&dir, TrackKind::Mic), bytes).unwrap();
        dir
    }

    #[tokio::test]
    async fn crashed_recording_is_recovered_and_transcribed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let backend = Arc::new(FakeBackend {
            text: "hello from the crash",
            fail: false,
            calls: AtomicUsize::new(0),
        });
        let (context, mut queue) = test_context(tmp.path(), backend.clone());

        recover_on_startup(Arc::clone(&context)).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.end_reason, Some(EndReason::Recovered));
        assert_eq!(meta.tracks[0].samples, 16_000);
        assert_eq!(context.recording.status().queued, vec![meta.id.clone()]);

        let item = queue.try_recv().expect("recovery queued the recording");
        process_one(&context, item).await;

        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert!(backend.calls.load(Ordering::Relaxed) > 0);
        let md = std::fs::read_to_string(store::transcript_path(&dir, &meta.id)).unwrap();
        assert!(md.contains("hello from the crash"), "{md}");
        assert_eq!(context.recording.status(), RecordingStatus::default());
    }

    #[tokio::test]
    async fn failed_processing_is_marked_and_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let backend = Arc::new(FakeBackend {
            text: "",
            fail: true,
            calls: AtomicUsize::new(0),
        });
        let (context, mut queue) = test_context(tmp.path(), backend);

        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Failed);
        assert!(meta
            .error
            .as_deref()
            .is_some_and(|e| e.contains("fake backend is down")));

        // Retry requeues it, and a second retry while queued is refused.
        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        assert_eq!(
            store::read_meta(&dir).unwrap().status,
            RecordingState::Processing
        );
        assert!(retry(&context, "last", &RecordingOptions::default())
            .await
            .is_err());
        assert!(queue.try_recv().is_ok());
    }

    #[tokio::test]
    async fn stop_and_cancel_without_a_recording_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeBackend {
            text: "",
            fail: false,
            calls: AtomicUsize::new(0),
        });
        let (context, _queue) = test_context(tmp.path(), backend);
        let none = RecordingOptions::default;
        let resp = handle_recording(
            Arc::clone(&context),
            RecordingAction::Stop,
            None,
            None,
            none(),
        )
        .await;
        assert!(
            matches!(resp, Response::Error { ref message } if message.contains("no recording"))
        );
        let resp = handle_recording(
            Arc::clone(&context),
            RecordingAction::Cancel,
            None,
            None,
            none(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }));
        let resp = handle_recording(context, RecordingAction::Status, None, None, none()).await;
        assert!(matches!(
            resp,
            Response::Recording { recording } if recording.phase == RecordingPhase::Idle
        ));
    }

    /// The options of a `recording` command as a client sends them.
    fn options_from_ipc(json: &str) -> RecordingOptions {
        match serde_json::from_str::<whisrs::Command>(json).unwrap() {
            whisrs::Command::Recording { options, .. } => options,
            other => panic!("not a recording command: {other:?}"),
        }
    }

    /// The ready notification names a speaker separation that failed, once,
    /// and stays as it was otherwise.
    #[test]
    fn the_ready_notification_says_when_speakers_were_not_separated() {
        let md = Path::new("/r/m/m.md");
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        assert_eq!(
            ready_message(md, &meta),
            "Recording transcript ready: /r/m/m.md"
        );
        meta.errors
            .push("system audio missing: audio/system.wav".into());
        meta.errors
            .push(whisrs::recording::speakers::separation_note(
                "the diarization sidecar failed on the system track: refused",
            ));
        assert_eq!(
            ready_message(md, &meta),
            "Recording transcript ready, without speakers (the diarization sidecar failed on \
             the system track: refused): /r/m/m.md"
        );
    }

    /// A user's name that no longer matches a voice is counted in the
    /// notification, the one place the user hears of it.
    #[test]
    fn the_ready_notification_counts_names_that_lost_their_voice() {
        let md = Path::new("/r/m/m.md");
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.errors.push(format!(
            "{USER_NAME_LOST}\"Sarah\" (was S1) matches no voice"
        ));
        assert_eq!(
            ready_message(md, &meta),
            "Recording transcript ready; 1 speaker name you gave no longer matches a voice: \
             /r/m/m.md"
        );
        meta.errors.push(format!(
            "{USER_NAME_LOST}\"Omar\" (was S2) matches no voice"
        ));
        meta.errors
            .push(whisrs::recording::speakers::separation_note("refused"));
        assert_eq!(
            ready_message(md, &meta),
            "Recording transcript ready, without speakers (refused); 2 speaker names you gave \
             no longer match a voice: /r/m/m.md"
        );
    }

    /// What a start stores: each track's language from the command (its own,
    /// else `language`), else from `[recording]`, and the others label.
    #[test]
    fn options_from_ipc_land_in_recording_json() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context_with(
            tmp.path(),
            working_backend(),
            "language = \"fr\"\nmic_language = \"it\"\n",
        );
        let stored = |json: &str| {
            let options = options_from_ipc(json).validated().unwrap();
            let (id, dir) = store::create(tmp.path(), Local::now(), None).unwrap();
            let meta = new_meta(id, None, Local::now(), &options, &context.config);
            store::write_meta(&dir, &meta).unwrap();
            store::read_meta(&dir).unwrap()
        };

        let meta = stored(
            r#"{"cmd":"recording","action":"start","mic_language":"DE",
                "system_language":"auto","others":" Video "}"#,
        );
        assert_eq!(meta.mic_language.as_deref(), Some("de"));
        assert_eq!(meta.system_language.as_deref(), Some("auto"));
        assert_eq!(meta.others_label.as_deref(), Some("Video"));

        let meta = stored(r#"{"cmd":"recording","action":"toggle","language":"es"}"#);
        assert_eq!(meta.mic_language.as_deref(), Some("es"));
        assert_eq!(meta.system_language.as_deref(), Some("es"));

        // The tray and the hotkey send none: the config's, and "Others".
        let meta = stored(r#"{"cmd":"recording","action":"toggle"}"#);
        assert_eq!(meta.mic_language.as_deref(), Some("it"));
        assert_eq!(meta.system_language.as_deref(), Some("fr"));
        assert_eq!(meta.others_label.as_deref(), Some("Others"));
        assert_eq!(meta.shared_mic, Some(false), "the config's");

        assert_eq!(meta.num_speakers, Some(0), "the config's");

        assert!(meta.participants.is_empty());

        let meta =
            stored(r#"{"cmd":"recording","action":"start","shared_mic":true,"num_speakers":4}"#);
        assert_eq!(meta.shared_mic, Some(true));
        assert_eq!(meta.num_speakers, Some(4));

        let meta = stored(
            r#"{"cmd":"recording","action":"toggle","participants":[" Sarah","Omar Haddad "]}"#,
        );
        assert_eq!(meta.participants, ["Sarah", "Omar Haddad"]);
    }

    /// `shared_mic` is stored at start from the config when the command
    /// gives none, kept by a retry without it, and replaced by one with it;
    /// the participants likewise.
    #[tokio::test]
    async fn shared_mic_is_stored_and_a_retry_replaces_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) =
            test_context_with(tmp.path(), working_backend(), "shared_mic = true\n");
        let (id, dir) = store::create(tmp.path(), Local::now(), None).unwrap();
        let mut meta = new_meta(id, None, Local::now(), &Default::default(), &context.config);
        assert_eq!(meta.shared_mic, Some(true), "from the config");
        meta.participants = vec!["Sarah".into()];
        meta.status = RecordingState::Failed;
        meta.tracks.push(track_meta(TrackKind::Mic, "test".into()));
        store::write_meta(&dir, &meta).unwrap();
        std::fs::write(
            store::audio_path(&dir, TrackKind::Mic),
            whisrs::audio::wav::encode_wav(&[0; 1600]).unwrap(),
        )
        .unwrap();

        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        assert_eq!(store::read_meta(&dir).unwrap().shared_mic, Some(true));
        assert_eq!(store::read_meta(&dir).unwrap().participants, ["Sarah"]);
        process_one(&context, queue.try_recv().unwrap()).await;

        let options = options_from_ipc(
            r#"{"cmd":"recording","action":"retry","id":"last","shared_mic":false,
                "participants":["Omar","Dana"]}"#,
        );
        retry(&context, "last", &options.validated().unwrap())
            .await
            .unwrap();
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.shared_mic, Some(false));
        assert_eq!(meta.participants, ["Omar", "Dana"]);
    }

    /// `recording name` for `speaker` and `name`, as the CLI sends it.
    async fn name_speaker(context: &Arc<DaemonContext>, speaker: &str, name: &str) -> Response {
        let options = options_from_ipc(&format!(
            r#"{{"cmd":"recording","action":"name","id":"last","speaker":{},"name":{}}}"#,
            serde_json::to_string(speaker).unwrap(),
            serde_json::to_string(name).unwrap()
        ));
        handle_recording(
            Arc::clone(context),
            RecordingAction::Name,
            None,
            Some("last".into()),
            options,
        )
        .await
    }

    /// A recording that crashed, recovered and transcribed: one speaker,
    /// the user ("Tester").
    async fn transcribed_recording(
        root: &Path,
    ) -> (
        Arc<DaemonContext>,
        mpsc::UnboundedReceiver<QueuedRecording>,
        PathBuf,
    ) {
        let dir = crashed_recording(root);
        let (context, mut queue) = test_context(root, working_backend());
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        assert_eq!(store::read_meta(&dir).unwrap().status, RecordingState::Done);
        (context, queue, dir)
    }

    /// `recording name` renames a speaker at once (recording.json and the
    /// transcript, without transcribing), a retry keeps the name, and an
    /// empty name clears it.
    #[tokio::test]
    async fn naming_a_speaker_rewrites_the_transcript_and_survives_a_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue, dir) = transcribed_recording(tmp.path()).await;
        let speaker = |dir: &Path| {
            let meta = store::read_meta(dir).unwrap();
            let me = &meta.speakers[0];
            (me.id.clone(), me.name.clone(), me.source.clone())
        };
        assert_eq!(speaker(&dir), ("me".into(), "Tester".into(), "self".into()));

        let response = name_speaker(&context, "Tester", "Yo").await;
        assert!(
            matches!(response, Response::Recording { .. }),
            "{response:?}"
        );
        assert!(queue.try_recv().is_err(), "nothing is transcribed");
        assert_eq!(speaker(&dir), ("me".into(), "Yo".into(), "user".into()));
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(
            meta.speaker_names.get("me").map(|n| n.name.as_str()),
            Some("Yo")
        );
        assert_eq!(
            meta.speaker_names["me"].anchor, None,
            "the user's own track"
        );
        let md = std::fs::read_to_string(store::transcript_path(&dir, &meta.id)).unwrap();
        assert!(md.contains("] Yo:** fine"), "{md}");
        assert_eq!(context.recording.status(), RecordingStatus::default());

        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&context, queue.try_recv().unwrap()).await;
        assert_eq!(speaker(&dir), ("me".into(), "Yo".into(), "user".into()));

        let response = name_speaker(&context, "me", "").await;
        assert!(
            matches!(response, Response::Recording { .. }),
            "{response:?}"
        );
        assert_eq!(speaker(&dir), ("me".into(), "Tester".into(), "self".into()));
        assert!(store::read_meta(&dir).unwrap().speaker_names.is_empty());
    }

    /// A recording queued or being transcribed, or without a transcript,
    /// is refused, as are an unknown speaker and a bad name, and nothing is
    /// written.
    #[tokio::test]
    async fn naming_is_refused_while_a_recording_is_busy() {
        let tmp = tempfile::tempdir().unwrap();
        crashed_recording(tmp.path());
        let (context, _queue) = test_context(tmp.path(), working_backend());
        let refused = |response: Response, want: &str| {
            let Response::Error { message } = &response else {
                panic!("accepted: {response:?}");
            };
            assert!(message.contains(want), "{want:?} not in {message:?}");
        };
        refused(
            name_speaker(&context, "me", "Yo").await,
            "has no transcript yet (status recording)",
        );

        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue, dir) = transcribed_recording(tmp.path()).await;
        let before = std::fs::read(dir.join("recording.json")).unwrap();
        // Queued for a retry, or being transcribed: reserved.
        let meta = store::read_meta(&dir).unwrap();
        let active = ActiveRecording::from_meta(&dir, &meta);
        context.recording.reserve(&active).unwrap();
        refused(
            name_speaker(&context, "me", "Yo").await,
            "is queued or being transcribed",
        );
        context.recording.unreserve(&meta.id);

        refused(
            name_speaker(&context, "S4", "Yo").await,
            "no speaker \"S4\"",
        );
        refused(name_speaker(&context, " ", "Yo").await, "which speaker?");
        refused(
            name_speaker(&context, "me", "Yo\u{7}").await,
            "single line of text",
        );
        let response = handle_recording(
            Arc::clone(&context),
            RecordingAction::Name,
            None,
            Some("last".into()),
            RecordingOptions::default(),
        )
        .await;
        refused(response, "needs the speaker and its name");
        assert_eq!(std::fs::read(dir.join("recording.json")).unwrap(), before);
        assert_eq!(context.recording.status(), RecordingStatus::default());
    }

    /// A rename holds its recording without showing it as queued, and
    /// meanwhile a retry, another rename and the retention sweep leave it
    /// alone.
    #[tokio::test]
    async fn a_held_recording_is_hidden_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context_with(
            tmp.path(),
            working_backend(),
            "audio_retention_days = 0
",
        );
        let (id, dir) = store::create(tmp.path(), Local::now(), Some("done")).unwrap();
        let mut meta = RecordingMeta::new(id.clone(), Some("done".into()), Local::now());
        meta.status = RecordingState::Done;
        meta.ended_at = Some(Local::now() - chrono::Duration::days(1));
        meta.tracks.push(track_meta(TrackKind::Mic, "test".into()));
        store::write_meta(&dir, &meta).unwrap();
        let audio = store::audio_path(&dir, TrackKind::Mic);
        std::fs::write(&audio, whisrs::audio::wav::encode_wav(&[0; 1600]).unwrap()).unwrap();

        context.recording.hold(&id).unwrap();
        assert_eq!(
            context.recording.status(),
            RecordingStatus::default(),
            "not shown"
        );
        assert!(context.recording.hold(&id).is_err());
        let err = retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("recording {id} is busy (being renamed or cleaned up); try again")
        );
        assert!(queue.try_recv().is_err());
        sweep(&context).await;
        assert!(audio.exists(), "the sweep waits for it");

        context.recording.release(&id);
        sweep(&context).await;
        assert!(!audio.exists());
        assert!(store::read_meta(&dir).unwrap().audio_deleted);
        assert_eq!(context.recording.status(), RecordingStatus::default());
        assert!(
            context.recording.hold(&id).is_ok(),
            "the sweep let go of it"
        );
    }

    /// `recording summarize` for the newest recording, as the CLI sends it.
    async fn summarize_last(context: &Arc<DaemonContext>) -> Response {
        handle_recording(
            Arc::clone(context),
            RecordingAction::Summarize,
            None,
            Some("last".into()),
            RecordingOptions::default(),
        )
        .await
    }

    fn refused(response: Response, want: &str) {
        let Response::Error { message } = &response else {
            panic!("accepted: {response:?}");
        };
        assert!(message.contains(want), "{want:?} not in {message:?}");
    }

    /// `recording summarize` goes through the processing queue: shown as
    /// summarizing, refused while queued (a second summary, a retry, a
    /// rename), then run by the worker, which rewrites the transcript with
    /// the summary and transcribes nothing. A recording without a
    /// transcript, or a config without an `[llm]`, is refused before
    /// anything is queued.
    #[tokio::test]
    async fn summarize_goes_through_the_queue_and_rewrites_the_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let backend = working_backend();
        // Nothing listens there: the transcript ("fine") is too short to
        // be sent, and a request would fail.
        let (context, mut queue) = test_context_with(
            tmp.path(),
            backend.clone(),
            "[llm]\napi_key = \"k\"\napi_url = \"http://127.0.0.1:1/v1/chat/completions\"\n",
        );
        refused(
            summarize_last(&context).await,
            "has no transcript yet (status recording)",
        );
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        let calls = backend.calls.load(Ordering::Relaxed);
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.summary, None);

        let response = summarize_last(&context).await;
        let Response::Recording { recording } = response else {
            panic!("refused: {response:?}");
        };
        assert_eq!(recording.phase, RecordingPhase::Processing);
        assert!(recording.summarizing, "{recording:?}");
        assert_eq!(recording.queued, std::slice::from_ref(&meta.id));
        assert_eq!(
            store::read_meta(&dir).unwrap().summary_pending,
            Some(SummaryRequest::Asked),
            "marked while queued"
        );
        refused(summarize_last(&context).await, "is already queued");
        assert!(retry(&context, "last", &RecordingOptions::default())
            .await
            .is_err());
        // A rename does not wait for a summary.
        let response = name_speaker(&context, "me", "Yo").await;
        assert!(
            matches!(response, Response::Recording { .. }),
            "{response:?}"
        );

        process_one(&context, queue.try_recv().expect("queued")).await;
        assert!(queue.try_recv().is_err(), "queued once");
        assert_eq!(
            backend.calls.load(Ordering::Relaxed),
            calls,
            "nothing transcribed"
        );
        let after = store::read_meta(&dir).unwrap();
        assert_eq!(after.summary.as_deref(), Some(summary::TOO_SHORT));
        assert!(after.summary_pending.is_none());
        assert_eq!(after.segments, meta.segments);
        assert_eq!(after.status, RecordingState::Done);
        let md = std::fs::read_to_string(store::transcript_path(&dir, &after.id)).unwrap();
        assert!(
            md.contains("## Summary\n\nToo short to summarize.\n\n## Transcript\n\n"),
            "{md}"
        );
        assert_eq!(context.recording.status(), RecordingStatus::default());

        let (no_llm, mut queue) = test_context(tmp.path(), working_backend());
        refused(summarize_last(&no_llm).await, "needs the [llm] section");
        assert!(queue.try_recv().is_err());
    }

    /// With `summary` on, a finished transcription queues a summary as a
    /// job of its own: the transcription job ends with the recording done
    /// and the summary waiting, shown as summarizing; the summary job then
    /// fills it in. A retry does the same, and with `summary` off a retry
    /// queues nothing and the old summary is marked stale. Without an
    /// `[llm]` the summary job notes why and the recording stays done.
    #[tokio::test]
    async fn an_automatic_summary_is_a_job_after_the_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let backend = working_backend();
        let (context, mut queue) = test_context_with(
            tmp.path(),
            backend.clone(),
            "summary = true\n[llm]\napi_key = \"k\"\napi_url = \"http://127.0.0.1:1/v1/chat/completions\"\n",
        );
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.summary, None, "not made in the transcription job");
        assert_eq!(meta.summary_pending, Some(SummaryRequest::Auto));
        let status = context.recording.status();
        assert!(
            status.summarizing && status.queued == [meta.id.clone()],
            "{status:?}"
        );
        let job = queue.try_recv().expect("a summary job");
        assert_eq!(job.recording.job, Job::Summarize);
        process_one(&context, job).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.summary.as_deref(), Some(summary::TOO_SHORT));
        assert!(meta.summary_pending.is_none());
        assert_eq!(context.recording.status(), RecordingStatus::default());

        // A retry with `summary` on transcribes, then summarizes again.
        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&context, queue.try_recv().unwrap()).await;
        assert!(
            store::read_meta(&dir).unwrap().summary_stale,
            "until the new summary"
        );
        let job = queue.try_recv().expect("summarized again");
        assert_eq!(job.recording.job, Job::Summarize);
        process_one(&context, job).await;
        assert!(!store::read_meta(&dir).unwrap().summary_stale);

        // With `summary` off: nothing queued, the summary kept and stale.
        let (off, mut off_queue) = test_context(tmp.path(), backend.clone());
        retry(&off, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&off, off_queue.try_recv().unwrap()).await;
        assert!(off_queue.try_recv().is_err(), "no summary job");
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.summary.as_deref(), Some(summary::TOO_SHORT));
        assert!(meta.summary_stale);

        // No [llm]: the job notes why, and the recording stays done.
        let (no_llm, mut no_llm_queue) = test_context_with(tmp.path(), backend, "summary = true\n");
        retry(&no_llm, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&no_llm, no_llm_queue.try_recv().unwrap()).await;
        process_one(&no_llm, no_llm_queue.try_recv().expect("a summary job")).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert!(meta.summary_pending.is_none());
        assert!(
            meta.errors
                .iter()
                .any(|n| n.starts_with(SUMMARY_FAILED) && n.contains("no [llm] section")),
            "{:?}",
            meta.errors
        );
    }

    /// The worker runs a waiting transcription before a waiting summary,
    /// and jobs of one kind in the order they came.
    #[test]
    fn transcriptions_run_before_summaries() {
        let job = |id: &str, job| QueuedRecording {
            recording: ActiveRecording { job, ..active(id) },
        };
        let mut pending = VecDeque::from([
            job("s1", Job::Summarize),
            job("t1", Job::Transcribe),
            job("s2", Job::Summarize),
            job("t2", Job::Transcribe),
        ]);
        let order: Vec<String> = std::iter::from_fn(|| next_job(&mut pending))
            .map(|item| item.recording.id)
            .collect();
        assert_eq!(order, ["t1", "t2", "s1", "s2"]);
    }

    /// A summary still pending at startup (a shutdown or a crash stopped
    /// it) is queued again, oldest first: one the user asked for always,
    /// an automatic one only while `summary` is still on (else its mark is
    /// cleared); a done recording without the mark is not.
    #[tokio::test]
    async fn a_pending_summary_is_queued_again_at_startup() {
        for summary_on in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let extra = if summary_on { "summary = true\n" } else { "" };
            let (context, mut queue) = test_context_with(tmp.path(), working_backend(), extra);
            let mut dirs = Vec::new();
            for (minutes, title, pending) in [
                (3, "asked", Some(SummaryRequest::Asked)),
                (2, "auto", Some(SummaryRequest::Auto)),
                (1, "done", None),
            ] {
                let started = Local::now() - chrono::Duration::minutes(minutes);
                let (id, dir) = store::create(tmp.path(), started, Some(title)).unwrap();
                let mut meta = RecordingMeta::new(id, Some(title.into()), started);
                meta.status = RecordingState::Done;
                meta.summary_pending = pending;
                store::write_meta(&dir, &meta).unwrap();
                dirs.push(dir);
            }
            recover_on_startup(Arc::clone(&context)).await;
            let mut queued = Vec::new();
            while let Ok(job) = queue.try_recv() {
                assert_eq!(job.recording.job, Job::Summarize);
                queued.push(job.recording.dir);
            }
            if summary_on {
                assert_eq!(queued, [dirs[0].clone(), dirs[1].clone()]);
            } else {
                assert_eq!(queued, [dirs[0].clone()]);
                assert_eq!(
                    store::read_meta(&dirs[1]).unwrap().summary_pending,
                    None,
                    "cleared"
                );
            }
        }
    }

    /// A hold waits for a transcription, queued or running, but not for a
    /// summary job, which writes under a hold of its own.
    #[test]
    fn a_hold_waits_for_a_transcription_only() {
        let (runtime, _status, _queue) = RecordingRuntime::new();
        let summary = |id: &str| ActiveRecording {
            job: Job::Summarize,
            ..active(id)
        };
        runtime.reserve(&summary("s")).unwrap();
        runtime.hold("s").unwrap();
        runtime.release("s");
        runtime.update(|v| v.processing = Some(summary("r")));
        runtime.hold("r").unwrap();
        assert!(runtime.hold("r").is_err(), "held once");
        runtime.release("r");
        runtime.reserve(&active("t")).unwrap();
        assert!(runtime.hold("t").is_err(), "a queued transcription");
        runtime.update(|v| v.processing = Some(active("p")));
        assert!(runtime.hold("p").is_err(), "a running transcription");
        assert!(
            runtime.reserve(&summary("p")).is_err(),
            "one job a recording"
        );
    }

    /// With `audio_retention_days = 0` and `summary` on, the audio goes as
    /// soon as the transcript is written, though its summary is queued, and
    /// the summary keeps that; a summary resumed at startup does not keep
    /// the startup sweep off its recording either.
    #[tokio::test]
    async fn the_audio_is_swept_with_a_summary_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, mut queue) = test_context_with(
            tmp.path(),
            working_backend(),
            "audio_retention_days = 0\nsummary = true\n\
             [llm]\napi_key = \"k\"\napi_url = \"http://127.0.0.1:1/v1/chat/completions\"\n",
        );
        let audio = store::audio_path(&dir, TrackKind::Mic);
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        assert!(!audio.exists(), "swept after the transcript");
        let job = queue.try_recv().expect("the summary job");
        process_one(&context, job).await;
        let meta = store::read_meta(&dir).unwrap();
        assert!(meta.audio_deleted && !audio.exists());
        assert_eq!(meta.summary.as_deref(), Some(summary::TOO_SHORT));

        // At startup: a done recording with audio and a summary pending.
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context_with(
            tmp.path(),
            working_backend(),
            "audio_retention_days = 0\n[llm]\napi_key = \"k\"\n",
        );
        let (id, dir) = store::create(tmp.path(), Local::now(), Some("done")).unwrap();
        let mut meta = RecordingMeta::new(id, Some("done".into()), Local::now());
        meta.status = RecordingState::Done;
        meta.ended_at = Some(Local::now());
        meta.tracks.push(track_meta(TrackKind::Mic, "test".into()));
        meta.summary_pending = Some(SummaryRequest::Asked);
        store::write_meta(&dir, &meta).unwrap();
        let audio = store::audio_path(&dir, TrackKind::Mic);
        std::fs::write(&audio, whisrs::audio::wav::encode_wav(&[0; 1600]).unwrap()).unwrap();
        recover_on_startup(Arc::clone(&context)).await;
        assert!(queue.try_recv().is_ok(), "the summary is queued again");
        assert!(
            !audio.exists(),
            "and the startup sweep still took the audio"
        );
    }

    /// Sixty words: a transcript long enough to be sent to the LLM.
    const LONG_SPEECH: &str = "one two three four five six seven eight nine ten one two three \
        four five six seven eight nine ten one two three four five six seven eight nine ten one \
        two three four five six seven eight nine ten one two three four five six seven eight nine \
        ten one two three four five six seven eight nine ten";

    /// A chat completion server that takes one request, says it arrived,
    /// and answers `content` once released.
    async fn held_llm(
        content: &'static str,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let _ = arrived_tx.send(());
            let _ = release_rx.await;
            let body = serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": content}}]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        (url, arrived_rx, release_tx)
    }

    /// `recording name` goes through while the LLM works on a summary,
    /// and the summary, written afterwards, keeps the new name and is
    /// marked stale, since it was made with the old one.
    #[tokio::test]
    async fn a_name_during_a_running_summary_goes_through_and_makes_it_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (url, arrived, release) = held_llm("{\"summary\": \"Tester counted.\"}").await;
        let backend = Arc::new(FakeBackend {
            text: LONG_SPEECH,
            fail: false,
            calls: AtomicUsize::new(0),
        });
        let (context, mut queue) = test_context_with(
            tmp.path(),
            backend,
            &format!("summary = true\n[llm]\napi_key = \"k\"\napi_url = \"{url}\"\n"),
        );
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;
        let job = queue.try_recv().expect("the summary job");
        let worker = {
            let context = Arc::clone(&context);
            tokio::spawn(async move { process_one(&context, job).await })
        };
        arrived.await.expect("the LLM was asked");
        assert!(context.recording.status().summarizing);
        let response = name_speaker(&context, "me", "Yo").await;
        assert!(
            matches!(response, Response::Recording { .. }),
            "{response:?}"
        );
        assert_eq!(store::read_meta(&dir).unwrap().speakers[0].name, "Yo");
        release.send(()).unwrap();
        worker.await.unwrap();

        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.summary.as_deref(), Some("Tester counted."));
        assert_eq!(meta.speakers[0].name, "Yo", "the rename stays");
        assert!(meta.summary_stale, "made with the old name");
        assert!(meta.summary_pending.is_none());
        assert_eq!(context.recording.status(), RecordingStatus::default());
    }

    /// Bad values from a client other than the CLI are refused before a
    /// start goes anywhere near a device. The daemon is marked shutting
    /// down, the first check in `start`, so a check that let a bad value
    /// through fails here on that error instead of opening the host's mic.
    #[tokio::test]
    async fn invalid_ipc_options_are_refused_before_any_device_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        context
            .recording
            .shutting_down
            .store(true, Ordering::Release);
        let dir = crashed_recording(tmp.path());
        let before = store::read_meta(&dir).unwrap();

        for (action, json) in [
            ("start", r#""language":"english""#),
            ("toggle", r#""mic_language":"""#),
            ("start", r#""system_language":"12""#),
            ("start", r#""others":"   ""#),
            ("toggle", r#""others":"a\nb""#),
            ("retry", r#""language":"german""#),
            ("start", r#""num_speakers":21"#),
            ("retry", r#""num_speakers":99"#),
            ("start", r#""participants":["Sarah",""]"#),
            ("toggle", r#""participants":["Sarah, Omar"]"#),
            ("retry", r#""participants":["Sarah","sarah"]"#),
        ] {
            let cmd = format!(r#"{{"cmd":"recording","action":"{action}",{json}}}"#);
            let whisrs::Command::Recording {
                action,
                title,
                id,
                options,
            } = serde_json::from_str::<whisrs::Command>(&cmd).unwrap()
            else {
                panic!("not a recording command");
            };
            let response = handle_recording(Arc::clone(&context), action, title, id, options).await;
            let Response::Error { message } = &response else {
                panic!("{cmd} was accepted: {response:?}");
            };
            assert!(
                message.starts_with("invalid language")
                    || message.contains("others label")
                    || message.starts_with("num_speakers is")
                    || message.starts_with("participants: "),
                "{cmd}: {message}"
            );
        }
        // Nothing was created, queued or rewritten.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        assert_eq!(store::read_meta(&dir).unwrap(), before);
        assert!(queue.try_recv().is_err());

        // Valid options get past the check, to the shutdown refusal.
        let options = options_from_ipc(
            r#"{"cmd":"recording","action":"start","language":"de","others":"Video"}"#,
        );
        let response = handle_recording(context, RecordingAction::Start, None, None, options).await;
        assert!(
            matches!(&response, Response::Error { message } if message.contains("shutting down")),
            "{response:?}"
        );
    }

    /// A backend that records the language of every request.
    #[derive(Default)]
    struct LanguageBackend {
        languages: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl TranscriptionBackend for LanguageBackend {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            unreachable!("recordings ask for segments")
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            config: &TranscriptionConfig,
        ) -> Result<Transcript> {
            self.languages.lock().unwrap().push(config.language.clone());
            Ok(Transcript {
                segments: Vec::new(),
            })
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    /// A retry's languages replace the stored ones and are saved; a retry
    /// without them keeps what is stored, whatever the config says by now.
    #[tokio::test]
    async fn retry_languages_are_saved_and_used() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let mut meta = store::read_meta(&dir).unwrap();
        meta.status = RecordingState::Failed;
        meta.mic_language = Some("en".into());
        meta.system_language = Some("en".into());
        store::write_meta(&dir, &meta).unwrap();
        let backend = Arc::new(LanguageBackend::default());
        let (context, mut queue) =
            test_context_with(tmp.path(), backend.clone(), "language = \"fr\"\n");

        let options = options_from_ipc(
            r#"{"cmd":"recording","action":"retry","id":"last","mic_language":"de"}"#,
        );
        let response = handle_recording(
            Arc::clone(&context),
            RecordingAction::Retry,
            None,
            Some("last".into()),
            options,
        )
        .await;
        assert!(
            matches!(response, Response::Recording { .. }),
            "{response:?}"
        );
        let saved = store::read_meta(&dir).unwrap();
        assert_eq!(saved.mic_language.as_deref(), Some("de"));
        assert_eq!(saved.system_language.as_deref(), Some("en"), "kept");
        process_one(&context, queue.try_recv().unwrap()).await;
        assert_eq!(*backend.languages.lock().unwrap(), ["de"]);
        let done = store::read_meta(&dir).unwrap();
        assert_eq!(done.status, RecordingState::Done);
        assert_eq!(done.model.as_deref(), Some("whisper-large-v3-turbo"));

        // `-l` covers both tracks; no options keep the stored ones.
        retry(
            &context,
            "last",
            &RecordingOptions {
                language: Some("it".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let saved = store::read_meta(&dir).unwrap();
        assert_eq!(saved.mic_language.as_deref(), Some("it"));
        assert_eq!(saved.system_language.as_deref(), Some("it"));
        process_one(&context, queue.try_recv().unwrap()).await;
        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        assert_eq!(
            store::read_meta(&dir).unwrap().mic_language.as_deref(),
            Some("it")
        );
        process_one(&context, queue.try_recv().unwrap()).await;
        assert_eq!(*backend.languages.lock().unwrap(), ["de", "it", "it"]);
    }

    fn working_backend() -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            text: "fine",
            fail: false,
            calls: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn shutdown_with_nothing_recording_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        tokio::time::timeout(Duration::from_secs(1), shutdown(&context))
            .await
            .expect("an idle shutdown returns at once");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        assert!(queue.try_recv().is_err());
        assert_eq!(context.recording.status(), RecordingStatus::default());
        // A start after shutdown began is refused.
        let err = start(&context, None, &RecordingOptions::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("shutting down"), "{err:#}");
    }

    #[tokio::test]
    async fn shutdown_saves_a_recording_for_the_next_start() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context(tmp.path(), working_backend());
        let (id, dir) = store::create(tmp.path(), Local::now(), Some("standup")).unwrap();
        let mut meta = RecordingMeta::new(id.clone(), Some("standup".into()), Local::now());
        meta.tracks = vec![track_meta(TrackKind::Mic, "default".into())];
        store::write_meta(&dir, &meta).unwrap();

        // A track fed by the test instead of a device.
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = spawn_track_writer(rx, store::audio_path(&dir, TrackKind::Mic), FLUSH_EVERY);
        tx.send(vec![1000i16; 3200]).unwrap();
        drop(tx);
        let started = Instant::now();
        let stopping = Arc::new(AtomicBool::new(false));
        *context.recording.session.lock().await = Some(RecordingSession {
            dir: dir.clone(),
            meta: meta.clone(),
            started,
            tracks: vec![TrackCapture::new(TrackKind::Mic, None, writer)],
            watch: None,
            stopping,
        });
        context
            .recording
            .update(|v| v.capturing = Some(ActiveRecording::from_meta(&dir, &meta)));

        shutdown(&context).await;

        let saved = store::read_meta(&dir).unwrap();
        assert_eq!(saved.status, RecordingState::Processing);
        assert_eq!(saved.end_reason, Some(EndReason::Shutdown));
        assert!(saved.ended_at.is_some());
        assert_eq!(saved.tracks[0].samples, 3200);
        assert!(context.recording.session.lock().await.is_none());
        assert_eq!(context.recording.status().phase, RecordingPhase::Idle);
        // And the next start picks it up.
        let interrupted = store::interrupted(tmp.path());
        assert_eq!(interrupted, vec![dir]);
    }

    #[tokio::test]
    async fn repeated_interruptions_end_in_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let backend = working_backend();
        let (context, mut queue) = test_context(tmp.path(), backend.clone());

        // Each "crash" is: the worker counts an attempt, then the daemon dies
        // before finishing; the next start recovers it.
        for attempt in 1..=MAX_ATTEMPTS {
            recover_on_startup(Arc::clone(&context)).await;
            let item = queue.try_recv().expect("requeued while under the limit");
            context.recording.update(|v| v.queued.clear());
            count_attempt(&item.recording.dir).unwrap();
            assert_eq!(store::read_meta(&dir).unwrap().attempts, attempt);
        }
        recover_on_startup(Arc::clone(&context)).await;
        assert!(queue.try_recv().is_err(), "not requeued after the limit");
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Failed);
        assert_eq!(
            meta.error.as_deref(),
            Some(
                format!(
                    "processing was interrupted 3 times; retry with whisrs recording retry {}",
                    meta.id
                )
                .as_str()
            )
        );
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);

        // A manual retry resets the count and processes it.
        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        assert_eq!(store::read_meta(&dir).unwrap().attempts, 0);
        process_one(&context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.attempts, 1);
    }

    #[tokio::test]
    async fn stuck_writer_is_reported_not_raced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mic.wav");
        // The test holds the sender: with no capture to drop, the writer
        // cannot finish.
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = spawn_track_writer(rx, path.clone(), FLUSH_EVERY);
        let stopping = AtomicBool::new(false);
        let outcomes = finish_tracks(
            vec![TrackCapture::new(TrackKind::Mic, None, writer)],
            &stopping,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .await;
        assert!(stopping.load(Ordering::Acquire));
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].stuck);
        assert!(outcomes[0].stats.is_err());

        // repair_unfinished leaves a stuck track's file alone.
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.tracks = vec![track_meta(TrackKind::Mic, "mic".into())];
        let before = std::fs::read(&path).unwrap_or_default();
        repair_unfinished(tmp.path(), &mut meta, &outcomes);
        assert_eq!(std::fs::read(&path).unwrap_or_default(), before);
        drop(tx);
    }

    #[tokio::test]
    async fn a_track_that_ends_before_the_stop_is_noted() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = spawn_track_writer(rx, tmp.path().join("system.wav"), FLUSH_EVERY);
        let mut track = TrackCapture::new(TrackKind::System, None, writer);
        let stopping = Arc::new(AtomicBool::new(false));
        watch_writer(&mut track, "m", &stopping, false);
        tx.send(vec![5i16; 1600]).unwrap();
        // The device goes away mid-recording.
        drop(tx);
        let early = Arc::clone(&track.ended_early);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !early.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "early end never noticed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let outcomes = finish_tracks(vec![track], &stopping, STOP_TIMEOUT, STOP_GRACE).await;
        assert!(outcomes[0].ended_early);
        assert!(!outcomes[0].stuck);
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.tracks = vec![track_meta(TrackKind::System, "sys".into())];
        apply_outcomes(&mut meta, Instant::now(), &outcomes);
        assert_eq!(meta.tracks[0].samples, 1600);
        assert_eq!(
            meta.errors,
            ["The system audio stopped delivering audio before the recording ended"]
        );

        // A writer that ends after the stop is a normal end.
        let (tx, rx) = mpsc::unbounded_channel::<whisrs::audio::AudioChunk>();
        let writer = spawn_track_writer(rx, tmp.path().join("mic.wav"), FLUSH_EVERY);
        let mut track = TrackCapture::new(TrackKind::Mic, None, writer);
        let stopping = Arc::new(AtomicBool::new(false));
        watch_writer(&mut track, "m", &stopping, false);
        stopping.store(true, Ordering::Release);
        drop(tx);
        let outcomes = finish_tracks(vec![track], &stopping, STOP_TIMEOUT, STOP_GRACE).await;
        assert!(!outcomes[0].ended_early);
    }

    /// A backend whose requests never finish, like a long transcription.
    struct HangingBackend;

    #[async_trait::async_trait]
    impl TranscriptionBackend for HangingBackend {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            std::future::pending().await
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> Result<Transcript> {
            std::future::pending().await
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_mid_processing_gives_the_attempt_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, queue) = test_context(tmp.path(), Arc::new(HangingBackend));
        recover_on_startup(Arc::clone(&context)).await;
        context
            .recording
            .set_worker(tokio::spawn(run_worker(Arc::clone(&context), queue)));

        // Wait until the worker has counted its attempt and is transcribing.
        let deadline = Instant::now() + Duration::from_secs(5);
        while store::read_meta(&dir).unwrap().attempts != 1
            || context.recording.status().queued.len() == 1
        {
            assert!(Instant::now() < deadline, "the worker never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown(&context).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.attempts, 0, "a graceful shutdown is not a crash");
        assert!(context.recording.worker.lock().unwrap().is_none());

        // The next start resumes it and it completes.
        let (next, mut queue) = test_context(tmp.path(), working_backend());
        recover_on_startup(Arc::clone(&next)).await;
        process_one(&next, queue.try_recv().expect("resumed")).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.attempts, 1);
    }

    #[tokio::test]
    async fn retry_of_a_pending_recording_leaves_its_meta_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        recover_on_startup(Arc::clone(&context)).await;
        let item = queue.try_recv().unwrap();
        // The worker took it and counted two attempts so far.
        context.recording.update(|v| {
            v.queued.clear();
            v.processing = Some(item.recording.clone());
        });
        let mut meta = store::read_meta(&dir).unwrap();
        meta.attempts = 2;
        store::write_meta(&dir, &meta).unwrap();

        let err = retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already queued"), "{err:#}");
        assert_eq!(store::read_meta(&dir).unwrap(), meta);
        assert!(queue.try_recv().is_err());
    }

    #[tokio::test]
    async fn retry_repairs_the_tracks_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        // A crash leftover that was marked failed without a repair.
        let mut meta = store::read_meta(&dir).unwrap();
        meta.status = RecordingState::Failed;
        meta.attempts = 3;
        store::write_meta(&dir, &meta).unwrap();
        let (context, mut queue) = test_context(tmp.path(), working_backend());

        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.attempts, 0);
        assert_eq!(meta.tracks[0].samples, 16_000);
        let reader = hound::WavReader::open(store::audio_path(&dir, TrackKind::Mic)).unwrap();
        assert_eq!(reader.duration(), 16_000);
        assert!(queue.try_recv().is_ok());
    }

    #[tokio::test]
    async fn recovery_skips_the_recording_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        let meta = store::read_meta(&dir).unwrap();
        *context.recording.session.lock().await = Some(RecordingSession {
            dir: dir.clone(),
            meta: meta.clone(),
            started: Instant::now(),
            tracks: Vec::new(),
            watch: None,
            stopping: Arc::new(AtomicBool::new(false)),
        });

        recover_on_startup(Arc::clone(&context)).await;
        assert!(queue.try_recv().is_err());
        assert_eq!(store::read_meta(&dir).unwrap(), meta);
    }

    /// A backend that panics mid-transcription.
    struct PanickingBackend;

    #[async_trait::async_trait]
    impl TranscriptionBackend for PanickingBackend {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            unreachable!("recordings ask for segments")
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> Result<Transcript> {
            panic!("backend exploded");
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    #[tokio::test]
    async fn a_panic_in_processing_fails_the_recording_and_keeps_the_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, queue) = test_context(tmp.path(), Arc::new(PanickingBackend));
        recover_on_startup(Arc::clone(&context)).await;
        let worker = tokio::spawn(run_worker(Arc::clone(&context), queue));

        let deadline = Instant::now() + Duration::from_secs(5);
        while store::read_meta(&dir).unwrap().status != RecordingState::Failed
            || context.recording.status().phase != RecordingPhase::Idle
        {
            assert!(Instant::now() < deadline, "the panic was never handled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let meta = store::read_meta(&dir).unwrap();
        let error = meta.error.as_deref().unwrap_or_default();
        assert!(
            error.starts_with("processing crashed: ") && error.contains("backend exploded"),
            "{error}"
        );
        assert_eq!(meta.attempts, 1);
        assert!(!worker.is_finished(), "the worker survives the panic");

        // And it still takes work: a retry is processed (and panics) again.
        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while store::read_meta(&dir).unwrap().status != RecordingState::Failed {
            assert!(Instant::now() < deadline, "the retry was never processed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.abort();
    }

    #[tokio::test]
    async fn shutdown_after_a_worker_panic_keeps_the_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        recover_on_startup(Arc::clone(&context)).await;
        let item = queue.try_recv().unwrap();
        context.recording.update(|v| {
            v.queued.clear();
            v.processing = Some(item.recording.clone());
        });
        count_attempt(&dir).unwrap();

        let worker = tokio::spawn(async { panic!("worker died") });
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
        context.recording.set_worker(worker);
        stop_worker(&context.recording).await;
        assert_eq!(
            store::read_meta(&dir).unwrap().attempts,
            1,
            "a panic is not an interruption"
        );
    }

    #[tokio::test]
    async fn recovery_skips_a_recording_a_retry_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        let before = store::read_meta(&dir).unwrap();
        // A retry reserved it and is repairing it.
        context
            .recording
            .reserve(&ActiveRecording::from_meta(&dir, &before))
            .unwrap();

        recover_on_startup(Arc::clone(&context)).await;
        assert!(queue.try_recv().is_err(), "not queued twice");
        assert_eq!(
            store::read_meta(&dir).unwrap(),
            before,
            "not repaired twice"
        );
        assert_eq!(context.recording.status().queued, vec![before.id.clone()]);

        // A recording recovery holds refuses a retry.
        context.recording.unreserve(&before.id);
        recover_on_startup(Arc::clone(&context)).await;
        let err = retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already queued"), "{err:#}");
        assert!(queue.try_recv().is_ok());
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn recovery_leaves_a_finished_recording_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crashed_recording(tmp.path());
        let mut meta = store::read_meta(&dir).unwrap();
        meta.status = RecordingState::Done;
        store::write_meta(&dir, &meta).unwrap();
        assert!(recover_interrupted(&dir).unwrap().is_none());
        assert_eq!(store::read_meta(&dir).unwrap(), meta);
    }

    #[test]
    fn a_not_transcribed_stop_is_notified_once() {
        let message = format!("{NOT_TRANSCRIBED}: the microphone track was still being written");
        assert!(error_already_notified(&message));
        assert!(!error_already_notified("no recording is in progress"));
    }

    #[tokio::test]
    async fn an_abandoned_start_is_deleted_only_without_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context(tmp.path(), working_backend());

        // Devices still opening: recording.json and a header-only track.
        let (_, empty) = store::create(tmp.path(), Local::now(), Some("empty")).unwrap();
        std::fs::write(
            store::audio_path(&empty, TrackKind::Mic),
            whisrs::audio::wav::encode_wav(&[]).unwrap(),
        )
        .unwrap();
        {
            let _starting = StartingGuard::new(&context.recording, &empty);
            abandon_start(&context.recording).await;
        }
        assert!(!empty.exists());

        // Audio already written: kept for recovery.
        let with_audio = crashed_recording(tmp.path());
        {
            let _starting = StartingGuard::new(&context.recording, &with_audio);
            abandon_start(&context.recording).await;
        }
        assert!(with_audio.exists());
        assert!(context.recording.starting.lock().unwrap().is_none());
    }

    /// One `[[llm_commands]]` entry, for the dictation refusal tests.
    const GERMAN: &str = "[[llm_commands]]\nname = \"german\"\n\
                          hotkey = \"Super+Shift+G\"\ninstruction = \"Translate to German.\"\n";

    fn idle_dictation() -> Arc<tokio::sync::Mutex<DaemonState>> {
        Arc::new(tokio::sync::Mutex::new(DaemonState::new()))
    }

    fn assert_refused(response: &Response, subject: &str) {
        let Response::Error { message } = response else {
            panic!("expected a refusal, got {response:?}");
        };
        assert_eq!(
            message,
            &format!("A recording is in progress. {subject} is off until you stop it.")
        );
    }

    /// A toggle from idle while a recording is in progress is refused before it
    /// opens the mic or moves the state machine. It carries a bad `-l`, the
    /// next check after the refusal, so a broken refusal fails here on the
    /// language error instead of opening the host's mic.
    #[tokio::test]
    async fn dictation_is_refused_while_a_recording_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context(tmp.path(), working_backend());
        context
            .recording
            .update(|v| v.capturing = Some(active("rec")));
        let ds = idle_dictation();

        let response = handle_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Some("english".into()),
        )
        .await;
        assert_refused(&response, "Dictation");
        let ds = ds.lock().await;
        assert_eq!(ds.state_machine.state(), State::Idle);
        assert!(ds.audio_capture.is_none(), "no capture is opened");
        assert!(ds.recording_started_at.is_none());
        assert!(ds.session_language.is_none());
    }

    /// The refusal for every mode: an error naming the mode while a recording
    /// is in progress, nothing while it is idle or being transcribed. Command mode
    /// and llm-command are tested here and by source order below, not
    /// through their handlers: with no seam before the selection and the
    /// mic, a broken refusal would copy the host's selection (sending Ctrl+C
    /// when it is empty) and open its mic.
    #[test]
    fn the_refusal_names_each_mode_and_fires_only_while_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context(tmp.path(), working_backend());
        let modes = [
            (DictationStart::Dictation, "Dictation"),
            (DictationStart::CommandMode, "Command mode"),
            (DictationStart::LlmCommand("german"), "'german'"),
        ];

        for (start, _) in modes {
            assert!(
                refuse_while_recording(&context, start).is_none(),
                "{start:?}"
            );
        }
        context
            .recording
            .update(|v| v.capturing = Some(active("rec")));
        for (start, subject) in modes {
            let response = refuse_while_recording(&context, start)
                .unwrap_or_else(|| panic!("{start:?} was not refused"));
            assert_refused(&response, subject);
        }
        context.recording.update(|v| {
            v.capturing = None;
            v.processing = Some(active("rec"));
        });
        for (start, _) in modes {
            assert!(
                refuse_while_recording(&context, start).is_none(),
                "{start:?}"
            );
        }
    }

    /// The llm-command refusal comes after the entry lookup, so a misspelled
    /// name keeps its config error during a recording. An unknown name never
    /// reaches a device, so the handler is safe to call here.
    #[tokio::test]
    async fn an_unknown_llm_command_keeps_its_config_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context_with(tmp.path(), working_backend(), GERMAN);
        context
            .recording
            .update(|v| v.capturing = Some(active("rec")));
        let ds = idle_dictation();

        let response =
            handle_llm_command(Arc::clone(&ds), Arc::clone(&context), "nope".into()).await;
        assert!(
            matches!(&response, Response::Error { message } if message.contains("no llm_commands entry")),
            "{response:?}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
    }

    /// Calls a refused start must not reach: the mic, the start cue, the
    /// selection (whose capture may send Ctrl+C), the focused window, and
    /// the two start fns that make those calls.
    const DEVICE_CALLS: &[&str] = &[
        "AudioCaptureHandle::",
        "feedback::play_start(",
        "capture_selection(",
        "acquire_selected_text(",
        "get_focused_window(",
        "command_mode_start(",
        "llm_command_start(",
    ];

    /// `source` before its test module, without comment lines, so neither
    /// prose nor the strings in a test can match.
    fn production_code(source: &str) -> String {
        source
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a production half")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The top-level fn starting at `signature`, up to its closing brace.
    fn fn_body<'a>(code: &'a str, signature: &str) -> &'a str {
        let start = code
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found"));
        let rest = &code[start..];
        &rest[..rest.find("\n}").expect("the fn ends")]
    }

    /// A recording start holds agent replies (and stops one playing) before
    /// anything that can take a while: the free-space check, the device
    /// lookup and opening the tracks. Not only once its devices are open.
    #[test]
    fn a_recording_start_holds_agent_replies_first() {
        // This file has a test-only item before its test module, so it is
        // cut at the module itself.
        let source = include_str!("recording.rs");
        let code = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        let body = fn_body(code, "async fn start(");
        let hold = body
            .find("crate::agent::hold_for_recording(context)")
            .expect("start holds agent replies");
        for later in ["check_space_to_start(", "resolve_system)", "open_tracks)"] {
            let at = body
                .find(later)
                .unwrap_or_else(|| panic!("{later} not found"));
            assert!(hold < at, "the hold comes after {later}");
        }
        // Bound to a name, so it lives until the start returns.
        assert!(body.contains("let _replies = crate::agent::hold_for_recording"));
    }

    /// Dictation and voice-command toasts never say "recording", the
    /// recording feature's word, so the two cannot be mistaken for each
    /// other. The audio gate's skip toast is built in one place and pinned
    /// by its own test (`gate_skip_message_per_origin`).
    #[test]
    fn dictation_toasts_never_say_recording() {
        for (file, source) in [
            ("dictation.rs", include_str!("dictation.rs")),
            ("command_mode.rs", include_str!("command_mode.rs")),
            ("pipeline.rs", include_str!("pipeline.rs")),
        ] {
            let code = production_code(source);
            let calls: Vec<&str> = code
                .split("send_notification(")
                .skip(1)
                .map(|call| &call[..call.find(");").unwrap_or(call.len())])
                .collect();
            assert!(!calls.is_empty(), "{file}: no toasts found");
            for call in calls {
                assert!(
                    !call.to_lowercase().contains("recording"),
                    "{file}: a toast says recording: {call}"
                );
            }
        }
    }

    /// Each start handler calls `refuse_while_recording` before anything in
    /// [`DEVICE_CALLS`]. This pins command mode and llm-command, whose
    /// handlers no test may run with a broken refusal; toggle is pinned too.
    /// The two start fns are private and called once each, from their
    /// handler, so nothing reaches them around the refusal.
    #[test]
    fn every_start_is_refused_before_it_touches_a_device() {
        let dictation = production_code(include_str!("dictation.rs"));
        let command_mode = production_code(include_str!("command_mode.rs"));
        let cases = [
            (&dictation, "fn handle_toggle(", "DictationStart::Dictation"),
            (
                &command_mode,
                "fn handle_command_mode(",
                "DictationStart::CommandMode",
            ),
            (
                &command_mode,
                "fn handle_llm_command(",
                "DictationStart::LlmCommand(",
            ),
        ];
        for (code, signature, mode) in cases {
            let body = fn_body(code, signature);
            let refusal = body
                .find(&format!("refuse_while_recording(&context, {mode}"))
                .unwrap_or_else(|| panic!("{signature} does not refuse {mode}"));
            for call in DEVICE_CALLS {
                assert!(
                    !body[..refusal].contains(call),
                    "{signature} reaches {call} before the recording refusal"
                );
            }
            assert!(
                DEVICE_CALLS
                    .iter()
                    .any(|call| body[refusal..].contains(call)),
                "{signature}: the start path after the refusal was not found"
            );
        }
        for start_fn in ["command_mode_start(", "llm_command_start("] {
            assert_eq!(
                command_mode.matches(start_fn).count(),
                2,
                "{start_fn} must be defined once and called once, by its handler"
            );
            assert!(command_mode.contains(&format!("async fn {start_fn}")));
        }
    }

    /// Only starts are refused. A dictation already recording when the
    /// recording started stops and goes on to transcription: with no device in the
    /// test it has no capture, so the batch path fails to collect audio and
    /// returns to idle. Command-mode and llm-command sessions still stop on
    /// their second press and cancel.
    #[tokio::test]
    async fn a_session_started_before_the_recording_still_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context_with(tmp.path(), working_backend(), GERMAN);
        let ds = idle_dictation();
        ds.lock()
            .await
            .state_machine
            .transition(Action::Toggle)
            .unwrap();
        context
            .recording
            .update(|v| v.capturing = Some(active("rec")));

        let response = handle_toggle(Arc::clone(&ds), Arc::clone(&context), None).await;
        assert!(
            matches!(response, Response::Ok { state: State::Idle }),
            "{response:?}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);

        {
            let mut ds = ds.lock().await;
            ds.state_machine.transition(Action::Toggle).unwrap();
            ds.command_mode = Some(CommandModeContext {
                selected_text: "selected".into(),
                llm_config: Default::default(),
            });
        }
        let response = handle_command_mode(Arc::clone(&ds), Arc::clone(&context)).await;
        assert!(
            matches!(
                response,
                Response::Ok {
                    state: State::Recording
                }
            ),
            "{response:?}"
        );
        let response = handle_cancel(Arc::clone(&ds), Arc::clone(&context)).await;
        assert!(
            matches!(response, Response::Ok { state: State::Idle }),
            "{response:?}"
        );

        {
            let mut ds = ds.lock().await;
            ds.state_machine.transition(Action::Toggle).unwrap();
            ds.llm_command = Some(LlmCommandContext {
                name: "german".into(),
                instruction: "Translate to German.".into(),
                llm_config: Default::default(),
            });
        }
        let response =
            handle_llm_command(Arc::clone(&ds), Arc::clone(&context), "german".into()).await;
        assert!(
            matches!(
                response,
                Response::Ok {
                    state: State::Recording
                }
            ),
            "{response:?}"
        );
        let response = handle_cancel(Arc::clone(&ds), Arc::clone(&context)).await;
        assert!(
            matches!(response, Response::Ok { state: State::Idle }),
            "{response:?}"
        );
        assert!(
            context.recording.is_capturing(),
            "the recording is untouched"
        );
    }

    /// Only a recording in progress refuses: one being transcribed or queued
    /// does not. The start carries a bad `-l`, so it fails at the language
    /// check right after the refusal, before any device opens; that error,
    /// not the recording's, shows the start got past the refusal.
    #[tokio::test]
    async fn a_recording_being_transcribed_does_not_block_dictation() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, _queue) = test_context(tmp.path(), working_backend());
        let ds = idle_dictation();

        context
            .recording
            .update(|v| v.processing = Some(active("proc")));
        assert_start_reaches_the_language_check(&ds, &context).await;

        context.recording.update(|v| {
            v.processing = None;
            v.queued = vec![active("next")];
        });
        assert_start_reaches_the_language_check(&ds, &context).await;
    }

    async fn assert_start_reaches_the_language_check(
        ds: &Arc<tokio::sync::Mutex<DaemonState>>,
        context: &Arc<DaemonContext>,
    ) {
        assert_eq!(context.recording.status().phase, RecordingPhase::Processing);
        let response =
            handle_toggle(Arc::clone(ds), Arc::clone(context), Some("english".into())).await;
        let Response::Error { message } = &response else {
            panic!("expected the language error, got {response:?}");
        };
        assert!(
            message.starts_with("invalid language 'english'"),
            "{message}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
    }

    #[test]
    fn disk_room_thresholds() {
        const MB: u64 = 1_000_000;
        // 5.76 MB a minute: 1 GB is about three hours.
        assert_eq!(minutes_that_fit(1000 * MB, Duration::ZERO), 173);
        assert_eq!(disk_room(1000 * MB, Duration::ZERO), DiskRoom::Enough);
        assert_eq!(disk_room(150 * MB, Duration::ZERO), DiskRoom::Low(26));
        assert_eq!(disk_room(12 * MB, Duration::ZERO), DiskRoom::Low(2));
        assert_eq!(disk_room(11 * MB, Duration::ZERO), DiskRoom::Full(1));
        assert_eq!(disk_room(0, Duration::ZERO), DiskRoom::Full(0));
        // An hour in, the echo-cancelled copy of that hour (115.2 MB) is
        // held back for processing: 200 MB is 14 minutes, not 34.
        let hour = Duration::from_secs(3600);
        assert_eq!(minutes_that_fit(200 * MB, Duration::ZERO), 34);
        assert_eq!(disk_room(200 * MB, hour), DiskRoom::Low(14));
        assert_eq!(disk_room(120 * MB, hour), DiskRoom::Full(0));
        // The start needs about 10 minutes plus headroom.
        assert_eq!(minutes_that_fit(DISK_MIN_TO_START, Duration::ZERO), 17);
    }

    fn fifty_mb_free(_: &Path) -> std::io::Result<u64> {
        Ok(50_000_000)
    }

    fn five_mb_free(_: &Path) -> std::io::Result<u64> {
        Ok(5_000_000)
    }

    fn unreadable_free_space(_: &Path) -> std::io::Result<u64> {
        Err(std::io::Error::other("statvfs failed"))
    }

    /// A start on a nearly full disk is refused before it creates a folder
    /// or opens a device; one whose free space cannot be read goes ahead.
    /// Test contexts have no devices ([`DEVICES`]): a start that got past
    /// a broken check would fail on that, never open the host's mic.
    #[tokio::test]
    async fn a_start_without_disk_space_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("recordings");
        let (context, _queue) = build_context(&root, working_backend(), "", fifty_mb_free);
        let err = start(
            &context,
            Some("standup".into()),
            &RecordingOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "not enough free disk space to record: 50 MB free in {}, and a recording \
                 needs 100 MB (about 10 minutes plus room to transcribe them)",
                root.display()
            )
        );
        assert!(!root.exists(), "no folder was created");
        assert!(!context.recording.is_capturing());

        assert!(
            check_space_to_start(unreadable_free_space, &root, FREE_SPACE_TIMEOUT)
                .await
                .is_ok()
        );
        assert!(
            check_space_to_start(store::free_space, tmp.path(), FREE_SPACE_TIMEOUT)
                .await
                .is_ok()
        );
    }

    /// With the disk almost full the watch stops the recording the way the
    /// time limit does: tracks finalized, `end_reason: disk_full`, queued.
    #[tokio::test]
    async fn a_full_disk_stops_the_recording_like_the_time_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = build_context(tmp.path(), working_backend(), "", five_mb_free);
        let (mut session, tx) = fed_session(tmp.path(), "standup");
        let id = session.meta.id.clone();
        let dir = session.dir.clone();
        tx.send(vec![1000i16; 3200]).unwrap();
        session.watch = Some(spawn_watch(
            &context,
            id.clone(),
            dir.clone(),
            180,
            Duration::from_millis(10),
        ));
        show_recording(&context, session).await;

        let deadline = Instant::now() + Duration::from_secs(5);
        while context.recording.session.lock().await.is_some() {
            assert!(Instant::now() < deadline, "the watch never stopped it");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The capture closes as a device would at a stop.
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(10);
        while store::read_meta(&dir).unwrap().status != RecordingState::Processing {
            assert!(Instant::now() < deadline, "never finalized");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.end_reason, Some(EndReason::DiskFull));
        assert_eq!(meta.tracks[0].samples, 3200);
        let item = tokio::time::timeout(Duration::from_secs(5), queue.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(item.recording.id, id);
        assert!(!context.recording.is_capturing());
    }

    /// Plenty of room: the watch keeps checking and leaves the recording be.
    #[tokio::test]
    async fn the_watch_leaves_a_recording_with_room_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) =
            build_context(tmp.path(), working_backend(), "", store::free_space);
        let (mut session, _tx) = fed_session(tmp.path(), "standup");
        session.watch = Some(spawn_watch(
            &context,
            session.meta.id.clone(),
            session.dir.clone(),
            180,
            Duration::from_millis(5),
        ));
        show_recording(&context, session).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(context.recording.session.lock().await.is_some());
        assert!(context.recording.is_capturing());
        assert!(queue.try_recv().is_err());
        let session = context.recording.session.lock().await.take().unwrap();
        session.watch.unwrap().abort();
    }

    /// A session fed by the test whose writer has already finished. The
    /// writer runs on the blocking pool, which a paused clock does not wait
    /// for: a stop's writer timeout could otherwise fire before it is done.
    fn finished_session(root: &Path) -> RecordingSession {
        let (session, tx) = fed_session(root, "standup");
        tx.send(vec![1000i16; 3200]).unwrap();
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.tracks[0].writer.is_finished() {
            assert!(Instant::now() < deadline, "the writer never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
        session
    }

    /// The time limit, through the watch on a paused clock: the recording
    /// stops at its limit to the second, finalized and queued like any stop.
    #[tokio::test(start_paused = true)]
    async fn the_watch_stops_a_recording_at_its_time_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        let mut session = finished_session(tmp.path());
        let (id, dir) = (session.meta.id.clone(), session.dir.clone());
        let started = tokio::time::Instant::now();
        session.watch = Some(spawn_watch(
            &context,
            id.clone(),
            dir.clone(),
            1,
            Duration::from_secs(60),
        ));
        show_recording(&context, session).await;

        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(context.recording.is_capturing(), "stopped before its limit");
        let item = tokio::time::timeout(Duration::from_secs(120), queue.recv())
            .await
            .expect("not stopped at its limit")
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert_eq!(item.recording.id, id);
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.end_reason, Some(EndReason::MaxDuration));
        assert_eq!(meta.tracks[0].samples, 3200);
        assert!(!context.recording.is_capturing());
    }

    /// A free-space read on a dead network mount: counts itself in `reads`
    /// and returns only once `release` is set.
    fn hang_until(reads: &AtomicUsize, release: &AtomicBool) -> std::io::Result<u64> {
        reads.fetch_add(1, Ordering::SeqCst);
        while !release.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(u64::MAX)
    }

    /// Sets its flag however the test ends, so a hung read returns and the
    /// runtime's shutdown does not wait on it forever.
    struct Release(&'static AtomicBool);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// `read`, failing the test instead of hanging it when a hung free-space
    /// read holds it up.
    async fn bounded<T>(read: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("held up by a hung free-space read")
    }

    static WATCH_READS: AtomicUsize = AtomicUsize::new(0);
    static WATCH_RELEASE: AtomicBool = AtomicBool::new(false);

    fn watch_hung_read(_: &Path) -> std::io::Result<u64> {
        hang_until(&WATCH_READS, &WATCH_RELEASE)
    }

    /// A free-space read that hangs (a dead network mount) is given up on
    /// after the timeout and never past the time limit, and is waited on
    /// again at the next check instead of started anew. Real time: a paused
    /// clock does not move while a blocking task runs.
    #[tokio::test]
    async fn a_hung_free_space_read_is_bounded_and_not_repeated() {
        let _release = Release(&WATCH_RELEASE);
        let tmp = tempfile::tempdir().unwrap();
        let mut reader = FreeSpaceReader::new(watch_hung_read, tmp.path().to_path_buf());
        reader.timeout = Duration::from_millis(20);
        let far = tokio::time::Instant::now() + Duration::from_secs(600);
        for _ in 0..3 {
            let asked = Instant::now();
            assert_eq!(bounded(reader.free("m", far)).await, None);
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "{:?}",
                asked.elapsed()
            );
        }
        // However long the timeout, never past the time limit.
        reader.timeout = Duration::from_secs(600);
        let asked = Instant::now();
        let limit = tokio::time::Instant::now() + Duration::from_millis(30);
        assert_eq!(bounded(reader.free("m", limit)).await, None);
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(WATCH_READS.load(Ordering::SeqCst), 1, "one read, waited on");

        // Once it answers, the next check takes its answer.
        WATCH_RELEASE.store(true, Ordering::Release);
        assert_eq!(bounded(reader.free("m", far)).await, Some(u64::MAX));
        assert_eq!(WATCH_READS.load(Ordering::SeqCst), 1);
    }

    static START_READS: AtomicUsize = AtomicUsize::new(0);
    static START_RELEASE: AtomicBool = AtomicBool::new(false);

    fn start_hung_read(_: &Path) -> std::io::Result<u64> {
        hang_until(&START_READS, &START_RELEASE)
    }

    /// A start whose free-space read hangs goes ahead without the check.
    #[tokio::test]
    async fn a_start_does_not_wait_on_a_hung_free_space_read() {
        let _release = Release(&START_RELEASE);
        let tmp = tempfile::tempdir().unwrap();
        let asked = Instant::now();
        bounded(check_space_to_start(
            start_hung_read,
            tmp.path(),
            Duration::from_millis(20),
        ))
        .await
        .unwrap();
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
    }

    /// What [`FlakyNetwork`] does with a request.
    const DOWN: u8 = 0;
    const UP: u8 = 1;
    const HANG: u8 = 2;

    /// A cloud backend whose network can be down (every request ends the
    /// way the retry layer gives up on one), back up, or slow (a request
    /// that hangs until `release`, then finds the network down).
    struct FlakyNetwork {
        mode: std::sync::atomic::AtomicU8,
        calls: AtomicUsize,
        release: tokio::sync::Notify,
    }

    impl FlakyNetwork {
        fn new(mode: u8) -> Arc<Self> {
            Arc::new(Self {
                mode: std::sync::atomic::AtomicU8::new(mode),
                calls: AtomicUsize::new(0),
                release: tokio::sync::Notify::new(),
            })
        }

        fn set(&self, mode: u8) {
            self.mode.store(mode, Ordering::Release);
        }
    }

    #[async_trait::async_trait]
    impl TranscriptionBackend for FlakyNetwork {
        async fn transcribe(&self, _audio: &[u8], _config: &TranscriptionConfig) -> Result<String> {
            unreachable!("recordings ask for segments")
        }
        fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
            false
        }
        fn stream_text(&self) -> StreamText {
            StreamText::Phrases
        }
        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &TranscriptionConfig,
        ) -> Result<Transcript> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mode = self.mode.load(Ordering::Acquire);
            if mode == HANG {
                self.release.notified().await;
            }
            match mode {
                DOWN | HANG => Err(whisrs::transcription::retry::NetworkUnavailable {
                    message: "could not reach the transcription service after 110 s of \
                              retries: Connection refused (os error 111)"
                        .into(),
                }
                .into()),
                _ => Ok(Transcript {
                    segments: vec![Segment {
                        start_ms: 0,
                        end_ms: 1000,
                        text: "back online".into(),
                        speaker: None,
                        words: None,
                    }],
                }),
            }
        }
        fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
            None
        }
    }

    /// A crashed recording whose first attempt found no network: it is
    /// left waiting, not failed, with the attempt counted as usual.
    async fn waiting_recording(
        context: &Arc<DaemonContext>,
        queue: &mut mpsc::UnboundedReceiver<QueuedRecording>,
        root: &Path,
    ) -> (PathBuf, String) {
        let dir = crashed_recording(root);
        recover_on_startup(Arc::clone(context)).await;
        process_one(context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Waiting);
        (dir, meta.id)
    }

    #[tokio::test]
    async fn a_network_failure_waits_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (dir, id) = waiting_recording(&context, &mut queue, tmp.path()).await;

        let meta = store::read_meta(&dir).unwrap();
        assert!(
            meta.error
                .as_deref()
                .is_some_and(|e| e.contains("could not reach the transcription service")),
            "{:?}",
            meta.error
        );
        let wait = meta.network_wait.clone().unwrap();
        assert_eq!(wait.attempts, 0);
        assert_eq!(wait.since, wait.last_attempt_at);
        assert_eq!(meta.attempts, 1, "the first attempt counts as usual");
        let status = context.recording.status();
        assert_eq!(status.phase, RecordingPhase::Idle);
        assert_eq!(status.waiting, vec![id]);
        // The next attempt finds the wait already started: no new notice.
        let (_, started) = mark_waiting(&dir, "still down", Local::now()).unwrap();
        assert!(!started);
    }

    #[test]
    fn network_retry_schedule() {
        let now = Local::now();
        let minutes = chrono::Duration::minutes;
        let mut meta = RecordingMeta::new("m".into(), None, now - minutes(600));
        meta.status = RecordingState::Waiting;
        // Stopped hours before its first failed try: the stop does not count.
        meta.ended_at = Some(now - minutes(500));
        assert_eq!(network_retry_due(&meta, now, false), NetworkRetry::Due);
        meta.network_wait = Some(NetworkWait {
            since: now - minutes(60),
            attempts: 3,
            last_attempt_at: now - minutes(5),
        });
        assert_eq!(network_retry_due(&meta, now, false), NetworkRetry::NotYet);
        assert_eq!(
            network_retry_due(&meta, now, true),
            NetworkRetry::Due,
            "a daemon start tries at once"
        );
        assert_eq!(
            network_retry_due(&meta, now + minutes(5), false),
            NetworkRetry::Due,
            "ten minutes after the last attempt"
        );

        // A day after the first failed try, or out of attempts: give up, at
        // startup too.
        let deadline = meta.network_deadline().unwrap();
        assert_eq!(deadline, now - minutes(60) + chrono::Duration::hours(24));
        assert_eq!(
            network_retry_due(&meta, deadline - minutes(1), false),
            NetworkRetry::Due
        );
        for startup in [false, true] {
            let NetworkRetry::GiveUp(why) = network_retry_due(&meta, deadline, startup) else {
                panic!("not given up at the deadline");
            };
            assert_eq!(
                why,
                "gave up waiting for the network 24 hours after its first failed try"
            );
        }
        meta.network_wait.as_mut().unwrap().attempts = NETWORK_RETRY_MAX;
        let NetworkRetry::GiveUp(why) = network_retry_due(&meta, now, true) else {
            panic!("not given up at the cap");
        };
        assert!(why.contains("after 144 automatic attempts"), "{why}");
    }

    /// Automatic attempts come every ten minutes, never count towards
    /// [`MAX_ATTEMPTS`], stay quiet, and finish the recording once the
    /// network is back.
    #[tokio::test]
    async fn automatic_retries_finish_the_recording_when_the_network_is_back() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (dir, id) = waiting_recording(&context, &mut queue, tmp.path()).await;
        let since = store::read_meta(&dir).unwrap().network_wait.unwrap().since;

        // Not yet due.
        retry_waiting(&context, Local::now() + chrono::Duration::minutes(5), false).await;
        assert!(queue.try_recv().is_err());
        assert_eq!(context.recording.status().waiting, vec![id.clone()]);

        // More automatic attempts than MAX_ATTEMPTS, all without network.
        for round in 1..=(MAX_ATTEMPTS + 2) {
            let later = Local::now() + chrono::Duration::minutes(11);
            retry_waiting(&context, later, false).await;
            let item = queue
                .try_recv()
                .expect("due ten minutes after the last attempt");
            assert!(item.recording.network_retry);
            assert!(
                context.recording.status().waiting.is_empty(),
                "queued, not waiting"
            );
            process_one(&context, item).await;
            let meta = store::read_meta(&dir).unwrap();
            assert_eq!(meta.status, RecordingState::Waiting);
            assert_eq!(meta.attempts, 1, "automatic attempts are not counted");
            let wait = meta.network_wait.unwrap();
            assert_eq!(wait.attempts, round);
            assert_eq!(wait.since, since, "still the same wait: notified once");
        }

        backend.set(UP);
        retry_waiting(
            &context,
            Local::now() + chrono::Duration::minutes(11),
            false,
        )
        .await;
        process_one(&context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.network_wait, None);
        assert_eq!(meta.attempts, 1);
        let md = std::fs::read_to_string(store::transcript_path(&dir, &id)).unwrap();
        assert!(md.contains("back online"), "{md}");
        assert_eq!(context.recording.status(), RecordingStatus::default());
    }

    /// A day after its first failed try, or after [`NETWORK_RETRY_MAX`]
    /// attempts, a waiting recording becomes a plain failure that a retry
    /// reruns.
    #[tokio::test]
    async fn waiting_ends_in_failed_after_a_day_or_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (dir, _) = waiting_recording(&context, &mut queue, tmp.path()).await;

        let day_later = Local::now() + chrono::Duration::hours(25);
        retry_waiting(&context, day_later, false).await;
        assert!(queue.try_recv().is_err(), "not queued");
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Failed);
        let error = meta.error.unwrap();
        assert!(
            error.starts_with(
                "gave up waiting for the network 24 hours after its first failed try: "
            ) && error.contains("could not reach the transcription service"),
            "{error}"
        );
        assert_eq!(context.recording.status(), RecordingStatus::default());

        // The cap, found by a restarted daemon.
        let mut meta = store::read_meta(&dir).unwrap();
        meta.status = RecordingState::Waiting;
        meta.network_wait.as_mut().unwrap().attempts = NETWORK_RETRY_MAX;
        store::write_meta(&dir, &meta).unwrap();
        let (next, mut next_queue) = test_context(tmp.path(), backend.clone());
        recover_on_startup(Arc::clone(&next)).await;
        assert!(next_queue.try_recv().is_err());
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Failed);
        assert!(meta
            .error
            .unwrap()
            .starts_with("gave up waiting for the network after 144"));

        // A manual retry reruns it, with a fresh count.
        backend.set(UP);
        retry(&next, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&next, next_queue.try_recv().unwrap()).await;
        assert_eq!(store::read_meta(&dir).unwrap().status, RecordingState::Done);
    }

    /// `whisrs recording retry` on a waiting recording runs it at once as a
    /// normal attempt: the wait and the counts start over.
    #[tokio::test]
    async fn a_manual_retry_ends_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (dir, _) = waiting_recording(&context, &mut queue, tmp.path()).await;

        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Processing);
        assert_eq!(meta.network_wait, None);
        assert_eq!(meta.attempts, 0);
        assert!(context.recording.status().waiting.is_empty());
        let item = queue.try_recv().unwrap();
        assert!(!item.recording.network_retry);
        // A tick meanwhile does not queue it twice.
        retry_waiting(&context, Local::now() + chrono::Duration::hours(1), false).await;
        assert!(queue.try_recv().is_err());

        backend.set(UP);
        process_one(&context, item).await;
        assert_eq!(store::read_meta(&dir).unwrap().status, RecordingState::Done);
    }

    /// The day of automatic tries counts from the first failed try, not the
    /// stop: a recording stopped days ago (a laptop shut before it was
    /// transcribed) whose first try finds no network still waits, and
    /// finishes once the network is back.
    #[tokio::test]
    async fn the_network_window_starts_at_the_first_failed_try() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let dir = crashed_recording(tmp.path());
        let mut meta = store::read_meta(&dir).unwrap();
        meta.ended_at = Some(Local::now() - chrono::Duration::days(3));
        store::write_meta(&dir, &meta).unwrap();
        recover_on_startup(Arc::clone(&context)).await;
        process_one(&context, queue.try_recv().unwrap()).await;

        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Waiting);
        let since = meta.network_wait.as_ref().unwrap().since;
        assert_eq!(
            meta.network_deadline(),
            Some(since + chrono::Duration::hours(24))
        );
        retry_waiting(
            &context,
            Local::now() + chrono::Duration::minutes(11),
            false,
        )
        .await;
        let item = queue.try_recv().expect("tried again, not given up");
        backend.set(UP);
        process_one(&context, item).await;
        assert_eq!(store::read_meta(&dir).unwrap().status, RecordingState::Done);
    }

    /// A manual retry that finds no network starts a new day of tries, even
    /// when the old wait was about to end.
    #[tokio::test]
    async fn a_manual_retry_starts_a_new_network_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), FlakyNetwork::new(DOWN));
        let (dir, _) = waiting_recording(&context, &mut queue, tmp.path()).await;
        let mut meta = store::read_meta(&dir).unwrap();
        let old = Local::now() - chrono::Duration::hours(23) - chrono::Duration::minutes(55);
        meta.network_wait = Some(NetworkWait {
            since: old,
            attempts: 100,
            last_attempt_at: old,
        });
        store::write_meta(&dir, &meta).unwrap();

        retry(&context, "last", &RecordingOptions::default())
            .await
            .unwrap();
        process_one(&context, queue.try_recv().unwrap()).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Waiting);
        let wait = meta.network_wait.clone().unwrap();
        assert!(
            wait.since > old + chrono::Duration::hours(23),
            "a new window"
        );
        assert_eq!(wait.attempts, 0);
        retry_waiting(
            &context,
            Local::now() + chrono::Duration::minutes(11),
            false,
        )
        .await;
        assert!(queue.try_recv().is_ok(), "tried again, not given up");
    }

    /// `cancel` acts on the recording in progress only: with none, it is the
    /// usual error, and a recording waiting for the network keeps waiting.
    #[tokio::test]
    async fn cancel_leaves_a_waiting_recording_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), FlakyNetwork::new(DOWN));
        let (dir, id) = waiting_recording(&context, &mut queue, tmp.path()).await;
        let before = store::read_meta(&dir).unwrap();

        let err = cancel(&context).await.unwrap_err();
        assert_eq!(err.to_string(), "no recording is in progress");
        assert_eq!(store::read_meta(&dir).unwrap(), before);
        assert_eq!(context.recording.status().waiting, vec![id]);
    }

    /// A restarted daemon tries a waiting recording at once, as an
    /// automatic attempt, and a shutdown during one puts it back to waiting
    /// without touching the crash count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_survives_a_daemon_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (dir, _) = waiting_recording(&context, &mut queue, tmp.path()).await;

        // The next daemon start: queued at once, its last attempt was just now.
        backend.set(HANG);
        let (next, next_queue) = test_context(tmp.path(), backend.clone());
        recover_on_startup(Arc::clone(&next)).await;
        assert_eq!(next.recording.status().queued.len(), 1);
        next.recording
            .set_worker(tokio::spawn(run_worker(Arc::clone(&next), next_queue)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while store::read_meta(&dir).unwrap().status != RecordingState::Processing {
            assert!(
                Instant::now() < deadline,
                "the automatic attempt never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown(&next).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Waiting, "back to waiting");
        assert_eq!(meta.attempts, 1, "not given back, never counted");
        assert_eq!(meta.network_wait.unwrap().attempts, 1);

        // And the one after that finishes it.
        backend.set(UP);
        let (last, mut last_queue) = test_context(tmp.path(), backend.clone());
        recover_on_startup(Arc::clone(&last)).await;
        let item = last_queue.try_recv().unwrap();
        assert!(item.recording.network_retry);
        process_one(&last, item).await;
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(meta.status, RecordingState::Done);
        assert_eq!(meta.attempts, 1);
    }

    /// An automatic attempt goes through the processing queue: while
    /// another recording is being transcribed it waits its turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automatic_attempt_waits_for_the_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FlakyNetwork::new(DOWN);
        let (context, mut queue) = test_context(tmp.path(), backend.clone());
        let (_, waiting_id) = waiting_recording(&context, &mut queue, tmp.path()).await;

        // Another recording is being transcribed (slowly).
        backend.set(HANG);
        let sent_before = backend.calls.load(Ordering::Relaxed);
        let busy = crashed_recording(tmp.path());
        let meta = recover_dir(&busy, None).unwrap();
        let busy_id = meta.id.clone();
        context
            .recording
            .enqueue(ActiveRecording::from_meta(&busy, &meta))
            .unwrap();
        context
            .recording
            .set_worker(tokio::spawn(run_worker(Arc::clone(&context), queue)));
        // The worker shows the busy recording as soon as it takes it, before
        // its request goes out, so wait for that request too.
        let deadline = Instant::now() + Duration::from_secs(5);
        while context.recording.status().id.as_deref() != Some(busy_id.as_str())
            || !context.recording.status().queued.is_empty()
            || backend.calls.load(Ordering::Relaxed) == sent_before
        {
            assert!(Instant::now() < deadline, "the worker never took it");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let calls = backend.calls.load(Ordering::Relaxed);

        retry_waiting(
            &context,
            Local::now() + chrono::Duration::minutes(11),
            false,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let status = context.recording.status();
        assert_eq!(
            status.id.as_deref(),
            Some(busy_id.as_str()),
            "still the busy one"
        );
        assert_eq!(status.queued, vec![waiting_id], "queued behind it");
        assert_eq!(
            backend.calls.load(Ordering::Relaxed),
            calls,
            "not sent meanwhile"
        );
        context
            .recording
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .abort();
    }

    /// A recording session with its folder and meta on disk and a mic track
    /// fed by the returned sender instead of a device: its writer finishes
    /// only once the sender is dropped, like a slow device at a stop.
    fn fed_session(
        root: &Path,
        title: &str,
    ) -> (
        RecordingSession,
        mpsc::UnboundedSender<whisrs::audio::AudioChunk>,
    ) {
        let (id, dir) = store::create(root, Local::now(), Some(title)).unwrap();
        let mut meta = RecordingMeta::new(id, Some(title.into()), Local::now());
        meta.tracks = vec![track_meta(TrackKind::Mic, "default".into())];
        store::write_meta(&dir, &meta).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = spawn_track_writer(rx, store::audio_path(&dir, TrackKind::Mic), FLUSH_EVERY);
        let session = RecordingSession {
            dir,
            meta,
            started: Instant::now(),
            tracks: vec![TrackCapture::new(TrackKind::Mic, None, writer)],
            watch: None,
            stopping: Arc::new(AtomicBool::new(false)),
        };
        (session, tx)
    }

    /// Install `session` the way `start` does once its devices are open.
    async fn show_recording(context: &DaemonContext, session: RecordingSession) {
        let mut slot = context.recording.session.lock().await;
        let active = ActiveRecording::from_meta(&session.dir, &session.meta);
        *slot = Some(session);
        context.recording.update(|v| v.capturing = Some(active));
    }

    /// A stop finalizes its tracks after releasing the session lock, so the
    /// next recording can start meanwhile. The first stop's late clear must
    /// leave that recording shown as capturing, and dictation refused.
    #[tokio::test]
    async fn a_slow_stop_does_not_hide_the_next_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let (context, mut queue) = test_context(tmp.path(), working_backend());
        let (first, first_tx) = fed_session(tmp.path(), "first");
        let first_id = first.meta.id.clone();
        show_recording(&context, first).await;

        let stopping = tokio::spawn({
            let context = Arc::clone(&context);
            async move { stop(&context, None, EndReason::Stopped).await }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let taken = context.recording.session.lock().await.is_none();
            if taken {
                break;
            }
            assert!(Instant::now() < deadline, "the stop never took the session");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The next recording starts while the first one's track is finalizing.
        let (next, next_tx) = fed_session(tmp.path(), "next");
        let next_id = next.meta.id.clone();
        show_recording(&context, next).await;
        drop(first_tx);
        stopping.await.unwrap().unwrap();

        assert_eq!(queue.try_recv().unwrap().recording.id, first_id);
        assert!(
            context.recording.is_capturing(),
            "the next recording is hidden"
        );
        assert_eq!(
            context.recording.status().id.as_deref(),
            Some(next_id.as_str())
        );
        assert!(refuse_while_recording(&context, DictationStart::Dictation).is_some());

        // Its own stop still clears it.
        drop(next_tx);
        stop(&context, None, EndReason::Stopped).await.unwrap();
        assert!(!context.recording.is_capturing());
    }
}
