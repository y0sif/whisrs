//! The TUI's threads: the daemon poll, the restart after a save, the lists
//! a picker needs (sound devices, model files), the API key test, the model
//! download, the microphone capture and setup's system checks. Every request
//! to the daemon, the network or the sound server is made here, never on the
//! loop that draws and reads keys: a blocking connect to a daemon that
//! stopped accepting has no timeout, a sound server can be slow to answer,
//! and the screen must not freeze on either.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whisrs::config::setup::key_test::{self, Endpoints, Target};
use whisrs::config::setup::uinput::Uinput;
use whisrs::config::setup::unit::UnitFile;
use whisrs::config::setup::{gnome, keybind, mic, model, uinput, unit, Run};
use whisrs::Config;

use whisrs::agent::{AgentAction, AgentPhase};
use whisrs::recording::{RecordingAction, RecordingOptions, RecordingPhase};
use whisrs::service::ServiceManager;
use whisrs::{Command, InfoAnswer, RequestError, Response, RestartOutcome, State};

use whisrs::audio::device::InputDeviceInfo;

use super::app::{MicEvent, Msg, Origin};
use super::options::{self, Device, Listing};
use super::schema::ListKind;
use super::screens::mic::analysis;
use super::screens::setup::{Checks, SystemAction};
use super::Env;

/// How often the daemon is asked what it is doing.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// How long one answer may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

/// Every how many polls a daemon known to be older is asked `info` again:
/// one restarted outside the TUI (`whisrs restart`, a new install) may know
/// it now.
const REPROBE_EVERY: u32 = 10;

/// What the TUI knows about the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonView {
    /// Not asked yet.
    Checking,
    Up(InfoAnswer),
    /// Nothing answers at the socket.
    Down,
    /// Something answers, but not in time or not sensibly.
    NoAnswer(String),
}

/// What a restart did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartReport {
    Restarted,
    /// The daemon is busy, named by what it is doing; nothing was restarted.
    Busy(&'static str),
    /// The daemon did not answer, so it may be busy; nothing was restarted.
    NotAnswering,
    Failed(ServiceManager),
    /// No whisrs service is installed.
    NoService,
}

/// What the list workers read: the sound server's inputs (a function, so a
/// test stands in for it) and the models folder.
#[derive(Clone)]
pub struct Lister {
    pub devices: fn() -> Vec<InputDeviceInfo>,
    pub models: PathBuf,
    /// Shortens the models folder to `~` in what a failure says.
    pub home: Option<String>,
}

/// What the setup and Mic workers reach: the key test's endpoints, the
/// model server, and the microphone (functions, so a test stands in).
#[derive(Clone)]
pub struct Tools {
    pub endpoints: Endpoints,
    pub models_from: String,
    pub open_mic: OpenMic,
    /// Called holding [`stderr_lock`].
    pub probe: fn(&str) -> Result<mic::Probe, mic::ProbeError>,
    pub machine: Machine,
}

impl Tools {
    pub fn real() -> Tools {
        Tools {
            endpoints: Endpoints::default(),
            models_from: model::BASE_URL.to_string(),
            open_mic: open_live_mic,
            probe: mic::probe,
            machine: Machine::real(),
        }
    }
}

/// What setup's checks and steps ask of this machine, as functions, so a
/// test stands in for systemctl, `which` and `/dev/uinput`.
#[derive(Clone, Copy)]
pub struct Machine {
    pub manager: fn() -> ServiceManager,
    pub uinput: fn() -> Uinput,
    pub unit: fn(ServiceManager) -> UnitFile,
    pub unit_path: fn(ServiceManager) -> Option<PathBuf>,
    pub enabled: fn(ServiceManager) -> bool,
    pub active: fn(ServiceManager) -> bool,
    pub whisrsd: fn() -> String,
    pub whisrs: fn() -> String,
    /// Runs the programs the steps run (systemctl, gnome-extensions).
    pub run: fn(&str, &[&str]) -> Result<(), String>,
}

impl Machine {
    pub fn real() -> Machine {
        Machine {
            manager: ServiceManager::detect,
            uinput: uinput::check,
            unit: unit::status,
            unit_path: unit::path,
            enabled: ServiceManager::service_installed,
            active: ServiceManager::is_active,
            whisrsd: whisrs::config::setup::which_whisrsd,
            whisrs: whisrs::config::setup::which_whisrs,
            run: whisrs::config::setup::system,
        }
    }
}

/// The poll thread, and what starts a restart, a listing, a key test, a
/// download, a capture or a system step.
pub struct Workers {
    tx: Sender<Msg>,
    socket: PathBuf,
    service: ServiceManager,
    lister: Lister,
    /// The daemon hung up on `info`: it is older than it, so it is asked
    /// `status` instead of logging an error at every poll, and `info` again
    /// now and then ([`REPROBE_EVERY`]).
    older: Arc<AtomicBool>,
    env: Env,
    tools: Tools,
    /// The running downloads' cancel flags, by id.
    downloads: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// The running capture's stop flag.
    mic: Mutex<Option<Arc<AtomicBool>>>,
}

impl Workers {
    /// Start the poll: it answers at once, then every [`POLL_EVERY`] when
    /// the answer changed, and stops when the TUI is gone.
    pub fn start(
        tx: Sender<Msg>,
        socket: &Path,
        service: ServiceManager,
        lister: Lister,
        env: Env,
        tools: Tools,
    ) -> Workers {
        let workers = Workers {
            tx,
            socket: socket.to_path_buf(),
            service,
            lister,
            older: Arc::new(AtomicBool::new(false)),
            env,
            tools,
            downloads: Arc::default(),
            mic: Mutex::new(None),
        };
        let (tx, socket, older) = (
            workers.tx.clone(),
            workers.socket.clone(),
            Arc::clone(&workers.older),
        );
        std::thread::spawn(move || {
            let mut last = None;
            for polls in 0u32.. {
                let view = poll(&socket, &older, polls % REPROBE_EVERY == 0);
                if last.as_ref() != Some(&view) {
                    if tx.send(Msg::Daemon(view.clone())).is_err() {
                        return;
                    }
                    last = Some(view);
                }
                std::thread::sleep(POLL_EVERY);
            }
        });
        workers
    }

    /// Restart the daemon, on a thread of its own, if it is idle (or, with
    /// `anyway`, silent); the answer comes back as [`Msg::Restarted`].
    pub fn restart(&self, anyway: bool) {
        let (tx, socket, service, older) = (
            self.tx.clone(),
            self.socket.clone(),
            self.service,
            Arc::clone(&self.older),
        );
        std::thread::spawn(move || {
            let report = restart(&socket, service, &older, anyway, || {
                service.service_installed()
            });
            let _ = tx.send(Msg::Restarted(report));
        });
    }
}

impl Workers {
    /// Make the list `kind` on a thread of its own; it comes back as
    /// [`Msg::Listed`].
    pub fn list(&self, kind: ListKind) {
        let (tx, lister) = (self.tx.clone(), self.lister.clone());
        std::thread::spawn(move || {
            let (listing, panicked) = list_caught(kind, &lister);
            if panicked {
                let _ = tx.send(Msg::Reclaim);
            }
            let _ = tx.send(Msg::Listed(listing));
        });
    }
}

impl Workers {
    /// Test the key (or model, or server) of `target` in `config`, the key
    /// resolved as the daemon would; the answer comes back as
    /// [`Msg::KeyTested`].
    pub fn test_key(&self, origin: Origin, path: String, config: Config, target: Target) {
        let (tx, env, endpoints) = (
            self.tx.clone(),
            self.env.clone(),
            self.tools.endpoints.clone(),
        );
        std::thread::spawn(move || {
            let tested = key_test::check(
                &config,
                &target,
                |var| env.get(var).map(str::to_string),
                &endpoints,
            );
            let _ = tx.send(Msg::KeyTested {
                origin,
                path,
                tested,
            });
        });
    }

    /// Download whisper model `name` into `dir` as download `id` (its
    /// `.part` is `model::part_path`); its progress and end come back as
    /// [`Msg::Download`], tagged `id`.
    pub fn download(&self, name: String, dir: PathBuf, id: u64) {
        let cancel = Arc::new(AtomicBool::new(false));
        lock(&self.downloads).insert(id, Arc::clone(&cancel));
        let (tx, from) = (self.tx.clone(), self.tools.models_from.clone());
        let downloads = Arc::clone(&self.downloads);
        std::thread::spawn(move || {
            let mut said = None::<Instant>;
            let result = model::download_from(
                &from,
                &name,
                &dir,
                id,
                |done, total| {
                    // About ten a second is enough to watch.
                    if said.is_none_or(|at| at.elapsed() >= Duration::from_millis(100)) {
                        said = Some(Instant::now());
                        let _ = tx.send(Msg::Download(Download::Progress { id, done, total }));
                    }
                },
                &cancel,
            );
            lock(&downloads).remove(&id);
            let _ = tx.send(Msg::Download(Download::Done { id, result }));
        });
    }

    /// Stop download `id`: it ends, its `.part` removed, at its next read.
    pub fn cancel_download(&self, id: u64) {
        if let Some(cancel) = lock(&self.downloads).remove(&id) {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Stop every download: the TUI is going.
    pub fn cancel_downloads(&self) {
        for (_, cancel) in lock(&self.downloads).drain() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Probe `device`, then capture from it and send about 30 readings a
    /// second, tagged `session`, until [`Workers::mic_stop`] or the next
    /// start. Stops on its own once the TUI is gone.
    pub fn mic_start(&self, session: u64, device: String) {
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(old) = lock(&self.mic).replace(Arc::clone(&stop)) {
            old.store(true, Ordering::Relaxed);
        }
        let (tx, tools) = (self.tx.clone(), self.tools.clone());
        std::thread::spawn(move || {
            let send = |event: MicEvent| tx.send(Msg::Mic { session, event }).is_ok();
            // A capture started since makes this one moot. Checked before
            // the lock and again once it is had: quick h/l presses would
            // otherwise queue a probe each behind it.
            let stopped = || stop.load(Ordering::Relaxed);
            if stopped() {
                return;
            }
            let probed = {
                let _one = stderr_lock();
                if stopped() {
                    return;
                }
                (tools.probe)(&device)
            };
            if !send(MicEvent::Probed(probed)) || stopped() {
                return;
            }
            match (tools.open_mic)(&device) {
                Ok(mut chunks) => meter_loop(chunks.as_mut(), &stop, send, std::thread::sleep),
                Err(why) => {
                    send(MicEvent::Failed(why));
                }
            }
        });
    }

    /// Stop the running capture.
    pub fn mic_stop(&self) {
        if let Some(stop) = lock(&self.mic).take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// Look at the machine for setup's last step; the answer comes back as
    /// [`Msg::Checked`].
    pub fn checks(&self, dirs: SetupDirs, overlay: bool) {
        let (tx, env, machine) = (self.tx.clone(), self.env.clone(), self.tools.machine);
        std::thread::spawn(move || {
            let checks = look(&dirs, overlay, &env, &machine);
            let _ = tx.send(Msg::Checked(Box::new(checks)));
        });
    }

    /// Do one of setup's system steps; the answer comes back as
    /// [`Msg::SystemDone`].
    pub fn system(&self, action: SystemAction, dirs: SetupDirs, checks: Checks) {
        let (tx, run) = (self.tx.clone(), self.tools.machine.run);
        std::thread::spawn(move || {
            let done = act(action, &dirs, &checks, &run);
            let _ = tx.send(Msg::SystemDone(action, done));
        });
    }
}

/// A download's news.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Download {
    Progress {
        id: u64,
        done: u64,
        total: Option<u64>,
    },
    Done {
        id: u64,
        result: Result<PathBuf, model::DownloadError>,
    },
}

impl Download {
    /// The download this is news of.
    pub fn id(&self) -> u64 {
        match self {
            Download::Progress { id, .. } | Download::Done { id, .. } => *id,
        }
    }
}

/// The folders setup's checks look in: the compositor configs, the GNOME
/// extensions.
#[derive(Debug, Clone)]
pub struct SetupDirs {
    pub config: PathBuf,
    pub data: PathBuf,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What setup's last step shows, looked up now on `machine`.
fn look(dirs: &SetupDirs, overlay: bool, env: &Env, machine: &Machine) -> Checks {
    let manager = (machine.manager)();
    let compositor = keybind::detect_from(|key| env.get(key).map(str::to_string));
    Checks {
        uinput: (machine.uinput)(),
        manager,
        unit: (machine.unit)(manager),
        unit_path: (machine.unit_path)(manager),
        enabled: (machine.enabled)(manager),
        active: (machine.active)(manager),
        whisrsd: (machine.whisrsd)(),
        whisrs: (machine.whisrs)(),
        binding: compositor
            .as_ref()
            .and_then(|c| keybind::status(c, &dirs.config)),
        gnome: (compositor == Some(keybind::Compositor::Gnome) && overlay)
            .then(|| gnome::installed(&dirs.data)),
        compositor,
    }
}

/// Do `action` on this machine, its programs run by `run`; what to say, or
/// why it failed.
fn act(
    action: SystemAction,
    dirs: &SetupDirs,
    checks: &Checks,
    run: Run,
) -> Result<Vec<String>, String> {
    let manager = checks.manager;
    let dest = || {
        checks
            .unit_path
            .clone()
            .ok_or_else(|| "no folder to install the service into".to_string())
    };
    match action {
        SystemAction::InstallService => {
            let dest = dest()?;
            let mut notes = unit::write(
                manager,
                &dest,
                &checks.whisrsd,
                whisrs::config::setup::find_contrib_file,
                Some(&dirs.config.join("rc/conf.d")),
            )
            .map_err(|e| format!("{e:#}"))?;
            notes.insert(0, format!("installed the service at {}", dest.display()));
            unit::enable(manager, run).map_err(|e| format!("{e:#}"))?;
            notes.push("enabled and started it".to_string());
            Ok(notes)
        }
        SystemAction::EnableService => unit::enable(manager, run)
            .map(|()| vec!["enabled and started the service".to_string()])
            .map_err(|e| format!("{e:#}")),
        SystemAction::Repoint => {
            let dest = dest()?;
            unit::repoint(&dest, &checks.whisrsd, run).map_err(|e| format!("{e:#}"))?;
            Ok(vec![format!("repointed the service at {}", checks.whisrsd)])
        }
        SystemAction::Bind => {
            let compositor = checks
                .compositor
                .clone()
                .ok_or_else(|| "no compositor found".to_string())?;
            let path = keybind::config_file(&compositor, &dirs.config)
                .ok_or_else(|| format!("whisrs does not write {}'s config", compositor.name()))?;
            keybind::append(&compositor, &path, &checks.whisrs).map_err(|e| format!("{e:#}"))?;
            let reload = match compositor {
                keybind::Compositor::Hyprland => {
                    "Hyprland does not reload by itself: run hyprctl reload to use it."
                }
                _ => "Reload Sway (Mod+Shift+C) to use it.",
            };
            Ok(vec![format!(
                "added {} to {}. {reload}",
                keybind::keys(&compositor),
                path.display()
            )])
        }
        SystemAction::InstallGnome => {
            let installed = gnome::install(gnome::source().as_deref(), &dirs.data, run)
                .map_err(|e| format!("{e:#}"))?;
            Ok(vec![match installed.enabled {
                Ok(()) => format!(
                    "installed and enabled the extension at {}; log out and back in if it does \
                     not show",
                    installed.path.display()
                ),
                Err(why) => format!(
                    "installed the extension at {}, but enabling it failed ({why}): run \
                     gnome-extensions enable {}",
                    installed.path.display(),
                    gnome::UUID
                ),
            }])
        }
    }
}

/// A microphone, as the meter reads it: one poll at a time.
pub trait Chunks: Send {
    fn poll(&mut self) -> Polled;
}

/// What one poll of a microphone gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Polled {
    Chunk(Vec<i16>),
    /// Nothing yet.
    Waiting,
    /// The capture stopped.
    Ended,
}

/// Open a microphone by its `[audio] device` name.
pub type OpenMic = fn(&str) -> Result<Box<dyn Chunks>, String>;

/// The real microphone, through the daemon's own capture.
struct LiveMic {
    _handle: whisrs::audio::capture::AudioCaptureHandle,
    rx: tokio::sync::mpsc::UnboundedReceiver<whisrs::audio::AudioChunk>,
}

impl Chunks for LiveMic {
    fn poll(&mut self) -> Polled {
        use tokio::sync::mpsc::error::TryRecvError;
        match self.rx.try_recv() {
            Ok(chunk) => Polled::Chunk(chunk),
            Err(TryRecvError::Empty) => Polled::Waiting,
            Err(TryRecvError::Disconnected) => Polled::Ended,
        }
    }
}

fn open_live_mic(device: &str) -> Result<Box<dyn Chunks>, String> {
    // ALSA prints to stderr while a device opens.
    let mut handle = quiet_stderr(|| {
        whisrs::audio::capture::AudioCaptureHandle::start_with_level_tx(device, None)
    })
    .map_err(|e| format!("{e:#}"))?;
    let rx = handle
        .take_receiver()
        .ok_or_else(|| "the capture gave no audio channel".to_string())?;
    Ok(Box::new(LiveMic {
        _handle: handle,
        rx,
    }))
}

/// Samples a reading is made of, at least: 33 ms at 16 kHz, for about 30
/// readings a second.
const WINDOW: usize = 533;

/// How long the meter waits when a poll had nothing.
const NAP: Duration = Duration::from_millis(5);

/// Read `chunks` until `stop`, sending a reading for every [`WINDOW`] of
/// audio (the latest audio at once when more came); a `send` that fails
/// (the TUI is gone) ends it too.
pub fn meter_loop(
    chunks: &mut dyn Chunks,
    stop: &AtomicBool,
    mut send: impl FnMut(MicEvent) -> bool,
    sleep: impl Fn(Duration),
) {
    let mut pending: Vec<i16> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match chunks.poll() {
            Polled::Chunk(chunk) => {
                pending.extend_from_slice(&chunk);
                continue;
            }
            Polled::Ended => {
                send(MicEvent::Failed("the microphone stopped".to_string()));
                return;
            }
            Polled::Waiting => {}
        }
        if pending.len() >= WINDOW {
            let reading = analysis::analyze(&pending, whisrs::audio::capture::SAMPLE_RATE);
            pending.clear();
            if !send(MicEvent::Reading(reading, Instant::now())) {
                return;
            }
        }
        sleep(NAP);
    }
}

/// [`list`], with a panic in it caught: it becomes the list's failure, so
/// the picker says so and the list can be asked for again. `true` when it
/// panicked: the panic hook gave the terminal back on the way, and the TUI
/// takes it again.
fn list_caught(kind: ListKind, lister: &Lister) -> (Listing, bool) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| list(kind, lister))) {
        Ok(listing) => (listing, false),
        Err(_) => {
            let failed = |what: &str| format!("listing the {what} failed");
            let listing = match kind {
                ListKind::Devices => Listing::Devices(Err(failed("input devices"))),
                ListKind::Models => Listing::Models(Err(failed("model files"))),
            };
            (listing, true)
        }
    }
}

/// The list `kind`, as `lister` finds it.
pub fn list(kind: ListKind, lister: &Lister) -> Listing {
    match kind {
        ListKind::Devices => {
            let found: Vec<Device> = (lister.devices)()
                .into_iter()
                .map(|info| Device {
                    monitor: info.is_monitor(),
                    id: info.id,
                    description: info.description,
                })
                .collect();
            Listing::Devices(if found.is_empty() {
                Err("no input devices found: is the sound server running?".to_string())
            } else {
                Ok(found)
            })
        }
        ListKind::Models => {
            Listing::Models(options::model_files(&lister.models, lister.home.as_deref()))
        }
    }
}

/// The sound server's inputs. ALSA and PulseAudio print to stderr while
/// they are listed, which is on `/dev/null` for the TUI's run ([`hush`]).
pub fn list_devices_quietly() -> Vec<InputDeviceInfo> {
    quiet_stderr(whisrs::audio::device::list_input_devices)
}

/// Held while something may print to stderr (a listing, a device opening),
/// and while the real stderr is handed to a child (`$EDITOR`, sudo), so the
/// two never meet: the child waits for the listing, or the listing for the
/// child. Never held while waiting for another lock, so it cannot deadlock.
pub fn stderr_lock() -> std::sync::MutexGuard<'static, ()> {
    static QUIET: std::sync::Mutex<()> = std::sync::Mutex::new(());
    QUIET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` holding [`stderr_lock`]: whatever it prints goes to `/dev/null`
/// for the run, never into a suspend's real stderr.
fn quiet_stderr<T>(f: impl FnOnce() -> T) -> T {
    let _one = stderr_lock();
    f()
}

/// The real stderr, kept while file descriptor 2 is on `/dev/null`; -1 when
/// it is not ([`hush`] has not run, or [`Hushed`] was dropped).
static REAL_STDERR: AtomicI32 = AtomicI32::new(-1);

/// Puts the real stderr back on 2 for good when dropped.
#[must_use]
pub struct Hushed(());

impl Drop for Hushed {
    fn drop(&mut self) {
        let saved = REAL_STDERR.swap(-1, Ordering::SeqCst);
        if saved >= 0 {
            // SAFETY: `saved` is the descriptor `hush` made and owns.
            unsafe {
                libc::dup2(saved, 2);
                libc::close(saved);
            }
        }
    }
}

/// Point file descriptor 2 at `/dev/null` for the TUI's run: ALSA, PipeWire
/// and anything else that prints to stderr would land on the screen. The
/// real one is kept (close-on-exec, so no child inherits it) and comes back
/// when the guard drops, for a suspend ([`speaking`]) and in a panic
/// ([`unhush`]).
pub fn hush() -> Hushed {
    if REAL_STDERR.load(Ordering::SeqCst) < 0 {
        // SAFETY: plain descriptor calls on 2, which always exists.
        let saved = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 0) };
        if saved >= 0 {
            REAL_STDERR.store(saved, Ordering::SeqCst);
            rehush();
        }
    }
    Hushed(())
}

/// The real stderr back on 2, the run still on: for the panic hook, and a
/// suspend. Takes no lock, so a panic while one is held cannot deadlock.
pub fn unhush() {
    let saved = REAL_STDERR.load(Ordering::SeqCst);
    if saved >= 0 {
        // SAFETY: `saved` is open while the run lasts.
        unsafe { libc::dup2(saved, 2) };
    }
}

/// `/dev/null` on 2 again, while the run lasts.
pub fn rehush() {
    use std::os::fd::AsRawFd;
    if REAL_STDERR.load(Ordering::SeqCst) < 0 {
        return;
    }
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: `null` outlives the call; dup2 makes 2 a copy of it.
        unsafe { libc::dup2(null.as_raw_fd(), 2) };
    }
}

/// The real stderr on 2 until dropped, for `$EDITOR` and sudo. Taken under
/// [`stderr_lock`], so nothing hushed prints into it meanwhile.
#[must_use]
pub struct Speaking(());

impl Drop for Speaking {
    fn drop(&mut self) {
        rehush();
    }
}

pub fn speaking() -> Speaking {
    unhush();
    Speaking(())
}

/// Ask the daemon once: `info`, or `status` for a daemon known to be older
/// unless `reprobe`. `older` follows the answer.
fn poll(socket: &Path, older: &AtomicBool, reprobe: bool) -> DaemonView {
    let answer = if older.load(Ordering::Relaxed) && !reprobe {
        whisrs::request_status_blocking_at(socket, Some(ANSWER_TIMEOUT)).map(InfoAnswer::Older)
    } else {
        whisrs::request_info_blocking_at(socket, Some(ANSWER_TIMEOUT))
    };
    match answer {
        Ok(answer) => {
            older.store(matches!(answer, InfoAnswer::Older(_)), Ordering::Relaxed);
            DaemonView::Up(answer)
        }
        Err(RequestError::NotRunning(_)) => {
            // Whatever starts next may be newer.
            older.store(false, Ordering::Relaxed);
            DaemonView::Down
        }
        Err(e) => DaemonView::NoAnswer(e.to_string()),
    }
}

/// What a busy daemon is doing, or `None` when it is idle and a restart
/// loses nothing. An older daemon's recording is not in its answer: see
/// [`gate`].
pub fn busy(answer: &InfoAnswer) -> Option<&'static str> {
    let dictation = match answer.state() {
        Some(State::Idle) => None,
        Some(State::Recording) => Some("dictating"),
        Some(State::Transcribing) => Some("transcribing"),
        Some(State::Synthesizing | State::Speaking) => Some("speaking"),
        None => Some("busy"),
    };
    let InfoAnswer::Info(info) = answer else {
        return dictation;
    };
    let recording = match info.recording {
        Some(RecordingPhase::Idle) => None,
        Some(RecordingPhase::Recording) => Some("recording"),
        Some(RecordingPhase::Processing) => Some("processing a recording"),
        None => Some("busy"),
    };
    recording
        .or(dictation)
        .or(info.agent_turn.then_some("running an agent turn"))
        .or(info.busy.then_some("busy"))
}

/// Whether a restart now loses work, asked fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gate {
    /// Idle, or not running: nothing to lose.
    Clear,
    Busy(&'static str),
    /// No answer in time: a daemon opening the mic holds the lock an older
    /// one's `status` waits on, so silence can be a dictation starting.
    NotAnswering,
}

/// Ask the daemon what a restart would cost: `info` first, every time, so a
/// daemon restarted into a newer one is known; for an older one, `status`,
/// then `recording status` and `agent list`.
fn gate(socket: &Path, older: &AtomicBool) -> Gate {
    let answer = match whisrs::request_info_blocking_at(socket, Some(ANSWER_TIMEOUT)) {
        Ok(answer) => answer,
        Err(RequestError::NotRunning(_)) => return Gate::Clear,
        Err(_) => return Gate::NotAnswering,
    };
    older.store(matches!(answer, InfoAnswer::Older(_)), Ordering::Relaxed);
    if let Some(doing) = busy(&answer) {
        return Gate::Busy(doing);
    }
    if matches!(answer, InfoAnswer::Info(_)) {
        return Gate::Clear;
    }
    match older_recording(socket) {
        Some(RecordingPhase::Idle) => {}
        Some(RecordingPhase::Recording) => return Gate::Busy("recording"),
        Some(RecordingPhase::Processing) => return Gate::Busy("processing a recording"),
        None => return Gate::NotAnswering,
    }
    match older_agents_busy(socket) {
        Some(false) => Gate::Clear,
        Some(true) => Gate::Busy("running an agent turn"),
        None => Gate::NotAnswering,
    }
}

/// The recording phase of a daemon older than `info`, from `recording
/// status` (v0.1.30 and later); `None` when it does not answer in time. One
/// that hangs up on it is older than recordings, and one that refuses it
/// was built without them: neither records.
fn older_recording(socket: &Path) -> Option<RecordingPhase> {
    let status = Command::Recording {
        action: RecordingAction::Status,
        title: None,
        id: None,
        options: RecordingOptions::default(),
    };
    match whisrs::request_blocking_at(socket, &status, Some(ANSWER_TIMEOUT)) {
        Ok(Response::Recording { recording }) => Some(recording.phase),
        Ok(Response::Error { .. })
        | Err(RequestError::Closed)
        | Err(RequestError::NotRunning(_)) => Some(RecordingPhase::Idle),
        Ok(_) | Err(_) => None,
    }
}

/// Whether an agent of a daemon older than `info` is not idle, from `agent
/// list` (v0.1.31 answers it with no agents configured too); `None` when it
/// does not answer in time. One that hangs up on it, or refuses it, has no
/// agents.
fn older_agents_busy(socket: &Path) -> Option<bool> {
    let list = Command::Agent {
        action: AgentAction::List,
        name: None,
        text: None,
        new: false,
    };
    match whisrs::request_blocking_at(socket, &list, Some(ANSWER_TIMEOUT)) {
        Ok(Response::Agents { agents }) => {
            Some(agents.iter().any(|agent| agent.phase != AgentPhase::Idle))
        }
        Ok(Response::Error { .. })
        | Err(RequestError::Closed)
        | Err(RequestError::NotRunning(_)) => Some(false),
        Ok(_) | Err(_) => None,
    }
}

/// Restart through the service manager, as `whisrs config` always has, but
/// only when the daemon is idle: a restart would lose the dictation,
/// recording or agent turn it is on. `anyway` (asked with `r`, then `y`)
/// overrides silence alone; the daemon is asked again, and a busy one is
/// still left alone. `installed` says whether a service is there to restart
/// through: without one, silence gets the hint to restart by hand, not the
/// question.
fn restart(
    socket: &Path,
    service: ServiceManager,
    older: &AtomicBool,
    anyway: bool,
    installed: impl Fn() -> bool,
) -> RestartReport {
    match gate(socket, older) {
        Gate::Clear => {}
        Gate::NotAnswering if anyway => {}
        Gate::NotAnswering if !installed() => return RestartReport::NoService,
        Gate::NotAnswering => return RestartReport::NotAnswering,
        Gate::Busy(doing) => return RestartReport::Busy(doing),
    }
    match service.restart() {
        RestartOutcome::Restarted => {
            older.store(false, Ordering::Relaxed);
            RestartReport::Restarted
        }
        RestartOutcome::Failed => RestartReport::Failed(service),
        RestartOutcome::NoService => RestartReport::NoService,
    }
}

#[cfg(test)]
mod list_tests {
    use whisrs::audio::device::DeviceHost;

    use super::*;

    fn two() -> Vec<InputDeviceInfo> {
        vec![
            InputDeviceInfo {
                host: DeviceHost::PulseAudio,
                id: "alsa_input.usb-yeti".into(),
                description: "Blue Yeti".into(),
            },
            InputDeviceInfo {
                host: DeviceHost::PulseAudio,
                id: "alsa_output.speaker.monitor".into(),
                description: "Monitor of Speaker".into(),
            },
        ]
    }

    fn none() -> Vec<InputDeviceInfo> {
        Vec::new()
    }

    /// The devices come from the lister given, never the sound server, and
    /// monitors are told apart.
    #[test]
    fn devices_come_from_the_lister() {
        let dir = tempfile::tempdir().unwrap();
        let lister = Lister {
            devices: two,
            models: dir.path().to_path_buf(),
            home: None,
        };
        let Listing::Devices(Ok(found)) = list(ListKind::Devices, &lister) else {
            panic!("no devices");
        };
        assert_eq!(found.len(), 2);
        assert!(!found[0].monitor && found[1].monitor);
        assert_eq!(found[0].description, "Blue Yeti");

        let empty = Lister {
            devices: none,
            ..lister
        };
        match list(ListKind::Devices, &empty) {
            Listing::Devices(Err(why)) => assert!(why.contains("sound server"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn models_are_read_from_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-tiny.bin"), b"x").unwrap();
        let lister = Lister {
            devices: none,
            models: dir.path().to_path_buf(),
            home: None,
        };
        let Listing::Models(Ok(files)) = list(ListKind::Models, &lister) else {
            panic!("no models");
        };
        assert_eq!(files.len(), 1);
    }

    /// A list on its thread answers through the channel.
    #[test]
    fn a_listing_comes_back_as_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let workers = Workers {
            tx,
            socket: dir.path().join("whisrs.sock"),
            service: ServiceManager::None,
            lister: Lister {
                devices: two,
                models: dir.path().to_path_buf(),
                home: None,
            },
            older: Arc::new(AtomicBool::new(false)),
            env: Env::default(),
            tools: Tools::real(),
            downloads: Arc::default(),
            mic: Mutex::new(None),
        };
        workers.list(ListKind::Devices);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Listed(Listing::Devices(Ok(found))) => assert_eq!(found.len(), 2),
            _ => panic!("not a device list"),
        }
    }

    /// The device and inode file descriptor 2 is on.
    fn stderr_file() -> (u64, u64) {
        // SAFETY: fstat into a zeroed struct of the right type.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(2, &mut stat) }, 0);
        (stat.st_dev, stat.st_ino)
    }

    /// Held by every test that looks at or moves file descriptor 2.
    static FD2: Mutex<()> = Mutex::new(());

    /// The device and inode of `/dev/null`.
    fn null_file() -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata("/dev/null").unwrap();
        (meta.dev(), meta.ino())
    }

    /// For the run, stderr is `/dev/null`; a suspend has the real one, as
    /// does a panic hook until the terminal is reclaimed; dropping the
    /// guard, or unwinding through it in a panic, gives it back for good.
    #[test]
    fn stderr_is_hushed_for_the_run_and_given_back() {
        let _fd2 = lock(&FD2);
        let before = stderr_file();
        let hushed = hush();
        assert_eq!(stderr_file(), null_file());
        {
            let _real = speaking();
            assert_eq!(stderr_file(), before, "a suspend has the real stderr");
        }
        assert_eq!(stderr_file(), null_file());
        unhush();
        assert_eq!(stderr_file(), before, "the panic hook gives it back");
        rehush();
        assert_eq!(stderr_file(), null_file());
        drop(hushed);
        assert_eq!(stderr_file(), before, "back after the run");
        assert_eq!(REAL_STDERR.load(Ordering::SeqCst), -1);
        // Out of a run, they change nothing.
        rehush();
        assert_eq!(stderr_file(), before);

        let caught = std::panic::catch_unwind(|| {
            let _hushed = hush();
            panic!("in the run");
        });
        assert!(caught.is_err());
        assert_eq!(stderr_file(), before, "back after a panic in the run");
        assert_eq!(REAL_STDERR.load(Ordering::SeqCst), -1);
    }

    /// Stderr is back after a quiet listing, a panicking one too, and the
    /// saved copy is not left open.
    #[test]
    fn stderr_comes_back_after_a_quiet_listing() {
        let _fd2 = lock(&FD2);
        let before = stderr_file();
        assert_eq!(quiet_stderr(|| 7), 7);
        assert_eq!(stderr_file(), before);
        let caught = std::panic::catch_unwind(|| quiet_stderr(|| panic!("in the listing")));
        assert!(caught.is_err());
        assert_eq!(stderr_file(), before);
        // Not poisoned for the next one.
        assert_eq!(quiet_stderr(|| 8), 8);
    }

    fn boom() -> Vec<InputDeviceInfo> {
        panic!("the sound server library panicked")
    }

    /// A microphone that gives what a test says.
    struct Scripted(std::collections::VecDeque<Polled>);

    impl Chunks for Scripted {
        fn poll(&mut self) -> Polled {
            self.0.pop_front().unwrap_or(Polled::Waiting)
        }
    }

    /// A reading comes for each window of audio, the latest at once, and
    /// a capture that stops says so.
    #[test]
    fn the_meter_reads_windows_until_stopped() {
        let stop = AtomicBool::new(false);
        let mut mic = Scripted(
            [
                Polled::Chunk(vec![3277; 300]),
                Polled::Waiting,
                Polled::Chunk(vec![3277; 300]),
                Polled::Chunk(vec![0; 2000]),
                Polled::Waiting,
                Polled::Ended,
            ]
            .into(),
        );
        let mut events = Vec::new();
        meter_loop(
            &mut mic,
            &stop,
            |event| {
                events.push(event);
                true
            },
            |_| {},
        );
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            MicEvent::Reading(reading, _) => {
                assert!(reading.rms_dbfs < -20.0 && reading.rms_dbfs > -40.0)
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(events[1], MicEvent::Failed("the microphone stopped".into()));

        // Stopped, it reads nothing more.
        stop.store(true, Ordering::Relaxed);
        let mut mic = Scripted([Polled::Chunk(vec![1; 5000])].into());
        let mut any = false;
        meter_loop(
            &mut mic,
            &stop,
            |_| {
                any = true;
                true
            },
            |_| {},
        );
        assert!(!any);
    }

    /// A capture whose device does not open says why, after the probe.
    #[test]
    fn a_mic_that_does_not_open_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let workers = Workers {
            tx,
            socket: dir.path().join("whisrs.sock"),
            service: ServiceManager::None,
            lister: Lister {
                devices: two,
                models: dir.path().to_path_buf(),
                home: None,
            },
            older: Arc::new(AtomicBool::new(false)),
            env: Env::default(),
            tools: Tools {
                open_mic: |_| Err("no such device".to_string()),
                probe: |_| {
                    Err(mic::ProbeError {
                        error: "no such device".into(),
                        devices: Vec::new(),
                    })
                },
                ..Tools::real()
            },
            downloads: Arc::default(),
            mic: Mutex::new(None),
        };
        workers.mic_start(7, "nope".into());
        let next = || match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Mic { session, event } => (session, event),
            _ => panic!("not a mic event"),
        };
        assert!(matches!(next(), (7, MicEvent::Probed(Err(_)))));
        assert_eq!(next(), (7, MicEvent::Failed("no such device".into())));
    }

    /// What the probes below were asked to look at.
    static PROBED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn counting_probe(device: &str) -> Result<mic::Probe, mic::ProbeError> {
        lock(&PROBED).push(device.to_string());
        Err(mic::ProbeError {
            error: "not here".into(),
            devices: Vec::new(),
        })
    }

    /// A capture superseded before its probe got the stderr lock never
    /// probes: quick h/l presses do not queue a probe each.
    #[test]
    fn a_superseded_probe_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let workers = Workers {
            tx,
            socket: dir.path().join("whisrs.sock"),
            service: ServiceManager::None,
            lister: Lister {
                devices: two,
                models: dir.path().to_path_buf(),
                home: None,
            },
            older: Arc::new(AtomicBool::new(false)),
            env: Env::default(),
            tools: Tools {
                open_mic: |_| Err("no such device".to_string()),
                probe: counting_probe,
                ..Tools::real()
            },
            downloads: Arc::default(),
            mic: Mutex::new(None),
        };
        {
            // Another probe, or a listing, holds the lock meanwhile.
            let _held = stderr_lock();
            workers.mic_start(1, "first".into());
            workers.mic_start(2, "second".into());
            std::thread::sleep(Duration::from_millis(100));
        }
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Mic {
                    session: 2,
                    event: MicEvent::Probed(_),
                } => break,
                Msg::Mic { session: 1, .. } => panic!("the superseded capture answered"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx
            .try_iter()
            .all(|msg| !matches!(msg, Msg::Mic { session: 1, .. })));
        assert_eq!(*lock(&PROBED), ["second"]);
    }

    fn fake_machine() -> Machine {
        Machine {
            manager: || ServiceManager::Systemd,
            uinput: || Uinput::Denied,
            unit: |_| UnitFile::Missing,
            unit_path: |_| Some(PathBuf::from("/nowhere/whisrs.service")),
            enabled: |_| false,
            active: |_| true,
            whisrsd: || "/opt/bin/whisrsd".into(),
            whisrs: || "/opt/bin/whisrs".into(),
            run: |_, _| Err("no programs in tests".to_string()),
        }
    }

    /// The checks come from the machine given: the service, uinput, the
    /// binaries, the keybinding in the config folder, the GNOME extension
    /// only on GNOME with the overlay on.
    #[test]
    fn look_asks_the_machine_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = SetupDirs {
            config: dir.path().join("cfg"),
            data: dir.path().join("data"),
        };
        let conf = dirs.config.join("hypr/hyprland.conf");
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "$mainMod = SUPER\n").unwrap();
        let hypr = Env::from_pairs(&[("HYPRLAND_INSTANCE_SIGNATURE", "x")]);
        let checks = look(&dirs, true, &hypr, &fake_machine());
        assert_eq!(checks.uinput, Uinput::Denied);
        assert_eq!(checks.manager, ServiceManager::Systemd);
        assert_eq!(checks.unit, UnitFile::Missing);
        assert_eq!(
            checks.unit_path.as_deref(),
            Some(Path::new("/nowhere/whisrs.service"))
        );
        assert!(!checks.enabled && checks.active);
        assert_eq!(checks.whisrsd, "/opt/bin/whisrsd");
        assert_eq!(checks.compositor, Some(keybind::Compositor::Hyprland));
        assert_eq!(checks.binding, Some(keybind::Binding::Unbound(conf)));
        assert_eq!(checks.gnome, None);

        let gnome = Env::from_pairs(&[("XDG_CURRENT_DESKTOP", "GNOME")]);
        assert_eq!(
            look(&dirs, true, &gnome, &fake_machine()).gnome,
            Some(false)
        );
        assert_eq!(look(&dirs, false, &gnome, &fake_machine()).gnome, None);
    }

    /// Each system step runs with the runner given: the service written
    /// and enabled, repointed, a keybinding appended once, the GNOME
    /// extension copied and enabled; a failure says how to do it by hand.
    #[test]
    fn act_runs_each_step_with_the_runner_given() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = SetupDirs {
            config: dir.path().join("cfg"),
            data: dir.path().join("data"),
        };
        let unit = dir.path().join("systemd/user/whisrs.service");
        let seen = std::cell::RefCell::new(Vec::new());
        let run = |program: &str, args: &[&str]| -> Result<(), String> {
            seen.borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(())
        };
        let mut checks = look(&dirs, true, &Env::default(), &fake_machine());
        checks.unit_path = Some(unit.clone());
        checks.compositor = Some(keybind::Compositor::Hyprland);

        let notes = act(SystemAction::InstallService, &dirs, &checks, &run).unwrap();
        assert!(
            notes[0].starts_with("installed the service at"),
            "{notes:?}"
        );
        assert!(std::fs::read_to_string(&unit)
            .unwrap()
            .contains("ExecStart=/opt/bin/whisrsd"));
        assert_eq!(
            seen.take(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now whisrs.service"
            ]
        );

        std::fs::write(&unit, "[Service]\nExecStart=/usr/bin/whisrsd\n").unwrap();
        act(SystemAction::Repoint, &dirs, &checks, &run).unwrap();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            "[Service]\nExecStart=/opt/bin/whisrsd\n"
        );
        assert_eq!(seen.take(), ["systemctl --user daemon-reload"]);

        let failing = |_: &str, _: &[&str]| -> Result<(), String> { Err("no".into()) };
        let why = act(SystemAction::EnableService, &dirs, &checks, &failing).unwrap_err();
        assert!(
            why.contains("systemctl --user enable --now whisrs.service"),
            "{why}"
        );

        let conf = dirs.config.join("hypr/hyprland.conf");
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "").unwrap();
        let notes = act(SystemAction::Bind, &dirs, &checks, &run).unwrap();
        assert!(notes[0].contains("run hyprctl reload"), "{notes:?}");
        act(SystemAction::Bind, &dirs, &checks, &run).unwrap();
        let bound = std::fs::read_to_string(&conf).unwrap();
        assert_eq!(bound.matches("whisrs toggle").count(), 1, "appended once");

        let notes = act(SystemAction::InstallGnome, &dirs, &checks, &run).unwrap();
        assert!(notes[0].starts_with("installed and enabled"), "{notes:?}");
        assert!(gnome::installed(&dirs.data));
        assert_eq!(
            seen.take(),
            [format!("gnome-extensions enable {}", gnome::UUID)]
        );

        checks.unit_path = None;
        assert!(act(SystemAction::Repoint, &dirs, &checks, &run).is_err());
    }

    /// A panic while listing is the list's failure, not a picker looking
    /// forever, and says the terminal must be taken back.
    #[test]
    fn a_panicking_listing_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let lister = Lister {
            devices: boom,
            models: dir.path().to_path_buf(),
            home: None,
        };
        let (listing, panicked) = list_caught(ListKind::Devices, &lister);
        assert!(panicked);
        assert_eq!(
            listing,
            Listing::Devices(Err("listing the input devices failed".into()))
        );
        let ok = Lister {
            devices: two,
            ..lister
        };
        assert!(!list_caught(ListKind::Devices, &ok).1);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    use whisrs::recording::RecordingStatus;
    use whisrs::DaemonInfo;

    use super::*;

    /// How a fake daemon meets one connection.
    enum Meet {
        Answer(Response),
        /// Close without answering: a daemon that cannot parse the command.
        HangUp,
        /// Keep the connection, unanswered, past the client's timeout.
        Silent,
    }

    use Meet::{Answer, HangUp, Silent};

    /// What a fake daemon was asked, once it met every connection it
    /// expected. A client that never asks fails the test, not hangs it.
    struct Asked(std::sync::mpsc::Receiver<Vec<String>>);

    impl Asked {
        fn join(self) -> Vec<String> {
            self.0
                .recv_timeout(Duration::from_secs(10))
                .expect("the daemon was not asked all it was meant to be")
        }
    }

    /// A daemon on a temp socket meeting each connection as the next of
    /// `meets`, which tells each command it read.
    fn fake_daemon(meets: Vec<Meet>) -> (tempfile::TempDir, PathBuf, Asked) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for meet in meets {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
                let cmd: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let name = cmd["cmd"].as_str().unwrap_or_default();
                let action = cmd["action"].as_str().map(|a| format!(" {a}"));
                seen.push(format!("{name}{}", action.unwrap_or_default()));
                match meet {
                    Answer(answer) => {
                        let frame = whisrs::encode_message(&answer).unwrap();
                        stream.write_all(&frame).unwrap();
                    }
                    HangUp => {}
                    // The client shut its side for writing, so a read ends
                    // at once: wait out its timeout instead.
                    Silent => std::thread::sleep(ANSWER_TIMEOUT + Duration::from_millis(500)),
                }
            }
            let _ = tx.send(seen);
        });
        (dir, path, Asked(rx))
    }

    fn ok(state: State) -> Meet {
        Answer(Response::Ok { state })
    }

    fn info(info: DaemonInfo) -> Meet {
        Answer(Response::Info { info })
    }

    fn idle() -> DaemonInfo {
        DaemonInfo {
            version: "0.2.0".into(),
            state: Some(State::Idle),
            recording: Some(RecordingPhase::Idle),
            ..Default::default()
        }
    }

    fn recording(phase: RecordingPhase) -> Meet {
        Answer(Response::Recording {
            recording: RecordingStatus {
                phase,
                ..Default::default()
            },
        })
    }

    /// After one hang-up on `info`, an older daemon is asked `status` only,
    /// so its journal does not get an error every poll.
    #[test]
    fn an_older_daemon_is_remembered() {
        let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Idle), ok(State::Recording)]);
        let older = AtomicBool::new(false);
        assert_eq!(
            poll(&path, &older, false),
            DaemonView::Up(InfoAnswer::Older(State::Idle))
        );
        assert!(older.load(Ordering::Relaxed));
        assert_eq!(
            poll(&path, &older, false),
            DaemonView::Up(InfoAnswer::Older(State::Recording))
        );
        assert_eq!(daemon.join(), vec!["info", "status", "status"]);
    }

    /// Now and then an older daemon is asked `info` again, and once `info`
    /// answers (a restart outside the TUI), it is not older any more.
    #[test]
    fn older_resets_when_info_answers_again() {
        let (_dir, path, daemon) = fake_daemon(vec![info(idle()), info(idle())]);
        let older = AtomicBool::new(true);
        assert_eq!(
            poll(&path, &older, true),
            DaemonView::Up(InfoAnswer::Info(idle()))
        );
        assert!(!older.load(Ordering::Relaxed));
        poll(&path, &older, false);
        assert_eq!(daemon.join(), vec!["info", "info"]);

        // The gate always asks `info` first.
        let (_dir, path, daemon) = fake_daemon(vec![info(idle())]);
        let older = AtomicBool::new(true);
        assert_eq!(gate(&path, &older), Gate::Clear);
        assert!(!older.load(Ordering::Relaxed));
        assert_eq!(daemon.join(), vec!["info"]);
    }

    /// No daemon is down, and whatever starts next is asked `info` again.
    #[test]
    fn no_daemon_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let older = AtomicBool::new(true);
        assert_eq!(
            poll(&dir.path().join("whisrs.sock"), &older, false),
            DaemonView::Down
        );
        assert!(!older.load(Ordering::Relaxed));
    }

    /// A daemon that does not answer in time is not restarted: an older
    /// one's `status` waits on the lock a dictation start holds.
    #[test]
    fn a_daemon_that_does_not_answer_is_not_restarted() {
        let (_dir, path, daemon) = fake_daemon(vec![Silent]);
        let older = AtomicBool::new(false);
        let report = restart(&path, ServiceManager::None, &older, false, || true);
        assert_eq!(report, RestartReport::NotAnswering);
        assert_eq!(daemon.join(), vec!["info"]);

        // With no service to restart through, the hint, not the question.
        let (_dir, path, daemon) = fake_daemon(vec![Silent]);
        let report = restart(&path, ServiceManager::None, &older, false, || false);
        assert_eq!(report, RestartReport::NoService);
        daemon.join();

        // An older daemon silent on `recording status`, or on `agent list`,
        // is not restarted either.
        for meets in [
            vec![HangUp, ok(State::Idle), Silent],
            vec![
                HangUp,
                ok(State::Idle),
                recording(RecordingPhase::Idle),
                Silent,
            ],
        ] {
            let asked = meets.len();
            let (_dir, path, daemon) = fake_daemon(meets);
            assert_eq!(gate(&path, &AtomicBool::new(false)), Gate::NotAnswering);
            assert_eq!(daemon.join().len(), asked);
        }
    }

    /// Asked anyway (`r`, then `y`), silence is overridden, and only that:
    /// the daemon is asked again, and a busy one is still left alone.
    #[test]
    fn a_restart_anyway_overrides_only_silence() {
        let (_dir, path, daemon) = fake_daemon(vec![Silent]);
        let report = restart(
            &path,
            ServiceManager::None,
            &AtomicBool::new(false),
            true,
            || true,
        );
        // Restarted through the service manager: here, none.
        assert_eq!(report, RestartReport::NoService);
        assert_eq!(daemon.join(), vec!["info"]);

        let (_dir, path, daemon) = fake_daemon(vec![info(DaemonInfo {
            recording: Some(RecordingPhase::Recording),
            ..idle()
        })]);
        let report = restart(
            &path,
            ServiceManager::None,
            &AtomicBool::new(false),
            true,
            || true,
        );
        assert_eq!(report, RestartReport::Busy("recording"));
        daemon.join();
    }

    /// An older daemon's recording and agents are asked for: v0.1.30 and
    /// v0.1.31 record, and v0.1.31 drives agents.
    #[test]
    fn an_older_daemon_that_records_or_runs_an_agent_is_not_restarted() {
        let no_agents = || Answer(Response::Agents { agents: Vec::new() });
        for (phase, gated) in [
            (RecordingPhase::Recording, Gate::Busy("recording")),
            (
                RecordingPhase::Processing,
                Gate::Busy("processing a recording"),
            ),
            (RecordingPhase::Idle, Gate::Clear),
        ] {
            let mut meets = vec![HangUp, ok(State::Idle), recording(phase)];
            // Only an idle recording goes on to the agents.
            if gated == Gate::Clear {
                meets.push(no_agents());
            }
            let (_dir, path, daemon) = fake_daemon(meets);
            assert_eq!(gate(&path, &AtomicBool::new(false)), gated);
            daemon.join();
        }
        // Older than recordings and agents (hangs up), or built without
        // recordings (refuses): nothing records, no agent runs.
        for last in [
            HangUp,
            Answer(Response::Error {
                message: "built without the recording feature".into(),
            }),
        ] {
            let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Idle), last, HangUp]);
            assert_eq!(gate(&path, &AtomicBool::new(false)), Gate::Clear);
            assert_eq!(
                daemon.join(),
                vec!["info", "status", "recording status", "agent list"]
            );
        }
        // An agent not idle is a turn the restart would stop.
        for (phase, gated) in [
            (
                whisrs::agent::AgentPhase::Working,
                Gate::Busy("running an agent turn"),
            ),
            (
                whisrs::agent::AgentPhase::NeedsYou,
                Gate::Busy("running an agent turn"),
            ),
            (whisrs::agent::AgentPhase::Idle, Gate::Clear),
        ] {
            let agents = Answer(Response::Agents {
                agents: vec![whisrs::agent::AgentInfo {
                    name: "jarvis".into(),
                    harness: "claude".into(),
                    phase,
                    session: None,
                    bg_id: None,
                }],
            });
            let (_dir, path, daemon) = fake_daemon(vec![
                HangUp,
                ok(State::Idle),
                recording(RecordingPhase::Idle),
                agents,
            ]);
            assert_eq!(gate(&path, &AtomicBool::new(false)), gated);
            daemon.join();
        }
        // A dictation is enough; the rest is not asked.
        let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Transcribing)]);
        let report = restart(
            &path,
            ServiceManager::None,
            &AtomicBool::new(false),
            false,
            || true,
        );
        assert_eq!(report, RestartReport::Busy("transcribing"));
        assert_eq!(daemon.join(), vec!["info", "status"]);
    }

    /// A restart is skipped while the daemon dictates, records or processes
    /// a recording, runs an agent turn, holds its lock, or is in a state
    /// this whisrs does not know.
    #[test]
    fn a_busy_daemon_is_not_restarted() {
        let with = |f: fn(&mut DaemonInfo)| {
            let mut info = idle();
            f(&mut info);
            busy(&InfoAnswer::Info(info))
        };
        assert_eq!(with(|_| {}), None);
        assert_eq!(
            with(|i| i.state = Some(State::Recording)),
            Some("dictating")
        );
        assert_eq!(
            with(|i| i.recording = Some(RecordingPhase::Recording)),
            Some("recording")
        );
        assert_eq!(
            with(|i| i.recording = Some(RecordingPhase::Processing)),
            Some("processing a recording")
        );
        assert_eq!(with(|i| i.agent_turn = true), Some("running an agent turn"));
        assert_eq!(with(|i| i.busy = true), Some("busy"));
        assert_eq!(with(|i| i.state = None), Some("busy"));
        assert_eq!(with(|i| i.recording = None), Some("busy"));
        assert_eq!(busy(&InfoAnswer::Older(State::Speaking)), Some("speaking"));
        assert_eq!(busy(&InfoAnswer::Older(State::Idle)), None);

        for (busy_info, doing) in [
            (
                DaemonInfo {
                    busy: true,
                    ..idle()
                },
                "busy",
            ),
            (
                DaemonInfo {
                    agent_turn: true,
                    ..idle()
                },
                "running an agent turn",
            ),
        ] {
            let (_dir, path, daemon) = fake_daemon(vec![info(busy_info)]);
            let report = restart(
                &path,
                ServiceManager::None,
                &AtomicBool::new(false),
                false,
                || true,
            );
            assert_eq!(report, RestartReport::Busy(doing));
            daemon.join();
        }
    }

    #[test]
    fn with_no_service_manager_there_is_no_service() {
        let dir = tempfile::tempdir().unwrap();
        let report = restart(
            &dir.path().join("whisrs.sock"),
            ServiceManager::None,
            &AtomicBool::new(false),
            false,
            || true,
        );
        assert_eq!(report, RestartReport::NoService);
    }
}
