//! The Claude Code driver: each turn is a background session (`claude --bg`).
//!
//! - First turn: `claude --bg --name whisrs-<agent> --permission-mode <m>
//!   --append-system-prompt <instruction> --settings <hooks> [args] -- <text>`.
//!   `--bg` prints `backgrounded · <bg id>`; the bg id is reported (and
//!   persisted) at once, then the session id is read from `claude agents
//!   --json --all` by that bg id (`--session-id` is ignored with `--bg`,
//!   measured on 2.1.301). The appended prompt is recorded on the first
//!   request and reused on every resume, so it is passed once.
//! - Next turn: refused while `claude agents` shows the session `working` (it
//!   runs on its own, and stopping it would lose that work). Otherwise
//!   `claude stop <bg id>` while its process is alive, a wait until `claude
//!   agents` lists it with no pid and the process it listed has exited
//!   (`claude stop` returns, and the listing drops the pid, before Claude
//!   Code settles the session), then `claude --bg --resume <session id> --
//!   <text>`, which keeps the session id and the saved options (`--settings`
//!   and `--remote-control` included). Any other flag, or a resume while the
//!   old process still runs, starts a copy without them. Every pid a listing
//!   shows is remembered until it is seen to exit, so a session stopped
//!   elsewhere (listed with no pid while its process still exits) is waited
//!   for too. A copy is never adopted: it is stopped, waited for and
//!   removed (never the session's own bg id), and the resume is tried once
//!   more after the old process is gone; a second copy fails the turn.
//! - Turn end: the `Stop` hook (`whisrs agent-hook`, installed through
//!   `--settings` on whisrs-started sessions only) carries
//!   `last_assistant_message`. Only a `Stop` received after this turn's launch
//!   counts: the old process of a resumed session can still send one. A
//!   `Notification` of type `permission_prompt` marks the agent needs-you. A
//!   poll of `claude agents --json --all` every ten seconds covers a missing
//!   hook: `done` reads the last assistant text from the transcript (and a
//!   `done` with none for a few polls is an interrupted turn); a vanished or
//!   exited session is an error.
//! - Under a systemd service, `--bg` launches run in a scope of their own
//!   (`systemd-run --user --scope`): the first one can start Claude Code's
//!   background service, which hosts every background session, the user's
//!   own included, and must not die with the whisrs unit.
//!
//! Only ids this driver learned from sessions whisrs started are ever passed
//! to `claude`; nothing is matched by name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::claude_tail::{TailMode, TranscriptTail};
use super::{
    Agent, AgentPhase, Driver, HookEvent, IdleEvent, Permissions, SessionRef, TurnIo, TurnOutcome,
    TurnRequest, TurnUpdate, TurnUpdates,
};

/// How often a running turn is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// `claude --bg` can start the background service first.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(90);

/// `claude agents --json --all` and `claude stop`.
const QUICK_TIMEOUT: Duration = Duration::from_secs(20);

/// `claude agents --json --all` for `whisrs agent`, which waits on it, and
/// for each check of a stopped session.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a stopped session is listed until its process is gone.
const STOP_POLL: Duration = Duration::from_millis(250);

/// How long a stopped session's process may take to exit. `claude stop`
/// returns, and `claude agents` drops the pid, before the process exits and
/// Claude Code settles the session (measured on 2.1.301: up to two seconds
/// after the stop), and a resume that lands first makes it start a copy.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// How often the listed process of a stopped session is checked once the
/// listing no longer shows it.
const EXIT_POLL: Duration = Duration::from_millis(50);

/// How long after that process exited a resume waits, for Claude Code to
/// settle the session (logged within milliseconds of the exit, measured).
const SETTLE_GRACE: Duration = Duration::from_millis(250);

/// How long a copy a resume just started may take to show in `claude
/// agents`, listed every [`STOP_POLL`]: until it does, its process is not
/// known, and a wait would take the unlisted copy for removed.
const COPY_LIST_WAIT: Duration = Duration::from_secs(5);

/// Resumes tried before a turn that only ever starts copies is given up.
const RESUME_ATTEMPTS: u32 = 2;

/// The error for a turn whose session's last process was still running
/// [`STOP_WAIT`] after `claude stop`: resuming then would start a copy.
pub const STOP_TIMEOUT: &str =
    "Claude Code did not stop the session's last run in time, so this turn was not sent. \
     Try it again in a moment.";

/// How long a `done` poll waits for the `Stop` hook before reading the
/// transcript instead. Hook events keep being handled meanwhile.
const DONE_GRACE: Duration = Duration::from_secs(2);

/// `done` polls in a row with no assistant text since the turn started before
/// the turn is given up as interrupted (a restart that landed between the
/// stop and the resume, or a resume that never ran).
pub const DONE_POLLS_WITHOUT_TEXT: u32 = 3;

/// The error for a turn that ended without a reply of its own.
pub const INTERRUPTED: &str = "That turn was interrupted, say it again.";

/// The error for a turn a restart interrupted before its launch printed a bg
/// id. The launch runs in a scope of its own and may well have started the
/// session, which whisrs cannot find without matching sessions by name: it
/// never does, so it says so instead of inviting the instruction again.
pub const LOST_TRACK: &str =
    "whisrs lost track of that turn after a restart, and it may still be running. \
     `claude agents` lists it.";

/// Attempts to find a just-launched session in `claude agents`.
const LIST_ATTEMPTS: u32 = 10;

/// The hook timeout written into the settings, in seconds. `whisrs
/// agent-hook` exits within two.
const HOOK_TIMEOUT_SECS: u32 = 5;

/// The Claude Code driver.
pub struct ClaudeDriver {
    /// `'<whisrs>' agent-hook --socket '<socket>'`, run by the session's hooks.
    hook_command: String,
    poll_interval: Duration,
    done_grace: Duration,
    /// The processes of our sessions, and the waits for them to exit.
    exits: Exits,
    /// Launch `--bg` in a scope of its own ([`in_service_unit`]).
    use_scope: bool,
}

impl ClaudeDriver {
    /// A driver whose sessions report to the daemon listening on `socket`
    /// through the `whisrs` binary at `whisrs_bin`.
    pub fn new(whisrs_bin: &Path, socket: &Path) -> Self {
        Self {
            hook_command: hook_command(whisrs_bin, socket),
            poll_interval: POLL_INTERVAL,
            done_grace: DONE_GRACE,
            exits: Exits::new(
                STOP_POLL,
                STOP_WAIT,
                EXIT_POLL,
                SETTLE_GRACE,
                COPY_LIST_WAIT,
            ),
            use_scope: in_service_unit(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// `--permission-mode <mode>` for a harness-neutral setting. `ask` is passed
/// as `default` explicitly: without the flag a session starts in the user's
/// `permissions.defaultMode`, which may well be `auto`.
pub fn permission_args(permissions: Permissions) -> [&'static str; 2] {
    let mode = match permissions {
        Permissions::Auto => "auto",
        Permissions::Bypass => "bypassPermissions",
        Permissions::Ask => "default",
    };
    ["--permission-mode", mode]
}

/// Quote `s` for a POSIX shell (Claude Code runs hook commands through one).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@%".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The hook command line. The socket is explicit because the session runs
/// under Claude Code's background service, whose environment
/// (`XDG_RUNTIME_DIR` included) is not the daemon's.
pub fn hook_command(whisrs_bin: &Path, socket: &Path) -> String {
    format!(
        "{} agent-hook --socket {}",
        shell_quote(&whisrs_bin.to_string_lossy()),
        shell_quote(&socket.to_string_lossy())
    )
}

/// The `--settings` JSON: `Stop` and the `permission_prompt` notification run
/// the hook command. Only sessions whisrs starts get it; the user's own
/// settings files are untouched.
pub fn hook_settings(hook_command: &str) -> String {
    let hook = serde_json::json!([{
        "type": "command",
        "command": hook_command,
        "timeout": HOOK_TIMEOUT_SECS,
    }]);
    serde_json::json!({
        "hooks": {
            "Stop": [{ "hooks": hook }],
            "Notification": [{ "matcher": "permission_prompt", "hooks": hook }],
        }
    })
    .to_string()
}

/// The display name of an agent's sessions in `claude agents`. Display only:
/// sessions are never looked up by it.
pub fn session_name(agent: &Agent) -> String {
    format!("whisrs-{}", agent.name)
}

/// Arguments of a session's first turn. `--` keeps a prompt that starts with
/// a dash from being read as a flag.
pub fn first_turn_args(agent: &Agent, settings: &str, text: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["--bg".into(), "--name".into(), session_name(agent)];
    args.extend(permission_args(agent.permissions).map(String::from));
    args.push("--append-system-prompt".into());
    args.push(agent.instruction.clone());
    args.push("--settings".into());
    args.push(settings.to_string());
    args.extend(agent.args.iter().cloned());
    args.push("--".into());
    args.push(text.to_string());
    args
}

/// Arguments of a later turn. No other flag: the session keeps its saved
/// options, and any flag passed here makes Claude Code start a copy instead
/// of continuing in place (measured on 2.1.301), so `permissions`, `args` and
/// `reply_instruction` apply from the next new session on.
pub fn resume_args(session_id: &str, text: &str) -> Vec<String> {
    ["--bg", "--resume", session_id, "--", text]
        .map(String::from)
        .to_vec()
}

/// The bg id in `claude --bg`'s output (`backgrounded · 7c5dcf5d · name`).
pub fn parse_backgrounded(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("backgrounded")?;
        let rest = rest.trim_start().strip_prefix('\u{b7}')?;
        let id = rest.split_whitespace().next()?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .then(|| id.to_string())
    })
}

/// The bg id of the copy a resume says it started instead of continuing in
/// place (`note: session 1e897e6d is already running in the background, so
/// this started a copy as 9e24892c. ...`, measured on 2.1.301).
pub fn parse_copy_note(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, rest) = line.split_once("started a copy as ")?;
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        (!id.is_empty()).then_some(id)
    })
}

/// The bg id of the copy a resume started instead of continuing in place,
/// if it did: the id its copy note names ([`parse_copy_note`]), else the bg
/// id it printed when that is not the session's (`session_bg`). A copy comes
/// back without the session's saved options (the `--settings` hooks,
/// `--remote-control`, the permission mode), so it is never adopted, and it
/// is stopped and removed. Never the session's own bg id: a note that names
/// it, with that id printed, is taken as continued in place, since removing
/// that id would remove the session itself.
pub fn copy_id(
    session_bg: Option<&str>,
    printed_bg: &str,
    copy_note: Option<&str>,
) -> Option<String> {
    let not_the_session = |id: &&str| session_bg != Some(*id);
    copy_note
        .filter(not_the_session)
        .or_else(|| session_bg.and(Some(printed_bg)).filter(not_the_session))
        .map(str::to_string)
}

/// The error for a turn Claude Code would only run in a copy of the session.
pub fn cannot_continue(agent: &Agent) -> String {
    format!(
        "Agent '{}': Claude Code could not continue the session, so this turn was not sent. \
         `whisrs agent {} --new` starts a fresh one.",
        agent.name, agent.name
    )
}

/// One background session as `claude agents --json --all` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub session_id: Option<String>,
    /// `working`, `blocked`, `done`, `failed` or `stopped`. An exited
    /// session keeps the state it had last.
    pub state: Option<String>,
    /// Its process is running (`pid` present).
    pub alive: bool,
    /// That process. `claude agents` drops it at a stop before the process
    /// has exited.
    pub pid: Option<u32>,
    /// What a waiting session waits for (`permission prompt`, ...).
    pub waiting_for: Option<String>,
    /// `status` is `busy`: the session still runs something, a background
    /// task included. Measured on 2.1.301: a turn that started a background
    /// command and replied lists `state: done` with `status: busy` for as
    /// long as that command runs.
    pub busy: bool,
}

/// Parse `claude agents --json --all` output.
pub fn parse_agent_list(stdout: &str) -> Result<Vec<Value>, String> {
    let start = stdout
        .find('[')
        .ok_or_else(|| "`claude agents --json` printed no JSON list".to_string())?;
    serde_json::from_str::<Vec<Value>>(stdout[start..].trim_end())
        .map_err(|e| format!("`claude agents --json` printed invalid JSON: {e}"))
}

/// The background session with this bg id, if listed.
pub fn find_session(list: &[Value], bg_id: &str) -> Option<ListedSession> {
    let entry = list.iter().find(|e| {
        e.get("id").and_then(Value::as_str) == Some(bg_id)
            && e.get("kind").and_then(Value::as_str) != Some("interactive")
    })?;
    let text = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_string);
    Some(ListedSession {
        session_id: text("sessionId"),
        state: text("state"),
        alive: entry.get("pid").is_some_and(|p| !p.is_null()),
        pid: entry
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok()),
        waiting_for: text("waitingFor"),
        busy: entry.get("status").and_then(Value::as_str) == Some("busy"),
    })
}

/// What a poll of `claude agents` says about the turn's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollState {
    /// Not listed: removed (`claude rm`).
    Gone,
    /// Listed, but its process is not running (stopped, killed, crashed).
    /// `state` is what it was last; `done` means the turn finished first.
    Exited { state: Option<String> },
    /// Running a turn.
    Working,
    /// Waiting for the user.
    Blocked { waiting_for: Option<String> },
    /// Running and idle: the turn finished.
    Done,
    /// Running, in a state this driver does not know.
    Other,
}

/// What a session does while no whisrs turn runs: `Working` while its
/// process runs a turn of its own or anything else (`status: busy`: a
/// background task, after its turn replied with `state: done`), `NeedsYou`
/// while that work waits for a prompt, else `Idle`. A new turn is refused
/// while it works: stopping the session for it would kill that work.
pub fn idle_phase(listed: Option<&ListedSession>) -> AgentPhase {
    let Some(listed) = listed.filter(|l| l.alive) else {
        return AgentPhase::Idle;
    };
    match poll_state(Some(listed)) {
        PollState::Working => AgentPhase::Working,
        PollState::Blocked { .. } => AgentPhase::NeedsYou,
        PollState::Done | PollState::Other if listed.busy => AgentPhase::Working,
        _ => AgentPhase::Idle,
    }
}

/// Classify a listed session for a running turn. A running session in a
/// terminal state (`failed`, `stopped`) is treated as exited. `done` ends
/// the turn even while `status` is `busy` (a background task the turn
/// started runs on after its reply); see [`idle_phase`] for between turns.
pub fn poll_state(listed: Option<&ListedSession>) -> PollState {
    let Some(listed) = listed else {
        return PollState::Gone;
    };
    if !listed.alive {
        return PollState::Exited {
            state: listed.state.clone(),
        };
    }
    match listed.state.as_deref() {
        Some("working") => PollState::Working,
        Some("blocked") => PollState::Blocked {
            waiting_for: listed.waiting_for.clone(),
        },
        Some("done") => PollState::Done,
        Some("failed" | "stopped") => PollState::Exited {
            state: listed.state.clone(),
        },
        _ => PollState::Other,
    }
}

/// What a parent Claude Code session sets in the environment of the
/// processes it runs. Removed from the `claude` child, so a daemon started
/// from inside a session does not make the agent look like one of its
/// subprocesses. Everything else passes, Claude Code's own configuration
/// included (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`,
/// `CLAUDE_CODE_USE_*`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, ...).
pub const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_JOB_DIR",
    "AI_AGENT",
];

/// Whether an environment variable is kept from the `claude` child: a
/// parent session's variable ([`PARENT_SESSION_VARS`]), or one of whisrs's
/// own `WHISRS_*` keys (API key overrides): the first `claude --bg` can start
/// Claude Code's background service, which keeps the environment it was
/// started with for every session it hosts.
pub fn strips_env(name: &str) -> bool {
    PARENT_SESSION_VARS.contains(&name) || name.starts_with("WHISRS_")
}

/// The notification and error text for a failed launch.
pub fn launch_failure(agent: &Agent, stdout: &str, stderr: &str) -> String {
    if stderr.contains("Workspace not trusted") || stdout.contains("Workspace not trusted") {
        return format!(
            "Agent '{}': Claude Code does not trust this folder yet. Open {} in Claude Code once \
             and accept the trust prompt.",
            agent.name,
            agent.dir.display()
        );
    }
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("warning:") && !l.starts_with("note:"))
        .unwrap_or("no output");
    format!(
        "Agent '{}': `{} --bg` failed: {detail}",
        agent.name, agent.command
    )
}

/// The error for a binary that cannot be started. Spawning in a missing
/// working directory fails with "not found" too, so the dir is checked first
/// (`dir_exists`).
pub fn spawn_failure(agent: &Agent, error: &std::io::Error, dir_exists: bool) -> String {
    if !dir_exists {
        format!(
            "Agent '{}': its dir {} does not exist.",
            agent.name,
            agent.dir.display()
        )
    } else if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "Agent '{}': `{}` was not found. Install Claude Code, or set command in [[agents]] \
             to its full path (a systemd user service may not have ~/.local/bin on PATH).",
            agent.name, agent.command
        )
    } else {
        format!(
            "Agent '{}': cannot run `{}`: {error}",
            agent.name, agent.command
        )
    }
}

/// Whether a `/proc/self/cgroup` listing puts this process in a systemd
/// service (`.../whisrs.service`), as opposed to a terminal's scope.
pub fn cgroup_is_service(cgroup: &str) -> bool {
    cgroup.lines().any(|line| {
        line.trim_end()
            .rsplit('/')
            .next()
            .is_some_and(|leaf| leaf.ends_with(".service"))
    })
}

/// Whether this daemon runs as a systemd service. Then a `claude --bg` it
/// starts directly could start Claude Code's background service inside the
/// whisrs unit, and a restart of whisrs (`KillMode=control-group`) would
/// kill it with every session it hosts. Checked once.
fn in_service_unit() -> bool {
    static IN_SERVICE: OnceLock<bool> = OnceLock::new();
    *IN_SERVICE.get_or_init(|| {
        std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty())
            || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| cgroup_is_service(&c))
    })
}

/// `systemd-run` arguments that run `command args` in a transient scope of
/// its own, named `unit` when given. The scope keeps the environment, the
/// working directory and the output pipes (`systemd-run --scope` execs the
/// command itself, as the same process). `no_expand` adds
/// `--expand-environment=no`: since systemd 254, `--scope` expands `$NAME`
/// and `${NAME}` in the command line as a service's `ExecStart=` does
/// (measured on 262: `$HOME` arrived as the home directory), which would
/// change an instruction or the user's words. An older systemd-run has no
/// such option, expands nothing and refuses the option
/// ([`expand_option_refused`]), so the launch is tried again without it.
pub fn scope_args(
    unit: Option<&str>,
    no_expand: bool,
    command: &str,
    args: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = ["--user", "--scope", "--quiet", "--collect"]
        .map(String::from)
        .to_vec();
    if no_expand {
        out.push("--expand-environment=no".to_string());
    }
    if let Some(unit) = unit {
        out.push(format!("--unit={unit}"));
    }
    out.push("--".to_string());
    out.push(command.to_string());
    out.extend(args.iter().cloned());
    out
}

/// Whether a failed `systemd-run` refused `--expand-environment` (systemd
/// older than 254), so the command never ran.
pub fn expand_option_refused(success: bool, stdout: &str, stderr: &str) -> bool {
    !success
        && stdout.trim().is_empty()
        && stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .is_some_and(|first| {
                first.contains("unrecognized option") && first.contains("--expand-environment")
            })
}

/// Whether a failed `systemd-run --scope` failed itself (no user manager,
/// no executable), so the command never ran and can be started directly.
pub fn scope_unavailable(success: bool, stdout: &str, stderr: &str) -> bool {
    if success || !stdout.trim().is_empty() {
        return false;
    }
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|first| {
            first.starts_with("Failed to ")
                && ["bus", "transient", "scope", "executable"]
                    .iter()
                    .any(|w| first.contains(w))
        })
}

/// The state letter of a `/proc/<pid>/stat` line (`R`, `S`, `Z`, ...): the
/// field after the command name, which is in parentheses and may contain
/// spaces and parentheses itself.
pub fn stat_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// The start time of a `/proc/<pid>/stat` line (field 22, clock ticks since
/// boot). With the pid it names one process: a pid the kernel gave to
/// another process since has another start time.
pub fn stat_start(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// A process `claude agents` listed for one of our sessions: its pid and its
/// start time ([`stat_start`]), so that a pid an unrelated process got since
/// is not taken for it (it would hold up a turn for nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListedProcess {
    pid: u32,
    started: u64,
}

impl ListedProcess {
    /// Process `pid`, while it runs. A zombie (exited, not reaped yet) does
    /// not run.
    fn running_now(pid: u32) -> Option<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        if matches!(stat_state(&stat)?, 'Z' | 'X' | 'x') {
            return None;
        }
        Some(Self {
            pid,
            started: stat_start(&stat)?,
        })
    }

    /// Whether it still runs: its pid runs, and started when it did.
    fn running(&self) -> bool {
        Self::running_now(self.pid).is_some_and(|now| now.started == self.started)
    }
}

/// What a hook payload means to a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookKind {
    /// The turn ended.
    Stop {
        message: Option<String>,
        transcript_path: Option<PathBuf>,
    },
    /// A permission prompt waits.
    PermissionPrompt,
    /// Anything else (idle notifications and the like).
    Other,
}

pub fn classify_hook(payload: &Value) -> HookKind {
    let event = payload.get("hook_event_name").and_then(Value::as_str);
    match event {
        Some("Stop") => HookKind::Stop {
            message: payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .map(str::to_string),
            transcript_path: payload
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        },
        Some("Notification")
            if payload.get("notification_type").and_then(Value::as_str)
                == Some("permission_prompt") =>
        {
            HookKind::PermissionPrompt
        }
        _ => HookKind::Other,
    }
}

/// Whether a hook event belongs to the process this turn launched: it was
/// received at or after `accept_after` (the launch). `None` accepts every
/// event (a turn watched again after a restart).
pub fn is_current(event: &HookEvent, accept_after: Option<Instant>) -> bool {
    accept_after.is_none_or(|after| event.received >= after)
}

/// The text of the last assistant message in a transcript (JSONL) written at
/// or after `since`, skipping subagent (sidechain) messages. `None` when the
/// turn has no assistant text yet.
pub fn last_assistant_text(transcript: &str, since: DateTime<Utc>) -> Option<String> {
    let mut last = None;
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if at.is_none_or(|at| at < since) {
            continue;
        }
        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        let text: Vec<&str> = content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect();
        if !text.is_empty() {
            last = Some(text.join("\n"));
        }
    }
    last
}

/// The name of the last tool the assistant asked to use in a transcript.
pub fn last_tool_use(transcript: &str) -> Option<String> {
    let mut last = None;
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) {
            for c in content {
                if c.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(name) = c.get("name").and_then(Value::as_str) {
                        last = Some(name.to_string());
                    }
                }
            }
        }
    }
    last
}

/// The spoken notice for a permission prompt.
pub fn needs_you_notice(tool: Option<&str>) -> String {
    match tool {
        Some(tool) if spoken_tool_name(tool).is_some() => format!(
            "Waiting for your approval to use {}.",
            spoken_tool_name(tool).unwrap_or_default()
        ),
        _ => "Waiting for your approval.".to_string(),
    }
}

/// A tool name worth saying: plain words, not an MCP server's
/// `mcp__server__tool` id.
fn spoken_tool_name(tool: &str) -> Option<String> {
    if tool.starts_with("mcp__") || tool.is_empty() || tool.len() > 32 {
        return None;
    }
    tool.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ')
        .then(|| tool.to_string())
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

/// Find a session's transcript by its id: `<config>/projects/*/<id>.jsonl`.
/// Only our own session's file is ever opened. Blocking.
pub(super) fn find_transcript(session_id: &str) -> Option<PathBuf> {
    find_transcript_in(&claude_config_dir()?.join("projects"), session_id)
}

/// [`find_transcript`] under a given `projects` dir.
pub(super) fn find_transcript_in(projects: &Path, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(&file))
        .find(|p| p.is_file())
}

/// Read and parse the session's transcript off the async runtime: a long
/// session's JSONL is megabytes.
async fn with_transcript<T: Send + 'static>(
    session: &SessionRef,
    parse: impl FnOnce(&str) -> Option<T> + Send + 'static,
) -> Option<T> {
    let path = session.transcript_path.clone();
    let session_id = session.session_id.clone();
    tokio::task::spawn_blocking(move || {
        let path = path.or_else(|| session_id.as_deref().and_then(find_transcript))?;
        let contents = std::fs::read_to_string(path).ok()?;
        parse(&contents)
    })
    .await
    .ok()
    .flatten()
}

/// The last assistant text of this turn, from the transcript.
async fn transcript_reply(session: &SessionRef, since: DateTime<Utc>) -> Option<String> {
    with_transcript(session, move |t| last_assistant_text(t, since)).await
}

/// The needs-you notice, naming the tool when the transcript shows it.
async fn notice(session: &SessionRef) -> String {
    let tool = with_transcript(session, last_tool_use).await;
    needs_you_notice(tool.as_deref())
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

/// Captured output of a finished process.
struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Why a process gave no output.
enum ExecError {
    Spawn(std::io::Error),
    Wait(std::io::Error),
    Timeout,
}

/// Run `program args` in the agent's dir, stdin closed, the parent session's
/// variables removed, killed if dropped or past `timeout`.
async fn exec(
    agent: &Agent,
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<Output, ExecError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(&agent.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if strips_env(name) {
                cmd.env_remove(name);
            }
        }
    }
    let child = cmd.spawn().map_err(ExecError::Spawn)?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(ExecError::Wait(e)),
        Err(_) => Err(ExecError::Timeout),
    }
}

/// The message for an [`ExecError`] of the agent's `claude` run with `args`.
fn exec_failure(agent: &Agent, args: &[String], timeout: Duration, error: ExecError) -> String {
    match error {
        ExecError::Spawn(e) => spawn_failure(agent, &e, agent.dir.is_dir()),
        ExecError::Wait(e) => format!("Agent '{}': `{}` failed: {e}", agent.name, agent.command),
        ExecError::Timeout => format!(
            "Agent '{}': `{} {}` did not finish in {} seconds",
            agent.name,
            agent.command,
            args.first().map(String::as_str).unwrap_or(""),
            timeout.as_secs()
        ),
    }
}

/// Run the agent's `claude` with `args`.
async fn run_claude(agent: &Agent, args: &[String], timeout: Duration) -> Result<Output, String> {
    exec(agent, &agent.command, args, timeout)
        .await
        .map_err(|e| exec_failure(agent, args, timeout, e))
}

/// Run a `claude --bg` launch: in a scope of its own when `use_scope` (this
/// daemon is a systemd service, [`in_service_unit`]), directly otherwise or
/// when `systemd-run` is missing or cannot create the scope.
async fn run_launch(agent: &Agent, args: &[String], use_scope: bool) -> Result<Output, String> {
    if use_scope {
        for no_expand in [true, false] {
            let scoped = scope_args(None, no_expand, &agent.command, args);
            match exec(agent, "systemd-run", &scoped, LAUNCH_TIMEOUT).await {
                Ok(out)
                    if no_expand
                        && expand_option_refused(out.success, &out.stdout, &out.stderr) =>
                {
                    debug!(
                        "agent '{}': systemd-run has no --expand-environment (older than \
                         254, which expands nothing); launching without it",
                        agent.name
                    );
                }
                Ok(out) if !scope_unavailable(out.success, &out.stdout, &out.stderr) => {
                    debug!("agent '{}': launched in a scope of its own", agent.name);
                    return Ok(out);
                }
                Ok(out) => {
                    warn!(
                        "agent '{}': cannot start claude in a scope of its own ({}); starting \
                         it directly, so Claude Code's background service may run inside the \
                         whisrs unit",
                        agent.name,
                        out.stderr.lines().next().unwrap_or("").trim()
                    );
                    break;
                }
                Err(ExecError::Spawn(e)) => {
                    debug!(
                        "agent '{}': systemd-run unavailable ({e}); starting claude directly",
                        agent.name
                    );
                    break;
                }
                Err(e) => return Err(exec_failure(agent, args, LAUNCH_TIMEOUT, e)),
            }
        }
    }
    run_claude(agent, args, LAUNCH_TIMEOUT).await
}

/// `claude agents --json --all`, parsed.
async fn list_sessions(agent: &Agent) -> Result<Vec<Value>, String> {
    list_sessions_within(agent, QUICK_TIMEOUT).await
}

/// [`list_sessions`] with its own timeout.
async fn list_sessions_within(agent: &Agent, timeout: Duration) -> Result<Vec<Value>, String> {
    let args = ["agents", "--json", "--all"].map(String::from);
    let out = run_claude(agent, &args, timeout).await?;
    if !out.success {
        return Err(format!(
            "`claude agents --json --all` failed: {}",
            out.stderr.trim()
        ));
    }
    parse_agent_list(&out.stdout)
}

/// Look a just-launched bg id up until `claude agents` lists it.
async fn learn_session(agent: &Agent, bg_id: &str) -> Result<ListedSession, String> {
    let mut last_error = None;
    for attempt in 0..LIST_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match list_sessions(agent).await {
            Ok(list) => {
                if let Some(found) = find_session(&list, bg_id) {
                    if found.session_id.is_some() {
                        return Ok(found);
                    }
                }
            }
            Err(e) => last_error = Some(e),
        }
    }
    Err(format!(
        "Agent '{}': Claude Code started session {bg_id} but does not list it{}",
        agent.name,
        last_error.map(|e| format!(" ({e})")).unwrap_or_default()
    ))
}

/// What a `claude --bg` launch printed.
struct Launch {
    bg_id: String,
    /// The bg id its copy note names, when it said it started a copy
    /// ([`parse_copy_note`]).
    copy_note: Option<String>,
}

/// Launch `args` and return the bg id it printed.
async fn launch(agent: &Agent, args: &[String], use_scope: bool) -> Result<Launch, String> {
    let out = run_launch(agent, args, use_scope).await?;
    for line in out.stderr.lines().filter(|l| !l.trim().is_empty()) {
        info!("agent '{}': claude: {}", agent.name, line.trim());
    }
    if !out.success {
        return Err(launch_failure(agent, &out.stdout, &out.stderr));
    }
    let bg_id = parse_backgrounded(&out.stdout).ok_or_else(|| {
        format!(
            "Agent '{}': `{} --bg` printed no session id: {}",
            agent.name,
            agent.command,
            out.stdout.trim()
        )
    })?;
    let copy_note = parse_copy_note(&out.stderr).or_else(|| parse_copy_note(&out.stdout));
    Ok(Launch { bg_id, copy_note })
}

/// `claude stop <bg id>`.
async fn stop_bg(agent: &Agent, bg_id: &str) -> Result<(), String> {
    let args = ["stop".to_string(), bg_id.to_string()];
    let out = run_claude(agent, &args, QUICK_TIMEOUT).await?;
    if out.success {
        debug!("agent '{}': stopped bg {bg_id}", agent.name);
        Ok(())
    } else {
        Err(format!(
            "`claude stop {bg_id}` failed: {}",
            out.stderr.trim()
        ))
    }
}

/// What `claude agents` showed once a stopped session's process was gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    /// Listed with no process: a resume continues it in place.
    Exited,
    /// No longer listed (`claude rm`).
    Gone,
}

/// The processes `claude agents` listed for our sessions, remembered per bg
/// id until they are seen to exit, and the waits for them. `claude stop`
/// returns, and the listing drops the pid, before the process exits (up to
/// two seconds, measured on 2.1.301), and a resume before it is gone starts
/// a copy. A session stopped elsewhere (`whisrs agent <name> --stop`, a turn
/// stopped during its launch, a wait that ran out) is then listed with no pid
/// while its process still runs, so the remembered pid is what a resume
/// waits for.
#[derive(Debug, Clone)]
struct Exits {
    seen: Arc<Mutex<HashMap<String, Vec<ListedProcess>>>>,
    /// How often a stopped session is listed until its process is gone.
    stop_poll: Duration,
    /// How long that may take in all ([`STOP_WAIT`]).
    stop_wait: Duration,
    /// How often a remembered process is checked once the listing lost it.
    exit_poll: Duration,
    /// The grace after it exited, for Claude Code to settle the session.
    settle_grace: Duration,
    /// How long a just-started copy may take to be listed
    /// ([`COPY_LIST_WAIT`]).
    copy_list_wait: Duration,
}

impl Exits {
    fn new(
        stop_poll: Duration,
        stop_wait: Duration,
        exit_poll: Duration,
        settle_grace: Duration,
        copy_list_wait: Duration,
    ) -> Self {
        Self {
            seen: Arc::default(),
            stop_poll,
            stop_wait,
            exit_poll,
            settle_grace,
            copy_list_wait,
        }
    }

    fn seen(&self) -> MutexGuard<'_, HashMap<String, Vec<ListedProcess>>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember the process a listing shows for `bg_id` (with its start
    /// time, read now), and forget those of it that exited.
    fn note(&self, bg_id: &str, listed: &ListedSession) {
        let Some(pid) = listed.pid else {
            return;
        };
        let mut seen = self.seen();
        let processes = seen.entry(bg_id.to_string()).or_default();
        processes.retain(ListedProcess::running);
        if !processes.iter().any(|p| p.pid == pid) {
            processes.extend(ListedProcess::running_now(pid));
        }
        if processes.is_empty() {
            seen.remove(bg_id);
        }
    }

    /// Whether a process of `bg_id` is remembered, exited or not.
    fn remembers(&self, bg_id: &str) -> bool {
        self.seen().contains_key(bg_id)
    }

    /// Whether a remembered process of `bg_id` still runs.
    fn runs(&self, bg_id: &str) -> bool {
        self.seen()
            .get(bg_id)
            .is_some_and(|processes| processes.iter().any(ListedProcess::running))
    }

    fn forget(&self, bg_id: &str) {
        self.seen().remove(bg_id);
    }

    /// Check `/proc` every `exit_poll` until no remembered process of
    /// `bg_id` runs, then forget them: `true`. `false` when one still runs
    /// `stop_wait` after `started`.
    async fn wait_remembered(&self, bg_id: &str, started: Instant) -> bool {
        while self.runs(bg_id) {
            if started.elapsed() >= self.stop_wait {
                return false;
            }
            tokio::time::sleep(self.exit_poll).await;
        }
        self.forget(bg_id);
        true
    }

    /// `claude stop <bg id>`, after a listing that remembers its process: a
    /// resume then waits for that process even once the listing no longer
    /// shows it. Stopping a stopped session is harmless.
    async fn stop_noting(&self, agent: &Agent, bg_id: &str) -> Result<(), String> {
        match list_sessions_within(agent, LIST_TIMEOUT).await {
            Ok(list) => {
                if let Some(listed) = find_session(&list, bg_id) {
                    self.note(bg_id, &listed);
                }
            }
            Err(e) => debug!("agent '{}': {e}", agent.name),
        }
        stop_bg(agent, bg_id).await
    }

    /// Wait until `claude agents` lists `bg_id` with no pid and every
    /// process remembered for it (listed before, or meanwhile) has exited,
    /// plus the settle grace, for at most `stop_wait`. A failed listing
    /// counts as still running, and so does one that does not list `bg_id`
    /// while a process remembered for it runs (a copy just started and not
    /// listed yet, or a session removed while its process exits): only with
    /// none running is it gone.
    async fn wait_for_exit(&self, agent: &Agent, bg_id: &str) -> Result<Stopped, String> {
        let started = Instant::now();
        loop {
            match list_sessions_within(agent, LIST_TIMEOUT).await {
                Ok(list) => match find_session(&list, bg_id) {
                    None if self.runs(bg_id) => debug!(
                        "agent '{}': bg {bg_id} is not listed, but its process still runs",
                        agent.name
                    ),
                    None => {
                        self.forget(bg_id);
                        return Ok(Stopped::Gone);
                    }
                    Some(listed) if !listed.alive => {
                        // Unlisted, but maybe still running: check the
                        // processes themselves, cheaply and often.
                        if self.wait_remembered(bg_id, started).await {
                            debug!(
                                "agent '{}': bg {bg_id} exited {} ms after the stop",
                                agent.name,
                                started.elapsed().as_millis()
                            );
                            tokio::time::sleep(self.settle_grace).await;
                            return Ok(Stopped::Exited);
                        }
                    }
                    Some(listed) => self.note(bg_id, &listed),
                },
                Err(e) => debug!("agent '{}': {e}", agent.name),
            }
            if started.elapsed() >= self.stop_wait {
                warn!(
                    "agent '{}': bg {bg_id} still runs {} s after `claude stop`",
                    agent.name,
                    self.stop_wait.as_secs()
                );
                return Err(format!("Agent '{}': {STOP_TIMEOUT}", agent.name));
            }
            tokio::time::sleep(self.stop_poll).await;
        }
    }

    /// Before resuming a session `claude agents` lists with no pid (stopped
    /// already: `--stop`, a turn stopped during its launch, a wait that ran
    /// out): wait for the processes remembered for it to exit, plus the
    /// settle grace, for at most `stop_wait`. At once when none is.
    async fn settle(&self, agent: &Agent, bg_id: &str) -> Result<(), String> {
        if !self.remembers(bg_id) {
            return Ok(());
        }
        let started = Instant::now();
        if !self.wait_remembered(bg_id, started).await {
            warn!(
                "agent '{}': the last process of bg {bg_id} still runs {} s after it was stopped",
                agent.name,
                self.stop_wait.as_secs()
            );
            return Err(format!("Agent '{}': {STOP_TIMEOUT}", agent.name));
        }
        debug!(
            "agent '{}': the last process of bg {bg_id} is gone ({} ms)",
            agent.name,
            started.elapsed().as_millis()
        );
        tokio::time::sleep(self.settle_grace).await;
        Ok(())
    }

    /// List until `claude agents` shows `bg_id`, every `stop_poll` for at
    /// most `copy_list_wait`, and remember the process it shows. `false`
    /// when it never showed.
    async fn note_when_listed(&self, agent: &Agent, bg_id: &str) -> bool {
        self.note_until(agent, bg_id, |_| true).await
    }

    /// [`Exits::note_when_listed`] until the listing shows `bg_id` with a
    /// process.
    async fn note_when_running(&self, agent: &Agent, bg_id: &str) -> bool {
        self.note_until(agent, bg_id, |listed| listed.alive).await
    }

    /// List every `stop_poll`, for at most `copy_list_wait`, remembering the
    /// process each listing shows for `bg_id`, until `shows` accepts its
    /// entry: `true`, else `false`.
    async fn note_until(
        &self,
        agent: &Agent,
        bg_id: &str,
        shows: impl Fn(&ListedSession) -> bool,
    ) -> bool {
        let started = Instant::now();
        loop {
            match list_sessions_within(agent, LIST_TIMEOUT).await {
                Ok(list) => {
                    if let Some(listed) = find_session(&list, bg_id) {
                        self.note(bg_id, &listed);
                        if shows(&listed) {
                            return true;
                        }
                    }
                }
                Err(e) => debug!("agent '{}': {e}", agent.name),
            }
            if started.elapsed() >= self.copy_list_wait {
                return false;
            }
            tokio::time::sleep(self.stop_poll).await;
        }
    }

    /// Stop what a resume that printed the session's own bg id with a copy
    /// note started ([`Launched::UnclearCopy`]). No listing showed its
    /// process yet, and `claude stop` drops the pid from the listing before
    /// the process exits, so it is listed first (a short retry, until it
    /// shows with a process) and remembered: a wait after the stop then
    /// waits for it. Never removed: it is the session itself.
    async fn stop_resumed(&self, agent: &Agent, bg_id: &str) -> Result<(), String> {
        if !self.note_when_running(agent, bg_id).await {
            debug!(
                "agent '{}': bg {bg_id} is not listed with a process after the resume; \
                 stopping it anyway",
                agent.name
            );
        }
        stop_bg(agent, bg_id).await
    }

    /// Stop and remove a copy a resume started ([`copy_id`]: never the
    /// session's own bg id). Its bg id came from our own launch's output, so
    /// it is ours; nothing else is ever removed. A copy just started may not
    /// be listed yet: it is listed first (a short retry, like a first
    /// launch's lookup), so its process is remembered and the wait does not
    /// take it for removed. `claude rm` waits for the copy's process to
    /// exit, since it still runs right after `claude stop`. Nothing of the
    /// copy is remembered afterwards, whatever happened. The error names the
    /// copy left behind.
    async fn discard_copy(&self, agent: &Agent, bg_id: &str) -> Result<(), String> {
        if !self.note_when_listed(agent, bg_id).await {
            debug!(
                "agent '{}': the copy {bg_id} is not listed; stopping it anyway",
                agent.name
            );
        }
        if let Err(e) = stop_bg(agent, bg_id).await {
            debug!("agent '{}': {e}", agent.name);
        }
        if self.wait_for_exit(agent, bg_id).await.is_err() {
            warn!(
                "agent '{}': the copy {bg_id} still runs after `claude stop`; removing it anyway",
                agent.name
            );
        }
        let args = ["rm".to_string(), bg_id.to_string()];
        let removed = run_claude(agent, &args, QUICK_TIMEOUT).await;
        self.forget(bg_id);
        let out = removed.map_err(|e| format!("cannot remove the copy {bg_id}: {e}"))?;
        if out.success {
            info!("agent '{}': removed the copy {bg_id}", agent.name);
            Ok(())
        } else {
            Err(format!(
                "cannot remove the copy {bg_id}: `claude rm {bg_id}` failed: {}",
                out.stderr.trim()
            ))
        }
    }
}

/// How a launch ended.
#[derive(Debug, PartialEq, Eq)]
enum Launched {
    /// The session runs: its bg id, and the ids reported.
    Running(String, SessionRef),
    /// A resume started a copy (its bg id), already stopped and removed and
    /// never reported.
    Copy(String),
    /// A resume printed the session's own bg id with a note that it started
    /// a copy as that id: whether it continued in place is unclear. Not
    /// reported, and never removed (that would remove the session itself):
    /// the caller notes its process and stops it ([`Exits::stop_resumed`]),
    /// waits for it and resumes once more, like after a copy. A second one
    /// is stopped too, and the turn is not sent.
    UnclearCopy,
}

/// A launch that runs on a task of its own, so stopping the turn cannot cut
/// it off between starting a session and reporting it. The bg id is
/// reported (`base` with that bg id) as soon as it is printed, before the
/// session id is looked up, so `--stop` and a restart know it. When the turn
/// was stopped meanwhile, the launch stops the session it started.
///
/// `resume_of` is the session a resume continues: a copy of it
/// ([`copy_id`]) is stopped and removed on the same task, so a stop of the
/// turn cannot leave it running, and is never reported.
async fn launch_reported(
    agent: &Agent,
    args: Vec<String>,
    base: SessionRef,
    updates: &TurnUpdates,
    exits: &Exits,
    use_scope: bool,
    resume_of: Option<&SessionRef>,
) -> Result<Launched, String> {
    let agent = agent.clone();
    let updates = updates.clone();
    let exits = exits.clone();
    let resume_of = resume_of.cloned();
    let task = tokio::spawn(async move {
        let Launch { bg_id, copy_note } = launch(&agent, &args, use_scope).await?;
        if let Some(original) = &resume_of {
            let session_bg = original.bg_id.as_deref();
            if let Some(copy) = copy_id(session_bg, &bg_id, copy_note.as_deref()) {
                warn!(
                    "agent '{}': resuming {} started a copy (bg {copy}) instead of continuing \
                     in place; stopping and removing the copy",
                    agent.name,
                    describe(original)
                );
                if let Err(e) = exits.discard_copy(&agent, &copy).await {
                    warn!("agent '{}': {e}", agent.name);
                }
                return Ok(Launched::Copy(copy));
            }
            if copy_note.is_some() {
                warn!(
                    "agent '{}': the resume's copy note names the session itself (bg {bg_id}); \
                     stopping it to resume once more",
                    agent.name
                );
                return Ok(Launched::UnclearCopy);
            }
        }
        let mut session = base;
        session.bg_id = Some(bg_id.clone());
        if updates.report_session(session.clone()) {
            info!(
                "agent '{}': the turn was stopped during the launch; stopping bg {bg_id}",
                agent.name
            );
            if let Err(e) = exits.stop_noting(&agent, &bg_id).await {
                warn!("agent '{}': {e}", agent.name);
            }
            return Err(format!("Agent '{}': the turn was stopped.", agent.name));
        }
        Ok(Launched::Running(bg_id, session))
    });
    task.await
        .map_err(|e| format!("the launch task failed: {e}"))?
}

/// A session for the log: its id, else its bg id.
fn describe(session: &SessionRef) -> String {
    match (&session.session_id, &session.bg_id) {
        (Some(id), _) => format!("session {id}"),
        (None, Some(bg)) => format!("bg {bg}"),
        (None, None) => "the session".to_string(),
    }
}

impl ClaudeDriver {
    /// The first turn of a new session, and when its launch was spawned.
    async fn first_turn(
        &self,
        agent: &Agent,
        text: &str,
        updates: &TurnUpdates,
    ) -> Result<(SessionRef, Instant), TurnOutcome> {
        let settings = hook_settings(&self.hook_command);
        let launched_at = Instant::now();
        let launched = launch_reported(
            agent,
            first_turn_args(agent, &settings, text),
            SessionRef::default(),
            updates,
            &self.exits,
            self.use_scope,
            None,
        )
        .await
        .map_err(TurnOutcome::Failed)?;
        let (bg_id, mut session) = match launched {
            Launched::Running(bg_id, session) => (bg_id, session),
            // Only a resume is checked for a copy.
            Launched::Copy(_) | Launched::UnclearCopy => {
                return Err(TurnOutcome::Failed(cannot_continue(agent)))
            }
        };
        match learn_session(agent, &bg_id).await {
            Ok(listed) => {
                self.exits.note(&bg_id, &listed);
                session.session_id = listed.session_id;
                info!(
                    "agent '{}': started session {} (bg {bg_id})",
                    agent.name,
                    session.session_id.as_deref().unwrap_or("?")
                );
            }
            // Watched by its bg id: the poll learns the session id.
            Err(e) => warn!("{e}; watching it by its bg id"),
        }
        Ok((session, launched_at))
    }

    /// `claude stop <bg id>`, then [`Exits::wait_for_exit`] for it and the
    /// processes remembered for it. Stopping a stopped session is harmless.
    async fn stop_and_wait(&self, agent: &Agent, bg_id: &str) -> Result<Stopped, String> {
        if let Err(e) = stop_bg(agent, bg_id).await {
            debug!("agent '{}': {e}", agent.name);
        }
        self.exits.wait_for_exit(agent, bg_id).await
    }

    /// A later turn: refused ([`TurnOutcome::Busy`]) while the session works
    /// on its own; otherwise stop the old process if it is alive and wait
    /// until it is gone (a session listed with no pid already: wait for the
    /// process it was last listed with), then resume. A resume that starts a
    /// copy anyway is never adopted: the copy is stopped and removed, the
    /// old process is waited for again and the resume is tried once more; a
    /// second copy fails the turn (a second unclear one is stopped first).
    /// Returns the session and when the resume was spawned: hook events
    /// received earlier are the old process's.
    async fn next_turn(
        &self,
        agent: &Agent,
        session: &SessionRef,
        text: &str,
        updates: &TurnUpdates,
    ) -> Result<(SessionRef, Instant), TurnOutcome> {
        let mut session_id = session.session_id.clone();
        if let Some(bg_id) = &session.bg_id {
            let alive = match list_sessions(agent).await {
                Ok(list) => match find_session(&list, bg_id) {
                    Some(listed) => {
                        self.exits.note(bg_id, &listed);
                        if session_id.is_none() {
                            session_id = listed.session_id.clone();
                        }
                        if idle_phase(Some(&listed)) == AgentPhase::Working {
                            info!(
                                "agent '{}': its session is working on its own; not stopped \
                                 for a new turn",
                                agent.name
                            );
                            return Err(TurnOutcome::Busy);
                        }
                        listed.alive
                    }
                    None => {
                        // Removed (`claude rm`): a resume would come back
                        // without its saved options, hooks included.
                        self.exits.forget(bg_id);
                        info!(
                            "agent '{}': session {} is no longer listed; starting a new one",
                            agent.name,
                            session_id.as_deref().unwrap_or(bg_id)
                        );
                        return self.first_turn(agent, text, updates).await;
                    }
                },
                // Cannot tell: stop it and wait, as if it ran.
                Err(e) => {
                    debug!("agent '{}': {e}", agent.name);
                    true
                }
            };
            if alive {
                let stopped = self
                    .stop_and_wait(agent, bg_id)
                    .await
                    .map_err(TurnOutcome::Failed)?;
                if stopped == Stopped::Gone {
                    info!(
                        "agent '{}': session {} was removed while it stopped; starting a new one",
                        agent.name,
                        session_id.as_deref().unwrap_or(bg_id)
                    );
                    return self.first_turn(agent, text, updates).await;
                }
            } else {
                // Stopped already, but the process it was last listed with
                // can still be exiting, and a resume before it is gone
                // starts a copy.
                self.exits
                    .settle(agent, bg_id)
                    .await
                    .map_err(TurnOutcome::Failed)?;
            }
        }
        let Some(session_id) = session_id else {
            return self.first_turn(agent, text, updates).await;
        };
        let base = SessionRef {
            session_id: Some(session_id.clone()),
            ..session.clone()
        };
        let mut attempt = 1;
        loop {
            let launched_at = Instant::now();
            let launched = launch_reported(
                agent,
                resume_args(&session_id, text),
                base.clone(),
                updates,
                &self.exits,
                self.use_scope,
                Some(&base),
            )
            .await
            .map_err(TurnOutcome::Failed)?;
            // `unclear`: the resume may run the turn as the session itself.
            let (copy, unclear) = match launched {
                Launched::Running(_, resumed) => return Ok((resumed, launched_at)),
                Launched::Copy(copy) => (copy, false),
                Launched::UnclearCopy => (
                    format!(
                        "{}, by its note",
                        session.bg_id.as_deref().unwrap_or("the session itself")
                    ),
                    true,
                ),
            };
            if attempt >= RESUME_ATTEMPTS {
                warn!(
                    "agent '{}': resume {attempt} of session {session_id} started a copy too \
                     (bg {copy}); the turn is not sent",
                    agent.name
                );
                // Stopped, so a turn reported as not sent does not run.
                if let Some(bg_id) = session.bg_id.as_deref().filter(|_| unclear) {
                    if let Err(e) = self.exits.stop_resumed(agent, bg_id).await {
                        warn!("agent '{}': {e}", agent.name);
                    }
                }
                return Err(TurnOutcome::Failed(cannot_continue(agent)));
            }
            attempt += 1;
            // Claude Code still saw the old process running: wait for it
            // to be gone, then resume once more. After an unclear resume,
            // also for the process that resume started.
            if let Some(bg_id) = &session.bg_id {
                let stopped = if unclear {
                    if let Err(e) = self.exits.stop_resumed(agent, bg_id).await {
                        debug!("agent '{}': {e}", agent.name);
                    }
                    self.exits.wait_for_exit(agent, bg_id).await
                } else {
                    self.stop_and_wait(agent, bg_id).await
                }
                .map_err(TurnOutcome::Failed)?;
                if stopped == Stopped::Gone {
                    info!(
                        "agent '{}': session {session_id} was removed meanwhile; starting a \
                         new one",
                        agent.name
                    );
                    return self.first_turn(agent, text, updates).await;
                }
            }
            info!(
                "agent '{}': resuming session {session_id} once more",
                agent.name
            );
        }
    }

    /// Wait for the turn of `session` to end. Hook events received before
    /// `accept_after` belong to an earlier process and are ignored. The
    /// transcript is followed meanwhile for activities and interims
    /// ([`TranscriptTail`]): interims only for a turn this daemon launched
    /// (`accept_after` set), not one watched again after a restart.
    async fn watch(
        &self,
        agent: &Agent,
        mut session: SessionRef,
        since: DateTime<Utc>,
        accept_after: Option<Instant>,
        hooks: &mut mpsc::UnboundedReceiver<HookEvent>,
        updates: &TurnUpdates,
    ) -> TurnOutcome {
        let mode = match accept_after {
            Some(_) => TailMode::Live,
            None => TailMode::Rewatch,
        };
        let _tail = TranscriptTail::spawn(session.clone(), since, updates.clone(), mode);
        let Some(bg_id) = session.bg_id.clone() else {
            // Only a turn watched again after a restart lacks a bg id: the
            // restart landed before its launch printed one.
            return TurnOutcome::Failed(format!("Agent '{}': {LOST_TRACK}", agent.name));
        };
        let mut poll = tokio::time::interval(self.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the hook usually comes first.
        poll.tick().await;
        let mut needs_you = false;
        let mut hooks_open = true;
        // Set by a `done` poll: when to read the transcript if the Stop hook
        // has not come by then.
        let mut done_deadline: Option<tokio::time::Instant> = None;
        let mut done_without_text = 0u32;
        loop {
            let deadline = done_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                event = hooks.recv(), if hooks_open => {
                    let Some(event) = event else {
                        hooks_open = false;
                        continue;
                    };
                    if !is_current(&event, accept_after) {
                        // The old process's: not this turn's reply, but a
                        // Stop with a message is work the session finished
                        // on its own, spoken as such.
                        match classify_hook(&event.payload) {
                            HookKind::Stop { message: Some(m), .. } if !m.trim().is_empty() => {
                                debug!(
                                    "agent '{}': a Stop from before this turn's launch is the \
                                     session's own reply",
                                    agent.name
                                );
                                updates.send(TurnUpdate::SessionReply(m));
                            }
                            _ => debug!(
                                "agent '{}': ignored a hook event from before this turn's launch",
                                agent.name
                            ),
                        }
                        continue;
                    }
                    match classify_hook(&event.payload) {
                        HookKind::Stop { message, transcript_path } => {
                            if let Some(path) = transcript_path {
                                if session.transcript_path.as_ref() != Some(&path) {
                                    session.transcript_path = Some(path);
                                    updates.send(TurnUpdate::Session(session.clone()));
                                }
                            }
                            let text = match message {
                                Some(m) if !m.trim().is_empty() => m,
                                _ => transcript_reply(&session, since).await.unwrap_or_default(),
                            };
                            return TurnOutcome::Reply(text);
                        }
                        HookKind::PermissionPrompt => {
                            if !needs_you {
                                needs_you = true;
                                updates.send(TurnUpdate::NeedsYou(notice(&session).await));
                            }
                        }
                        HookKind::Other => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline), if done_deadline.is_some() => {
                    done_deadline = None;
                    if let Some(text) = transcript_reply(&session, since).await {
                        info!(
                            "agent '{}': turn done without a Stop hook; read the transcript",
                            agent.name
                        );
                        return TurnOutcome::PolledReply(text);
                    }
                    // `done` before this turn wrote anything: the previous
                    // turn's state for a moment, or a turn that never ran.
                    done_without_text += 1;
                    if done_without_text >= DONE_POLLS_WITHOUT_TEXT {
                        warn!(
                            "agent '{}': the session is done but this turn wrote nothing",
                            agent.name
                        );
                        return TurnOutcome::Failed(format!("Agent '{}': {INTERRUPTED}", agent.name));
                    }
                }
                _ = poll.tick() => {
                    let list = match list_sessions(agent).await {
                        Ok(list) => list,
                        Err(e) => {
                            warn!("agent '{}': poll failed: {e}", agent.name);
                            continue;
                        }
                    };
                    let listed = find_session(&list, &bg_id);
                    if let Some(listed) = &listed {
                        self.exits.note(&bg_id, listed);
                    }
                    // A launch watched again after a restart may not know
                    // its session id yet: learn it, so hooks match.
                    if let Some(id) = listed.as_ref().and_then(|l| l.session_id.clone()) {
                        if session.session_id.is_none() {
                            session.session_id = Some(id);
                            updates.send(TurnUpdate::Session(session.clone()));
                        }
                    }
                    match poll_state(listed.as_ref()) {
                        PollState::Gone => {
                            // Forget it: the next turn starts a new session,
                            // and nothing of it is waited for again.
                            self.exits.forget(&bg_id);
                            updates.send(TurnUpdate::Session(SessionRef::default()));
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session ended, Claude Code no longer lists it. \
                                 The next turn starts a new one.",
                                agent.name
                            ));
                        }
                        PollState::Exited { state } => {
                            if state.as_deref() == Some("done") {
                                if let Some(text) = transcript_reply(&session, since).await {
                                    return TurnOutcome::PolledReply(text);
                                }
                            }
                            if state.as_deref() == Some("failed") {
                                return TurnOutcome::Failed(format!(
                                    "Agent '{}': the session failed. Open it with `claude attach \
                                     {bg_id}` to see why.",
                                    agent.name
                                ));
                            }
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session ended before replying. The next turn \
                                 continues it.",
                                agent.name
                            ));
                        }
                        PollState::Blocked { waiting_for } => {
                            done_without_text = 0;
                            done_deadline = None;
                            if !needs_you {
                                needs_you = true;
                                let notice = match waiting_for.as_deref() {
                                    Some("permission prompt") | None => notice(&session).await,
                                    Some(_) => "The agent needs you.".to_string(),
                                };
                                updates.send(TurnUpdate::NeedsYou(notice));
                            }
                        }
                        PollState::Working => {
                            done_without_text = 0;
                            done_deadline = None;
                            if needs_you {
                                needs_you = false;
                                updates.send(TurnUpdate::Working);
                            }
                        }
                        PollState::Done => {
                            // Give the Stop hook a moment: it carries the
                            // message without reading the transcript.
                            if done_deadline.is_none() {
                                done_deadline =
                                    Some(tokio::time::Instant::now() + self.done_grace);
                            }
                        }
                        PollState::Other => {}
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Driver for ClaudeDriver {
    async fn run_turn(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        let TurnIo { mut hooks, updates } = io;
        if !agent.dir.is_dir() {
            return TurnOutcome::Failed(format!(
                "Agent '{}': its dir {} no longer exists.",
                agent.name,
                agent.dir.display()
            ));
        }
        let launched = if session.session_id.is_some() || session.bg_id.is_some() {
            self.next_turn(agent, &session, &request.text, &updates)
                .await
        } else {
            self.first_turn(agent, &request.text, &updates).await
        };
        let (session, launched_at) = match launched {
            Ok(s) => s,
            Err(outcome) => return outcome,
        };
        updates.send(TurnUpdate::Session(session.clone()));
        self.watch(
            agent,
            session,
            request.started_at,
            Some(launched_at),
            &mut hooks,
            &updates,
        )
        .await
    }

    async fn rewatch(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        let TurnIo { mut hooks, updates } = io;
        // The old daemon saw the launch; every event that reaches this one
        // is the running process's.
        self.watch(
            agent,
            session,
            request.started_at,
            None,
            &mut hooks,
            &updates,
        )
        .await
    }

    /// `claude stop`, after a listing that remembers the session's process,
    /// so the next turn waits for it to exit before resuming.
    async fn stop(&self, agent: &Agent, session: &SessionRef) -> Result<(), String> {
        match &session.bg_id {
            Some(bg_id) => self.exits.stop_noting(agent, bg_id).await,
            None => Ok(()),
        }
    }

    /// Forget the processes remembered for the session: it is never resumed
    /// again, and a live one would stay remembered for good.
    fn forget(&self, session: &SessionRef) {
        if let Some(bg_id) = &session.bg_id {
            self.exits.forget(bg_id);
        }
    }

    fn hook_matches(&self, payload: &Value, session: &SessionRef) -> bool {
        match (
            payload.get("session_id").and_then(Value::as_str),
            session.session_id.as_deref(),
        ) {
            (Some(theirs), Some(ours)) => theirs == ours,
            _ => false,
        }
    }

    async fn session_phase(
        &self,
        agent: &Agent,
        session: &SessionRef,
    ) -> Result<AgentPhase, String> {
        let Some(bg_id) = &session.bg_id else {
            return Ok(AgentPhase::Idle);
        };
        let list = list_sessions(agent).await?;
        Ok(self.phase_noting(&list, bg_id))
    }

    /// One `claude agents --json --all` per distinct `command` (it lists
    /// every session), with a short timeout: `whisrs agent` waits on it.
    async fn session_phases(
        &self,
        sessions: &[(std::sync::Arc<Agent>, SessionRef)],
    ) -> Vec<Result<AgentPhase, String>> {
        let mut lists: Vec<(String, Result<Vec<Value>, String>)> = Vec::new();
        let mut phases = Vec::with_capacity(sessions.len());
        for (agent, session) in sessions {
            let Some(bg_id) = &session.bg_id else {
                phases.push(Ok(AgentPhase::Idle));
                continue;
            };
            if !lists.iter().any(|(command, _)| command == &agent.command) {
                let list = list_sessions_within(agent, LIST_TIMEOUT).await;
                lists.push((agent.command.clone(), list));
            }
            let list = lists
                .iter()
                .find(|(command, _)| command == &agent.command)
                .map(|(_, list)| list);
            phases.push(match list {
                Some(Ok(list)) => Ok(self.phase_noting(list, bg_id)),
                Some(Err(e)) => Err(e.clone()),
                None => Err("no session list".to_string()),
            });
        }
        phases
    }

    async fn idle_event(&self, payload: &Value, session: &SessionRef) -> IdleEvent {
        match classify_hook(payload) {
            HookKind::Stop {
                message: Some(message),
                ..
            } if !message.trim().is_empty() => IdleEvent::Reply(message),
            HookKind::Stop { .. } => IdleEvent::Ended,
            HookKind::PermissionPrompt => IdleEvent::NeedsYou(notice(session).await),
            HookKind::Other => IdleEvent::Ignored,
        }
    }
}

/// What the session with `bg_id` does, from a `claude agents` listing.
fn listed_phase(list: &[Value], bg_id: &str) -> AgentPhase {
    idle_phase(find_session(list, bg_id).as_ref())
}

impl ClaudeDriver {
    /// [`listed_phase`], remembering the process the listing shows.
    fn phase_noting(&self, list: &[Value], bg_id: &str) -> AgentPhase {
        if let Some(listed) = find_session(list, bg_id) {
            self.exits.note(bg_id, &listed);
        }
        listed_phase(list, bg_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Harness, DEFAULT_REPLY_INSTRUCTION};
    use std::os::unix::fs::PermissionsExt;

    fn agent(permissions: Permissions) -> Agent {
        Agent {
            name: "jarvis".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::Claude,
            dir: PathBuf::from("/proj"),
            permissions,
            command: "claude".into(),
            args: vec!["--model".into(), "haiku".into()],
            instruction: DEFAULT_REPLY_INSTRUCTION.into(),
            feedback: crate::agent::progress::Feedback::Brief,
        }
    }

    #[test]
    fn permissions_map_to_claude_modes() {
        assert_eq!(
            permission_args(Permissions::Auto),
            ["--permission-mode", "auto"]
        );
        assert_eq!(
            permission_args(Permissions::Bypass),
            ["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(
            permission_args(Permissions::Ask),
            ["--permission-mode", "default"]
        );
    }

    #[test]
    fn first_turn_passes_everything_once_and_ends_with_the_prompt() {
        let a = agent(Permissions::Auto);
        let args = first_turn_args(&a, "{}", "-rf is not a flag here");
        assert_eq!(
            args,
            [
                "--bg",
                "--name",
                "whisrs-jarvis",
                "--permission-mode",
                "auto",
                "--append-system-prompt",
                DEFAULT_REPLY_INSTRUCTION,
                "--settings",
                "{}",
                "--model",
                "haiku",
                "--",
                "-rf is not a flag here",
            ]
        );
    }

    #[test]
    fn a_resume_passes_no_flag_but_the_session() {
        assert_eq!(
            resume_args("cc7a37cf-9343", "-next"),
            ["--bg", "--resume", "cc7a37cf-9343", "--", "-next"]
        );
    }

    #[test]
    fn the_hook_settings_run_the_hook_on_stop_and_permission_prompts() {
        let cmd = hook_command(
            Path::new("/usr/bin/whisrs"),
            Path::new("/run/user/1000/whisrs.sock"),
        );
        assert_eq!(
            cmd,
            "/usr/bin/whisrs agent-hook --socket /run/user/1000/whisrs.sock"
        );
        let settings: Value = serde_json::from_str(&hook_settings(&cmd)).unwrap();
        assert_eq!(
            settings.pointer("/hooks/Stop/0/hooks/0/command"),
            Some(&Value::from(cmd.clone()))
        );
        assert_eq!(
            settings.pointer("/hooks/Notification/0/matcher"),
            Some(&Value::from("permission_prompt"))
        );
        assert_eq!(
            settings.pointer("/hooks/Notification/0/hooks/0/command"),
            Some(&Value::from(cmd))
        );
        assert_eq!(
            settings.pointer("/hooks/Stop/0/hooks/0/timeout"),
            Some(&Value::from(HOOK_TIMEOUT_SECS))
        );
    }

    #[test]
    fn hook_paths_are_shell_quoted() {
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote("/my dir/whisrs"), "'/my dir/whisrs'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn the_bg_id_is_read_from_the_launch_output() {
        let out = "Starting background service\u{2026}\nbackgrounded \u{b7} cc7a37cf \u{b7} whisrs-probe1\n  claude agents             list sessions\n";
        assert_eq!(parse_backgrounded(out).as_deref(), Some("cc7a37cf"));
        assert_eq!(
            parse_backgrounded("backgrounded \u{b7} 7c5dcf5d\n").as_deref(),
            Some("7c5dcf5d")
        );
        assert_eq!(parse_backgrounded("nothing here"), None);
    }

    /// Real `claude agents --json --all` output (2.1.301), trimmed.
    const LIST: &str = r#"[
      {"id":"35622b7b","cwd":"/home/u/Career","kind":"background","startedAt":1,"sessionId":"35622b7b-c088","name":"whisrs-jarvis","state":"blocked"},
      {"pid":1690246,"id":"cc7a37cf","cwd":"/proj","kind":"background","startedAt":2,"sessionId":"cc7a37cf-9343","name":"whisrs-probe1","status":"idle","state":"done"},
      {"pid":1690243,"id":"e237a624","cwd":"/proj","kind":"background","startedAt":3,"sessionId":"e237a624-7f3b","name":"whisrs-probe2","status":"waiting","waitingFor":"permission prompt","state":"blocked"},
      {"pid":1522977,"cwd":"/home/u/whisrs","kind":"interactive","startedAt":4,"sessionId":"ddc53afe-477f","name":"whisrs-d9","status":"busy"}
    ]"#;

    /// Measured on 2.1.301 (this branch's e2e): a turn that started `sleep
    /// 30` in the background and replied lists `state: done` with `status:
    /// busy` while the sleep runs. Between turns that is working (a new turn
    /// would `claude stop` it and kill the task); for a running turn `done`
    /// still ends it.
    #[test]
    fn a_background_task_keeps_the_session_working_between_turns() {
        let list = parse_agent_list(
            r#"[{"pid":1974658,"id":"0350dec6","cwd":"/proj","kind":"background","startedAt":1,"sessionId":"0350dec6-5eb2","name":"whisrs-jarvis","status":"busy","state":"done"}]"#,
        )
        .unwrap();
        let listed = find_session(&list, "0350dec6").unwrap();
        assert!(listed.busy);
        assert_eq!(idle_phase(Some(&listed)), AgentPhase::Working);
        assert_eq!(listed_phase(&list, "0350dec6"), AgentPhase::Working);
        assert_eq!(poll_state(Some(&listed)), PollState::Done);
        // Idle, waiting, exited and missing sessions are not working.
        let list = parse_agent_list(LIST).unwrap();
        assert_eq!(listed_phase(&list, "cc7a37cf"), AgentPhase::Idle);
        assert_eq!(listed_phase(&list, "e237a624"), AgentPhase::NeedsYou);
        assert_eq!(listed_phase(&list, "35622b7b"), AgentPhase::Idle);
        assert_eq!(listed_phase(&list, "nope"), AgentPhase::Idle);
        let exited_busy = ListedSession {
            alive: false,
            ..listed
        };
        assert_eq!(idle_phase(Some(&exited_busy)), AgentPhase::Idle);
    }

    #[test]
    fn sessions_are_found_by_bg_id_only() {
        let list = parse_agent_list(LIST).unwrap();
        assert_eq!(
            find_session(&list, "cc7a37cf"),
            Some(ListedSession {
                session_id: Some("cc7a37cf-9343".into()),
                state: Some("done".into()),
                alive: true,
                pid: Some(1690246),
                waiting_for: None,
                busy: false,
            })
        );
        let blocked = find_session(&list, "e237a624").unwrap();
        assert_eq!(blocked.waiting_for.as_deref(), Some("permission prompt"));
        let exited = find_session(&list, "35622b7b").unwrap();
        assert!(!exited.alive);
        // Names are never matched, and interactive sessions have no bg id.
        assert_eq!(find_session(&list, "whisrs-jarvis"), None);
        assert_eq!(find_session(&list, "ddc53afe"), None);
    }

    /// An exited entry keeps its last state (`35622b7b` has no pid but says
    /// `blocked`): the poll goes by the process, not the state.
    #[test]
    fn the_poll_goes_by_the_process_first() {
        let list = parse_agent_list(LIST).unwrap();
        assert_eq!(
            poll_state(find_session(&list, "35622b7b").as_ref()),
            PollState::Exited {
                state: Some("blocked".into())
            }
        );
        assert_eq!(
            poll_state(find_session(&list, "cc7a37cf").as_ref()),
            PollState::Done
        );
        assert_eq!(
            poll_state(find_session(&list, "e237a624").as_ref()),
            PollState::Blocked {
                waiting_for: Some("permission prompt".into())
            }
        );
        assert_eq!(
            poll_state(find_session(&list, "nope").as_ref()),
            PollState::Gone
        );
        let listed = |state: &str, alive: bool| ListedSession {
            session_id: None,
            state: Some(state.into()),
            alive,
            pid: alive.then_some(1),
            waiting_for: None,
            busy: false,
        };
        assert_eq!(
            poll_state(Some(&listed("working", true))),
            PollState::Working
        );
        assert_eq!(
            poll_state(Some(&listed("working", false))),
            PollState::Exited {
                state: Some("working".into())
            }
        );
        assert_eq!(
            poll_state(Some(&listed("stopped", true))),
            PollState::Exited {
                state: Some("stopped".into())
            }
        );
    }

    #[test]
    fn the_list_parser_skips_a_preface() {
        let out = format!("Starting background service\u{2026}\n{LIST}\n");
        assert_eq!(parse_agent_list(&out).unwrap().len(), 4);
        assert!(parse_agent_list("no json").is_err());
    }

    /// Exactly what a parent session sets (measured from a hook's
    /// environment on 2.1.301) and whisrs's own keys go; Claude Code's own
    /// configuration passes.
    #[test]
    fn only_a_parent_sessions_variables_are_stripped() {
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "CLAUDE_PROJECT_DIR",
            "CLAUDE_JOB_DIR",
            "AI_AGENT",
            "WHISRS_OPENAI_API_KEY",
            "WHISRS_DEEPGRAM_API_KEY",
        ] {
            assert!(strips_env(name), "{name}");
        }
        for name in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
            "CLAUDE_CODE_SOMETHING_NEW",
            "CLAUDE_SOMETHING",
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!strips_env(name), "{name}");
        }
    }

    #[test]
    fn an_untrusted_dir_gets_the_trust_hint() {
        let a = agent(Permissions::Auto);
        let stderr = "Workspace not trusted. Run `claude` in /proj once and accept the trust \
                      prompt, then retry.\n";
        assert_eq!(
            launch_failure(&a, "", stderr),
            "Agent 'jarvis': Claude Code does not trust this folder yet. Open /proj in Claude \
             Code once and accept the trust prompt."
        );
        assert_eq!(
            launch_failure(&a, "", "warning: x\nerror: boom\n"),
            "Agent 'jarvis': `claude --bg` failed: error: boom"
        );
    }

    /// Spawning in a missing dir also fails with "not found": the dir is
    /// named, not the binary.
    #[test]
    fn a_spawn_failure_names_the_dir_before_the_command() {
        let a = agent(Permissions::Auto);
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        for (row, dir_exists, want) in [
            (
                "a missing binary",
                true,
                "Agent 'jarvis': `claude` was not found. Install Claude Code, or set command in \
                 [[agents]] to its full path (a systemd user service may not have ~/.local/bin \
                 on PATH).",
            ),
            (
                "a missing dir",
                false,
                "Agent 'jarvis': its dir /proj does not exist.",
            ),
        ] {
            assert_eq!(spawn_failure(&a, &e, dir_exists), want, "{row}");
        }
    }

    #[test]
    fn a_service_cgroup_is_told_from_a_terminal_scope() {
        assert!(cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/whisrs.service\n"
        ));
        assert!(!cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-ab.scope\n"
        ));
        assert!(!cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/init.scope\n"
        ));
        assert!(!cgroup_is_service(""));
    }

    #[test]
    fn a_scoped_launch_wraps_the_command_unchanged() {
        let args = vec!["--bg".to_string(), "--".to_string(), "-x".to_string()];
        assert_eq!(
            scope_args(None, true, "/usr/bin/claude", &args),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--expand-environment=no",
                "--",
                "/usr/bin/claude",
                "--bg",
                "--",
                "-x"
            ]
        );
        // Named, for an older systemd-run (no expansion option).
        assert_eq!(
            scope_args(Some("whisrs-agent-x.scope"), false, "sh", &[]),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit=whisrs-agent-x.scope",
                "--",
                "sh"
            ]
        );
    }

    /// systemd-run before 254 refuses `--expand-environment` (getopt's own
    /// message); anything else is not that.
    #[test]
    fn an_older_systemd_run_refusing_the_expansion_option_is_recognized() {
        assert!(expand_option_refused(
            false,
            "",
            "systemd-run: unrecognized option '--expand-environment=no'\n"
        ));
        assert!(!expand_option_refused(
            false,
            "",
            "systemd-run: unrecognized option '--bogus'\n"
        ));
        assert!(!expand_option_refused(
            true,
            "",
            "systemd-run: unrecognized option '--expand-environment=no'\n"
        ));
        assert!(!expand_option_refused(
            false,
            "backgrounded",
            "systemd-run: unrecognized option '--expand-environment=no'\n"
        ));
    }

    /// systemd-run's own failures fall back to a direct launch
    /// (`a_scope_that_never_ran_falls_back` in `process.rs` runs them); a
    /// failure of claude itself (which ran) does not launch it twice.
    #[test]
    fn only_systemd_runs_own_failures_fall_back() {
        assert!(!scope_unavailable(false, "", "Workspace not trusted.\n"));
        assert!(!scope_unavailable(false, "", "error: boom\n"));
        assert!(!scope_unavailable(
            false,
            "backgrounded \u{b7} x\n",
            "Failed to connect to bus\n"
        ));
        assert!(!scope_unavailable(true, "", "Failed to connect to bus\n"));
    }

    /// Real hook payloads (2.1.301, trimmed).
    #[test]
    fn hook_payloads_are_classified() {
        let stop: Value = serde_json::from_str(
            r#"{"session_id":"cc7a37cf-9343","transcript_path":"/t/cc7a37cf-9343.jsonl","cwd":"/proj","permission_mode":"auto","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"pong"}"#,
        )
        .unwrap();
        assert_eq!(
            classify_hook(&stop),
            HookKind::Stop {
                message: Some("pong".into()),
                transcript_path: Some("/t/cc7a37cf-9343.jsonl".into()),
            }
        );
        let perm: Value = serde_json::from_str(
            r#"{"session_id":"e237a624","hook_event_name":"Notification","message":"Claude needs your permission","notification_type":"permission_prompt"}"#,
        )
        .unwrap();
        assert_eq!(classify_hook(&perm), HookKind::PermissionPrompt);
        let idle: Value = serde_json::from_str(
            r#"{"session_id":"cc7a37cf","hook_event_name":"Notification","message":"Claude is waiting for your input","notification_type":"idle_prompt"}"#,
        )
        .unwrap();
        assert_eq!(classify_hook(&idle), HookKind::Other);
    }

    #[test]
    fn hooks_match_by_stored_session_id_only() {
        let driver = ClaudeDriver::new(Path::new("/w"), Path::new("/s"));
        let session = SessionRef {
            session_id: Some("cc7a37cf-9343".into()),
            bg_id: Some("cc7a37cf".into()),
            transcript_path: None,
            pending: false,
        };
        let ours = serde_json::json!({"session_id": "cc7a37cf-9343"});
        let theirs = serde_json::json!({"session_id": "ddc53afe-477f"});
        assert!(driver.hook_matches(&ours, &session));
        assert!(!driver.hook_matches(&theirs, &session));
        assert!(!driver.hook_matches(&ours, &SessionRef::default()));
        assert!(!driver.hook_matches(&serde_json::json!({}), &session));
    }

    const TRANSCRIPT: &str = r#"{"type":"user","timestamp":"2026-10-24T17:52:46.983Z","message":{"role":"user","content":"Reply pong"}}
{"type":"assistant","timestamp":"2026-10-24T17:52:50.078Z","message":{"role":"assistant","content":[{"type":"text","text":"pong"}]}}
{"type":"user","timestamp":"2026-10-24T17:54:24.940Z","message":{"role":"user","content":"Again"}}
{"type":"assistant","timestamp":"2026-10-24T17:54:26.000Z","message":{"role":"assistant","content":[{"type":"thinking"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:26.500Z","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:27.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:27.504Z","message":{"role":"assistant","content":[{"type":"text","text":"pong again"}]}}
not json
"#;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_transcript_reply_is_this_turns_last_text() {
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T17:54:24Z")).as_deref(),
            Some("pong again")
        );
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T17:50:00Z")).as_deref(),
            Some("pong again")
        );
        // A turn that has written nothing yet is not done.
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T18:00:00Z")),
            None
        );
    }

    #[test]
    fn the_permission_notice_names_the_last_tool() {
        assert_eq!(last_tool_use(TRANSCRIPT).as_deref(), Some("Bash"));
        assert_eq!(
            needs_you_notice(Some("Bash")),
            "Waiting for your approval to use Bash."
        );
        assert_eq!(
            needs_you_notice(Some("mcp__github__create_pr")),
            "Waiting for your approval."
        );
        assert_eq!(needs_you_notice(None), "Waiting for your approval.");
    }

    // -----------------------------------------------------------------------
    // `watch` against a fake `claude`
    // -----------------------------------------------------------------------

    /// The fake `claude`: logs every call to `calls.log`; the n-th `agents`
    /// prints `agents-<n>.json` when present, else `list.json`, or after a
    /// `stop` (when `list-stopped.json` exists) `list.json` for as many more
    /// calls as `alive-after-stop` says, then `list-stopped.json`; `rm`
    /// fails while `rm-fails` exists; the n-th `--bg` prints `bg-<n>.out`
    /// (and `bg-<n>.err` on stderr) when present, else a launch of
    /// `abc12345`.
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
d=$(dirname "$0")
echo "$*" >> "$d/calls.log"
case "$1" in
  agents)
    a=$(cat "$d/agents.count" 2>/dev/null || echo 0); a=$((a + 1)); echo $a > "$d/agents.count"
    if [ -f "$d/agents-$a.json" ]; then
      cat "$d/agents-$a.json"
    elif [ -f "$d/stopped" ] && [ -f "$d/list-stopped.json" ]; then
      n=$(cat "$d/stopped")
      if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$d/stopped"; cat "$d/list.json"
      else cat "$d/list-stopped.json"; fi
    else
      cat "$d/list.json"
    fi ;;
  stop)
    if [ -f "$d/alive-after-stop" ]; then cp "$d/alive-after-stop" "$d/stopped"
    else echo 0 > "$d/stopped"; fi
    echo "stopped $2" ;;
  rm)
    if [ -f "$d/rm-fails" ]; then echo "cannot remove $2" >&2; exit 1; fi
    echo "removed $2" ;;
  --bg)
    n=$(cat "$d/bg.count" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > "$d/bg.count"
    [ -f "$d/bg-$n.err" ] && cat "$d/bg-$n.err" >&2
    if [ -f "$d/bg-$n.out" ]; then cat "$d/bg-$n.out"
    else printf 'backgrounded \302\267 abc12345 \302\267 whisrs-jarvis\n'; fi ;;
  *) exit 1 ;;
esac
"#;

    /// A temp dir with [`FAKE_CLAUDE`] in it.
    struct FakeClaude {
        dir: tempfile::TempDir,
    }

    impl FakeClaude {
        fn new(list: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("claude");
            // Written by `cp`, not by this process: a test thread that forks
            // while this process holds the script open for writing would
            // make running it fail with ETXTBSY ("Text file busy").
            let source = dir.path().join("claude.src");
            std::fs::write(&source, FAKE_CLAUDE).unwrap();
            let copied = std::process::Command::new("cp")
                .arg(&source)
                .arg(&script)
                .status()
                .unwrap();
            assert!(copied.success());
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.path().join("list.json"), list).unwrap();
            std::fs::write(dir.path().join("transcript.jsonl"), "").unwrap();
            Self { dir }
        }

        /// Write `name` in the fake's dir.
        fn file(&self, name: &str, contents: &str) {
            std::fs::write(self.dir.path().join(name), contents).unwrap();
        }

        fn agent(&self) -> Agent {
            let mut a = agent(Permissions::Auto);
            a.dir = self.dir.path().to_path_buf();
            a.command = self
                .dir
                .path()
                .join("claude")
                .to_string_lossy()
                .into_owned();
            a
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
        }

        fn session(&self) -> SessionRef {
            SessionRef {
                session_id: Some("abc12345-1111".into()),
                bg_id: Some("abc12345".into()),
                transcript_path: Some(self.dir.path().join("transcript.jsonl")),
                pending: false,
            }
        }
    }

    fn fast_driver(poll: Duration, grace: Duration) -> ClaudeDriver {
        ClaudeDriver {
            hook_command: "true".into(),
            poll_interval: poll,
            done_grace: grace,
            exits: fast_exits(),
            use_scope: false,
        }
    }

    fn fast_exits() -> Exits {
        Exits::new(
            Duration::from_millis(5),
            Duration::from_secs(5),
            Duration::from_millis(5),
            Duration::from_millis(1),
            Duration::from_millis(50),
        )
    }

    /// A pid no process has (above any `pid_max`): a listed session whose
    /// process exits the moment it is stopped.
    const GONE_PID: u32 = 999_999_999;

    fn entry(pid: Option<u32>, state: &str) -> String {
        let pid = pid.map(|p| format!("\"pid\":{p},")).unwrap_or_default();
        format!(
            r#"[{{{pid}"id":"abc12345","kind":"background","sessionId":"abc12345-1111","name":"whisrs-jarvis","state":"{state}"}}]"#
        )
    }

    fn stop_event(message: &str, received: Instant) -> HookEvent {
        HookEvent {
            received,
            payload: serde_json::json!({
                "session_id": "abc12345-1111",
                "hook_event_name": "Stop",
                "last_assistant_message": message,
            }),
        }
    }

    /// The old process of a resumed session can send its Stop while the
    /// resume launches: only a Stop received after the launch ends the turn.
    /// The earlier one is work the session finished on its own, reported as
    /// such (spoken as a follow-up), not dropped.
    #[tokio::test]
    async fn a_stop_from_before_the_launch_is_not_the_reply() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "working"));
        let driver = fast_driver(Duration::from_secs(3600), DONE_GRACE);
        let (hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, mut rx) = TurnUpdates::channel();
        let before = Instant::now();
        let launched = before + Duration::from_millis(5);
        hooks_tx.send(stop_event("STALE", before)).unwrap();
        hooks_tx
            .send(stop_event("fresh", launched + Duration::from_millis(1)))
            .unwrap();
        let outcome = driver
            .watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                Some(launched),
                &mut hooks,
                &updates,
            )
            .await;
        assert_eq!(outcome, TurnOutcome::Reply("fresh".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            TurnUpdate::SessionReply("STALE".into())
        );
        assert!(rx.try_recv().is_err());
        // A turn watched again after a restart takes every event.
        assert!(is_current(&stop_event("x", before), None));
        assert!(!is_current(&stop_event("x", before), Some(launched)));
    }

    /// `driver.watch` of `fake`'s agent in `session`, with no hook sent (the
    /// hooks stay open meanwhile) and at most 10 s: how it ended, and what
    /// it sent.
    async fn watch_unhooked(
        driver: &ClaudeDriver,
        fake: &FakeClaude,
        session: SessionRef,
        since: DateTime<Utc>,
        launched: Option<Instant>,
    ) -> (TurnOutcome, mpsc::UnboundedReceiver<TurnUpdate>) {
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, rx) = TurnUpdates::channel();
        let agent = fake.agent();
        let watched = driver.watch(&agent, session, since, launched, &mut hooks, &updates);
        let outcome = tokio::time::timeout(Duration::from_secs(10), watched)
            .await
            .expect("the watch ends within 10 s");
        (outcome, rx)
    }

    /// An exited entry keeps its last state: a killed process mid-turn
    /// (`working`, no pid) ends the turn instead of waiting forever.
    #[tokio::test]
    async fn an_exited_session_ends_the_turn() {
        let fake = FakeClaude::new(&entry(None, "working"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(10));
        let launched = Some(Instant::now());
        let (outcome, _) =
            watch_unhooked(&driver, &fake, fake.session(), Utc::now(), launched).await;
        match outcome {
            TurnOutcome::Failed(m) => assert!(m.contains("the session ended"), "{m}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// A vanished session ends the turn and is forgotten.
    #[tokio::test]
    async fn a_vanished_session_ends_the_turn_and_is_forgotten() {
        let fake = FakeClaude::new("[]");
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(10));
        let (outcome, mut rx) =
            watch_unhooked(&driver, &fake, fake.session(), Utc::now(), None).await;
        assert!(matches!(outcome, TurnOutcome::Failed(ref m) if m.contains("no longer lists it")));
        assert_eq!(
            rx.try_recv().unwrap(),
            TurnUpdate::Session(SessionRef::default())
        );
    }

    /// `done` with nothing written since the turn started (a restart between
    /// the stop and the resume) does not wait forever: after a few polls the
    /// turn ends as interrupted.
    #[tokio::test]
    async fn done_without_text_ends_as_interrupted() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let (outcome, _) = watch_unhooked(&driver, &fake, fake.session(), Utc::now(), None).await;
        assert_eq!(
            outcome,
            TurnOutcome::Failed(format!("Agent 'jarvis': {INTERRUPTED}"))
        );
        // One `claude agents` per poll: the first poll that said done, and
        // one more per grace that found no text.
        let polls = fake
            .calls()
            .lines()
            .filter(|l| l.starts_with("agents"))
            .count();
        assert!(polls >= DONE_POLLS_WITHOUT_TEXT as usize, "{polls} polls");
    }

    /// `done` with this turn's text in the transcript (the Stop hook never
    /// came) is the reply.
    #[tokio::test]
    async fn done_reads_the_transcript_when_the_hook_is_missing() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        let since = Utc::now() - chrono::Duration::seconds(5);
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"content":[{{"type":"text","text":"from the transcript"}}]}}}}"#,
            Utc::now().to_rfc3339()
        );
        std::fs::write(fake.dir.path().join("transcript.jsonl"), line + "\n").unwrap();
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let (outcome, _) = watch_unhooked(&driver, &fake, fake.session(), since, None).await;
        // Read without the Stop hook: that hook may still come, and must not
        // be spoken twice.
        assert_eq!(
            outcome,
            TurnOutcome::PolledReply("from the transcript".into())
        );
    }

    /// While a `done` poll waits for the Stop hook, other hook events are
    /// still handled (the permission prompt is announced), not swallowed.
    #[tokio::test]
    async fn the_done_grace_keeps_handling_hooks() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        let driver = fast_driver(Duration::from_millis(30), Duration::from_secs(3));
        let (hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, mut rx) = TurnUpdates::channel();
        tokio::spawn(async move {
            // After the first `done` poll (at 30 ms) set the grace.
            tokio::time::sleep(Duration::from_millis(400)).await;
            let prompt = serde_json::json!({
                "session_id": "abc12345-1111",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            });
            hooks_tx.send(HookEvent::now(prompt)).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            hooks_tx
                .send(stop_event("late reply", Instant::now()))
                .unwrap();
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                Some(Instant::now()),
                &mut hooks,
                &updates,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Reply("late reply".into()));
        let mut saw_needs_you = false;
        while let Ok(update) = rx.try_recv() {
            saw_needs_you |= matches!(update, TurnUpdate::NeedsYou(_));
        }
        assert!(saw_needs_you, "the permission prompt was swallowed");
    }

    /// A turn watched again after a restart that landed before its session
    /// id was looked up learns the id from the poll.
    #[tokio::test]
    async fn a_rewatch_by_bg_id_learns_the_session_id() {
        let fake = FakeClaude::new(&entry(None, "stopped"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let mut session = fake.session();
        session.session_id = None;
        let (_, mut rx) = watch_unhooked(&driver, &fake, session, Utc::now(), None).await;
        match rx.try_recv().unwrap() {
            TurnUpdate::Session(s) => assert_eq!(s.session_id.as_deref(), Some("abc12345-1111")),
            other => panic!("expected the session id, got {other:?}"),
        }
    }

    /// A `--stop` that lands while `claude --bg` runs: the launch still
    /// finishes, reports its bg id, and stops the session it started.
    #[tokio::test]
    async fn a_launch_that_outlives_its_turn_stops_its_session() {
        let fake = FakeClaude::new("[]");
        let (updates, _rx) = TurnUpdates::channel();
        assert_eq!(updates.stop(), None);
        let result = launch_reported(
            &fake.agent(),
            vec!["--bg".into(), "--".into(), "hi".into()],
            SessionRef::default(),
            &updates,
            &fast_exits(),
            false,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(fake.calls().contains("stop abc12345"), "{}", fake.calls());
        // Not stopped: the bg id is reported and nothing is stopped.
        let fake = FakeClaude::new("[]");
        let (updates, _rx) = TurnUpdates::channel();
        let Launched::Running(bg_id, session) = launch_reported(
            &fake.agent(),
            vec!["--bg".into(), "--".into(), "hi".into()],
            SessionRef::default(),
            &updates,
            &fast_exits(),
            false,
            None,
        )
        .await
        .unwrap() else {
            panic!("a first launch is never a copy");
        };
        assert_eq!(bg_id, "abc12345");
        assert_eq!(updates.stop(), Some(session));
        assert!(!fake.calls().contains("stop"), "{}", fake.calls());
    }

    /// The next turn is refused, not `claude stop`-ped, while the session
    /// works on its own.
    #[tokio::test]
    async fn a_working_session_is_not_stopped_for_a_new_turn() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "working"));
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, _rx) = TurnUpdates::channel();
        let result = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await;
        assert_eq!(result.unwrap_err(), TurnOutcome::Busy);
        assert!(!fake.calls().contains("stop"), "{}", fake.calls());
        // Done with a background task still running (`status: busy`): the
        // same refusal, so the task is not killed by the stop.
        let busy =
            entry(Some(GONE_PID), "done").replace(r#""state":"#, r#""status":"busy","state":"#);
        let fake_busy = FakeClaude::new(&busy);
        let result = driver
            .next_turn(&fake_busy.agent(), &fake_busy.session(), "next", &updates)
            .await;
        assert_eq!(result.unwrap_err(), TurnOutcome::Busy);
        assert!(!fake_busy.calls().contains("stop"), "{}", fake_busy.calls());
        assert_eq!(
            driver.session_phase(&fake.agent(), &fake.session()).await,
            Ok(AgentPhase::Working)
        );
        // Done, its process gone (as after the stop of the next turn, which
        // `a_resume_waits_for_the_stopped_process_to_exit` runs): idle.
        let fake = FakeClaude::new(&entry(None, "done"));
        assert_eq!(
            driver.session_phase(&fake.agent(), &fake.session()).await,
            Ok(AgentPhase::Idle)
        );
    }

    /// The calls of the fake after the first one that starts with `prefix`.
    fn calls_after(calls: &str, prefix: &str) -> Vec<String> {
        calls
            .lines()
            .skip_while(|l| !l.starts_with(prefix))
            .skip(1)
            .map(str::to_string)
            .collect()
    }

    /// `claude stop` returns before the old process is killed: the resume
    /// waits until `claude agents` lists the session with no process, so it
    /// continues in place instead of starting a copy (a copy loses the
    /// hooks and `--remote-control`).
    #[tokio::test]
    async fn a_resume_waits_for_the_stopped_process_to_exit() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        fake.file("list-stopped.json", &entry(None, "done"));
        fake.file("alive-after-stop", "3");
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, _rx) = TurnUpdates::channel();
        let (session, _) = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert_eq!(session.bg_id.as_deref(), Some("abc12345"));
        let calls = fake.calls();
        let after_stop = calls_after(&calls, "stop abc12345");
        // Three listings still show the process, the fourth shows it gone,
        // and only then is the session resumed.
        assert_eq!(
            after_stop,
            [
                "agents --json --all",
                "agents --json --all",
                "agents --json --all",
                "agents --json --all",
                "--bg --resume abc12345-1111 -- next",
            ],
            "{calls}"
        );
    }

    /// A `sleep 0.4` child, and when it was spawned: a bound measured from
    /// then holds however slow the setup after it is, as the child exits
    /// 0.4 s after its spawn on a loaded machine too.
    fn sleeper() -> (std::process::Child, Instant) {
        let child = std::process::Command::new("sleep")
            .arg("0.4")
            .spawn()
            .unwrap();
        (child, Instant::now())
    }

    /// `claude agents` drops the pid at the stop, before the process has
    /// exited and Claude Code settled the session (measured): the resume
    /// waits for the listed process itself.
    #[tokio::test]
    async fn a_resume_waits_for_the_listed_process_itself() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new(&entry(Some(child.id()), "done"));
        fake.file("list-stopped.json", &entry(None, "done"));
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, _rx) = TurnUpdates::channel();
        driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "resumed {} ms in, while the process still ran",
            started.elapsed().as_millis()
        );
        // It exited (a zombie until now: exited counts, reaped or not).
        assert!(child.try_wait().unwrap().is_some());
        let calls = fake.calls();
        assert_eq!(
            calls_after(&calls, "stop abc12345"),
            ["agents --json --all", "--bg --resume abc12345-1111 -- next"],
            "{calls}"
        );
    }

    #[test]
    fn the_process_state_is_read_after_the_command_name() {
        assert_eq!(stat_state("2971692 (claude) S 2971682 1 1 0"), Some('S'));
        assert_eq!(stat_state("42 (a (weird) name) Z 1 0"), Some('Z'));
        assert_eq!(stat_state("garbage"), None);
        // Field 22 is the start time, after a name with spaces too.
        let stat = "2971692 (a (b) c) S 2971682 1 1 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 9 0 \
                    123456789 1000 2";
        assert_eq!(stat_start(stat), Some(123_456_789));
        assert_eq!(stat_start("1 (x) S 1 2"), None);
        assert!(ListedProcess::running_now(std::process::id()).is_some());
        assert!(ListedProcess::running_now(GONE_PID).is_none());
    }

    /// A remembered pid an unrelated process got since (another start time)
    /// is not taken for the session's process.
    #[test]
    fn a_reused_pid_is_not_the_listed_process() {
        let me = ListedProcess::running_now(std::process::id()).unwrap();
        assert!(me.running());
        let reused = ListedProcess {
            started: me.started + 1,
            ..me
        };
        assert!(!reused.running());
    }

    /// A stopped process that never exits fails the turn after the cap
    /// instead of resuming into a copy.
    #[tokio::test]
    async fn a_stop_that_never_lands_fails_the_turn() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        let mut driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        driver.exits.stop_wait = Duration::from_millis(100);
        let (updates, _rx) = TurnUpdates::channel();
        let result = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await;
        assert_eq!(
            result.unwrap_err(),
            TurnOutcome::Failed(format!("Agent 'jarvis': {STOP_TIMEOUT}"))
        );
        assert!(!fake.calls().contains("--bg"), "{}", fake.calls());
        assert_eq!(
            crate::agent::spoken::spoken_error(&format!("Agent 'jarvis': {STOP_TIMEOUT}")),
            "Claude Code did not stop the session's last run in time, so this turn was not sent."
        );
    }

    /// The note Claude Code prints when a resume ran in a copy.
    const COPY_NOTE: &str = "note: session abc12345 is already running in the background, so \
         this started a copy as copy0001. `claude attach abc12345` opens the original.\n";

    /// A resume that starts a copy anyway: the copy is stopped and removed
    /// (never reported, never adopted), the original is waited for again,
    /// and the second resume continues in place.
    #[tokio::test]
    async fn a_copy_is_removed_and_the_resume_tried_once_more() {
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        fake.file("list-stopped.json", &with_copy(&entry(None, "done"), None));
        fake.file(
            "bg-1.out",
            "backgrounded \u{b7} copy0001 \u{b7} whisrs-jarvis\n",
        );
        fake.file("bg-1.err", COPY_NOTE);
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, mut rx) = TurnUpdates::channel();
        let (session, _) = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert_eq!(session, fake.session());
        let calls = fake.calls();
        let resumes = calls
            .lines()
            .filter(|l| l.starts_with("--bg --resume abc12345-1111"))
            .count();
        assert_eq!(resumes, 2, "{calls}");
        let after_copy = calls_after(&calls, "--bg --resume");
        // The copy is listed (its process noted), stopped, waited for, and
        // only then removed.
        assert_eq!(
            after_copy[..4],
            [
                "agents --json --all",
                "stop copy0001",
                "agents --json --all",
                "rm copy0001"
            ],
            "{calls}"
        );
        // The original is stopped (harmless when dead) and waited for again.
        assert_eq!(after_copy[4], "stop abc12345", "{calls}");
        assert_eq!(after_copy[5], "agents --json --all", "{calls}");
        assert!(!calls.contains("rm abc12345"), "{calls}");
        // Only the original's ids were ever reported.
        while let Ok(update) = rx.try_recv() {
            if let TurnUpdate::Session(s) = update {
                assert_eq!(s.bg_id.as_deref(), Some("abc12345"), "{s:?}");
            }
        }
        // A copy is also told by its bg id alone, with no note.
        let fake = FakeClaude::new(&with_copy(&entry(None, "done"), None));
        fake.file(
            "bg-1.out",
            "backgrounded \u{b7} copy0001 \u{b7} whisrs-jarvis\n",
        );
        let (updates, _rx) = TurnUpdates::channel();
        driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert!(fake.calls().contains("rm copy0001"), "{}", fake.calls());
    }

    /// A second copy fails the turn and says how to go on; both copies are
    /// stopped and removed.
    #[tokio::test]
    async fn a_second_copy_fails_the_turn() {
        let fake = FakeClaude::new(&entry(None, "done"));
        for n in 1..=2 {
            fake.file(
                &format!("bg-{n}.out"),
                &format!("backgrounded \u{b7} copy000{n} \u{b7} whisrs-jarvis\n"),
            );
        }
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, mut rx) = TurnUpdates::channel();
        let result = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await;
        let message = cannot_continue(&fake.agent());
        assert_eq!(result.unwrap_err(), TurnOutcome::Failed(message.clone()));
        assert!(message.contains("`whisrs agent jarvis --new`"), "{message}");
        assert_eq!(
            crate::agent::spoken::spoken_error(&message),
            "Claude Code could not continue the session, so this turn was not sent."
        );
        let calls = fake.calls();
        for copy in ["copy0001", "copy0002"] {
            assert!(calls.contains(&format!("stop {copy}")), "{calls}");
            assert!(calls.contains(&format!("rm {copy}")), "{calls}");
        }
        assert!(rx.try_recv().is_err(), "a copy was reported");
    }

    /// The copy is the id its note names, else a printed bg id other than
    /// the session's; never the session's own bg id.
    #[test]
    fn a_copy_is_told_by_its_bg_id_or_its_note() {
        assert_eq!(parse_copy_note(COPY_NOTE).as_deref(), Some("copy0001"));
        assert_eq!(
            parse_copy_note("note: woke session 9e24892c with its saved options (--model)."),
            None
        );
        let ours = Some("abc12345");
        let copy = |session, printed, note| copy_id(session, printed, note);
        assert_eq!(copy(ours, "abc12345", None), None);
        assert_eq!(copy(ours, "copy0001", None).as_deref(), Some("copy0001"));
        // The note names the copy even when the session's own id is printed.
        assert_eq!(
            copy(ours, "abc12345", Some("copy0001")).as_deref(),
            Some("copy0001")
        );
        // The note wins over another printed id.
        assert_eq!(
            copy(ours, "copy0002", Some("copy0001")).as_deref(),
            Some("copy0001")
        );
        // A note that names the session itself never makes it the copy.
        assert_eq!(copy(ours, "abc12345", Some("abc12345")), None);
        assert_eq!(
            copy(ours, "copy0002", Some("abc12345")).as_deref(),
            Some("copy0002")
        );
        assert_eq!(
            copy(None, "abc12345", Some("copy0001")).as_deref(),
            Some("copy0001")
        );
        assert_eq!(copy(None, "abc12345", None), None);
    }

    /// `list` (one entry, as [`entry`] writes it) with a copy `copy0001`
    /// listed too: `claude agents` lists every session a launch started.
    fn with_copy(list: &str, pid: Option<u32>) -> String {
        let copy = entry(pid, "done")
            .replace("abc12345-1111", "copy0001-2222")
            .replace("abc12345", "copy0001");
        format!(
            "{},{}",
            list.trim_end_matches(']'),
            copy.trim_start_matches('[')
        )
    }

    /// The note of a resume that says it started a copy as the session
    /// itself (and prints its bg id).
    const SELF_COPY_NOTE: &str = "note: session abc12345 is already running in the background, \
         so this started a copy as abc12345.\n";

    /// A resume whose copy note names the session itself (and prints its bg
    /// id) may or may not have continued in place: the session is stopped
    /// and waited for, never removed (that would remove the session
    /// itself), and resumed once more. A second such note fails the turn
    /// like a second copy.
    #[tokio::test]
    async fn a_copy_note_naming_the_session_stops_it_and_resumes_again() {
        // Stopped before the turn (the first listing), running once resumed.
        let resumed = |fake: &FakeClaude| {
            fake.file("agents-1.json", &entry(None, "done"));
            fake.file("list-stopped.json", &entry(None, "done"));
            fake.file("bg-1.err", SELF_COPY_NOTE);
        };
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        resumed(&fake);
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, mut rx) = TurnUpdates::channel();
        let (session, _) = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert_eq!(session, fake.session());
        let calls = fake.calls();
        assert!(!calls.contains("rm "), "{calls}");
        let after_first = calls_after(&calls, "--bg --resume");
        // Listed (its process noted) before the stop, then waited for.
        assert_eq!(
            after_first,
            [
                "agents --json --all",
                "stop abc12345",
                "agents --json --all",
                "--bg --resume abc12345-1111 -- next",
            ],
            "{calls}"
        );
        // Only the second resume, which continued in place, was reported.
        let reported: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(reported, [TurnUpdate::Session(fake.session())]);

        // The note again on the second resume: the turn is not sent, and
        // what that resume started is stopped, not left running the turn.
        let fake = FakeClaude::new(&entry(Some(GONE_PID), "done"));
        resumed(&fake);
        fake.file("bg-2.err", SELF_COPY_NOTE);
        let (updates, _rx) = TurnUpdates::channel();
        let result = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await;
        assert_eq!(
            result.unwrap_err(),
            TurnOutcome::Failed(cannot_continue(&fake.agent()))
        );
        let calls = fake.calls();
        assert!(!calls.contains("rm "), "{calls}");
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.starts_with("--bg --resume"))
                .count(),
            2,
            "{calls}"
        );
        let after_second: Vec<&str> = calls
            .lines()
            .rev()
            .take_while(|l| !l.starts_with("--bg --resume"))
            .collect();
        assert_eq!(after_second.first(), Some(&"stop abc12345"), "{calls}");
        assert!(
            after_second[1..]
                .iter()
                .all(|l| *l == "agents --json --all"),
            "{calls}"
        );
    }

    /// The process an unclear resume started is listed before it is stopped,
    /// so the next resume waits until it exited: `claude stop` drops its pid
    /// from the listing at once, and a resume while it runs starts a copy.
    #[tokio::test]
    async fn an_unclear_resume_is_waited_for_before_resuming_again() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new(&entry(Some(child.id()), "done"));
        fake.file("agents-1.json", &entry(None, "done"));
        fake.file("list-stopped.json", &entry(None, "done"));
        fake.file("bg-1.err", SELF_COPY_NOTE);
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, _rx) = TurnUpdates::channel();
        driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "resumed again {} ms in, while the resumed process still ran",
            started.elapsed().as_millis()
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    /// A copy just started may not be listed yet: it is listed again until
    /// it shows, so its process is remembered and waited for, and removed
    /// only once that process exited (not at once, taken for removed).
    /// Nothing of it is remembered afterwards.
    #[tokio::test]
    async fn a_copy_not_listed_yet_is_waited_for_before_it_is_removed() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new(&with_copy(&entry(None, "done"), Some(child.id())));
        fake.file("agents-1.json", &entry(None, "done"));
        fake.file("list-stopped.json", &with_copy(&entry(None, "done"), None));
        // The second listing shows the copy; under a loaded test run two
        // fake `claude` calls can take longer than `fast_exits`' 50 ms, so
        // this test waits as long as it takes (the wait ends once listed).
        let exits = Exits::new(
            Duration::from_millis(5),
            Duration::from_secs(5),
            Duration::from_millis(5),
            Duration::from_millis(1),
            Duration::from_secs(5),
        );
        exits.discard_copy(&fake.agent(), "copy0001").await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "removed {} ms in, while the copy still ran",
            started.elapsed().as_millis()
        );
        assert!(child.try_wait().unwrap().is_some());
        let calls = fake.calls();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "agents --json --all",
                "agents --json --all",
                "stop copy0001",
                "agents --json --all",
                "rm copy0001"
            ],
            "{calls}"
        );
        assert!(!exits.remembers("copy0001"));
    }

    /// A session no listing shows while a process remembered for it still
    /// runs is not gone yet: the wait goes by that process. Once it exited,
    /// the session is gone.
    #[tokio::test]
    async fn an_unlisted_session_is_gone_only_once_its_process_exited() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new("[]");
        let exits = fast_exits();
        let listed = find_session(
            &parse_agent_list(&entry(Some(child.id()), "working")).unwrap(),
            "abc12345",
        )
        .unwrap();
        exits.note("abc12345", &listed);
        assert_eq!(
            exits.wait_for_exit(&fake.agent(), "abc12345").await,
            Ok(Stopped::Gone)
        );
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "gone {} ms in, while its process still ran",
            started.elapsed().as_millis()
        );
        assert!(child.try_wait().unwrap().is_some());
        assert!(!exits.remembers("abc12345"));
    }

    /// What the driver remembers of a session goes when the runtime forgets
    /// the session (`--new`, `--reset`), when a poll finds it removed, and
    /// after a copy was discarded even when its wait ran out; a live
    /// process would otherwise stay remembered for good.
    #[tokio::test]
    async fn remembered_processes_are_forgotten_with_their_session() {
        let me = || ListedProcess::running_now(std::process::id()).unwrap();
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let fake = FakeClaude::new("[]");
        driver.exits.seen().insert("abc12345".into(), vec![me()]);
        driver.forget(&fake.session());
        assert!(!driver.exits.remembers("abc12345"));
        driver.forget(&SessionRef::default());

        // A poll finds the session removed.
        driver.exits.seen().insert("abc12345".into(), vec![me()]);
        let (outcome, _) = watch_unhooked(&driver, &fake, fake.session(), Utc::now(), None).await;
        assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
        assert!(!driver.exits.remembers("abc12345"));

        // A copy whose process outlives the wait (this test process).
        let fake = FakeClaude::new(&with_copy(&entry(None, "done"), Some(std::process::id())));
        let mut exits = fast_exits();
        exits.stop_wait = Duration::from_millis(50);
        exits.discard_copy(&fake.agent(), "copy0001").await.unwrap();
        assert!(fake.calls().contains("rm copy0001"), "{}", fake.calls());
        assert!(!exits.remembers("copy0001"));
    }

    /// A copy is removed only once its process exited (`claude rm` right
    /// after `claude stop` lands while it still runs), and a removal that
    /// fails names the copy.
    #[tokio::test]
    async fn a_copy_is_removed_after_its_process_exited() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new(&with_copy(&entry(None, "done"), Some(child.id())));
        fake.file("list-stopped.json", &with_copy(&entry(None, "done"), None));
        // `claude rm` fails: the error names the copy.
        fake.file("rm-fails", "");
        let e = fast_exits()
            .discard_copy(&fake.agent(), "copy0001")
            .await
            .unwrap_err();
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "removed {} ms in, while the copy still ran",
            started.elapsed().as_millis()
        );
        assert!(child.try_wait().unwrap().is_some());
        assert!(e.contains("copy0001") && e.contains("cannot remove"), "{e}");
    }

    /// A session stopped moments ago (`whisrs agent <name> --stop`) is
    /// listed with no pid while its process still exits: the next turn
    /// waits for the process the stop's listing showed, then resumes in
    /// place, with no second `claude stop`.
    #[tokio::test]
    async fn a_resume_after_a_stop_waits_for_the_remembered_process() {
        let (mut child, started) = sleeper();
        let fake = FakeClaude::new(&entry(Some(child.id()), "done"));
        fake.file("list-stopped.json", &entry(None, "done"));
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (agent, session) = (fake.agent(), fake.session());
        driver.stop(&agent, &session).await.unwrap();
        let (updates, _rx) = TurnUpdates::channel();
        driver
            .next_turn(&agent, &session, "next", &updates)
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "resumed {} ms in, while the stopped process still ran",
            started.elapsed().as_millis()
        );
        assert!(child.try_wait().unwrap().is_some());
        let calls = fake.calls();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "agents --json --all",
                "stop abc12345",
                "agents --json --all",
                "--bg --resume abc12345-1111 -- next"
            ],
            "{calls}"
        );
        // Its exit was seen: the turn after it does not wait again.
        assert!(!driver.exits.remembers("abc12345"));
    }

    /// A wait that ran out leaves the process remembered: the next turn,
    /// which finds the session listed with no pid, waits for it too.
    #[tokio::test]
    async fn a_wait_that_ran_out_is_waited_for_on_the_next_turn() {
        let mut child = std::process::Command::new("sleep")
            .arg("0.5")
            .spawn()
            .unwrap();
        let fake = FakeClaude::new(&entry(Some(child.id()), "done"));
        fake.file("list-stopped.json", &entry(None, "done"));
        let mut driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        driver.exits.stop_wait = Duration::from_millis(50);
        let (agent, session) = (fake.agent(), fake.session());
        let (updates, _rx) = TurnUpdates::channel();
        let result = driver.next_turn(&agent, &session, "next", &updates).await;
        assert_eq!(
            result.unwrap_err(),
            TurnOutcome::Failed(format!("Agent 'jarvis': {STOP_TIMEOUT}"))
        );
        assert!(driver.exits.remembers("abc12345"));
        driver.exits.stop_wait = Duration::from_secs(5);
        driver
            .next_turn(&agent, &session, "next", &updates)
            .await
            .unwrap();
        assert!(
            child.try_wait().unwrap().is_some(),
            "resumed while the stopped process still ran"
        );
        assert!(fake.calls().contains("--bg --resume"), "{}", fake.calls());
    }

    /// A remembered pid that an unrelated process got since does not hold
    /// up the turn: it resumes at once.
    #[tokio::test]
    async fn a_reused_pid_does_not_hold_up_a_turn() {
        let fake = FakeClaude::new(&entry(None, "done"));
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let me = ListedProcess::running_now(std::process::id()).unwrap();
        driver.exits.seen().insert(
            "abc12345".into(),
            vec![ListedProcess {
                started: me.started + 1,
                ..me
            }],
        );
        let started = Instant::now();
        let (updates, _rx) = TurnUpdates::channel();
        driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "held up {} ms by an unrelated process",
            started.elapsed().as_millis()
        );
        assert!(!driver.exits.remembers("abc12345"));
    }

    /// A turn watched again after a restart that landed before its launch
    /// printed a bg id: the launch may have started the session anyway, so
    /// the message says whisrs lost track of it and does not invite the
    /// instruction again.
    #[tokio::test]
    async fn a_restart_during_a_first_launch_does_not_invite_a_repeat() {
        let fake = FakeClaude::new("[]");
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let session = SessionRef::default();
        let (outcome, _) = watch_unhooked(&driver, &fake, session, Utc::now(), None).await;
        let TurnOutcome::Failed(message) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert_eq!(message, format!("Agent 'jarvis': {LOST_TRACK}"));
        assert!(message.contains("may still be running"), "{message}");
        assert!(!message.contains("say it again"), "{message}");
        assert_eq!(
            crate::agent::spoken::spoken_error(&message),
            "Whisrs lost track of that turn after a restart, and it may still be running."
        );
        // Nothing was asked of Claude Code: no session is looked up by name.
        assert_eq!(fake.calls(), "");
    }

    /// With no whisrs turn running, a Stop is the session's own reply (with
    /// nothing in it, the end of that work) and a permission prompt is the
    /// work it does on its own waiting for the user. Anything else is
    /// ignored.
    #[tokio::test]
    async fn idle_events_are_replies_or_permission_prompts() {
        let fake = FakeClaude::new("[]");
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let session = fake.session();
        let stop = stop_event("on its own", Instant::now()).payload;
        assert_eq!(
            driver.idle_event(&stop, &session).await,
            IdleEvent::Reply("on its own".into())
        );
        let prompt = serde_json::json!({
            "session_id": "abc12345-1111",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
        });
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        std::fs::write(
            fake.dir.path().join("transcript.jsonl"),
            format!("{line}\n"),
        )
        .unwrap();
        assert_eq!(
            driver.idle_event(&prompt, &session).await,
            IdleEvent::NeedsYou("Waiting for your approval to use Bash.".into())
        );
        let idle = serde_json::json!({
            "session_id": "abc12345-1111",
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
        });
        assert_eq!(driver.idle_event(&idle, &session).await, IdleEvent::Ignored);
        let empty = stop_event("  ", Instant::now()).payload;
        assert_eq!(driver.idle_event(&empty, &session).await, IdleEvent::Ended);
        let bare = serde_json::json!({
            "session_id": "abc12345-1111",
            "hook_event_name": "Stop",
        });
        assert_eq!(driver.idle_event(&bare, &session).await, IdleEvent::Ended);
    }

    /// `whisrs agent` asks `claude agents` once for every agent (it lists
    /// every session), not once per agent.
    #[tokio::test]
    async fn the_list_asks_claude_once_for_every_agent() {
        let list = r#"[
            {"pid":1,"id":"abc12345","kind":"background","sessionId":"abc12345-1111","state":"working"},
            {"pid":2,"id":"def67890","kind":"background","sessionId":"def67890-2222","state":"blocked"},
            {"id":"0a1b2c3d","kind":"background","sessionId":"0a1b2c3d-3333","state":"done"}
        ]"#;
        let fake = FakeClaude::new(list);
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let agent = std::sync::Arc::new(fake.agent());
        let bg = |id: &str| SessionRef {
            bg_id: Some(id.into()),
            ..SessionRef::default()
        };
        let sessions = vec![
            (std::sync::Arc::clone(&agent), bg("abc12345")),
            (std::sync::Arc::clone(&agent), bg("def67890")),
            (std::sync::Arc::clone(&agent), bg("0a1b2c3d")),
            (std::sync::Arc::clone(&agent), bg("gone0000")),
            (std::sync::Arc::clone(&agent), SessionRef::default()),
        ];
        let phases = driver.session_phases(&sessions).await;
        assert_eq!(
            phases,
            vec![
                Ok(AgentPhase::Working),
                Ok(AgentPhase::NeedsYou),
                Ok(AgentPhase::Idle),
                Ok(AgentPhase::Idle),
                Ok(AgentPhase::Idle),
            ]
        );
        let calls = fake.calls();
        assert_eq!(calls.lines().count(), 1, "{calls}");
        assert_eq!(calls.trim(), "agents --json --all");
    }
}
