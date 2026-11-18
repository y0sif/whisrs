//! The process-per-turn driver: each turn is one harness process that prints
//! JSON lines and exits (OpenCode's `opencode run`, Codex's `codex exec`). A
//! harness module says how a turn starts and what a line of its output means
//! ([`TurnHarness`]); [`ProcessDriver`] does the rest, the same for every such
//! harness:
//!
//! - **Spawn:** the agent's `command` with the harness's arguments, in the
//!   agent's dir, with the daemon's environment minus what a parent session
//!   sets for the processes it runs (Claude Code's,
//!   [`super::claude::strips_env`], and the harness's own,
//!   [`TurnHarness::strips_env`]) and with `PWD` set to the dir: a cwd alone
//!   leaves the daemon's `PWD`, and OpenCode resolves its directory from
//!   `PWD`. In a process group of its own, and in a transient systemd scope
//!   of its own ([`TurnScopes`], `systemd-run --user --scope`, which execs
//!   the harness as the same process), so a stop reaches every process the
//!   turn started, also one that left the group for a session of its own
//!   (Copilot's tool shells do). Without `systemd-run`, or when it cannot
//!   make the scope, the harness is started directly. Stdin is the prompt,
//!   then closed, or `/dev/null`: OpenCode and Codex read an open stdin until
//!   it ends.
//! - **The session:** an id the launch names that the agent does not have
//!   yet (OpenCode's, minted by whisrs, [`Launch::session`]) is reported as
//!   soon as the process runs, pending ([`SessionRef::pending`]) until a
//!   text or a tool call shows the agent working in it; an id the output
//!   names is reported when it is new.
//! - **The daemon's end ends the turn:** the child is spawned from a thread
//!   that waits for it, with `PR_SET_PDEATHSIG` = SIGINT, so it is
//!   interrupted when the daemon exits or dies. A turn never outlives its
//!   daemon (it would run unwatched, and the next turn would run beside it):
//!   anything an earlier turn left in its scope, a background job included
//!   (a process that ignored the SIGINT, or one a turn that ended normally
//!   left running), is stopped when the next daemon starts
//!   ([`TurnScopes::sweep`]), and a turn watched again after a restart ends
//!   at once ([`cut_off`]).
//! - **Output:** stdout line by line ([`TurnHarness::events`]): a tool call
//!   is an activity ([`activity_for_tool`]) and releases the assistant text
//!   before it as an interim ([`InterimTracker`]), and the text of the last
//!   message is the reply. Stderr is kept for the error, which the harness
//!   may recognize ([`TurnHarness::stderr_failure`]). A line is never kept
//!   whole past a cap ([`STDOUT_LINE_MAX`], [`STDERR_LINE_MAX`]).
//! - **Stop:** SIGINT, then SIGTERM, then SIGKILL, each sent only while
//!   something of the turn is left, after the previous one's grace
//!   ([`STOP_GRACES`]), to the process group, to every process of the tree
//!   seen so far ([`Tree`]) and to the scope (`systemctl --user kill`). A
//!   turn dropped while its process runs (its task aborted) is stopped the
//!   same way, and `--stop` waits for that stop. A turn stopped while its
//!   process was still being started (before it was registered) has
//!   nothing for `--stop` to wait for, so `--stop` returns at once; the
//!   process is stopped the same way in the background as soon as it runs,
//!   and the agent's next turn waits for that.
//!
//! Between turns no process runs: a session never works on its own, waits
//! for a prompt or sends a hook.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{oneshot, watch, OnceCell};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::progress::{activity_for_tool, InterimTracker};
use super::{
    claude, Agent, AgentPhase, Driver, IdleEvent, SessionRef, TurnIo, TurnOutcome, TurnRequest,
    TurnUpdate, TurnUpdates,
};

/// How long each stop signal gets: SIGINT (the harness interrupts its turn
/// and exits; OpenCode in 0.2 s, measured), then SIGTERM, and after SIGKILL
/// how long the exit may take to be seen.
pub const STOP_GRACES: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(2),
    Duration::from_secs(2),
];

/// The stop signals, in order, with their names for `systemctl kill`.
const STOP_SIGNALS: [(libc::c_int, &str); 3] = [
    (libc::SIGINT, "SIGINT"),
    (libc::SIGTERM, "SIGTERM"),
    (libc::SIGKILL, "SIGKILL"),
];

/// How often a stop checks whether the turn's processes are gone.
const STOP_POLL: Duration = Duration::from_millis(50);

/// After the process exited, how long its output is still read: a process
/// it left behind can hold its stdout open.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// The most stderr lines kept for an error.
const STDERR_LINES: usize = 40;

/// The longest stdout line read, in bytes. A longer one is skipped (a JSON
/// line cut short says nothing), with a warning; it is never kept whole.
pub const STDOUT_LINE_MAX: usize = 1 << 20;

/// The longest stderr line kept, in bytes: only its start is ever quoted
/// ([`ERROR_MAX`]). A longer one is cut there, with a warning.
pub const STDERR_LINE_MAX: usize = 8 << 10;

/// The longest output line logged, in characters.
const LOG_LINE_MAX: usize = 300;

/// The longest harness error quoted in a notification, in characters.
const ERROR_MAX: usize = 300;

/// How long a `systemctl --user` call may take.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// What a harness module provides
// ---------------------------------------------------------------------------

/// How one turn's process starts. The program is the agent's `command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The arguments after `command`.
    pub args: Vec<String>,
    /// Written to stdin, which is then closed. `None`: stdin is `/dev/null`.
    pub stdin: Option<String>,
    /// Set in the child's environment, after the stripped variables are
    /// removed.
    pub env: Vec<(String, String)>,
    /// The session the turn runs in, when the launch names it (OpenCode's
    /// `--session <id>`). An id the agent does not have yet is reported as
    /// the process starts, pending, so a turn stopped or cut off before its
    /// first line keeps it. `None` when the harness names it in its output
    /// (Codex).
    pub session: Option<String>,
}

/// What a line of a harness's output says about the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// The id of the session the turn runs in.
    Session(String),
    /// Assistant text of the message `message` (the harness's id for it).
    /// The text parts of one message join, and the turn's last message is
    /// its reply.
    Text { message: String, text: String },
    /// The agent used a tool, by the harness's name for it.
    Tool(String),
    /// The turn failed: the harness's message.
    Error(String),
    /// An error the agent may recover from: it fails the turn, with the
    /// harness's message, unless a text comes after it (Copilot's
    /// `model_call` error, after which the agent can go on and reply).
    RecoverableError(String),
}

/// A harness whose turn is one process. Every method is required: there is
/// no behavior to inherit by leaving one out.
pub trait TurnHarness: Send + Sync + 'static {
    /// The harness's name in messages ("OpenCode").
    fn label(&self) -> &'static str;

    /// How a turn starts. `session` is the agent's session, empty for its
    /// first turn (or after `--new`), and pending while no turn has shown
    /// the agent working in it. An error is actionable and fails the turn
    /// as it is.
    fn launch(
        &self,
        agent: &Agent,
        session: &SessionRef,
        request: &TurnRequest,
    ) -> Result<Launch, String>;

    /// Whether the daemon's environment variable `name` is removed from the
    /// child besides those every driver removes
    /// ([`super::claude::strips_env`]): what a parent session of this harness
    /// sets for the processes it runs.
    fn strips_env(&self, name: &str) -> bool;

    /// The events of one stdout line, in order. None for a line that says
    /// nothing about the turn, or is not the harness's JSON.
    fn events(&self, line: &str) -> Vec<StreamEvent>;

    /// The failure of a turn that printed no error event and no reply and
    /// exited unsuccessfully, from its last stderr lines, when the harness
    /// recognizes them (a session it no longer has); `None` leaves it to
    /// the line that mentions an error ([`stderr_detail`]).
    fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Pure parts
// ---------------------------------------------------------------------------

/// What a turn's output said so far, and the updates it is worth.
#[derive(Debug)]
pub struct StreamState {
    /// The session id known: the agent's, then the newest the output named.
    session_id: Option<String>,
    /// That id is pending ([`SessionRef::pending`]).
    pending: bool,
    tracker: InterimTracker,
    /// The last activity sent, so a run of the same tool sends it once.
    activity: Option<String>,
    /// Assistant texts with their message ids, in order.
    texts: Vec<(String, String)>,
    /// The last error the harness reported.
    error: Option<String>,
    /// The last recoverable error ([`StreamEvent::RecoverableError`]) while
    /// no text came after it.
    recoverable: Option<String>,
    /// A stdout line too long to read ([`STDOUT_LINE_MAX`]) came after the
    /// last text: it may have been the final message, so there is no reply.
    skipped_after_text: bool,
}

impl StreamState {
    /// The state of a turn in `session_id` (`None` for a new session).
    pub fn new(session_id: Option<String>) -> Self {
        Self::for_session(&SessionRef {
            session_id,
            ..SessionRef::default()
        })
    }

    /// The state of a turn in `session`, pending or not.
    pub fn for_session(session: &SessionRef) -> Self {
        Self {
            session_id: session.session_id.clone(),
            pending: session.pending && session.session_id.is_some(),
            tracker: InterimTracker::new(),
            activity: None,
            texts: Vec::new(),
            error: None,
            recoverable: None,
            skipped_after_text: false,
        }
    }

    /// Take one event, reporting what the runtime needs to `updates`.
    pub fn event(&mut self, event: StreamEvent, updates: &TurnUpdates) {
        match event {
            StreamEvent::Session(id) => {
                if self.session_id.as_deref() != Some(id.as_str()) {
                    self.session_id = Some(id);
                    self.pending = false;
                    self.report(updates);
                }
            }
            StreamEvent::Text { message, text } => {
                self.confirm(updates);
                self.tracker.on_text(&text);
                self.texts.push((message, text));
                self.skipped_after_text = false;
                self.recoverable = None;
            }
            StreamEvent::Tool(name) => {
                self.confirm(updates);
                let activity = activity_for_tool(&name);
                if self.activity.as_ref() != Some(&activity) {
                    self.activity = Some(activity.clone());
                    updates.send(TurnUpdate::Activity(activity));
                }
                if let Some(raw) = self.tracker.on_tool() {
                    updates.send(TurnUpdate::Interim(raw));
                }
            }
            StreamEvent::Error(message) => self.error = Some(message),
            StreamEvent::RecoverableError(message) => self.recoverable = Some(message),
        }
    }

    /// A stdout line was skipped, too long to read: until a text comes
    /// after it, the turn has no reply. The line may have been the final
    /// message, and an earlier one (an acknowledgement, spoken already) is
    /// never the reply in its place.
    pub fn skipped_line(&mut self) {
        self.skipped_after_text = true;
    }

    /// The agent works in the session: a pending id is confirmed, so the
    /// next turn continues it as it is.
    fn confirm(&mut self, updates: &TurnUpdates) {
        if self.pending {
            self.pending = false;
            self.report(updates);
        }
    }

    fn report(&self, updates: &TurnUpdates) {
        updates.send(TurnUpdate::Session(SessionRef {
            session_id: self.session_id.clone(),
            bg_id: None,
            transcript_path: None,
            pending: self.pending,
        }));
    }

    /// The turn's final message: the text parts of its last message, raw.
    /// `None` when it wrote none, its last message is empty, or a line too
    /// long to read came after it ([`StreamState::skipped_line`]).
    pub fn reply(&self) -> Option<String> {
        if self.skipped_after_text {
            return None;
        }
        let (last, _) = self.texts.last()?;
        let text = self
            .texts
            .iter()
            .filter(|(message, _)| message == last)
            .map(|(_, text)| text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// The turn ended: what the tracker holds is the final message, never
    /// an interim.
    fn end(&mut self) {
        self.tracker.end();
    }
}

/// How a turn's process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Status(ExitStatus),
    /// Its exit could not be read.
    Lost(String),
}

impl Exit {
    fn success(&self) -> bool {
        matches!(self, Exit::Status(status) if status.success())
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Status(status) => write!(f, "{status}"),
            Exit::Lost(e) => write!(f, "exit unknown: {e}"),
        }
    }
}

/// How the turn ended, from what its output said and how its process
/// exited. An error the harness reported fails it (a recoverable one only
/// when no text came after it); otherwise the last message is the reply (a
/// non-zero exit after one is only logged); a turn with neither fails with
/// what stderr said, as the harness reads it
/// ([`TurnHarness::stderr_failure`]) or else generically.
pub fn outcome(
    agent: &Agent,
    harness: &dyn TurnHarness,
    state: &StreamState,
    stderr: &[String],
    exit: &Exit,
) -> TurnOutcome {
    let label = harness.label();
    if let Some(error) = state.error.as_ref().or(state.recoverable.as_ref()) {
        return TurnOutcome::Failed(format!(
            "Agent '{}': {label} failed: {}",
            agent.name,
            sentence(error)
        ));
    }
    if let Some(reply) = state.reply() {
        if !exit.success() {
            warn!(
                "agent '{}': {label} replied, then exited with {exit}",
                agent.name
            );
        }
        return TurnOutcome::Reply(reply);
    }
    if exit.success() {
        return TurnOutcome::Failed(format!(
            "Agent '{}': {label} ended the turn without a reply.",
            agent.name
        ));
    }
    if let Some(message) = harness.stderr_failure(agent, stderr) {
        return TurnOutcome::Failed(message);
    }
    let detail = stderr_detail(stderr)
        .map(|line| sentence(&line))
        .unwrap_or_else(|| format!("it exited with {exit}."));
    TurnOutcome::Failed(format!("Agent '{}': {label} failed: {detail}", agent.name))
}

/// The stderr line that best says what went wrong: the first that mentions
/// an error, else the last one. Color codes removed; `None` without any.
pub fn stderr_detail(lines: &[String]) -> Option<String> {
    let lines: Vec<String> = lines
        .iter()
        .map(|l| strip_ansi(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .find(|l| l.to_ascii_lowercase().contains("error"))
        .or(lines.last())
        .cloned()
}

/// `text` on one line, capped at [`ERROR_MAX`] characters, ending in
/// punctuation.
pub fn sentence(text: &str) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > ERROR_MAX {
        line = line.chars().take(ERROR_MAX).collect::<String>() + "...";
    }
    if !line.ends_with(['.', '!', '?']) {
        line.push('.');
    }
    line
}

/// `text` without ANSI escape sequences (colors, cursor moves).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, up to the final byte.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// The error of a turn watched again after a restart: its process ended
/// with the old daemon (see the module doc), so it never replied. A session
/// the agent was never seen working in (none yet, or pending) may not have
/// heard of the turn at all.
pub fn cut_off(agent: &Agent, label: &str, session: &SessionRef) -> String {
    let then = if session.session_id.is_none() || session.pending {
        format!("It may not have reached {label} at all, so say it again.")
    } else {
        "The session keeps what it did so far.".to_string()
    };
    format!(
        "Agent '{}': whisrs restarted during that turn, which stopped it before {label} replied. \
         {then}",
        agent.name
    )
}

/// The error for a process that cannot be started. Spawning in a missing
/// working directory fails with "not found" too, so the dir is checked
/// first (`dir_exists`).
pub fn spawn_failure(
    agent: &Agent,
    label: &str,
    error: &std::io::Error,
    dir_exists: bool,
) -> String {
    if !dir_exists {
        format!(
            "Agent '{}': its dir {} does not exist.",
            agent.name,
            agent.dir.display()
        )
    } else if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "Agent '{}': `{}` was not found. Install {label}, or set command in [[agents]] to its \
             full path (a systemd user service may not have ~/.local/bin on PATH).",
            agent.name, agent.command
        )
    } else {
        format!(
            "Agent '{}': cannot run `{}`: {error}",
            agent.name, agent.command
        )
    }
}

/// `N` random bytes from the kernel (`getrandom`), for ids a harness
/// takes from whisrs.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let mut filled = 0;
    while filled < N {
        // SAFETY: writes at most `N - filled` bytes into `bytes[filled..]`.
        let n = unsafe {
            libc::getrandom(
                bytes[filled..].as_mut_ptr().cast::<libc::c_void>(),
                N - filled,
                0,
            )
        };
        if n > 0 {
            filled += n as usize;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // No kernel randomness (never on a supported kernel): the clock
            // still keeps ids apart.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            for (i, b) in bytes[filled..].iter_mut().enumerate() {
                *b = (nanos >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(151);
            }
            break;
        }
    }
    bytes
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Where a turn's processes are: its group, its tree, its scope
// ---------------------------------------------------------------------------

/// Whether any process of the group `pgid` is left (a zombie counts).
fn group_alive(pgid: u32) -> bool {
    let Some(pgid) = group_target(pgid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks the group exists.
    let rc = unsafe { libc::kill(pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Send `signal` to the group `pgid`.
fn signal_group(pgid: u32, signal: libc::c_int) {
    if let Some(pgid) = group_target(pgid) {
        // SAFETY: a plain kill(2) of our child's own process group.
        unsafe { libc::kill(pgid, signal) };
    }
}

/// The negative pid that names the group `pgid` for kill(2). Never 0 or
/// 1: `kill(0)` would signal the daemon's own group and `kill(-1)` every
/// process it may signal.
fn group_target(pgid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pgid)
        .ok()
        .filter(|&p| p > 1)
        .map(|p| -p)
}

/// The state letter and start time of process `pid` (`/proc/<pid>/stat`).
fn proc_stat(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some((claude::stat_state(&stat)?, claude::stat_start(&stat)?))
}

/// The children of process `pid`, from every one of its threads
/// (`/proc/<pid>/task/<tid>/children`).
fn proc_children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for task in tasks.flatten() {
        if let Ok(list) = std::fs::read_to_string(task.path().join("children")) {
            children.extend(
                list.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    children
}

/// The processes of a turn's tree seen so far, by pid and start time (a pid
/// the kernel gave to another process since has another start time). A
/// process that left the turn's group (`setsid`) is found through its
/// parent, and kept once seen, so it is still signalled after that parent
/// exited. One whose parent exited before any walk saw it is out of reach
/// here; the turn's scope still holds it ([`Scope`]).
#[derive(Debug, Default)]
struct Tree {
    seen: Vec<(u32, u64)>,
}

impl Tree {
    /// Add every live descendant of `root` and of the processes seen.
    fn grow(&mut self, root: Option<u32>) {
        let mut stack: Vec<u32> = root.into_iter().chain(self.alive()).collect();
        let mut walked = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !walked.insert(pid) {
                continue;
            }
            for child in proc_children(pid) {
                if let Some((_, start)) = proc_stat(child) {
                    if !self.seen.contains(&(child, start)) {
                        self.seen.push((child, start));
                    }
                    stack.push(child);
                }
            }
        }
    }

    /// The processes seen that still run (a zombie does not).
    fn alive(&self) -> Vec<u32> {
        self.seen
            .iter()
            .filter(|&&(pid, start)| {
                proc_stat(pid).is_some_and(|(state, now)| now == start && state != 'Z')
            })
            .map(|&(pid, _)| pid)
            .collect()
    }

    /// Send `signal` to every process seen that still runs.
    fn signal(&self, signal: libc::c_int) {
        let daemon = std::process::id();
        for pid in self.alive() {
            if pid <= 1 || pid == daemon {
                continue;
            }
            if let Ok(pid) = libc::pid_t::try_from(pid) {
                // SAFETY: a plain kill(2) of a process of the turn's tree.
                unsafe { libc::kill(pid, signal) };
            }
        }
    }
}

/// Run `systemctl --user args`: its stdout, or why it gave none.
async fn systemctl(args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("systemctl");
    cmd.arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("systemctl: {e}"))?;
    match tokio::time::timeout(SYSTEMCTL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Ok(Err(e)) => Err(format!("systemctl: {e}")),
        Err(_) => Err(format!(
            "systemctl --user {} did not answer in {} seconds",
            args.first().copied().unwrap_or(""),
            SYSTEMCTL_TIMEOUT.as_secs()
        )),
    }
}

/// Whether the cgroup v2 hierarchy is mounted at `/sys/fs/cgroup`, where a
/// scope's `cgroup.procs` lists its processes. Checked once.
fn unified_cgroups() -> bool {
    static UNIFIED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *UNIFIED.get_or_init(|| Path::new("/sys/fs/cgroup/cgroup.controllers").exists())
}

/// Whether the cgroup directory `dir` holds a process. A directory systemd
/// removed with its unit holds none.
fn cgroup_populated(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("cgroup.procs")).is_ok_and(|p| !p.trim().is_empty())
}

/// The transient scope a turn runs in. Every process of the turn is in it,
/// whatever its group or session, so signalling the scope reaches them all.
#[derive(Debug)]
struct Scope {
    unit: String,
    /// Its cgroup directory, once systemd named it.
    cgroup: Mutex<Option<PathBuf>>,
}

impl Scope {
    fn new(unit: String) -> Self {
        Self {
            unit,
            cgroup: Mutex::new(None),
        }
    }

    fn cgroup(&self) -> Option<PathBuf> {
        self.cgroup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Learn its cgroup from systemd, unless known. A unit systemd does not
    /// have (not made yet, or gone) has none.
    async fn locate(&self) {
        if self.cgroup().is_some() || !unified_cgroups() {
            return;
        }
        let args = ["show", "--property=ControlGroup", "--value", &self.unit];
        if let Ok(out) = systemctl(&args).await {
            let path = out.trim();
            if path.starts_with('/') {
                let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
                *self.cgroup.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
            }
        }
    }

    /// Whether a process is left in it, as far as its cgroup shows: one not
    /// located (made yet, or on a system without cgroup v2) shows none.
    fn populated(&self) -> bool {
        self.cgroup().is_some_and(|dir| cgroup_populated(&dir))
    }

    /// Send `signal` (`name`) to every process in it. A unit systemd does
    /// not have is nothing to signal.
    async fn signal(&self, name: &str) {
        let signal = format!("--signal={name}");
        if let Err(e) = systemctl(&["kill", &signal, &self.unit]).await {
            debug!("{}: {name} not sent: {e}", self.unit);
        }
    }

    /// Have systemd stop the unit, in case anything is left in it after the
    /// signals; one that already ended is gone (`--collect`).
    async fn release(&self) {
        if let Err(e) = systemctl(&["stop", "--no-block", &self.unit]).await {
            debug!("{}: not stopped: {e}", self.unit);
        }
    }

    /// Stop a scope a previous daemon left (no process of it is ours to
    /// wait for): each signal while something is left in it, after the
    /// previous one's grace, then the unit.
    async fn stop_leftover(&self, graces: &[Duration; 3]) -> Result<(), String> {
        let mut result = Err(format!("something in {} did not exit", self.unit));
        for (step, ((_, name), grace)) in STOP_SIGNALS.iter().zip(graces).enumerate() {
            self.locate().await;
            // The first signal goes out even to a scope whose cgroup cannot
            // be read: systemd still delivers it.
            if step > 0 && !self.populated() {
                result = Ok(());
                break;
            }
            self.signal(name).await;
            let deadline = Instant::now() + *grace;
            while self.populated() && Instant::now() < deadline {
                tokio::time::sleep(STOP_POLL).await;
            }
            if !self.populated() {
                result = Ok(());
                break;
            }
        }
        self.release().await;
        result
    }
}

/// The transient systemd scopes turns run in, one per turn:
/// `whisrs-agent-<instance>-<agent>-<turn>.scope` in the user's systemd
/// (`systemd-run --user --scope`). `<instance>` is a hash of the daemon's
/// `agents.json` path, so a daemon only ever stops its own turns' scopes,
/// never those of another whisrsd of the same user (a test daemon with its
/// own data dir beside the real one).
///
/// A scope is outside the daemon's cgroup (`whisrs.service`'s, under
/// systemd), so a stopped or restarted daemon does not take it along: the
/// harness is interrupted when the daemon ends (`PR_SET_PDEATHSIG`), and
/// anything an earlier turn left in its scope, a background job included
/// (also one a turn that ended normally left running), is stopped when the
/// next daemon starts ([`TurnScopes::sweep`]), before any turn runs: as a
/// restart of the service's cgroup would stop it.
#[derive(Debug)]
pub struct TurnScopes {
    /// `whisrs-agent-<instance>-`, or `None` for no scopes at all.
    prefix: Option<String>,
    graces: [Duration; 3],
    swept: OnceCell<()>,
    /// A turn was started directly because systemd-run could not make its
    /// scope: said once as a warning, then in the debug log.
    unscoped: std::sync::atomic::AtomicBool,
}

impl TurnScopes {
    /// The scopes of the daemon whose agents are kept at `store`.
    pub fn new(store: &Path) -> Self {
        Self::with_prefix(Some(scope_prefix(store)), STOP_GRACES)
    }

    /// No scopes: every turn starts directly, and nothing is swept.
    pub fn off() -> Self {
        Self::with_prefix(None, STOP_GRACES)
    }

    fn with_prefix(prefix: Option<String>, graces: [Duration; 3]) -> Self {
        Self {
            prefix,
            graces,
            swept: OnceCell::new(),
            unscoped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A turn of `agent` starts directly: systemd-run said `why`.
    fn note_unscoped(&self, agent: &str, label: &str, why: &str) {
        if self
            .unscoped
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            debug!("agent '{agent}': no scope ({why}); starting {label} directly");
        } else {
            warn!(
                "agent '{agent}': systemd-run did not start {label} in a scope ({why}); starting \
                 it directly, so a stop reaches only its process tree (said once)"
            );
        }
    }

    /// How a turn of `agent` starts: in a scope of its own, named for it.
    fn wrap(&self, agent: &str) -> Wrap {
        match &self.prefix {
            Some(prefix) => {
                let agent: String = agent.chars().take(64).collect();
                Wrap::Scope {
                    unit: format!("{prefix}{agent}-{}.scope", hex(&random_bytes::<4>())),
                    no_expand: true,
                }
            }
            None => Wrap::Direct,
        }
    }

    /// Stop anything a previous daemon's turns left in their scopes, a
    /// background job included, once: started when the daemon starts (also
    /// a daemon with no agents left), and waited for by every turn.
    pub async fn sweep(&self) {
        let Some(prefix) = &self.prefix else {
            return;
        };
        self.swept
            .get_or_init(|| async {
                let pattern = format!("{prefix}*");
                let listing = match systemctl(&[
                    "list-units",
                    "--all",
                    "--plain",
                    "--no-legend",
                    "--no-pager",
                    "--type=scope",
                    &pattern,
                ])
                .await
                {
                    Ok(listing) => listing,
                    Err(e) => {
                        debug!("agents: no turn scopes to look for: {e}");
                        return;
                    }
                };
                for unit in listed_units(&listing, prefix) {
                    info!(
                        "agents: stopping {unit}: anything an earlier turn left in its scope, a \
                         background job included"
                    );
                    if let Err(e) = Scope::new(unit).stop_leftover(&self.graces).await {
                        warn!("agents: {e}");
                    }
                }
            })
            .await;
    }
}

/// `whisrs-agent-<instance>-` for the daemon whose agents are kept at
/// `store`: eight hex digits of a 64-bit FNV-1a hash of the path.
pub fn scope_prefix(store: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in store.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("whisrs-agent-{:08x}-", hash as u32)
}

/// The scope units of a `systemctl list-units --plain --no-legend` listing
/// (`UNIT LOAD ACTIVE SUB DESCRIPTION`) whose names start with `prefix`,
/// unless they already ended (`inactive`, `failed`).
pub fn listed_units(listing: &str, prefix: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let unit = columns.next()?;
            let active = columns.nth(1).unwrap_or("");
            (unit.starts_with(prefix)
                && unit.ends_with(".scope")
                && !matches!(active, "inactive" | "failed"))
            .then(|| unit.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The running process
// ---------------------------------------------------------------------------

/// How a turn's process is started.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wrap {
    /// In the transient scope `unit` (`systemd-run --user --scope`), with
    /// `--expand-environment=no` when `no_expand`
    /// ([`claude::scope_args`]).
    Scope { unit: String, no_expand: bool },
    /// Directly.
    Direct,
}

impl Wrap {
    /// What to try when a turn started this way never ran: `read` is all it
    /// printed. A systemd-run older than 254 refused
    /// `--expand-environment` (it expands nothing, so it goes without); one
    /// that could not make the scope, or found no harness to run, leaves
    /// the start to a direct spawn.
    fn after(&self, read: &Read) -> Option<Wrap> {
        let Wrap::Scope { unit, no_expand } = self else {
            return None;
        };
        if read.stdout_lines > 0 {
            return None;
        }
        let stderr = read.stderr.join("\n");
        let success = read.exit.success();
        if *no_expand && claude::expand_option_refused(success, "", &stderr) {
            return Some(Wrap::Scope {
                unit: unit.clone(),
                no_expand: false,
            });
        }
        claude::scope_unavailable(success, "", &stderr).then_some(Wrap::Direct)
    }
}

/// A turn's running process, kept so `--stop` can end it.
#[derive(Debug)]
struct TurnProcess {
    /// Its pid, which is also its process group's id.
    pid: u32,
    /// The scope it runs in, when it was started in one.
    scope: Option<Scope>,
    /// Its exit, once the thread that waits for it saw it.
    exit: watch::Receiver<Option<Exit>>,
    /// The processes it started, as far as a stop saw them.
    tree: Mutex<Tree>,
    /// The stop, run once however many ask for it.
    stopped: OnceCell<Result<(), String>>,
}

impl TurnProcess {
    fn new(pid: u32, scope: Option<Scope>, exit: watch::Receiver<Option<Exit>>) -> Self {
        Self {
            pid,
            scope,
            exit,
            tree: Mutex::new(Tree::default()),
            stopped: OnceCell::new(),
        }
    }

    fn exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    fn tree(&self) -> std::sync::MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Walk its tree again: from the process itself while it runs (once it
    /// was reaped its pid may be another's), and from what was seen.
    fn grow(&self) {
        let root = (!self.exited()).then_some(self.pid);
        self.tree().grow(root);
    }

    /// The process exited and nothing of its group, its tree or its scope
    /// is left.
    fn gone(&self) -> bool {
        self.exited()
            && !group_alive(self.pid)
            && self.tree().alive().is_empty()
            && !self.scope.as_ref().is_some_and(Scope::populated)
    }

    /// Stop it ([`TurnProcess::escalate`]), once: a second caller waits for
    /// the first one's stop.
    async fn stop(&self, graces: &[Duration; 3]) -> Result<(), String> {
        self.stopped
            .get_or_init(|| self.escalate(graces))
            .await
            .clone()
    }

    /// SIGINT, SIGTERM, SIGKILL to the group, the tree and the scope, each
    /// while something of the turn is left, after the previous one's grace.
    /// Then the scope's unit is stopped too.
    async fn escalate(&self, graces: &[Duration; 3]) -> Result<(), String> {
        let mut result = Err(format!(
            "its process {} or one it started did not exit",
            self.pid
        ));
        for ((signal, name), grace) in STOP_SIGNALS.iter().zip(graces) {
            if let Some(scope) = &self.scope {
                scope.locate().await;
            }
            self.grow();
            if self.gone() {
                result = Ok(());
                break;
            }
            debug!(
                "turn process {}: {name} to its group, tree and scope",
                self.pid
            );
            signal_group(self.pid, *signal);
            self.tree().signal(*signal);
            if let Some(scope) = &self.scope {
                scope.signal(name).await;
            }
            if self.wait_gone(*grace).await {
                result = Ok(());
                break;
            }
        }
        if let Some(scope) = &self.scope {
            scope.release().await;
        }
        result
    }

    /// Wait up to `within` for [`TurnProcess::gone`], walking the tree as
    /// it goes, so a child is seen before its parent exits.
    async fn wait_gone(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            self.grow();
            if self.gone() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(STOP_POLL).await;
        }
    }
}

/// The processes of running turns, by agent name.
#[derive(Debug, Default)]
struct TurnProcesses {
    running: Mutex<HashMap<String, Arc<TurnProcess>>>,
}

impl TurnProcesses {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TurnProcess>>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, agent: &str) -> Option<Arc<TurnProcess>> {
        self.map().get(agent).cloned()
    }

    fn insert(&self, agent: &str, process: Arc<TurnProcess>) {
        self.map().insert(agent.to_string(), process);
    }

    /// Forget `process` as `agent`'s, unless another one replaced it.
    fn remove(&self, agent: &str, process: &Arc<TurnProcess>) {
        let mut map = self.map();
        if map.get(agent).is_some_and(|p| Arc::ptr_eq(p, process)) {
            map.remove(agent);
        }
    }
}

/// Stop `process` on a task of `runtime`, and forget it once it is gone:
/// a process nobody reads any more (its turn was dropped, or stopped before
/// the process was registered). It stays registered until then, so a
/// `--stop` and the next turn wait for it.
fn stop_detached(
    runtime: &tokio::runtime::Handle,
    processes: &Arc<TurnProcesses>,
    agent: &str,
    process: &Arc<TurnProcess>,
    graces: [Duration; 3],
) {
    let processes = Arc::clone(processes);
    let process = Arc::clone(process);
    let agent = agent.to_string();
    runtime.spawn(async move {
        if let Err(e) = process.stop(&graces).await {
            warn!("agent '{agent}': stopping a turn nobody reads: {e}");
        }
        processes.remove(&agent, &process);
    });
}

/// Keeps a turn's process registered while the turn runs. Dropped with the
/// turn: a process that still runs then (the turn's task was aborted) is
/// stopped on a task of its own ([`stop_detached`]).
struct Registered {
    processes: Arc<TurnProcesses>,
    agent: String,
    process: Arc<TurnProcess>,
    graces: [Duration; 3],
}

impl Drop for Registered {
    fn drop(&mut self) {
        if self.process.exited() {
            self.processes.remove(&self.agent, &self.process);
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => stop_detached(
                &runtime,
                &self.processes,
                &self.agent,
                &self.process,
                self.graces,
            ),
            // No runtime left to wait on: interrupt it, as the daemon's exit
            // does (PR_SET_PDEATHSIG).
            Err(_) => signal_group(self.process.pid, libc::SIGINT),
        }
    }
}

/// A spawned turn process.
struct Spawned {
    /// As registered for a stop.
    process: Arc<TurnProcess>,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    exit: watch::Receiver<Option<Exit>>,
}

/// In the child, between fork and exec: get SIGINT when the thread that
/// spawned it ends (it waits for the child, so only with the daemon), and
/// fail if the daemon is already gone. Only async-signal-safe calls. Kept
/// across the exec of `systemd-run` and its exec of the harness (neither is
/// set-user-ID).
fn interrupt_with_parent(parent: u32) -> std::io::Result<()> {
    // SAFETY: prctl(PR_SET_PDEATHSIG) takes a signal number and changes
    // only this process.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } as u32 != parent {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// What a turn's process is started with, besides its [`Launch`].
struct SpawnSpec<'a> {
    wrap: &'a Wrap,
    /// Removes the daemon's variable from the child.
    strips: &'a (dyn Fn(&str) -> bool + Sync),
    processes: &'a Arc<TurnProcesses>,
    updates: &'a TurnUpdates,
    /// A session to report once the process runs ([`Launch::session`]).
    report: Option<SessionRef>,
    graces: [Duration; 3],
}

/// Spawn `launch` for `agent` (see the module doc) from a thread that
/// registers the child in `processes` (before the turn hears of it, so a
/// turn dropped meanwhile leaves it to a stop), reports its session or
/// learns that the turn was stopped meanwhile (then, or when nobody reads
/// it, it stops the child itself, [`stop_detached`]), writes the prompt,
/// waits for the child and reports its exit.
async fn spawn(agent: &Agent, launch: &Launch, spec: SpawnSpec<'_>) -> std::io::Result<Spawned> {
    let (mut cmd, scope) = match spec.wrap {
        Wrap::Scope { unit, no_expand } => {
            let mut cmd = std::process::Command::new("systemd-run");
            cmd.args(claude::scope_args(
                Some(unit),
                *no_expand,
                &agent.command,
                &launch.args,
            ));
            (cmd, Some(unit.clone()))
        }
        Wrap::Direct => {
            let mut cmd = std::process::Command::new(&agent.command);
            cmd.args(&launch.args);
            (cmd, None)
        }
    };
    cmd.current_dir(&agent.dir)
        .stdin(if launch.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if (spec.strips)(name) {
                cmd.env_remove(name);
            }
        }
    }
    cmd.env("PWD", &agent.dir);
    for (name, value) in &launch.env {
        cmd.env(name, value);
    }
    let parent = std::process::id();
    // SAFETY: the closure only makes async-signal-safe calls.
    unsafe {
        cmd.pre_exec(move || interrupt_with_parent(parent));
    }
    type Pipes = (
        Arc<TurnProcess>,
        Option<std::process::ChildStdout>,
        Option<std::process::ChildStderr>,
    );
    let (ready_tx, ready_rx) = oneshot::channel::<std::io::Result<Pipes>>();
    let (exit_tx, exit_rx) = watch::channel(None);
    let prompt = launch.stdin.clone();
    let name = agent.name.clone();
    let registry = Arc::clone(spec.processes);
    let updates = spec.updates.clone();
    let report = spec.report;
    let graces = spec.graces;
    let runtime = tokio::runtime::Handle::current();
    let exit_seen = exit_rx.clone();
    std::thread::Builder::new()
        .name(format!("agent-{name}"))
        .spawn(move || {
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let pid = child.id();
            let process = Arc::new(TurnProcess::new(pid, scope.map(Scope::new), exit_seen));
            registry.insert(&name, Arc::clone(&process));
            // In one step with the runtime's stop (`TurnUpdates::stop`):
            // either that stop finds the process registered, or this sees
            // the stop.
            let stopped = match report {
                Some(session) => updates.report_session(session),
                None => updates.stopped(),
            };
            let stdin = child.stdin.take();
            let pipes = (
                Arc::clone(&process),
                child.stdout.take(),
                child.stderr.take(),
            );
            let unread = ready_tx.send(Ok(pipes)).is_err();
            if stopped || unread {
                info!(
                    "agent '{name}': the turn was {} as its process {pid} started; stopping it",
                    if stopped { "stopped" } else { "dropped" }
                );
                // Its stdin closes unwritten: the prompt never reaches it.
                drop(stdin);
                stop_detached(&runtime, &registry, &name, &process, graces);
            } else if let (Some(mut stdin), Some(prompt)) = (stdin, prompt) {
                if let Err(e) = stdin.write_all(prompt.as_bytes()) {
                    debug!("turn process {pid}: the prompt was not written: {e}");
                }
            }
            let exit = match child.wait() {
                Ok(status) => Exit::Status(status),
                Err(e) => Exit::Lost(e.to_string()),
            };
            let _ = exit_tx.send(Some(exit));
        })?;
    let (process, stdout, stderr) = ready_rx
        .await
        .map_err(|_| std::io::Error::other("the thread that spawns the turn ended"))??;
    let missing = || std::io::Error::other("the turn process has no output pipe");
    Ok(Spawned {
        process,
        stdout: tokio::process::ChildStdout::from_std(stdout.ok_or_else(missing)?)?,
        stderr: tokio::process::ChildStderr::from_std(stderr.ok_or_else(missing)?)?,
        exit: exit_rx,
    })
}

/// One line at most [`LOG_LINE_MAX`] characters long, for the log.
fn for_log(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() > LOG_LINE_MAX {
        line.chars().take(LOG_LINE_MAX).collect::<String>() + "..."
    } else {
        line.to_string()
    }
}

/// One line of a turn's output, at most its reader's cap long.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    bytes: Vec<u8>,
    /// It was longer: the rest was dropped as it was read.
    cut: bool,
}

/// Reads lines, keeping at most `max` bytes of each: the rest of a longer
/// line is dropped as it arrives, never buffered. Cancel safe, as
/// `read_until` is: a line read in part stays here for the next call.
struct LineReader<R> {
    reader: R,
    max: usize,
    line: Vec<u8>,
    cut: bool,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    fn new(reader: R, max: usize) -> Self {
        Self {
            reader,
            max,
            line: Vec::new(),
            cut: false,
        }
    }

    /// The next line, with its newline when it fit; `None` at the end.
    async fn next(&mut self) -> std::io::Result<Option<Line>> {
        loop {
            // The only await: nothing is taken from the reader before it
            // returns, and what follows it is not interrupted.
            let buf = self.reader.fill_buf().await?;
            if buf.is_empty() {
                if self.line.is_empty() && !self.cut {
                    return Ok(None);
                }
                return Ok(Some(self.take()));
            }
            let (len, ends) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            let room = self.max.saturating_sub(self.line.len());
            if len > room {
                self.line.extend_from_slice(&buf[..room]);
                self.cut = true;
            } else {
                self.line.extend_from_slice(&buf[..len]);
            }
            self.reader.consume(len);
            if ends {
                return Ok(Some(self.take()));
            }
        }
    }

    fn take(&mut self) -> Line {
        Line {
            bytes: std::mem::take(&mut self.line),
            cut: std::mem::replace(&mut self.cut, false),
        }
    }
}

/// What reading a turn's process gave.
struct Read {
    state: StreamState,
    stderr: Vec<String>,
    exit: Exit,
    /// Lines on stdout, skipped ones included.
    stdout_lines: usize,
}

/// Read the turn's stdout and stderr until both end and the process
/// exited, or [`DRAIN_AFTER_EXIT`] after it exited.
async fn read_turn(
    harness: &dyn TurnHarness,
    agent: &str,
    stdout: impl AsyncRead + Unpin,
    stderr: impl AsyncRead + Unpin,
    mut exit: watch::Receiver<Option<Exit>>,
    state: StreamState,
    updates: &TurnUpdates,
) -> Read {
    let label = harness.label();
    let mut state = state;
    let mut stdout = LineReader::new(BufReader::new(stdout), STDOUT_LINE_MAX);
    let mut stderr = LineReader::new(BufReader::new(stderr), STDERR_LINE_MAX);
    let (mut out_done, mut err_done) = (false, false);
    let (mut out_cut, mut err_cut) = (false, false);
    let mut stdout_lines = 0;
    let mut errors: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut drain_until: Option<Instant> = exit
        .borrow()
        .is_some()
        .then(|| Instant::now() + DRAIN_AFTER_EXIT);
    loop {
        if out_done && err_done && drain_until.is_some() {
            break;
        }
        let drain = drain_until.unwrap_or_else(Instant::now);
        tokio::select! {
            read = stdout.next(), if !out_done => {
                match read {
                    Ok(None) | Err(_) => out_done = true,
                    Ok(Some(line)) if line.cut => {
                        stdout_lines += 1;
                        state.skipped_line();
                        if !out_cut {
                            out_cut = true;
                            warn!(
                                "agent '{agent}': {label} printed a line longer than {} KiB; \
                                 skipped (said once per turn)",
                                STDOUT_LINE_MAX >> 10
                            );
                        }
                    }
                    Ok(Some(line)) => {
                        stdout_lines += 1;
                        let line = String::from_utf8_lossy(&line.bytes);
                        debug!("agent '{agent}': {}", for_log(&line));
                        for event in harness.events(&line) {
                            state.event(event, updates);
                        }
                    }
                }
            }
            read = stderr.next(), if !err_done => {
                match read {
                    Ok(None) | Err(_) => err_done = true,
                    Ok(Some(line)) => {
                        if line.cut && !err_cut {
                            err_cut = true;
                            warn!(
                                "agent '{agent}': {label} printed a stderr line longer than {} \
                                 KiB; only its start is kept (said once per turn)",
                                STDERR_LINE_MAX >> 10
                            );
                        }
                        let line = String::from_utf8_lossy(&line.bytes);
                        debug!("agent '{agent}': stderr: {}", for_log(&strip_ansi(&line)));
                        if errors.len() == STDERR_LINES {
                            errors.pop_front();
                        }
                        errors.push_back(line.trim_end().to_string());
                    }
                }
            }
            // Ok once the exit is sent, Err when the thread ended without one.
            _ = exit.changed(), if drain_until.is_none() => {
                drain_until = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
            _ = tokio::time::sleep_until(drain), if drain_until.is_some() => break,
        }
    }
    state.end();
    let exit = exit
        .borrow()
        .clone()
        .unwrap_or_else(|| Exit::Lost("the thread that waits for it ended".to_string()));
    Read {
        state,
        stderr: errors.into(),
        exit,
        stdout_lines,
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// The [`Driver`] of a harness whose turn is one process.
pub struct ProcessDriver<H> {
    harness: H,
    processes: Arc<TurnProcesses>,
    scopes: Arc<TurnScopes>,
    graces: [Duration; 3],
}

impl<H: TurnHarness> ProcessDriver<H> {
    /// A driver whose turns run in `scopes` (shared by every such driver
    /// of the daemon, so a leftover is swept once).
    pub fn new(harness: H, scopes: Arc<TurnScopes>) -> Self {
        Self::with_graces(harness, scopes, STOP_GRACES)
    }

    /// [`ProcessDriver::new`] with the stop graces injected, for tests.
    fn with_graces(harness: H, scopes: Arc<TurnScopes>, graces: [Duration; 3]) -> Self {
        Self {
            harness,
            processes: Arc::new(TurnProcesses::default()),
            scopes,
            graces,
        }
    }

    /// Whether the daemon's variable `name` is removed from the child.
    fn strips_env(&self, name: &str) -> bool {
        claude::strips_env(name) || self.harness.strips_env(name)
    }

    /// Wait until a process of `agent` left from a stopped or dropped turn
    /// is gone: never two of one session at once.
    async fn wait_previous(&self, agent: &Agent) {
        if let Some(previous) = self.processes.get(&agent.name) {
            debug!(
                "agent '{}': waiting for the last turn's process to stop",
                agent.name
            );
            if let Err(e) = previous.stop(&self.graces).await {
                warn!("agent '{}': {}: {e}", agent.name, self.harness.label());
            }
            self.processes.remove(&agent.name, &previous);
        }
    }
}

#[async_trait]
impl<H: TurnHarness> Driver for ProcessDriver<H> {
    async fn run_turn(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        // No hooks: nothing is ever routed to a process-per-turn session.
        let TurnIo { hooks: _, updates } = io;
        let label = self.harness.label();
        if !agent.dir.is_dir() {
            return TurnOutcome::Failed(format!(
                "Agent '{}': its dir {} no longer exists.",
                agent.name,
                agent.dir.display()
            ));
        }
        // What a previous daemon's turn left running goes first.
        self.scopes.sweep().await;
        self.wait_previous(agent).await;
        let launch = match self.harness.launch(agent, &session, &request) {
            Ok(launch) => launch,
            Err(message) => return TurnOutcome::Failed(message),
        };
        debug!(
            "agent '{}': {} {}",
            agent.name,
            agent.command,
            launch.args.join(" ")
        );
        // A session the launch names that the agent does not have yet is
        // the agent's once its process runs, pending.
        let fresh = launch
            .session
            .as_ref()
            .filter(|id| session.session_id.as_deref() != Some(id.as_str()))
            .map(|id| SessionRef {
                session_id: Some(id.clone()),
                bg_id: None,
                transcript_path: None,
                pending: true,
            });
        let running = fresh.clone().unwrap_or_else(|| session.clone());
        let mut report = fresh;
        let mut wrap = self.scopes.wrap(&agent.name);
        loop {
            let strips = |name: &str| self.strips_env(name);
            let spec = SpawnSpec {
                wrap: &wrap,
                strips: &strips,
                processes: &self.processes,
                updates: &updates,
                report: report.clone(),
                graces: self.graces,
            };
            let spawned = match spawn(agent, &launch, spec).await {
                Ok(spawned) => spawned,
                Err(e) if wrap != Wrap::Direct && e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(
                        "agent '{}': systemd-run unavailable ({e}); starting {label} directly",
                        agent.name
                    );
                    wrap = Wrap::Direct;
                    continue;
                }
                Err(e) => {
                    // A start that never ran the harness leaves the session
                    // as it was.
                    if report.is_none() && running != session {
                        updates.send(TurnUpdate::Session(session.clone()));
                    }
                    return TurnOutcome::Failed(spawn_failure(
                        agent,
                        label,
                        &e,
                        agent.dir.is_dir(),
                    ));
                }
            };
            report = None;
            match &wrap {
                Wrap::Scope { unit, .. } => info!(
                    "agent '{}': {label} turn running (pid {}, in {unit})",
                    agent.name, spawned.process.pid
                ),
                Wrap::Direct => info!(
                    "agent '{}': {label} turn running (pid {})",
                    agent.name, spawned.process.pid
                ),
            }
            let _registered = Registered {
                processes: Arc::clone(&self.processes),
                agent: agent.name.clone(),
                process: Arc::clone(&spawned.process),
                graces: self.graces,
            };
            let read = read_turn(
                &self.harness,
                &agent.name,
                spawned.stdout,
                spawned.stderr,
                spawned.exit,
                StreamState::for_session(&running),
                &updates,
            )
            .await;
            if let Some(next) = wrap.after(&read) {
                let why = read.stderr.first().map(String::as_str).unwrap_or("").trim();
                if next == Wrap::Direct && why.contains("find executable") {
                    // No harness to run: the direct start says so.
                    debug!("agent '{}': {why}; starting {label} directly", agent.name);
                } else if next == Wrap::Direct {
                    self.scopes.note_unscoped(&agent.name, label, why);
                } else {
                    debug!(
                        "agent '{}': systemd-run has no --expand-environment (older than 254, \
                         which expands nothing); trying again without it",
                        agent.name
                    );
                }
                wrap = next;
                continue;
            }
            info!(
                "agent '{}': {label} exited with {} ({} text part(s))",
                agent.name,
                read.exit,
                read.state.texts.len()
            );
            return outcome(agent, &self.harness, &read.state, &read.stderr, &read.exit);
        }
    }

    /// The turn's process ended with the old daemon: it never replied. What
    /// it left in its scope is stopped first.
    async fn rewatch(
        &self,
        agent: &Agent,
        session: SessionRef,
        _request: TurnRequest,
        _io: TurnIo,
    ) -> TurnOutcome {
        self.scopes.sweep().await;
        TurnOutcome::Failed(cut_off(agent, self.harness.label(), &session))
    }

    /// Stop the turn's process and wait until it is gone (also one a
    /// dropped turn is stopping already).
    async fn stop(&self, agent: &Agent, _session: &SessionRef) -> Result<(), String> {
        let Some(process) = self.processes.get(&agent.name) else {
            return Ok(());
        };
        let stopped = process.stop(&self.graces).await;
        self.processes.remove(&agent.name, &process);
        stopped.map_err(|e| format!("{}: {e}", self.harness.label()))
    }

    /// Nothing is remembered between turns.
    fn forget(&self, _session: &SessionRef) {}

    /// No hooks.
    fn hook_matches(&self, _payload: &Value, _session: &SessionRef) -> bool {
        false
    }

    /// Between turns no process runs: idle.
    async fn session_phase(
        &self,
        _agent: &Agent,
        _session: &SessionRef,
    ) -> Result<AgentPhase, String> {
        Ok(AgentPhase::Idle)
    }

    async fn session_phases(
        &self,
        sessions: &[(Arc<Agent>, SessionRef)],
    ) -> Vec<Result<AgentPhase, String>> {
        vec![Ok(AgentPhase::Idle); sessions.len()]
    }

    /// No hooks.
    async fn idle_event(&self, _payload: &Value, _session: &SessionRef) -> IdleEvent {
        IdleEvent::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::progress::Feedback;
    use crate::agent::{Harness, Permissions};
    use std::path::Path;
    use tokio::sync::mpsc;

    /// A harness run by `sh -c <script>`, with a line protocol of its own:
    /// `S <id>` a session, `T <message> <text>` a text, `U <tool>` a tool
    /// call, `E <message>` an error; any other line is a text of message
    /// `raw`. A stderr line `gone` is a session it no longer has. With
    /// `session`, its launch names a session as OpenCode's does: the
    /// agent's, else this one.
    struct Fake {
        script: String,
        session: Option<String>,
    }

    impl TurnHarness for Fake {
        fn label(&self) -> &'static str {
            "Fake"
        }

        fn launch(
            &self,
            _agent: &Agent,
            session: &SessionRef,
            request: &TurnRequest,
        ) -> Result<Launch, String> {
            Ok(Launch {
                args: vec!["-c".into(), self.script.clone(), "fake".into()],
                stdin: Some(request.text.clone()),
                env: vec![("FAKE_SET".into(), "set by launch".into())],
                session: self
                    .session
                    .as_ref()
                    .map(|minted| session.session_id.clone().unwrap_or(minted.clone())),
            })
        }

        fn strips_env(&self, name: &str) -> bool {
            name == "FAKE_PARENT"
        }

        fn events(&self, line: &str) -> Vec<StreamEvent> {
            let line = line.trim_end();
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            vec![match kind {
                "S" => StreamEvent::Session(rest.to_string()),
                "T" => {
                    let (message, text) = rest.split_once(' ').unwrap_or((rest, ""));
                    StreamEvent::Text {
                        message: message.to_string(),
                        text: text.to_string(),
                    }
                }
                "U" => StreamEvent::Tool(rest.to_string()),
                "E" => StreamEvent::Error(rest.to_string()),
                _ => StreamEvent::Text {
                    message: "raw".to_string(),
                    text: line.to_string(),
                },
            }]
        }

        fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String> {
            stderr
                .iter()
                .any(|l| l == "gone")
                .then(|| format!("Agent '{}': its session is gone.", agent.name))
        }
    }

    fn harness() -> Fake {
        Fake {
            script: String::new(),
            session: None,
        }
    }

    fn agent(dir: &Path) -> Agent {
        Agent {
            name: "coder".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::OpenCode,
            dir: dir.to_path_buf(),
            permissions: Permissions::Auto,
            command: "/bin/sh".into(),
            args: Vec::new(),
            instruction: "Speak.".into(),
            feedback: Feedback::Brief,
        }
    }

    fn request(text: &str) -> TurnRequest {
        TurnRequest {
            text: text.to_string(),
            started_at: chrono::Utc::now(),
        }
    }

    fn turn_io() -> (TurnIo, mpsc::UnboundedReceiver<TurnUpdate>) {
        let (updates, rx) = TurnUpdates::channel();
        let (_hooks_tx, hooks) = mpsc::unbounded_channel();
        (TurnIo { hooks, updates }, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<TurnUpdate>) -> Vec<TurnUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    fn session(id: &str) -> TurnUpdate {
        TurnUpdate::Session(SessionRef {
            session_id: Some(id.to_string()),
            bg_id: None,
            transcript_path: None,
            pending: false,
        })
    }

    fn pending(id: &str) -> TurnUpdate {
        TurnUpdate::Session(SessionRef {
            session_id: Some(id.to_string()),
            bg_id: None,
            transcript_path: None,
            pending: true,
        })
    }

    fn text(message: &str, text: &str) -> StreamEvent {
        StreamEvent::Text {
            message: message.into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_session_id_is_reported_only_when_it_is_new() {
        let (updates, mut rx) = TurnUpdates::channel();
        // A resumed turn's own id: nothing to report.
        let mut state = StreamState::new(Some("ses_a".into()));
        state.event(StreamEvent::Session("ses_a".into()), &updates);
        assert!(drain(&mut rx).is_empty());
        // Another one (a harness that started a new session): reported once.
        state.event(StreamEvent::Session("ses_b".into()), &updates);
        state.event(StreamEvent::Session("ses_b".into()), &updates);
        assert_eq!(drain(&mut rx), [session("ses_b")]);
        // A new session's first line names it.
        let mut state = StreamState::new(None);
        state.event(StreamEvent::Session("ses_c".into()), &updates);
        assert_eq!(drain(&mut rx), [session("ses_c")]);
        assert_eq!(
            updates.latest().unwrap().session_id.as_deref(),
            Some("ses_c")
        );
    }

    #[test]
    fn tools_are_activities_and_release_the_text_before_them() {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::new(None);
        state.event(text("m1", "<spoken>On it.</spoken> Checking."), &updates);
        state.event(StreamEvent::Tool("shell".into()), &updates);
        // The same activity again is not sent twice; nothing held.
        state.event(StreamEvent::Tool("bash".into()), &updates);
        state.event(StreamEvent::Tool("read".into()), &updates);
        state.event(text("m2", "Done."), &updates);
        state.event(text("m2", "<spoken>All done.</spoken>"), &updates);
        state.end();
        assert_eq!(
            drain(&mut rx),
            [
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken> Checking.".into()),
                TurnUpdate::Activity("Reading files".into()),
            ]
        );
        // The reply is the last message's parts, joined.
        assert_eq!(
            state.reply().as_deref(),
            Some("Done.\n<spoken>All done.</spoken>")
        );
    }

    #[test]
    fn text_text_end_speaks_nothing_extra() {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::new(None);
        state.event(text("m1", "<spoken>Found it.</spoken>"), &updates);
        state.event(text("m2", "<spoken>Fixed it.</spoken>"), &updates);
        state.end();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(state.reply().as_deref(), Some("<spoken>Fixed it.</spoken>"));
        // No text, no reply.
        assert_eq!(StreamState::new(None).reply(), None);
        let mut blank = StreamState::new(None);
        blank.event(text("m1", "  "), &updates);
        assert_eq!(blank.reply(), None);
        // An empty last message is no reply, never the message before it.
        let mut empty_last = StreamState::new(None);
        empty_last.event(text("m1", "<spoken>On it.</spoken>"), &updates);
        empty_last.event(text("m2", ""), &updates);
        assert_eq!(empty_last.reply(), None);
    }

    /// A line too long to read after the last text may have been the final
    /// message: no reply, never the text before it. A text after the
    /// skipped line is the reply again.
    #[test]
    fn a_skipped_line_after_the_last_text_leaves_no_reply() {
        let (updates, _rx) = TurnUpdates::channel();
        let mut state = StreamState::new(None);
        state.skipped_line();
        state.event(text("m1", "<spoken>On it.</spoken>"), &updates);
        assert_eq!(state.reply().as_deref(), Some("<spoken>On it.</spoken>"));
        state.skipped_line();
        assert_eq!(state.reply(), None);
        state.event(text("m2", "Done."), &updates);
        assert_eq!(state.reply().as_deref(), Some("Done."));
    }

    fn status(code: i32) -> Exit {
        use std::os::unix::process::ExitStatusExt;
        Exit::Status(ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn the_outcome_prefers_an_error_then_the_reply_then_stderr() {
        let a = agent(Path::new("/proj"));
        let h = harness();
        let (updates, _rx) = TurnUpdates::channel();
        let mut replied = StreamState::new(None);
        replied.event(text("m", "Done. <spoken>Done.</spoken>"), &updates);
        assert_eq!(
            outcome(&a, &h, &replied, &[], &status(0)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        // A reply, then a non-zero exit: still the reply.
        assert_eq!(
            outcome(&a, &h, &replied, &[], &status(1)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        // An error the harness reported wins, on one line.
        let mut failed = StreamState::new(None);
        failed.event(text("m", "Partial."), &updates);
        failed.event(
            StreamEvent::Error("Could not connect.\n  Check the port".into()),
            &updates,
        );
        assert_eq!(
            outcome(&a, &h, &failed, &[], &status(1)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: Could not connect. Check the port.".into()
            )
        );
        let empty = StreamState::new(None);
        assert_eq!(
            outcome(&a, &h, &empty, &[], &status(0)),
            TurnOutcome::Failed("Agent 'coder': Fake ended the turn without a reply.".into())
        );
        let stderr = vec![
            "\u{1b}[93m\u{1b}[1m! \u{1b}[0mpermission requested: shell; auto-rejecting".into(),
            "error: unknown option --standalone".into(),
            "see --help".into(),
        ];
        assert_eq!(
            outcome(&a, &h, &empty, &stderr, &status(2)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: error: unknown option --standalone.".into()
            )
        );
        assert_eq!(
            outcome(&a, &h, &empty, &[], &status(3)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: it exited with exit status: 3.".into()
            )
        );
        // What the harness recognizes in stderr comes before the generic line,
        // but never before an error event or a reply.
        let gone = vec!["error: thread not found".into(), "gone".into()];
        assert_eq!(
            outcome(&a, &h, &empty, &gone, &status(1)),
            TurnOutcome::Failed("Agent 'coder': its session is gone.".into())
        );
        assert_eq!(
            outcome(&a, &h, &replied, &gone, &status(1)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        assert_eq!(
            outcome(&a, &h, &empty, &gone, &status(0)),
            TurnOutcome::Failed("Agent 'coder': Fake ended the turn without a reply.".into())
        );
    }

    /// A recoverable error fails the turn only while no text came after
    /// it; an error the harness reported fails it whatever came after.
    #[test]
    fn a_recoverable_error_fails_the_turn_only_without_a_later_text() {
        let a = agent(Path::new("/proj"));
        let h = harness();
        let (updates, _rx) = TurnUpdates::channel();
        let recoverable = || StreamEvent::RecoverableError("The model call failed".into());
        // The agent went on and replied: the reply.
        let mut recovered = StreamState::new(None);
        recovered.event(recoverable(), &updates);
        recovered.event(text("m", "Done."), &updates);
        assert_eq!(
            outcome(&a, &h, &recovered, &[], &status(0)),
            TurnOutcome::Reply("Done.".into())
        );
        // Nothing after it: it fails the turn, also over a text before it
        // (an acknowledgement, never the reply). A result line naming the
        // session after it changes nothing.
        let failed =
            TurnOutcome::Failed("Agent 'coder': Fake failed: The model call failed.".into());
        let mut after_ack = StreamState::new(None);
        after_ack.event(text("m1", "On it."), &updates);
        after_ack.event(StreamEvent::Tool("bash".into()), &updates);
        after_ack.event(recoverable(), &updates);
        after_ack.event(StreamEvent::Session("ses_a".into()), &updates);
        assert_eq!(outcome(&a, &h, &after_ack, &[], &status(0)), failed);
        let mut alone = StreamState::new(None);
        alone.event(recoverable(), &updates);
        assert_eq!(outcome(&a, &h, &alone, &[], &status(0)), failed);
        // A text clears only the recoverable one.
        let mut both = StreamState::new(None);
        both.event(StreamEvent::Error("Refused".into()), &updates);
        both.event(recoverable(), &updates);
        both.event(text("m", "Done."), &updates);
        assert_eq!(
            outcome(&a, &h, &both, &[], &status(1)),
            TurnOutcome::Failed("Agent 'coder': Fake failed: Refused.".into())
        );
    }

    #[test]
    fn stderr_details_skip_colors_and_prefer_an_error_line() {
        assert_eq!(
            stderr_detail(&[
                "\u{1b}[1mwarning: slow\u{1b}[0m".into(),
                " last line ".into()
            ]),
            Some("last line".into())
        );
        assert_eq!(stderr_detail(&["".into(), "  ".into()]), None);
        assert_eq!(
            strip_ansi("\u{1b}[93m\u{1b}[1m!\u{1b}[0m permission\u{1b}7"),
            "! permission"
        );
    }

    #[test]
    fn every_driver_strips_a_parent_sessions_variables_and_whisrs_keys() {
        let driver = ProcessDriver::new(harness(), Arc::new(TurnScopes::off()));
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "WHISRS_GROQ_API_KEY",
            "FAKE_PARENT",
        ] {
            assert!(driver.strips_env(name), "{name}");
        }
        for name in ["PATH", "HOME", "CLAUDE_CONFIG_DIR", "FAKE_SET"] {
            assert!(!driver.strips_env(name), "{name}");
        }
    }

    #[test]
    fn group_targets_never_name_the_daemon_or_everyone() {
        assert_eq!(group_target(0), None);
        assert_eq!(group_target(1), None);
        assert_eq!(group_target(u32::MAX), None);
        assert_eq!(group_target(4242), Some(-4242));
    }

    const TEST_GRACES: [Duration; 3] = [
        Duration::from_millis(600),
        Duration::from_millis(600),
        Duration::from_millis(1500),
    ];

    /// A driver of `script`, started directly (no scope): the process tree
    /// is all a stop has.
    fn fake(script: &str) -> Arc<ProcessDriver<Fake>> {
        fake_in(script, None, TurnScopes::off())
    }

    fn fake_in(
        script: &str,
        session: Option<&str>,
        scopes: TurnScopes,
    ) -> Arc<ProcessDriver<Fake>> {
        Arc::new(ProcessDriver::with_graces(
            Fake {
                script: script.to_string(),
                session: session.map(String::from),
            },
            Arc::new(scopes),
            TEST_GRACES,
        ))
    }

    /// A whole turn: the prompt on stdin (closed, so `cat` ends), the
    /// session reported, the activity and the interim, then the reply.
    #[tokio::test]
    async fn a_turn_reads_the_process_to_its_reply() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake(
            "read -r prompt; echo \"S ses_new\"; echo \"T m1 <spoken>On it.</spoken>\"; \
             echo \"U shell\"; echo \"T m2 You said: $prompt <spoken>Done.</spoken>\"",
        );
        let (io, mut rx) = turn_io();
        let outcome = driver
            .run_turn(
                &agent(dir.path()),
                SessionRef::default(),
                request("hello there"),
                io,
            )
            .await;
        assert_eq!(
            outcome,
            TurnOutcome::Reply("You said: hello there <spoken>Done.</spoken>".into())
        );
        assert_eq!(
            drain(&mut rx),
            [
                session("ses_new"),
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken>".into()),
            ]
        );
        // Nothing is left registered once the process exited.
        assert!(driver.processes.get("coder").is_none());
    }

    #[tokio::test]
    async fn the_child_runs_in_the_dir_with_pwd_and_without_stripped_variables() {
        let dir = tempfile::tempdir().unwrap();
        let driver =
            fake("echo \"pwd=$(pwd) PWD=$PWD set=$FAKE_SET home=${HOME:-none}\"; cat >/dev/null");
        let mut a = agent(dir.path());
        a.name = "env".into();
        let (io, _rx) = turn_io();
        // `HOME` stands in for a stripped variable: the test process has it.
        let launch = driver
            .harness
            .launch(&a, &SessionRef::default(), &request("x"))
            .unwrap();
        let strips = |name: &str| name == "HOME";
        let spawned = spawn(
            &a,
            &launch,
            SpawnSpec {
                wrap: &Wrap::Direct,
                strips: &strips,
                processes: &driver.processes,
                updates: &io.updates,
                report: None,
                graces: TEST_GRACES,
            },
        )
        .await
        .unwrap();
        let read = read_turn(
            &driver.harness,
            "env",
            spawned.stdout,
            spawned.stderr,
            spawned.exit,
            StreamState::new(None),
            &io.updates,
        )
        .await;
        let dir = dir.path();
        assert_eq!(
            read.state.reply().as_deref(),
            Some(format!("pwd={0} PWD={0} set=set by launch home=none", dir.display()).as_str())
        );
        assert!(read.exit.success(), "{}", read.exit);
    }

    #[tokio::test]
    async fn a_missing_dir_is_named() {
        let driver = fake("true");
        let gone = agent(Path::new("/nonexistent/dir"));
        let (io, _rx) = turn_io();
        let TurnOutcome::Failed(message) = driver
            .run_turn(&gone, SessionRef::default(), request("x"), io)
            .await
        else {
            panic!("a missing dir must fail the turn");
        };
        assert!(message.contains("no longer exists"), "{message}");
    }

    /// Wait until `path` holds a pid: the script got that far.
    async fn wait_for_pid(path: &Path) -> u32 {
        for _ in 0..200 {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the script never wrote {}", path.display());
    }

    fn alive(pid: u32) -> bool {
        // A zombie is gone for this purpose: reaped by whoever owns it.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| claude::stat_state(&stat))
            .is_some_and(|state| state != 'Z')
    }

    /// A turn of `driver` for `a` in a task of its own, as the runtime runs
    /// one.
    fn spawn_turn(
        driver: &Arc<ProcessDriver<Fake>>,
        a: &Arc<Agent>,
        io: TurnIo,
    ) -> tokio::task::JoinHandle<TurnOutcome> {
        let (driver, a) = (Arc::clone(driver), Arc::clone(a));
        tokio::spawn(async move {
            driver
                .run_turn(&a, SessionRef::default(), request("x"), io)
                .await
        })
    }

    /// Start a turn running `script` (it writes its background child's pid
    /// to `pid`), wait until it runs, then stop it as `--stop` does: abort
    /// the task, then `Driver::stop`. How long the stop took.
    async fn stop_after_start(script: &str, dir: &Path) -> (Duration, u32, u32) {
        let driver = fake(script);
        let a = Arc::new(agent(dir));
        let (io, _rx) = turn_io();
        let task = spawn_turn(&driver, &a, io);
        let child = wait_for_pid(&dir.join("pid")).await;
        let leader = driver
            .processes
            .get("coder")
            .unwrap_or_else(|| panic!("{script}: registered"))
            .pid;
        let started = Instant::now();
        task.abort();
        driver
            .stop(&a, &SessionRef::default())
            .await
            .unwrap_or_else(|e| panic!("{script}: the stop ends the group: {e}"));
        let took = started.elapsed();
        assert!(driver.processes.get("coder").is_none(), "{script}");
        (took, leader, child)
    }

    /// A stop sends the next signal a grace after the one before only while
    /// the harness ignores it. The rows stop at once, each its own turn.
    #[tokio::test]
    async fn a_stop_escalates_only_as_far_as_the_harness_ignores() {
        // (row, script, the stop took at least, and less than, in ms)
        let rows = [
            // Ignored by the script, and by its background job as by every
            // shell's: SIGINT did nothing for a grace, SIGTERM ended both.
            (
                "SIGINT ignored",
                "trap '' INT; sleep 30 & echo $! > pid; wait",
                600,
                1200 + 600,
            ),
            (
                "SIGINT and SIGTERM ignored",
                "trap '' INT TERM; sleep 30 & echo $! > pid; wait",
                1200,
                1200 + 1500,
            ),
            // Ended before the SIGTERM a first grace later. A shell's
            // background job ignores SIGINT, so the script execs.
            ("SIGINT handled", "echo $$ > pid; exec sleep 30", 0, 600),
        ];
        let stops = rows.map(|(row, script, ..)| {
            tokio::spawn(async move {
                let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{row}: {e}"));
                stop_after_start(script, dir.path()).await
            })
        });
        for ((row, _, at_least, less_than), stop) in rows.into_iter().zip(stops) {
            let (took, leader, child) = stop.await.unwrap_or_else(|e| panic!("{row}: {e}"));
            assert!(took >= Duration::from_millis(at_least), "{row}: {took:?}");
            assert!(took < Duration::from_millis(less_than), "{row}: {took:?}");
            assert!(!alive(leader) && !alive(child), "{row}");
            assert!(!group_alive(leader), "{row}");
        }
    }

    /// A turn dropped with no stop (its task aborted, as when a new turn
    /// replaces it) still has its process stopped.
    #[tokio::test]
    async fn a_dropped_turn_stops_its_process() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake("trap '' INT; sleep 30 & echo $! > pid; wait");
        let a = Arc::new(agent(dir.path()));
        let (io, _rx) = turn_io();
        let task = spawn_turn(&driver, &a, io);
        let child = wait_for_pid(&dir.path().join("pid")).await;
        task.abort();
        for _ in 0..100 {
            if driver.processes.get("coder").is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(driver.processes.get("coder").is_none());
        assert!(!alive(child));
    }

    #[tokio::test]
    async fn between_turns_nothing_runs_and_a_rewatch_ends_at_once() {
        let driver = fake("true");
        let a = Arc::new(agent(Path::new("/proj")));
        let s = SessionRef {
            session_id: Some("ses_x".into()),
            bg_id: None,
            transcript_path: None,
            pending: false,
        };
        assert_eq!(driver.session_phase(&a, &s).await, Ok(AgentPhase::Idle));
        assert_eq!(
            driver.session_phases(&[(Arc::clone(&a), s.clone())]).await,
            vec![Ok(AgentPhase::Idle)]
        );
        let payload = serde_json::json!({"session_id": "ses_x"});
        assert!(!driver.hook_matches(&payload, &s));
        assert_eq!(driver.idle_event(&payload, &s).await, IdleEvent::Ignored);
        let (io, _rx) = turn_io();
        let TurnOutcome::Failed(message) = driver.rewatch(&a, s.clone(), request(""), io).await
        else {
            panic!("a rewatch ends the turn");
        };
        assert_eq!(message, cut_off(&a, "Fake", &s));
        assert!(
            message.contains("whisrs restarted during that turn")
                && message.ends_with("The session keeps what it did so far."),
            "{message}"
        );
        assert!(!message.contains('\u{2014}'));
        // A turn that may not have reached the harness says so.
        for unsure in [
            SessionRef::default(),
            SessionRef {
                pending: true,
                ..s.clone()
            },
        ] {
            let message = cut_off(&a, "Fake", &unsure);
            assert!(
                message.ends_with("It may not have reached Fake at all, so say it again."),
                "{message}"
            );
        }
    }

    /// Whether this machine can run a transient user scope
    /// (`systemd-run --user --scope`): the scope tests skip without one, as
    /// on CI.
    fn user_scopes() -> bool {
        std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Scopes with a prefix of their own, so a test only ever sweeps its own.
    fn test_scopes() -> TurnScopes {
        TurnScopes::with_prefix(
            Some(format!("whisrs-agent-test{}-", hex(&random_bytes::<4>()))),
            TEST_GRACES,
        )
    }

    /// A pid that is still a live process (not a zombie).
    fn alive_pid(path: &Path) -> Option<u32> {
        let pid = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
        alive(pid).then_some(pid)
    }

    /// A process that left the turn's group into a session of its own
    /// (`setsid`, as Copilot's tool shells do) and ignores SIGINT is found
    /// through its parent, kept once seen, and ended by the SIGTERM after
    /// the parent is gone: the tree, with no scope.
    #[tokio::test]
    async fn a_stop_reaches_a_process_that_left_the_group() {
        // `setsid` is util-linux, not in every build sandbox (the Nix one).
        // Without it the script's child exits at once and proves nothing.
        if std::process::Command::new("setsid")
            .arg("true")
            .status()
            .is_err()
        {
            eprintln!("skipped: no setsid here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = "setsid sleep 41 & echo $! > pid; sleep 30";
        let (took, leader, child) = stop_after_start(script, dir.path()).await;
        assert!(!alive(leader), "the leader is gone");
        assert!(!alive(child), "the setsid child {child} is gone");
        // SIGINT ended the shell; the child ignored it until the SIGTERM.
        assert!(took >= Duration::from_millis(600), "{took:?}");
    }

    /// In a scope, a stop also reaches a process whose parent exited before
    /// the stop, out of reach of any tree walk.
    #[tokio::test]
    async fn a_stop_reaches_an_orphan_through_the_scope() {
        if !user_scopes() {
            eprintln!("skipped: no systemd user scopes here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let scopes = test_scopes();
        let driver = fake_in("(setsid sleep 42 & echo $! > pid); sleep 30", None, scopes);
        let a = Arc::new(agent(dir.path()));
        let (io, _rx) = turn_io();
        let task = spawn_turn(&driver, &a, io);
        let orphan = wait_for_pid(&dir.path().join("pid")).await;
        let process = driver.processes.get("coder").expect("registered");
        let unit = process.scope.as_ref().expect("in a scope").unit.clone();
        // Not the leader's child: its subshell exited.
        let stat = std::fs::read_to_string(format!("/proc/{orphan}/stat")).unwrap();
        let ppid: u32 = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(ppid, process.pid);
        task.abort();
        driver
            .stop(&a, &SessionRef::default())
            .await
            .expect("the stop ends the scope");
        assert!(!alive(orphan), "the orphan {orphan} is gone");
        assert!(!alive(process.pid));
        let listed = systemctl(&["list-units", "--all", "--plain", "--no-legend", &unit])
            .await
            .unwrap_or_default();
        assert!(listed_units(&listed, &unit).is_empty(), "{listed}");
    }

    /// What a previous daemon's turn left in its scope (a process that
    /// ignored the SIGINT the daemon's end sent) is stopped by the sweep,
    /// once, before any turn; another daemon's scope is not touched.
    #[tokio::test]
    async fn the_sweep_stops_what_a_previous_daemon_left() {
        if !user_scopes() {
            eprintln!("skipped: no systemd user scopes here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let scopes = test_scopes();
        let other = test_scopes();
        let prefix = scopes.prefix.clone().unwrap();
        let start = |unit: String, pid: &str| {
            let pid = dir.path().join(pid);
            std::process::Command::new("systemd-run")
                .args(claude::scope_args(
                    Some(&unit),
                    true,
                    "sh",
                    &[
                        "-c".to_string(),
                        format!(
                            "trap '' INT; (setsid sleep 43 & echo $! > {}); sleep 43",
                            pid.display()
                        ),
                    ],
                ))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut left = start(format!("{prefix}coder-0000aaaa.scope"), "left");
        let other_unit = format!("{}coder-0000bbbb.scope", other.prefix.clone().unwrap());
        let mut kept = start(other_unit.clone(), "kept");
        let left_pid = wait_for_pid(&dir.path().join("left")).await;
        let kept_pid = wait_for_pid(&dir.path().join("kept")).await;
        let started = Instant::now();
        scopes.sweep().await;
        // SIGINT ignored by both: the SIGTERM a grace later ended them.
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert!(!alive(left_pid));
        let _ = left.wait();
        assert!(
            alive_pid(&dir.path().join("kept")).is_some(),
            "another daemon's scope is left alone"
        );
        // Once: a second sweep (a turn's) does nothing.
        let again = Instant::now();
        scopes.sweep().await;
        assert!(again.elapsed() < Duration::from_millis(300));
        // Clean up the other one.
        Scope::new(other_unit)
            .stop_leftover(&TEST_GRACES)
            .await
            .unwrap();
        assert!(!alive(kept_pid));
        let _ = kept.wait();
    }

    /// A turn stopped before its process was registered (`--stop` found
    /// nothing to stop): the spawn sees the stop, never writes the prompt,
    /// and stops the process itself, which stays registered until it is
    /// gone.
    #[tokio::test]
    async fn a_process_started_after_the_stop_is_stopped_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake("read -r said; echo \"T m heard:$said\"; sleep 30");
        let a = agent(dir.path());
        let (io, _rx) = turn_io();
        // The runtime's stop came first: nothing registered for it to find.
        io.updates.stop();
        assert_eq!(driver.stop(&a, &SessionRef::default()).await, Ok(()));
        let started = Instant::now();
        let outcome = driver
            .run_turn(&a, SessionRef::default(), request("delete it"), io)
            .await;
        // Not the 30 s of its sleep: it was stopped.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            !matches!(&outcome, TurnOutcome::Reply(r) if r.contains("delete it")),
            "the prompt reached it: {outcome:?}"
        );
        for _ in 0..100 {
            if driver.processes.get("coder").is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(driver.processes.get("coder").is_none());
    }

    /// A session the launch names is the agent's as soon as the process
    /// runs, pending: a turn stopped before its first line keeps it.
    #[tokio::test]
    async fn a_minted_session_is_reported_at_spawn_pending() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake_in(
            "echo $$ > pid; sleep 30",
            Some("ses_new"),
            TurnScopes::off(),
        );
        let a = Arc::new(agent(dir.path()));
        let (io, mut rx) = turn_io();
        let updates = io.updates.clone();
        let task = spawn_turn(&driver, &a, io);
        wait_for_pid(&dir.path().join("pid")).await;
        // What `--stop` reads: the pending id, though no line named it.
        let latest = updates.stop().expect("reported at spawn");
        assert_eq!(latest.session_id.as_deref(), Some("ses_new"));
        assert!(latest.pending);
        task.abort();
        driver.stop(&a, &SessionRef::default()).await.unwrap();
        assert_eq!(drain(&mut rx), [pending("ses_new")]);
    }

    /// A pending session stays pending through a turn that only failed
    /// (nothing shows the agent working in it), and is confirmed by the
    /// first text or tool call of a later one.
    #[tokio::test]
    async fn only_a_text_or_a_tool_confirms_a_pending_session() {
        let dir = tempfile::tempdir().unwrap();
        let failed = fake_in(
            "echo \"S ses_new\"; echo \"E Model unavailable\"; exit 1",
            Some("ses_new"),
            TurnScopes::off(),
        );
        let (io, mut rx) = turn_io();
        let outcome = failed
            .run_turn(&agent(dir.path()), SessionRef::default(), request("x"), io)
            .await;
        assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
        assert_eq!(drain(&mut rx), [pending("ses_new")]);
        let stored = SessionRef {
            session_id: Some("ses_new".into()),
            pending: true,
            ..SessionRef::default()
        };
        // The next turn: the same id, still pending until the text.
        let replied = fake_in(
            "echo \"S ses_new\"; echo \"T m done\"",
            Some("ses_other"),
            TurnScopes::off(),
        );
        let (io, mut rx) = turn_io();
        let outcome = replied
            .run_turn(&agent(dir.path()), stored, request("x"), io)
            .await;
        assert_eq!(outcome, TurnOutcome::Reply("done".into()));
        assert_eq!(drain(&mut rx), [session("ses_new")]);
    }

    /// A start that never ran the harness (not found) names the command and
    /// leaves the session as it was: nothing to continue.
    #[tokio::test]
    async fn a_harness_that_never_ran_leaves_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake_in("true", Some("ses_new"), TurnScopes::off());
        let mut a = agent(dir.path());
        a.command = "/nonexistent/fake-harness".into();
        let (io, mut rx) = turn_io();
        let TurnOutcome::Failed(message) = driver
            .run_turn(&a, SessionRef::default(), request("x"), io)
            .await
        else {
            panic!("a missing command fails the turn");
        };
        assert!(
            message.contains("`/nonexistent/fake-harness` was not found. Install Fake"),
            "{message}"
        );
        assert!(drain(&mut rx).is_empty());
        // In a scope, systemd-run runs but finds no harness: it is started
        // directly, and the session reported for the scope is taken back.
        if user_scopes() {
            let driver = fake_in("true", Some("ses_new"), test_scopes());
            let (io, mut rx) = turn_io();
            let TurnOutcome::Failed(message) = driver
                .run_turn(&a, SessionRef::default(), request("x"), io)
                .await
            else {
                panic!("a missing command fails the turn");
            };
            assert!(message.contains("was not found"), "{message}");
            assert_eq!(
                drain(&mut rx),
                [
                    pending("ses_new"),
                    TurnUpdate::Session(SessionRef::default())
                ]
            );
        }
    }

    /// In a scope, the turn is the same: the prompt in, the session, the
    /// reply out, every variable as in a direct start.
    #[tokio::test]
    async fn a_turn_in_a_scope_reads_like_a_direct_one() {
        if !user_scopes() {
            eprintln!("skipped: no systemd user scopes here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let driver = fake_in(
            "read -r said; echo \"S ses_s\"; echo \"T m $said / $FAKE_SET / ${PWD} / $(cat /proc/self/cgroup)\"",
            None,
            test_scopes(),
        );
        let (io, mut rx) = turn_io();
        let outcome = driver
            .run_turn(
                &agent(dir.path()),
                SessionRef::default(),
                request("cost is $HOME"),
                io,
            )
            .await;
        let TurnOutcome::Reply(reply) = outcome else {
            panic!("{outcome:?}");
        };
        // The prompt arrives as said: nothing expanded it.
        assert!(
            reply.starts_with("cost is $HOME / set by launch / "),
            "{reply}"
        );
        assert!(reply.contains(&dir.path().display().to_string()), "{reply}");
        assert!(
            reply.contains("/whisrs-agent-test") && reply.ends_with(".scope"),
            "{reply}"
        );
        assert_eq!(drain(&mut rx), [session("ses_s")]);
    }

    /// The scope's command line is not expanded either.
    #[test]
    fn scope_wraps_keep_the_command_line_as_written() {
        let scopes = test_scopes();
        let Wrap::Scope { unit, no_expand } = scopes.wrap("coder") else {
            panic!("a scope");
        };
        assert!(no_expand);
        assert!(unit.starts_with(scopes.prefix.as_deref().unwrap()));
        assert!(
            unit.ends_with(".scope") && unit.contains("-coder-"),
            "{unit}"
        );
        assert_ne!(scopes.wrap("coder"), scopes.wrap("coder"));
        assert_eq!(TurnScopes::off().wrap("coder"), Wrap::Direct);
        // A long name is cut, so the unit name stays valid.
        let Wrap::Scope { unit, .. } = scopes.wrap(&"a".repeat(300)) else {
            panic!("a scope");
        };
        assert!(unit.len() < 120, "{unit}");
    }

    #[test]
    fn scope_prefixes_name_the_daemon_by_its_store() {
        let a = scope_prefix(Path::new("/home/u/.local/share/whisrs/agents.json"));
        let b = scope_prefix(Path::new("/tmp/test/xdg/data/whisrs/agents.json"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            scope_prefix(Path::new("/home/u/.local/share/whisrs/agents.json"))
        );
        assert!(
            a.starts_with("whisrs-agent-") && a.ends_with('-') && a.len() == 22,
            "{a}"
        );
    }

    #[test]
    fn only_this_daemons_running_scopes_are_listed() {
        let p = "whisrs-agent-0badcafe-";
        let listing = "\
whisrs-agent-0badcafe-coder-1a2b3c4d.scope loaded active running [systemd-run] /usr/bin/opencode run
whisrs-agent-0badcafe-cx-5e6f7a8b.scope loaded inactive dead [systemd-run] codex
whisrs-agent-0badcafe-cp-00000000.scope loaded failed failed [systemd-run] copilot
whisrs-agent-12345678-coder-1a2b3c4d.scope loaded active running [systemd-run] opencode
whisrs-agent-0badcafe-x.service loaded active running x
";
        assert_eq!(
            listed_units(listing, p),
            ["whisrs-agent-0badcafe-coder-1a2b3c4d.scope"]
        );
        assert!(listed_units("", p).is_empty());
    }

    /// What a start in a scope that never ran the harness tries next.
    #[test]
    fn a_scope_that_never_ran_falls_back() {
        let wrap = Wrap::Scope {
            unit: "u.scope".into(),
            no_expand: true,
        };
        let read = |stderr: &str, lines: usize, code: i32| Read {
            state: StreamState::new(None),
            stderr: stderr.lines().map(String::from).collect(),
            exit: status(code),
            stdout_lines: lines,
        };
        assert_eq!(
            wrap.after(&read(
                "systemd-run: unrecognized option '--expand-environment=no'",
                0,
                1
            )),
            Some(Wrap::Scope {
                unit: "u.scope".into(),
                no_expand: false
            })
        );
        for stderr in [
            "Failed to connect to user scope bus via local transport: No such file or directory",
            "Failed to start transient scope unit: Access denied",
            "Failed to find executable opencode: No such file or directory",
        ] {
            assert_eq!(
                wrap.after(&read(stderr, 0, 1)),
                Some(Wrap::Direct),
                "{stderr}"
            );
        }
        // The harness ran: its own failure is the turn's.
        assert_eq!(wrap.after(&read("Failed to connect to bus", 1, 1)), None);
        assert_eq!(wrap.after(&read("error: unknown option --x", 0, 2)), None);
        assert_eq!(wrap.after(&read("", 0, 0)), None);
        assert_eq!(
            Wrap::Direct.after(&read("Failed to start transient scope unit: x", 0, 1)),
            None
        );
    }

    /// A line past the cap is never kept whole: a long stdout line is
    /// skipped and the turn goes on; a long stderr line keeps its start. A
    /// skipped line after the last text may have been the final message:
    /// the turn then has no reply, never the text before it.
    #[tokio::test]
    async fn an_oversized_line_is_skipped_or_cut() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake(
            "head -c 2500000 /dev/zero | tr '\\0' 'x'; echo; echo \"T m after the long line\"; \
             head -c 20000 /dev/zero | tr '\\0' 'e' >&2; echo >&2; echo 'error: the last line' >&2; \
             exit 1",
        );
        let (io, _rx) = turn_io();
        let outcome = driver
            .run_turn(&agent(dir.path()), SessionRef::default(), request("x"), io)
            .await;
        assert_eq!(outcome, TurnOutcome::Reply("after the long line".into()));
        let driver = fake(
            "echo \"T m1 <spoken>On it.</spoken>\"; echo 'U shell'; \
             head -c 2500000 /dev/zero | tr '\\0' 'x'; echo",
        );
        let (io, mut rx) = turn_io();
        let outcome = driver
            .run_turn(&agent(dir.path()), SessionRef::default(), request("x"), io)
            .await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed("Agent 'coder': Fake ended the turn without a reply.".into())
        );
        // The acknowledgement was spoken once, as the interim.
        assert!(drain(&mut rx).contains(&TurnUpdate::Interim("<spoken>On it.</spoken>".into())));
        // Directly: the stderr kept, cut.
        let mut reader = LineReader::new(BufReader::new(&b"abcdefgh\nij\nklmnopqrstu"[..]), 4);
        let mut lines = Vec::new();
        while let Some(line) = reader.next().await.unwrap() {
            lines.push((String::from_utf8(line.bytes).unwrap(), line.cut));
        }
        assert_eq!(
            lines,
            [
                ("abcd".to_string(), true),
                ("ij\n".to_string(), false),
                ("klmn".to_string(), true)
            ]
        );
    }
}
