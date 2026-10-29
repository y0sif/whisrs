//! The TUI's threads: the daemon poll and the restart after a save. Every
//! request to the daemon is made here, never on the loop that draws and
//! reads keys: a blocking connect to a daemon that stopped accepting has no
//! timeout, and the screen must not freeze on it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use whisrs::recording::RecordingPhase;
use whisrs::service::ServiceManager;
use whisrs::{InfoAnswer, RequestError, RestartOutcome, State};

use super::app::Msg;

/// How often the daemon is asked what it is doing.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// How long one answer may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(1);

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

/// What the restart after a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartReport {
    Restarted,
    /// The daemon is busy, named by what it is doing; nothing was restarted.
    Busy(&'static str),
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
    /// `status` from then on instead of logging an error at every poll.
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
            loop {
                let view = poll(&socket, &older);
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

    /// Restart the daemon to pick up a saved config, on a thread of its own;
    /// the answer comes back as [`Msg::Restarted`].
    pub fn restart(&self) {
        let (tx, socket, service, older) = (
            self.tx.clone(),
            self.socket.clone(),
            self.service,
            Arc::clone(&self.older),
        );
        std::thread::spawn(move || {
            let report = restart(&socket, service, &older);
            let _ = tx.send(Msg::Restarted(report));
        });
    }
}

/// Ask the daemon once: `info`, or `status` for a daemon known to be older.
fn poll(socket: &Path, older: &AtomicBool) -> DaemonView {
    let answer = if older.load(Ordering::Relaxed) {
        whisrs::request_status_blocking_at(socket, Some(ANSWER_TIMEOUT)).map(InfoAnswer::Older)
    } else {
        whisrs::request_info_blocking_at(socket, Some(ANSWER_TIMEOUT))
    };
    match answer {
        Ok(answer) => {
            if matches!(answer, InfoAnswer::Older(_)) {
                older.store(true, Ordering::Relaxed);
            }
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
/// loses nothing.
pub fn busy(answer: &InfoAnswer) -> Option<&'static str> {
    let dictation = match answer.state() {
        Some(State::Idle) => None,
        Some(State::Recording) => Some("dictating"),
        Some(State::Transcribing) => Some("transcribing"),
        Some(State::Synthesizing | State::Speaking) => Some("speaking"),
        None => Some("busy"),
    };
    let recording = match answer {
        InfoAnswer::Info(info) => match info.recording {
            Some(RecordingPhase::Idle) => None,
            Some(RecordingPhase::Recording) => Some("recording"),
            Some(RecordingPhase::Processing) => Some("processing a recording"),
            None => Some("busy"),
        },
        InfoAnswer::Older(_) => None,
    };
    recording.or(dictation)
}

/// Restart through the service manager, as `whisrs config` always has, but
/// not while the daemon is busy: a restart would lose the dictation or the
/// recording it is on.
fn restart(socket: &Path, service: ServiceManager, older: &AtomicBool) -> RestartReport {
    if let DaemonView::Up(answer) = poll(socket, older) {
        if let Some(doing) = busy(&answer) {
            return RestartReport::Busy(doing);
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

    use whisrs::Response;

    use super::*;

    /// A daemon on a temp socket that answers each connection with the next
    /// of `answers`, or hangs up for `None` (an older daemon on `info`). The
    /// thread returns each command it read.
    fn fake_daemon(
        answers: Vec<Option<Response>>,
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
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
                let cmd: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen.push(cmd["cmd"].as_str().unwrap_or_default().to_string());
                if let Some(answer) = answer {
                    let frame = whisrs::encode_message(&answer).unwrap();
                    stream.write_all(&frame).unwrap();
                }
            }
            seen
        });
        (dir, path, daemon)
    }

    fn ok(state: State) -> Option<Response> {
        Some(Response::Ok { state })
    }

    /// After one hang-up on `info`, an older daemon is asked `status` only,
    /// so its journal does not get an error every poll.
    #[test]
    fn an_older_daemon_is_remembered() {
        let (_dir, path, daemon) = fake_daemon(vec![None, ok(State::Idle), ok(State::Recording)]);
        let older = AtomicBool::new(false);
        assert_eq!(
            poll(&path, &older),
            DaemonView::Up(InfoAnswer::Older(State::Idle))
        );
        assert!(older.load(Ordering::Relaxed));
        assert_eq!(
            poll(&path, &older),
            DaemonView::Up(InfoAnswer::Older(State::Recording))
        );
        assert_eq!(daemon.join().unwrap(), vec!["info", "status", "status"]);
    }

    #[test]
    fn a_daemon_that_knows_info_is_asked_info() {
        let info = whisrs::DaemonInfo {
            version: "0.2.0".into(),
            state: Some(State::Idle),
            ..Default::default()
        };
        let (_dir, path, daemon) = fake_daemon(vec![
            Some(Response::Info { info: info.clone() }),
            Some(Response::Info { info: info.clone() }),
        ]);
        let older = AtomicBool::new(false);
        for _ in 0..2 {
            assert_eq!(
                poll(&path, &older),
                DaemonView::Up(InfoAnswer::Info(info.clone()))
            );
        }
        assert!(!older.load(Ordering::Relaxed));
        assert_eq!(daemon.join().unwrap(), vec!["info", "info"]);
    }

    /// No daemon is down, and whatever starts next is asked `info` again.
    #[test]
    fn no_daemon_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let older = AtomicBool::new(true);
        assert_eq!(
            poll(&dir.path().join("whisrs.sock"), &older),
            DaemonView::Down
        );
        assert!(!older.load(Ordering::Relaxed));
    }

    /// A restart is skipped while the daemon dictates, records or processes
    /// a recording, or is in a state this whisrs does not know.
    #[test]
    fn a_busy_daemon_is_not_restarted() {
        let info = |state, recording| {
            InfoAnswer::Info(whisrs::DaemonInfo {
                state,
                recording,
                ..Default::default()
            })
        };
        let idle = Some(State::Idle);
        assert_eq!(busy(&info(idle, Some(RecordingPhase::Idle))), None);
        assert_eq!(
            busy(&info(Some(State::Recording), Some(RecordingPhase::Idle))),
            Some("dictating")
        );
        assert_eq!(
            busy(&info(idle, Some(RecordingPhase::Recording))),
            Some("recording")
        );
        assert_eq!(
            busy(&info(idle, Some(RecordingPhase::Processing))),
            Some("processing a recording")
        );
        assert_eq!(busy(&info(None, Some(RecordingPhase::Idle))), Some("busy"));
        assert_eq!(busy(&info(idle, None)), Some("busy"));
        assert_eq!(busy(&InfoAnswer::Older(State::Speaking)), Some("speaking"));
        assert_eq!(busy(&InfoAnswer::Older(State::Idle)), None);

        let (_dir, path, daemon) = fake_daemon(vec![None, ok(State::Transcribing)]);
        let report = restart(&path, ServiceManager::None, &AtomicBool::new(false));
        assert_eq!(report, RestartReport::Busy("transcribing"));
        daemon.join().unwrap();
    }

    #[test]
    fn with_no_service_manager_there_is_no_service() {
        let dir = tempfile::tempdir().unwrap();
        let report = restart(
            &dir.path().join("whisrs.sock"),
            ServiceManager::None,
            &AtomicBool::new(false),
        );
        assert_eq!(report, RestartReport::NoService);
    }
}
