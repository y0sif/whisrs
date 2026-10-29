//! The TUI's threads: the daemon poll and the restart after a save. Every
//! request to the daemon is made here, never on the loop that draws and
//! reads keys: a blocking connect to a daemon that stopped accepting has no
//! timeout, and the screen must not freeze on it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use whisrs::recording::{RecordingAction, RecordingOptions, RecordingPhase};
use whisrs::service::ServiceManager;
use whisrs::{Command, InfoAnswer, RequestError, Response, RestartOutcome, State};

use super::app::Msg;

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

/// The poll thread, and what starts a restart.
pub struct Workers {
    tx: Sender<Msg>,
    socket: PathBuf,
    service: ServiceManager,
    /// The daemon hung up on `info`: it is older than it, so it is asked
    /// `status` instead of logging an error at every poll, and `info` again
    /// now and then ([`REPROBE_EVERY`]).
    older: Arc<AtomicBool>,
}

impl Workers {
    /// Start the poll: it answers at once, then every [`POLL_EVERY`] when
    /// the answer changed, and stops when the TUI is gone.
    pub fn start(tx: Sender<Msg>, socket: &Path, service: ServiceManager) -> Workers {
        let workers = Workers {
            tx,
            socket: socket.to_path_buf(),
            service,
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

    /// Restart the daemon, on a thread of its own; the answer comes back as
    /// [`Msg::Restarted`]. Unless `anyway`, only when it is idle.
    pub fn restart(&self, anyway: bool) {
        let (tx, socket, service, older) = (
            self.tx.clone(),
            self.socket.clone(),
            self.service,
            Arc::clone(&self.older),
        );
        std::thread::spawn(move || {
            let report = restart(&socket, service, &older, anyway);
            let _ = tx.send(Msg::Restarted(report));
        });
    }
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
/// daemon restarted into a newer one is known; for an older one, `status`
/// and then `recording status`.
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
        Some(RecordingPhase::Idle) => Gate::Clear,
        Some(RecordingPhase::Recording) => Gate::Busy("recording"),
        Some(RecordingPhase::Processing) => Gate::Busy("processing a recording"),
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

/// Restart through the service manager, as `whisrs config` always has, but
/// unless `anyway` only when the daemon is idle: a restart would lose the
/// dictation, recording or agent turn it is on.
fn restart(
    socket: &Path,
    service: ServiceManager,
    older: &AtomicBool,
    anyway: bool,
) -> RestartReport {
    if !anyway {
        match gate(socket, older) {
            Gate::Clear => {}
            Gate::Busy(doing) => return RestartReport::Busy(doing),
            Gate::NotAnswering => return RestartReport::NotAnswering,
        }
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

    /// A daemon on a temp socket meeting each connection as the next of
    /// `meets`. The thread returns each command it read.
    fn fake_daemon(
        meets: Vec<Meet>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        std::thread::JoinHandle<Vec<String>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = std::thread::spawn(move || {
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
            seen
        });
        (dir, path, daemon)
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
        assert_eq!(daemon.join().unwrap(), vec!["info", "status", "status"]);
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
        assert_eq!(daemon.join().unwrap(), vec!["info", "info"]);

        // The gate always asks `info` first.
        let (_dir, path, daemon) = fake_daemon(vec![info(idle())]);
        let older = AtomicBool::new(true);
        assert_eq!(gate(&path, &older), Gate::Clear);
        assert!(!older.load(Ordering::Relaxed));
        assert_eq!(daemon.join().unwrap(), vec!["info"]);
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
        let report = restart(&path, ServiceManager::None, &AtomicBool::new(false), false);
        assert_eq!(report, RestartReport::NotAnswering);
        assert_eq!(daemon.join().unwrap(), vec!["info"]);

        // An older daemon silent on `recording status` is not either.
        let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Idle), Silent]);
        assert_eq!(gate(&path, &AtomicBool::new(false)), Gate::NotAnswering);
        assert_eq!(
            daemon.join().unwrap(),
            vec!["info", "status", "recording status"]
        );
    }

    /// Asked anyway (`r`, then `y`), it restarts without asking the daemon.
    #[test]
    fn a_restart_anyway_asks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let report = restart(
            &dir.path().join("whisrs.sock"),
            ServiceManager::None,
            &AtomicBool::new(false),
            true,
        );
        assert_eq!(report, RestartReport::NoService);
    }

    /// An older daemon's recording is asked for: v0.1.30 and v0.1.31 record.
    #[test]
    fn an_older_daemon_that_records_is_not_restarted() {
        for (phase, gated) in [
            (RecordingPhase::Recording, Gate::Busy("recording")),
            (
                RecordingPhase::Processing,
                Gate::Busy("processing a recording"),
            ),
            (RecordingPhase::Idle, Gate::Clear),
        ] {
            let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Idle), recording(phase)]);
            assert_eq!(gate(&path, &AtomicBool::new(false)), gated);
            daemon.join().unwrap();
        }
        // Older than recordings (hangs up), or built without them (refuses):
        // nothing records.
        for last in [
            HangUp,
            Answer(Response::Error {
                message: "built without the recording feature".into(),
            }),
        ] {
            let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Idle), last]);
            assert_eq!(gate(&path, &AtomicBool::new(false)), Gate::Clear);
            daemon.join().unwrap();
        }
        // A dictation is enough; the recording is not asked.
        let (_dir, path, daemon) = fake_daemon(vec![HangUp, ok(State::Transcribing)]);
        let report = restart(&path, ServiceManager::None, &AtomicBool::new(false), false);
        assert_eq!(report, RestartReport::Busy("transcribing"));
        assert_eq!(daemon.join().unwrap(), vec!["info", "status"]);
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
            let report = restart(&path, ServiceManager::None, &AtomicBool::new(false), false);
            assert_eq!(report, RestartReport::Busy(doing));
            daemon.join().unwrap();
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
        );
        assert_eq!(report, RestartReport::NoService);
    }
}
