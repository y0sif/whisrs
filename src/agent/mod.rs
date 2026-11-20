//! Agent mode: send a voice (or typed) instruction to a coding agent session
//! running in a configured directory, and hear a short reply when its turn
//! ends (`[[agents]]`).
//!
//! This module holds what both binaries share: the `[[agents]]` config and
//! its validation, the IPC types, the driver abstraction every harness
//! implements and the default spoken-reply instruction, plus the pure parts
//! of what an agent says and shows while it works ([`progress`],
//! [`display`]). The runtime that owns the sessions lives in the daemon
//! (`src/daemon/agent.rs`).
//!
//! Two driver shapes fit [`Driver`]: Claude Code's turn is launched as a
//! background session and ends on a hook event or a poll
//! ([`claude::ClaudeDriver`]); a process-per-turn driver
//! ([`process::ProcessDriver`], OpenCode's [`opencode::OpenCode`], Codex's
//! [`codex::Codex`] and Copilot's [`copilot::Copilot`]) spawns one child per
//! turn, reads the session id and the final message from its JSON output,
//! returns when the child exits, and stops the child when the turn is
//! stopped or dropped. The runtime only ever calls the [`Driver`] methods.

pub mod claude;
pub mod claude_tail;
pub mod codex;
pub mod copilot;
pub mod display;
pub mod opencode;
pub mod process;
pub mod progress;
pub mod spoken;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::hotkey;
use progress::Feedback;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// One `[[agents]]` entry as written in `config.toml`.
///
/// Every field but `name` and `dir` has a default, and `harness` and
/// `permissions` are plain strings checked by [`resolve_agents`], so an entry
/// with an unknown value is skipped with a warning instead of failing the
/// whole config parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Unique, `[a-z0-9_-]`. Used by `whisrs agent <name>`.
    #[serde(default)]
    pub name: String,
    /// Optional global hotkey, same syntax as `[hotkeys]`: a voice turn.
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Optional global hotkey for a voice turn that starts a fresh session
    /// (`whisrs agent <name> --new`). Same syntax and clash checks as
    /// `hotkey`.
    #[serde(default)]
    pub new_session_hotkey: Option<String>,
    /// Which coding agent: `claude` (default), `opencode`, `codex` or
    /// `copilot`.
    #[serde(default = "default_harness")]
    pub harness: String,
    /// The directory the agent works in. `~` is expanded, and a relative
    /// path is taken from the daemon's working directory; it must exist.
    #[serde(default)]
    pub dir: String,
    /// `auto` (default), `bypass` or `ask`, mapped per harness.
    #[serde(default = "default_permissions")]
    pub permissions: String,
    /// The harness binary. Default: the harness name on `PATH`.
    #[serde(default)]
    pub command: Option<String>,
    /// Extra arguments, passed on the first turn of a session (Codex and
    /// Copilot: on every turn, since a resumed turn keeps none of them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Replaces the built-in voice instruction ([`DEFAULT_REPLY_INSTRUCTION`]).
    #[serde(default)]
    pub reply_instruction: Option<String>,
    /// How much the agent says while it works: `summary` (only the reply),
    /// `brief` (default: an acknowledgement, then the reply) or
    /// `conversational` (also updates at real milestones). See
    /// [`progress::Feedback`].
    #[serde(default = "default_feedback")]
    pub feedback: String,
}

fn default_harness() -> String {
    "claude".to_string()
}

fn default_permissions() -> String {
    "auto".to_string()
}

fn default_feedback() -> String {
    Feedback::default().as_str().to_string()
}

/// The coding agent an entry drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    /// Claude Code (`claude --bg`).
    Claude,
    /// OpenCode 2 (`opencode run`, a process per turn).
    OpenCode,
    /// Codex CLI (`codex exec`, a process per turn).
    Codex,
    /// GitHub Copilot CLI (`copilot` in prompt mode, a process per turn).
    Copilot,
}

impl Harness {
    /// Every harness this build drives, for messages.
    pub const ALL: &'static [Harness] = &[
        Harness::Claude,
        Harness::OpenCode,
        Harness::Codex,
        Harness::Copilot,
    ];

    pub fn parse(s: &str) -> Option<Harness> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Harness::Claude),
            "opencode" => Some(Harness::OpenCode),
            "codex" => Some(Harness::Codex),
            "copilot" => Some(Harness::Copilot),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::OpenCode => "opencode",
            Harness::Codex => "codex",
            Harness::Copilot => "copilot",
        }
    }

    /// The binary looked up on `PATH` when `command` is unset.
    pub fn default_command(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::OpenCode => "opencode",
            Harness::Codex => "codex",
            Harness::Copilot => "copilot",
        }
    }

    /// The harness's name in messages.
    pub fn label(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::OpenCode => "OpenCode",
            Harness::Codex => "Codex",
            Harness::Copilot => "Copilot",
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much the agent may do without asking, harness-neutral. Each driver
/// maps it to its own flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permissions {
    /// The harness decides what is safe (Claude: auto mode).
    Auto,
    /// No permission checks at all.
    Bypass,
    /// Ask before acting.
    Ask,
}

impl Permissions {
    pub fn parse(s: &str) -> Option<Permissions> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Permissions::Auto),
            "bypass" => Some(Permissions::Bypass),
            "ask" => Some(Permissions::Ask),
            _ => None,
        }
    }
}

/// A validated `[[agents]]` entry, ready to drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    /// The configured hotkey, if any (validity is checked by the listener).
    pub hotkey: Option<String>,
    /// The hotkey of a fresh-session voice turn, if any.
    pub new_session_hotkey: Option<String>,
    pub harness: Harness,
    /// `dir` as an absolute path ([`absolute_dir`]); an existing directory
    /// at load time.
    pub dir: PathBuf,
    pub permissions: Permissions,
    /// The binary to run: `command`, or the harness default.
    pub command: String,
    pub args: Vec<String>,
    /// The voice instruction: `reply_instruction`, or the default, then the
    /// request for interims of `feedback` ([`progress::feedback_delta`]).
    pub instruction: String,
    /// How much the agent says while it works.
    pub feedback: Feedback,
}

/// Whether `name` is a valid agent name: non-empty, `[a-z0-9_-]`.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Expand a leading `~` (`~` or `~/...`) to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// An agent's `dir` as an absolute path: `~` expanded ([`expand_tilde`]),
/// and a relative path joined to `cwd`, the daemon's working directory (`None`
/// when it has none, which leaves the path relative). A harness is started
/// in the dir and some take their directory from `PWD` instead (OpenCode),
/// which must then be absolute. `.` components are dropped; `..` and
/// symlinks are kept as written.
pub fn absolute_dir(dir: &str, cwd: Option<&Path>) -> PathBuf {
    let dir = expand_tilde(dir);
    let dir = match cwd {
        Some(cwd) if dir.is_relative() => cwd.join(dir),
        _ => dir,
    };
    dir.components().collect()
}

/// Validate `[[agents]]`: the usable agents, plus a warning for each entry
/// skipped or partly ignored. Shared by `Config::validate` (which logs the
/// warnings at startup) and the daemon runtime (which drives the agents), so
/// the two never disagree about which entries exist.
///
/// Skipped: a missing, invalid or duplicate name, an unknown harness or
/// permissions value, and a missing or nonexistent dir. An invalid hotkey is
/// warned about but keeps the entry (the CLI still reaches it), and so does
/// an unknown feedback level, which is `brief` then.
pub fn resolve_agents(entries: &[AgentConfig]) -> (Vec<Agent>, Vec<String>) {
    let cwd = std::env::current_dir().ok();
    resolve_agents_with(entries, cwd.as_deref(), |p| p.is_dir())
}

/// [`resolve_agents`] with the working directory and the directory check
/// injected, for tests.
fn resolve_agents_with(
    entries: &[AgentConfig],
    cwd: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> (Vec<Agent>, Vec<String>) {
    let mut agents: Vec<Agent> = Vec::new();
    let mut warnings = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.name.trim();
        if name.is_empty() {
            warnings.push(format!(
                "[[agents]] entry {} has no name, so it is skipped",
                index + 1
            ));
            continue;
        }
        if !valid_agent_name(name) {
            warnings.push(format!(
                "[[agents]] name '{name}' is skipped: use lowercase letters, digits, '-' and '_'"
            ));
            continue;
        }
        if agents.iter().any(|a| a.name == name) {
            warnings.push(format!(
                "[[agents]] has a duplicate name '{name}': only the first one is used"
            ));
            continue;
        }
        let Some(harness) = Harness::parse(&entry.harness) else {
            let known: Vec<&str> = Harness::ALL.iter().map(|h| h.as_str()).collect();
            warnings.push(format!(
                "[[agents]] '{name}' has an unknown harness '{}', so it is skipped (this build \
                 drives: {})",
                entry.harness,
                known.join(", ")
            ));
            continue;
        };
        let Some(permissions) = Permissions::parse(&entry.permissions) else {
            warnings.push(format!(
                "[[agents]] '{name}' has an unknown permissions value '{}', so it is skipped \
                 (use auto, bypass or ask)",
                entry.permissions
            ));
            continue;
        };
        if entry.dir.trim().is_empty() {
            warnings.push(format!(
                "[[agents]] '{name}' has no dir, so it is skipped: set dir to the project the \
                 agent works in"
            ));
            continue;
        }
        let dir = absolute_dir(&entry.dir, cwd);
        if !is_dir(&dir) {
            warnings.push(format!(
                "[[agents]] '{name}' is skipped: its dir {} does not exist",
                dir.display()
            ));
            continue;
        }
        let set = |spec: &Option<String>| {
            spec.as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        };
        let hotkey = set(&entry.hotkey);
        let new_session_hotkey = set(&entry.new_session_hotkey);
        for (key, spec, cli) in [
            ("hotkey", &hotkey, format!("whisrs agent {name}")),
            (
                "new_session_hotkey",
                &new_session_hotkey,
                format!("whisrs agent {name} --new"),
            ),
        ] {
            if let Some(spec) = spec {
                if let Err(e) = hotkey::parse_hotkey(spec) {
                    warnings.push(format!(
                        "[[agents]] '{name}' has an invalid {key} '{spec}': {e}. The agent still \
                         works through `{cli}`"
                    ));
                }
            }
        }
        let command = entry
            .command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| expand_tilde(c).to_string_lossy().into_owned())
            .unwrap_or_else(|| harness.default_command().to_string());
        let feedback = Feedback::parse(&entry.feedback).unwrap_or_else(|| {
            let known: Vec<&str> = Feedback::ALL.iter().map(|f| f.as_str()).collect();
            warnings.push(format!(
                "[[agents]] '{name}' has an unknown feedback value '{}', so it uses {} (use {})",
                entry.feedback,
                Feedback::default(),
                known.join(", ")
            ));
            Feedback::default()
        });
        let instruction = format!(
            "{}{}",
            entry
                .reply_instruction
                .as_deref()
                .map(str::trim)
                .filter(|i| !i.is_empty())
                .unwrap_or(DEFAULT_REPLY_INSTRUCTION),
            progress::feedback_delta(feedback)
        );
        agents.push(Agent {
            name: name.to_string(),
            hotkey,
            new_session_hotkey,
            harness,
            dir,
            permissions,
            command,
            args: entry.args.clone(),
            instruction,
            feedback,
        });
    }
    (agents, warnings)
}

/// The built-in voice instruction: Claude's appended system prompt,
/// OpenCode's first-message prefix, Codex's `developer_instructions` and
/// Copilot's custom instructions file. It asks for a
/// `<spoken>` reply, but whisrs never relies on it ([`spoken::spoken_reply`]).
/// The agent's feedback level appends its request for interims
/// ([`progress::feedback_delta`]).
pub const DEFAULT_REPLY_INSTRUCTION: &str = "You are being driven by voice through whisrs. The \
user spoke this request, and speech recognition can mishear words, so read it charitably; if a \
misheard word could make you do something destructive or irreversible, ask first. Work as you \
normally would. The user will hear your final message through text-to-speech rather than read \
it, so end every final message with a spoken reply inside <spoken></spoken> tags, written to be \
heard: lead with the outcome; one to three short sentences, under 40 words, up to 60 only when \
you need a decision; plain spoken words with no code, file paths, URLs, markdown, lists or \
symbols; say numbers the way a person would. If you need the user to decide or answer \
something, end with exactly one direct question; otherwise ask nothing and do not offer more \
help. Do not narrate your steps, apologize or repeat the request. Be calm, precise and brief, \
like a capable assistant reporting to the person they work for. Anything outside the tags stays \
on screen for when the user looks at the session.";

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// What a `{"cmd":"agent"}` request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentAction {
    /// `whisrs agent`: every configured agent and its state.
    List,
    /// `whisrs agent <name>`, or its hotkey: record a voice turn (a second
    /// press stops the recording). With `new` (`whisrs agent <name> --new`,
    /// or its `new_session_hotkey`) the transcript starts a fresh session.
    Toggle,
    /// `whisrs agent <name> --text "..."`: a typed turn; with `new`, in a
    /// fresh session.
    Send,
    /// `whisrs agent <name> --reset`: forget the session, start nothing.
    Reset,
    /// `whisrs agent <name> --stop`: stop the running turn, keep the session.
    Stop,
}

/// What an agent is doing, as `whisrs agent` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPhase {
    /// No turn running.
    Idle,
    /// A turn is running.
    Working,
    /// The turn waits for the user (a permission prompt).
    NeedsYou,
}

impl std::fmt::Display for AgentPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AgentPhase::Idle => "idle",
            AgentPhase::Working => "working",
            AgentPhase::NeedsYou => "needs-you",
        })
    }
}

/// One row of `whisrs agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub harness: String,
    pub phase: AgentPhase,
    /// The session id, once there is one: its first eight characters
    /// beside a bg id (Claude), else all of it, as the harness takes it
    /// (`opencode --session <id>`, `codex resume <id>`, `copilot
    /// --resume=<id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The id of the background process running the session (Claude: the
    /// id `claude attach`, `claude logs` and `claude stop` take), once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg_id: Option<String>,
}

/// The refusal for a turn while the agent's whisrs turn is still running.
/// Spoken and notified.
pub const STILL_WORKING: &str = "Still working on the last one.";

/// The refusal for a turn while the agent's session works on its own, with
/// no whisrs turn running (a background task it started, or a follow-up
/// after one). Spoken and notified; `whisrs agent <name> --stop` ends that
/// work.
pub const BACKGROUND_TASK: &str = "It's still running a background task.";

// ---------------------------------------------------------------------------
// Driver abstraction
// ---------------------------------------------------------------------------

/// What the runtime remembers about an agent's session between turns, and
/// persists in `agents.json`. Ids are only ever ones a driver learned from a
/// session whisrs started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    /// The harness's session id (Claude: the conversation UUID; OpenCode:
    /// `ses_...`, minted by whisrs; Codex: the thread UUID its first line
    /// names; Copilot: a UUID minted by whisrs).
    #[serde(default)]
    pub session_id: Option<String>,
    /// The id of the background process running it (Claude: the short id
    /// `claude --bg` prints). `None` for process-per-turn drivers.
    #[serde(default)]
    pub bg_id: Option<String>,
    /// Where the harness writes the conversation, once known.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// The id is known, but no turn has shown the agent working in the
    /// session yet (a text or a tool call): OpenCode's or Copilot's id,
    /// which whisrs mints and reports as the session's first turn starts,
    /// so a turn stopped or cut off before its first line keeps it. Until
    /// then every turn starts the session over on the same id (OpenCode:
    /// with the first turn's options and instruction; Copilot: with
    /// `--session-id`, never `--resume`). Never set for an id the harness
    /// named.
    #[serde(default)]
    pub pending: bool,
}

/// One turn to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRequest {
    /// What the user said or typed.
    pub text: String,
    /// When the turn was started (a reply older than this belongs to an
    /// earlier turn).
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Progress a driver reports while a turn runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnUpdate {
    /// The session ids changed (first turn, or a resume that started a copy).
    Session(SessionRef),
    /// The turn waits for the user. The text is a short spoken notice.
    NeedsYou(String),
    /// The turn is running again after a wait.
    Working,
    /// A reply the session's old process produced on its own before this
    /// turn's launch (work it finished by itself, like a follow-up after a
    /// background command): not this turn's reply, but spoken as one the
    /// session produced on its own. The text is the raw final message.
    SessionReply(String),
    /// What the turn does now, as a short label for the overlay ("Running
    /// a command", [`progress::activity_for_tool`]).
    Activity(String),
    /// The raw text of an assistant message a tool call followed, so known
    /// not to be the turn's final message: maybe an acknowledgement or a
    /// progress update ([`spoken::interim_line`], [`progress::ProgressGate`]).
    Interim(String),
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The agent's final message, raw (the runtime turns it into speech),
    /// from the harness's end-of-turn event.
    Reply(String),
    /// The final message, read without the end-of-turn event (a poll found
    /// the turn done). That event can still arrive after the turn ended,
    /// carrying the same message, and is then not spoken twice.
    PolledReply(String),
    /// The session works on its own (no whisrs turn running), so the turn
    /// was not sent: stopping that work for it would lose it.
    Busy,
    /// The turn could not run or did not finish. The text is actionable and
    /// goes to a notification as-is.
    Failed(String),
}

/// What a hook event for one of our sessions means while no whisrs turn
/// runs ([`Driver::idle_event`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    /// The session finished work it did on its own: its raw final message.
    Reply(String),
    /// That work ended with no message (a `Stop` with nothing in it): no
    /// longer working or waiting, and nothing to say.
    Ended,
    /// Work the session does on its own waits for a permission prompt. The
    /// text is a short spoken notice.
    NeedsYou(String),
    /// Anything else.
    Ignored,
}

/// A hook payload as the runtime routes it, with when the daemon received
/// it: a driver that just restarted a session's process can tell the old
/// process's events (received before the launch) from the new one's.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub received: Instant,
    pub payload: serde_json::Value,
}

impl HookEvent {
    /// An event received now.
    pub fn now(payload: serde_json::Value) -> Self {
        Self {
            received: Instant::now(),
            payload,
        }
    }
}

/// What a running turn and the runtime share outside the update channel,
/// under one lock.
#[derive(Debug, Default)]
struct TurnShared {
    /// The newest session ids the turn reported.
    latest: Option<SessionRef>,
    /// The runtime stopped the turn (`--stop`).
    stopped: bool,
}

/// Where a running turn reports progress. A session change is also kept
/// here synchronously, so `--stop` reads the newest ids even before the
/// runtime applied the update (a bg id whose session id is still being
/// looked up); and a launch that finishes after the stop learns of it
/// ([`TurnUpdates::report_session`]) and stops what it started.
#[derive(Debug, Clone)]
pub struct TurnUpdates {
    tx: mpsc::UnboundedSender<TurnUpdate>,
    shared: Arc<Mutex<TurnShared>>,
}

impl TurnUpdates {
    /// A handle and the receiver the runtime applies updates from.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<TurnUpdate>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                shared: Arc::new(Mutex::new(TurnShared::default())),
            },
            rx,
        )
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, TurnShared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report progress. A [`TurnUpdate::Session`] is also kept as the
    /// newest ids.
    pub fn send(&self, update: TurnUpdate) {
        if let TurnUpdate::Session(session) = &update {
            self.shared().latest = Some(session.clone());
        }
        let _ = self.tx.send(update);
    }

    /// Report new session ids and tell whether the turn was stopped
    /// meanwhile, in one step with [`TurnUpdates::stop`]: either the stop
    /// sees these ids, or this returns `true` and the caller stops the
    /// session it just started.
    pub fn report_session(&self, session: SessionRef) -> bool {
        let stopped = {
            let mut shared = self.shared();
            shared.latest = Some(session.clone());
            shared.stopped
        };
        let _ = self.tx.send(TurnUpdate::Session(session));
        stopped
    }

    /// Whether the turn was stopped, in one step with [`TurnUpdates::stop`]
    /// as [`TurnUpdates::report_session`] is: a process registered for the
    /// stop before this returns `false` is one the stop finds.
    pub fn stopped(&self) -> bool {
        self.shared().stopped
    }

    /// Mark the turn stopped and return the newest ids it reported.
    pub fn stop(&self) -> Option<SessionRef> {
        let mut shared = self.shared();
        shared.stopped = true;
        shared.latest.clone()
    }

    /// The newest ids the turn reported, if any.
    pub fn latest(&self) -> Option<SessionRef> {
        self.shared().latest.clone()
    }
}

/// The channels between a running turn and the runtime.
pub struct TurnIo {
    /// Hook events the runtime routed to this turn
    /// ([`Driver::hook_matches`]).
    pub hooks: mpsc::UnboundedReceiver<HookEvent>,
    /// Progress for the runtime to record and announce.
    pub updates: TurnUpdates,
}

/// A coding-agent harness. One implementation per harness; adding one must
/// not change the runtime.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Run one turn from launch to its end. `session` is the agent's current
    /// session (empty for a new one). Dropped (aborted) on `--stop`, so a
    /// child it spawns must be stopped when it is dropped (`kill_on_drop`,
    /// or the stop of [`process::ProcessDriver`]).
    async fn run_turn(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome;

    /// Watch a turn launched before the daemon restarted until it ends. A
    /// process-per-turn driver lost its child with the daemon and reports
    /// that.
    async fn rewatch(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome;

    /// Stop the running turn and keep the session. Called after the turn's
    /// task was aborted; returns once the turn's process is gone.
    async fn stop(&self, agent: &Agent, session: &SessionRef) -> Result<(), String>;

    /// The runtime forgot `session` (`--new`, `--reset`): drop what the
    /// driver remembers to continue it later. The session itself is left
    /// alone; it keeps running in the harness. A process-per-turn driver
    /// remembers nothing between turns.
    fn forget(&self, session: &SessionRef);

    /// Whether a hook payload (`whisrs agent-hook`) belongs to `session`.
    /// Matched by stored ids only, never by name.
    fn hook_matches(&self, payload: &serde_json::Value, session: &SessionRef) -> bool;

    /// What `session` does while no whisrs turn runs: `Working` when the
    /// harness shows it running on its own (a follow-up after a background
    /// command, or the user typing in it), `NeedsYou` when that work waits
    /// for a prompt, else `Idle`. A new turn is refused while it works, so
    /// the work is not stopped for it. A process-per-turn driver has no
    /// session between turns and answers `Idle`.
    async fn session_phase(
        &self,
        agent: &Agent,
        session: &SessionRef,
    ) -> Result<AgentPhase, String>;

    /// [`Driver::session_phase`] for several agents at once (`whisrs
    /// agent`), one answer per entry in order. A harness that lists every
    /// session in one call asks once, not once per agent.
    async fn session_phases(
        &self,
        sessions: &[(Arc<Agent>, SessionRef)],
    ) -> Vec<Result<AgentPhase, String>>;

    /// What a hook payload for `session` means while no whisrs turn runs:
    /// the final message of work the session finished on its own (spoken
    /// like a reply), that work ending with nothing to say, a permission
    /// prompt it waits on (needs-you), or nothing.
    async fn idle_event(&self, payload: &serde_json::Value, session: &SessionRef) -> IdleEvent;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            hotkey: None,
            new_session_hotkey: None,
            harness: "claude".to_string(),
            dir: "/proj".to_string(),
            permissions: "auto".to_string(),
            command: None,
            args: Vec::new(),
            reply_instruction: None,
            feedback: "brief".to_string(),
        }
    }

    fn resolve(entries: &[AgentConfig]) -> (Vec<Agent>, Vec<String>) {
        resolve_agents_with(entries, Some(Path::new("/daemon/cwd")), |p| {
            p == Path::new("/proj")
        })
    }

    #[test]
    fn a_minimal_entry_takes_the_defaults() {
        let parsed: crate::Config =
            toml::from_str("[[agents]]\nname = \"jarvis\"\ndir = \"/proj\"\n").unwrap();
        let (agents, warnings) = resolve(&parsed.agents);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            agents,
            vec![Agent {
                name: "jarvis".to_string(),
                hotkey: None,
                new_session_hotkey: None,
                harness: Harness::Claude,
                dir: PathBuf::from("/proj"),
                permissions: Permissions::Auto,
                command: "claude".to_string(),
                args: Vec::new(),
                instruction: format!(
                    "{DEFAULT_REPLY_INSTRUCTION}{}",
                    progress::feedback_delta(Feedback::Brief)
                ),
                feedback: Feedback::Brief,
            }]
        );
    }

    #[test]
    fn every_field_is_read() {
        let parsed: crate::Config = toml::from_str(
            r#"
[[agents]]
name = "jarvis"
hotkey = "Super+Shift+J"
new_session_hotkey = "Super+Shift+N"
harness = "claude"
dir = "/proj"
permissions = "ask"
command = "/opt/claude/bin/claude"
args = ["--remote-control", "jarvis"]
reply_instruction = "Be terse."
feedback = "Conversational"
"#,
        )
        .unwrap();
        let (agents, warnings) = resolve(&parsed.agents);
        assert!(warnings.is_empty(), "{warnings:?}");
        let a = &agents[0];
        assert_eq!(a.hotkey.as_deref(), Some("Super+Shift+J"));
        assert_eq!(a.new_session_hotkey.as_deref(), Some("Super+Shift+N"));
        assert_eq!(a.permissions, Permissions::Ask);
        assert_eq!(a.command, "/opt/claude/bin/claude");
        assert_eq!(a.args, ["--remote-control", "jarvis"]);
        assert_eq!(a.feedback, Feedback::Conversational);
        assert_eq!(
            a.instruction,
            format!(
                "Be terse.{}",
                progress::feedback_delta(Feedback::Conversational)
            )
        );
    }

    /// `summary` adds nothing to the instruction; an unknown level warns and
    /// is `brief`, and the agent is kept.
    #[test]
    fn feedback_shapes_the_instruction_and_unknown_is_brief() {
        let mut e = entry("jarvis");
        e.feedback = " SUMMARY ".into();
        e.reply_instruction = Some(" Be terse. ".into());
        let (agents, warnings) = resolve(&[e]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(agents[0].feedback, Feedback::Summary);
        assert_eq!(agents[0].instruction, "Be terse.");

        let mut e = entry("jarvis");
        e.feedback = "chatty".into();
        let (agents, warnings) = resolve(&[e]);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].feedback, Feedback::Brief);
        assert!(agents[0]
            .instruction
            .ends_with(progress::feedback_delta(Feedback::Brief)));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("unknown feedback value 'chatty'")
                && warnings[0].contains("uses brief")
                && warnings[0].contains("summary, brief, conversational"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn invalid_entries_are_skipped_with_a_warning() {
        let mut bad_name = entry("Jarvis!");
        bad_name.dir = "/proj".into();
        let mut no_dir = entry("nodir");
        no_dir.dir = String::new();
        let mut missing_dir = entry("missing");
        missing_dir.dir = "/nope".into();
        let mut unknown_harness = entry("oc");
        unknown_harness.harness = "aider".into();
        let mut unknown_perm = entry("perm");
        unknown_perm.permissions = "bypassPermissions".into();
        let entries = vec![
            entry(""),
            bad_name,
            entry("jarvis"),
            entry("jarvis"),
            no_dir,
            missing_dir,
            unknown_harness,
            unknown_perm,
        ];
        let (agents, warnings) = resolve(&entries);
        assert_eq!(
            agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["jarvis"]
        );
        assert_eq!(warnings.len(), 7, "{warnings:#?}");
        assert!(warnings[0].contains("has no name"));
        assert!(warnings[1].contains("'Jarvis!'"));
        assert!(warnings[2].contains("duplicate name 'jarvis'"));
        assert!(warnings[3].contains("has no dir"));
        assert!(warnings[4].contains("/nope does not exist"));
        assert!(
            warnings[5].contains("unknown harness 'aider'")
                && warnings[5].contains("drives: claude, opencode, codex, copilot"),
            "{}",
            warnings[5]
        );
        assert!(warnings[6].contains("unknown permissions value 'bypassPermissions'"));
    }

    #[test]
    fn an_invalid_hotkey_warns_but_keeps_the_agent() {
        let mut e = entry("jarvis");
        e.hotkey = Some("Super+Nope".into());
        e.new_session_hotkey = Some("Super+Nada".into());
        let (agents, warnings) = resolve(&[e]);
        assert_eq!(agents.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("invalid hotkey 'Super+Nope'"));
        assert!(
            warnings[1].contains("invalid new_session_hotkey 'Super+Nada'")
                && warnings[1].contains("`whisrs agent jarvis --new`"),
            "{}",
            warnings[1]
        );
    }

    #[test]
    fn a_blank_hotkey_or_command_is_unset() {
        let mut e = entry("jarvis");
        e.hotkey = Some("  ".into());
        e.new_session_hotkey = Some("".into());
        e.command = Some(" ".into());
        let (agents, warnings) = resolve(&[e]);
        assert!(warnings.is_empty());
        assert_eq!(agents[0].hotkey, None);
        assert_eq!(agents[0].new_session_hotkey, None);
        assert_eq!(agents[0].command, "claude");
    }

    #[test]
    fn each_harness_entry_runs_its_own_binary_by_default() {
        for (written, harness, command, label) in [
            ("claude", Harness::Claude, "claude", "Claude Code"),
            (" OpenCode ", Harness::OpenCode, "opencode", "OpenCode"),
            ("Codex", Harness::Codex, "codex", "Codex"),
            (" Copilot", Harness::Copilot, "copilot", "Copilot"),
        ] {
            let mut e = entry("coder");
            e.harness = written.into();
            let (agents, warnings) = resolve(&[e]);
            assert!(warnings.is_empty(), "{written}: {warnings:?}");
            let [agent] = &agents[..] else {
                panic!("{written}: {agents:?}");
            };
            assert_eq!(agent.harness, harness, "{written}");
            assert_eq!(agent.command, command, "{written}");
            assert_eq!(harness.to_string(), command, "{written}");
            assert_eq!(harness.label(), label, "{written}");
        }
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs/~/x"), PathBuf::from("/abs/~/x"));
    }

    /// Every harness gets an absolute dir: OpenCode takes its directory from
    /// `PWD`, which a relative dir would leave pointing nowhere once the
    /// child is in it.
    #[test]
    fn a_relative_dir_is_taken_from_the_daemons_working_directory() {
        let cwd = Path::new("/home/u");
        assert_eq!(
            absolute_dir("Projects/site", Some(cwd)),
            PathBuf::from("/home/u/Projects/site")
        );
        assert_eq!(
            absolute_dir("./site/./src/", Some(cwd)),
            PathBuf::from("/home/u/site/src")
        );
        // `..` is kept: it means what the filesystem says.
        assert_eq!(
            absolute_dir("../site", Some(cwd)),
            PathBuf::from("/home/u/../site")
        );
        assert_eq!(absolute_dir("/proj/", Some(cwd)), PathBuf::from("/proj"));
        let home = dirs::home_dir().unwrap();
        assert_eq!(absolute_dir("~/Projects", Some(cwd)), home.join("Projects"));
        // No working directory to resolve against: left as written.
        assert_eq!(absolute_dir("site", None), PathBuf::from("site"));
        for harness in ["claude", "opencode", "codex", "copilot"] {
            let mut e = entry("rel");
            e.harness = harness.into();
            e.dir = "Projects".into();
            let (agents, warnings) = resolve_agents_with(&[e], Some(Path::new("/home/u")), |p| {
                p == Path::new("/home/u/Projects")
            });
            assert!(warnings.is_empty(), "{harness}: {warnings:?}");
            assert_eq!(
                agents[0].dir,
                PathBuf::from("/home/u/Projects"),
                "{harness}"
            );
            assert!(agents[0].dir.is_absolute());
        }
    }

    #[test]
    fn names_are_lowercase_ascii_words() {
        assert!(valid_agent_name("jarvis"));
        assert!(valid_agent_name("my_agent-2"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name("Jarvis"));
        assert!(!valid_agent_name("a b"));
        assert!(!valid_agent_name("ärger"));
    }

    #[test]
    fn harness_and_permissions_parse_case_insensitively() {
        assert_eq!(Harness::parse(" Claude "), Some(Harness::Claude));
        assert_eq!(Harness::parse("OpenCode"), Some(Harness::OpenCode));
        assert_eq!(Harness::parse(" CODEX"), Some(Harness::Codex));
        assert_eq!(Harness::parse("COPILOT "), Some(Harness::Copilot));
        assert_eq!(Harness::parse("aider"), None);
        for harness in Harness::ALL {
            assert_eq!(Harness::parse(harness.as_str()), Some(*harness));
        }
        assert_eq!(Permissions::parse("BYPASS"), Some(Permissions::Bypass));
        assert_eq!(Permissions::parse("yolo"), None);
    }

    #[test]
    fn agent_info_wire_format() {
        let info = AgentInfo {
            name: "jarvis".into(),
            harness: "claude".into(),
            phase: AgentPhase::NeedsYou,
            session: Some("cc7a37cf".into()),
            bg_id: Some("cc7a37cf".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(
            json,
            r#"{"name":"jarvis","harness":"claude","phase":"needs-you","session":"cc7a37cf","bg_id":"cc7a37cf"}"#
        );
        // An older daemon's row (no bg id) still parses.
        let old: AgentInfo = serde_json::from_str(
            r#"{"name":"jarvis","harness":"claude","phase":"idle","session":"cc7a37cf"}"#,
        )
        .unwrap();
        assert_eq!(old.bg_id, None);
        assert_eq!(serde_json::from_str::<AgentInfo>(&json).unwrap(), info);
        assert_eq!(AgentPhase::NeedsYou.to_string(), "needs-you");
    }

    /// `--stop` and a launch that finishes meanwhile agree in one step:
    /// either the stop sees the new ids, or the launch learns it was
    /// stopped.
    #[test]
    fn a_stop_and_a_late_launch_never_both_miss_the_session() {
        let session = SessionRef {
            session_id: None,
            bg_id: Some("cc7a37cf".into()),
            transcript_path: None,
            pending: false,
        };
        // The launch reports first: the stop gets the bg id.
        let (updates, mut rx) = TurnUpdates::channel();
        assert!(!updates.report_session(session.clone()));
        assert_eq!(updates.stop(), Some(session.clone()));
        assert_eq!(rx.try_recv().unwrap(), TurnUpdate::Session(session.clone()));
        // The stop comes first: the launch is told to stop what it started.
        let (updates, _rx) = TurnUpdates::channel();
        assert!(!updates.stopped());
        assert_eq!(updates.stop(), None);
        assert!(updates.stopped());
        assert!(updates.report_session(session.clone()));
        // Plain sends keep the newest ids too.
        let (updates, _rx) = TurnUpdates::channel();
        assert_eq!(updates.latest(), None);
        updates.send(TurnUpdate::Session(session.clone()));
        updates.send(TurnUpdate::Working);
        updates.send(TurnUpdate::Activity("Running a command".into()));
        assert_eq!(updates.latest(), Some(session.clone()));
        assert_eq!(updates.stop(), Some(session));
    }

    #[test]
    fn the_default_instruction_asks_for_spoken_tags() {
        assert!(DEFAULT_REPLY_INSTRUCTION.contains("<spoken></spoken>"));
        assert!(!DEFAULT_REPLY_INSTRUCTION.contains('\u{2014}'));
    }
}
