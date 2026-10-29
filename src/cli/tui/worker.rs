//! The TUI's threads: the daemon poll, the restart after a save, and the
//! lists a picker needs (sound devices, model files). Every request to the
//! daemon, and every listing, is made here, never on the loop that draws
//! and reads keys: a blocking connect to a daemon that stopped accepting has
//! no timeout, a sound server can be slow to answer, and the screen must not
//! freeze on either.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use whisrs::agent::{AgentAction, AgentPhase};
use whisrs::recording::{RecordingAction, RecordingOptions, RecordingPhase};
use whisrs::service::ServiceManager;
use whisrs::{Command, InfoAnswer, RequestError, Response, RestartOutcome, State};

use whisrs::audio::device::InputDeviceInfo;

use super::app::Msg;
use super::options::{self, Device, Listing};
use super::schema::ListKind;

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

/// The poll thread, and what starts a restart or a listing.
pub struct Workers {
    tx: Sender<Msg>,
    socket: PathBuf,
    service: ServiceManager,
    lister: Lister,
    /// The daemon hung up on `info`: it is older than it, so it is asked
    /// `status` instead of logging an error at every poll, and `info` again
    /// now and then ([`REPROBE_EVERY`]).
    older: Arc<AtomicBool>,
}

impl Workers {
    /// Start the poll: it answers at once, then every [`POLL_EVERY`] when
    /// the answer changed, and stops when the TUI is gone.
    pub fn start(
        tx: Sender<Msg>,
        socket: &Path,
        service: ServiceManager,
        lister: Lister,
    ) -> Workers {
        let workers = Workers {
            tx,
            socket: socket.to_path_buf(),
            service,
            lister,
            older: Arc::new(AtomicBool::new(false)),
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

/// The sound server's inputs, with stderr shut while they are listed: ALSA
/// and PulseAudio print to it, which would land on the TUI's screen.
pub fn list_devices_quietly() -> Vec<InputDeviceInfo> {
    quiet_stderr(whisrs::audio::device::list_input_devices)
}

/// Held while file descriptor 2 may be on `/dev/null`, and around handing
/// it to a child (`$EDITOR`), which then waits for a listing to put it back.
pub fn stderr_lock() -> std::sync::MutexGuard<'static, ()> {
    static QUIET: std::sync::Mutex<()> = std::sync::Mutex::new(());
    QUIET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` with file descriptor 2 on `/dev/null`, and put it back after,
/// a panic in `f` too. One at a time: two overlapping would put back each
/// other's `/dev/null`.
fn quiet_stderr<T>(f: impl FnOnce() -> T) -> T {
    use std::os::fd::AsRawFd;

    /// Puts the saved descriptor back on 2 when dropped.
    struct Restore(libc::c_int);

    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a descriptor this owns; 2 always exists.
            unsafe {
                libc::dup2(self.0, 2);
                libc::close(self.0);
            }
        }
    }

    let _one = stderr_lock();
    let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
        return f();
    };
    // SAFETY: plain descriptor calls. The copy is close-on-exec, so no child
    // started meanwhile inherits it.
    let saved = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 0) };
    if saved < 0 {
        return f();
    }
    let _back = Restore(saved);
    // SAFETY: as above; `null` outlives the call.
    unsafe { libc::dup2(null.as_raw_fd(), 2) };
    f()
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

    /// Stderr is back after a quiet listing, a panicking one too, and the
    /// saved copy is not left open.
    #[test]
    fn stderr_comes_back_after_a_quiet_listing() {
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
