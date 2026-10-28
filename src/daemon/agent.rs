//! Agent mode runtime (`[[agents]]`): per-agent slots, running turns, the
//! reply queue, the IPC handlers and the hook events.
//!
//! Lives in [`DaemonContext`] behind its own lock, outside [`DaemonState`]:
//! the dictation state machine only gains the agent recording sink (a voice
//! turn records exactly like command mode's instruction, then hands the
//! transcript here). A turn runs on its own task, so the state machine is
//! back to Idle as soon as the transcript is ready and dictation keeps
//! working while agents run. Replies wait in a FIFO and are spoken when the
//! daemon is Idle and no recording captures (its system track would pick
//! them up). A reply the session produces on its own, with no whisrs turn
//! running (a background command finished and the agent went on), is spoken
//! the same way.
//!
//! While a turn runs, its driver reports what it does (an activity label
//! for the agent overlay) and the messages a tool call followed: an
//! acknowledgement or a progress update the agent wrote, spoken as the
//! agent's `feedback` level allows ([`ProgressGate`]). They wait in the same
//! FIFO under the same holds, but only for a while (they expire), never
//! come back after a recording stopped them, and give way to the reply.
//!
//! The agent overlay's model ([`Board`]) is owned by one task
//! ([`board_loop`]), fed by the turns, the reply queue and the agent the
//! voice belongs to ([`crate::context::StateBus`]); it publishes the rows on
//! `DaemonContext::agent_board`. A session that works or waits on its own,
//! with no whisrs turn running, shows there too, until a `Stop` or a listing
//! ([`idle_loop`]) says that ended.
//!
//! The runtime is harness-neutral: it calls [`Driver`] and never names a
//! harness outside [`AgentRuntime::new`].
//!
//! With no usable `[[agents]]` entry nothing here runs: no task, no process,
//! no file read or written.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};

use whisrs::agent::claude::ClaudeDriver;
use whisrs::agent::codex::Codex;
use whisrs::agent::copilot::Copilot;
use whisrs::agent::display::{AgentVisual, Board};
use whisrs::agent::opencode::OpenCode;
use whisrs::agent::process::{ProcessDriver, TurnScopes};
use whisrs::agent::progress::{InterimKind, ProgressGate};
use whisrs::agent::spoken::same_reply;
use whisrs::agent::store::{self, AgentRecord, AgentStore};
use whisrs::agent::{
    resolve_agents, spoken, Agent, AgentAction, AgentInfo, AgentPhase, Driver, Harness, HookEvent,
    IdleEvent, SessionRef, TurnIo, TurnOutcome, TurnRequest, TurnUpdate, TurnUpdates,
    BACKGROUND_TASK, STILL_WORKING,
};
use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::feedback;
use whisrs::state::Action;
use whisrs::{Config, Response, State};

use crate::command_mode::{claim_session, collect_instruction_audio};
use crate::context::{AgentRecordingContext, AgentVoice, DaemonContext, DaemonState};
use crate::notify::{send_notification, send_notification_for, truncate_preview};
use crate::pipeline::{
    format_api_error, format_no_microphone_error, report_nothing_heard, transcribe_batch_audio,
    BatchOptions, BatchOrigin,
};
#[cfg(feature = "recording")]
use crate::recording::{refuse_while_recording, DictationStart};
use crate::speak::{speak_text, stop_read_aloud, tts_backend, SpeakAttempt, SpeakOrigin};

/// How long a hook event no turn claimed is kept: a first turn's `Stop` can
/// arrive before `claude agents` has told us its session id.
const UNCLAIMED_TTL: Duration = Duration::from_secs(120);

/// At most this many unclaimed hook events are kept.
const UNCLAIMED_MAX: usize = 32;

/// How long a reply notification stays up.
const REPLY_NOTIFICATION_MS: u32 = 10_000;

/// Spoken when a voice turn's audio held nothing to send (too short,
/// silent, an echo of the prompt, or an empty transcript).
const DIDNT_CATCH: &str = "I didn't catch that.";

/// The longest instruction a refusal notification repeats, in bytes.
const REFUSED_INSTRUCTION_MAX: usize = 500;

/// What a turn that waits for you is called when it never said what it
/// waits for (its needs-you notice names that).
const WAITING_FOR_YOU: &str = "It's waiting for you.";

/// After the needs-you notice, when forgetting the session (`--new`,
/// `--reset`) is refused because the turn waits for you.
const WAITING_HINT: &str = "A plain press answers it; to start fresh, stop it first:";

/// The queue key of a needs-you notice ([`Utterance::notice`]), as announced
/// and as spoken again for a refused fresh-session press: one at a time per
/// agent, so a refusal while the announcement still waits (another agent's
/// reply holds the queue) does not queue it twice.
const WAITING_KEY: &str = "waiting-for-you";

/// How soon a session the overlay shows working or waiting on its own (no
/// whisrs turn running) is first listed, so its row ends without a `Stop`
/// too ([`idle_loop`]). As often as a turn's fallback poll.
const IDLE_POLL: Duration = Duration::from_secs(10);

/// The longest wait between those listings: each one doubles the wait, up
/// to this, so a row shown all day (a background dev server) or a listing
/// that keeps failing costs one `claude agents` every two minutes. It starts
/// over at [`IDLE_POLL`] when the rows shown change or a turn starts.
const IDLE_POLL_MAX: Duration = Duration::from_secs(120);

/// Now, on the runtime's clock: what interim expiries and the board's times
/// are measured in. The same as `Instant::now()` outside a test that pauses
/// the clock.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// The `Instant` of a wall-clock time in the past (a turn's start read back
/// from `agents.json` after a restart), on the runtime's clock. Now when it
/// is not in the past.
fn instant_at(at: chrono::DateTime<Utc>) -> Instant {
    let now = now();
    (Utc::now() - at)
        .to_std()
        .ok()
        .and_then(|ago| now.checked_sub(ago))
        .unwrap_or(now)
}

/// Whether an agent error was already notified where it happened: the
/// busy refusals (a turn that waits for you included), and the refusal
/// while a recording runs (`refuse_while_recording` notifies it).
pub(crate) fn error_already_notified(message: &str) -> bool {
    message.starts_with(STILL_WORKING)
        || message.starts_with(BACKGROUND_TASK)
        || message.contains(WAITING_HINT)
        || message.starts_with("A recording is in progress.")
}

/// Why a new turn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// The agent's whisrs turn is still running.
    Turn,
    /// Its session works on its own, with no whisrs turn running (a
    /// background task, or a follow-up after one). Stopping that work for a
    /// new turn would lose it, so `whisrs agent <name> --stop` is the way on.
    Background,
}

impl Busy {
    /// The spoken notice.
    fn notice(self) -> &'static str {
        match self {
            Busy::Turn => STILL_WORKING,
            Busy::Background => BACKGROUND_TASK,
        }
    }

    /// The error a CLI request gets back.
    fn message(self, agent: &str) -> String {
        match self {
            Busy::Turn => STILL_WORKING.to_string(),
            Busy::Background => format!(
                "{BACKGROUND_TASK} `whisrs agent {agent} --stop` ends that task, then send it again."
            ),
        }
    }
}

/// The notification for a refused turn. With the instruction (it was
/// transcribed or typed, and is not sent), it repeats it so the user can
/// send it again.
fn busy_notification(agent: &str, busy: Busy, instruction: Option<&str>) -> String {
    let mut body = format!("{agent}: {}", busy.notice());
    if let Some(text) = instruction.map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str(&format!(
            " Not sent: \"{}\".",
            truncate_preview(text, REFUSED_INSTRUCTION_MAX)
        ));
    }
    if busy == Busy::Background {
        body.push_str(&format!(
            " `whisrs agent {agent} --stop` ends that task, then send it again."
        ));
    }
    body
}

/// One running turn.
struct RunningTurn {
    /// Tells a finished task from the one that replaced it.
    generation: u64,
    /// When it started: only hook events that arrived later can be its own.
    started: Instant,
    task: tokio::task::JoinHandle<()>,
    /// The instruction it sends, for the notification when it is refused.
    text: String,
    /// Hook events for the driver.
    hooks: mpsc::UnboundedSender<HookEvent>,
    /// The newest session ids the turn reported, and its stop flag
    /// ([`TurnUpdates::stop`]).
    updates: TurnUpdates,
    /// Which of the turn's interims are spoken, at the agent's `feedback`
    /// level. Dies with the turn.
    gate: ProgressGate,
}

/// What the runtime holds per agent at run time (the persisted part is the
/// agent's [`AgentRecord`]).
struct Slot {
    phase: AgentPhase,
    turn: Option<RunningTurn>,
    /// The raw reply of the last turn, when it ended on a poll: that turn's
    /// own `Stop` can still arrive after it, carrying the same reply, and is
    /// not spoken twice. Taken by the next `Stop` with no turn running, and
    /// cleared when another turn ends.
    polled_reply: Option<String>,
    /// The notice spoken when the agent last started waiting for you
    /// ("Waiting for your approval to use Bash."). Meant only while `phase`
    /// is needs-you.
    needs_you: Option<String>,
    /// With no whisrs turn running, the overlay shows the session working
    /// on its own: a turn was refused because it does, as `whisrs agent`
    /// lists it. `phase` stays idle, so a press is refused as before, at
    /// its transcript.
    own_work: bool,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Idle,
            turn: None,
            polled_reply: None,
            needs_you: None,
            own_work: false,
        }
    }
}

impl Slot {
    /// Mark the agent needs-you, with the notice that says so.
    fn wait_for_you(&mut self, notice: &str) {
        self.phase = AgentPhase::NeedsYou;
        self.needs_you = Some(notice.to_string());
    }

    /// Whether the overlay shows the session's own work, or a permission
    /// prompt it waits on, with no whisrs turn running: until a `Stop`, or
    /// a listing that shows it neither works nor waits ([`idle_loop`]).
    fn shows_idle(&self) -> bool {
        self.turn.is_none() && (self.own_work || self.phase == AgentPhase::NeedsYou)
    }

    /// End what [`Slot::shows_idle`] shows: `true` when it showed something.
    fn end_idle(&mut self) -> bool {
        let shown = self.shows_idle();
        if shown {
            self.phase = AgentPhase::Idle;
            self.own_work = false;
        }
        shown
    }

    /// The needs-you notice while a whisrs turn of the agent waits for you.
    fn waiting(&self) -> Option<&str> {
        (self.turn.is_some() && self.phase == AgentPhase::NeedsYou)
            .then(|| self.needs_you.as_deref().unwrap_or(WAITING_FOR_YOU))
    }
}

struct Inner {
    store: AgentStore,
    slots: HashMap<String, Slot>,
    /// Hook events no running turn matched yet.
    unclaimed: VecDeque<HookEvent>,
    next_generation: u64,
}

/// One queued utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Utterance {
    agent: String,
    text: String,
    /// The refusal notice this speaks, as its key ([`Busy::notice`], or
    /// [`WAITING_KEY`]): one of a kind per agent queued at a time, and none
    /// that says what the one playing says ([`AgentRuntime::say_keyed`]).
    /// `None` for replies and every other line, which are never
    /// deduplicated.
    notice: Option<&'static str>,
    /// An acknowledgement or a progress update, and until when it is worth
    /// saying. `None` for replies, errors and notices, which never expire.
    interim: Option<Interim>,
}

/// What makes an utterance an interim ([`Utterance::interim`]): dropped
/// once it expires, when its agent's reply arrives and on `whisrs cancel`,
/// and never put back after a recording stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Interim {
    kind: InterimKind,
    expires: Instant,
}

/// What the runtime tells the agent overlay's board ([`board_loop`]). The
/// voice comes from the state broadcast instead.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BoardEvent {
    /// A turn started at `since` (for one watched again after a restart,
    /// when it first started), or the overlay shows the session's own work
    /// from then ([`Slot::own_work`]).
    TurnStarted { agent: String, since: Instant },
    /// What the running turn does now.
    Activity { agent: String, label: String },
    /// The turn, or the session's own work, waits for a permission prompt.
    NeedsYou { agent: String, at: Instant },
    /// It runs again after a wait.
    Working { agent: String },
    /// The turn ended, with a reply (`ok`) or an error.
    TurnEnded {
        agent: String,
        ok: bool,
        at: Instant,
    },
    /// The agents with an utterance queued or playing.
    Pending { agents: Vec<String>, at: Instant },
    /// Forget what is shown for the agent (`--stop`, `--reset`, the end of
    /// its session's own work or wait). Its voice stays.
    Clear { agent: String },
}

/// Holds agent replies while it lives: a command-mode or `[[llm_commands]]`
/// press between its state check and its recording, or a read-aloud press
/// until its read-aloud started (the selection capture releases the
/// dictation lock), or a recording that is starting. Dropping it wakes the
/// reply queue.
pub(crate) struct ReplyHold<'a> {
    rt: &'a AgentRuntime,
}

impl Drop for ReplyHold<'_> {
    fn drop(&mut self) {
        self.rt.reply_holds.fetch_sub(1, Ordering::SeqCst);
        self.rt.utterance_ready.notify_one();
    }
}

/// The agent runtime.
pub(crate) struct AgentRuntime {
    agents: Vec<Arc<Agent>>,
    drivers: HashMap<Harness, Arc<dyn Driver>>,
    /// The transient scopes process-per-turn turns run in, shared by their
    /// drivers: what a previous daemon's turn left in one is stopped at
    /// startup ([`start_background`]).
    scopes: Arc<TurnScopes>,
    store_path: PathBuf,
    inner: Mutex<Inner>,
    /// Replies and notices waiting for the daemon to be Idle.
    utterances: std::sync::Mutex<VecDeque<Utterance>>,
    utterance_ready: Notify,
    /// The utterance being spoken, until its playback ends.
    playing: std::sync::Mutex<Option<Utterance>>,
    /// TTS is enabled: utterances are queued at all.
    speak: bool,
    /// Live [`ReplyHold`]s.
    reply_holds: AtomicUsize,
    /// The dictation state, set once the reply queue starts: a recording
    /// start stops an agent reply playing through it.
    daemon_state: OnceLock<Arc<Mutex<DaemonState>>>,
    /// Numbers `agents.json` snapshots; the newest written one is kept in
    /// `saved_seq`, so a write that lost the race to a newer one is skipped.
    save_seq: AtomicU64,
    saved_seq: Arc<std::sync::Mutex<u64>>,
    /// Where the agent overlay's board is told what changed; its receiver
    /// waits for the board task (`board_rx`). `None` without agents.
    board_tx: Option<mpsc::UnboundedSender<BoardEvent>>,
    board_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<BoardEvent>>>,
    /// The agents the board was last told have an utterance pending.
    last_pending: std::sync::Mutex<Vec<String>>,
    /// Wakes [`idle_loop`] when a session starts showing work or a wait of
    /// its own ([`Slot::shows_idle`]), or a turn starts: either starts its
    /// wait over.
    idle_shown: Notify,
}

impl AgentRuntime {
    /// The runtime for `config`'s usable agents. With none it is inert and
    /// reads no file. The warnings for skipped entries are logged by
    /// `Config::validate`.
    pub(crate) fn new(config: &Config, socket: &Path) -> Self {
        let (agents, _warnings) = resolve_agents(&config.agents);
        if agents.is_empty() {
            return Self::disabled();
        }
        let store_path = store::store_path();
        let (mut store, load_error) = store::load(&store_path);
        if let Some(e) = load_error {
            warn!("agents: {e}");
        }
        // A changed harness or dir starts a fresh session.
        let mut changed = false;
        for agent in &agents {
            if let Some(record) = store.agents.get(&agent.name) {
                if !record.matches(agent.harness.as_str(), &agent.dir) {
                    info!(
                        "agent '{}': harness or dir changed, the next turn starts a new session",
                        agent.name
                    );
                    store.agents.insert(
                        agent.name.clone(),
                        AgentRecord::new(agent.harness.as_str(), &agent.dir),
                    );
                    changed = true;
                }
            }
        }
        if changed {
            if let Err(e) = store::save(&store_path, &store) {
                warn!("agents: cannot write {}: {e:#}", store_path.display());
            }
        }
        let whisrs = whisrs_binary();
        info!(
            "agents: {} configured ({}); hooks run {}",
            agents.len(),
            agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            whisrs.display()
        );
        let mut drivers: HashMap<Harness, Arc<dyn Driver>> = HashMap::new();
        drivers.insert(
            Harness::Claude,
            Arc::new(ClaudeDriver::new(&whisrs, socket)),
        );
        let scopes = Arc::new(TurnScopes::new(&store_path));
        drivers.insert(
            Harness::OpenCode,
            Arc::new(ProcessDriver::new(OpenCode, Arc::clone(&scopes))),
        );
        drivers.insert(
            Harness::Codex,
            Arc::new(ProcessDriver::new(Codex, Arc::clone(&scopes))),
        );
        drivers.insert(
            Harness::Copilot,
            Arc::new(ProcessDriver::new(
                Copilot::beside_store(&store_path),
                Arc::clone(&scopes),
            )),
        );
        let (board_tx, board_rx) = mpsc::unbounded_channel();
        Self {
            agents: agents.into_iter().map(Arc::new).collect(),
            drivers,
            scopes,
            store_path,
            inner: Mutex::new(Inner {
                store,
                slots: HashMap::new(),
                unclaimed: VecDeque::new(),
                next_generation: 1,
            }),
            utterances: std::sync::Mutex::new(VecDeque::new()),
            utterance_ready: Notify::new(),
            playing: std::sync::Mutex::new(None),
            speak: config.tts.as_ref().is_some_and(|t| t.enabled),
            reply_holds: AtomicUsize::new(0),
            daemon_state: OnceLock::new(),
            save_seq: AtomicU64::new(0),
            saved_seq: Arc::new(std::sync::Mutex::new(0)),
            board_tx: Some(board_tx),
            board_rx: std::sync::Mutex::new(Some(board_rx)),
            last_pending: std::sync::Mutex::new(Vec::new()),
            idle_shown: Notify::new(),
        }
    }

    /// A runtime with no agents.
    pub(crate) fn disabled() -> Self {
        Self {
            agents: Vec::new(),
            drivers: HashMap::new(),
            scopes: Arc::new(TurnScopes::off()),
            store_path: PathBuf::new(),
            inner: Mutex::new(Inner {
                store: AgentStore::default(),
                slots: HashMap::new(),
                unclaimed: VecDeque::new(),
                next_generation: 1,
            }),
            utterances: std::sync::Mutex::new(VecDeque::new()),
            utterance_ready: Notify::new(),
            playing: std::sync::Mutex::new(None),
            speak: false,
            reply_holds: AtomicUsize::new(0),
            daemon_state: OnceLock::new(),
            save_seq: AtomicU64::new(0),
            saved_seq: Arc::new(std::sync::Mutex::new(0)),
            board_tx: None,
            board_rx: std::sync::Mutex::new(None),
            last_pending: std::sync::Mutex::new(Vec::new()),
            idle_shown: Notify::new(),
        }
    }

    /// Whether any agent is configured.
    pub(crate) fn is_enabled(&self) -> bool {
        !self.agents.is_empty()
    }

    /// The usable agents (for the hotkey listener).
    pub(crate) fn agents(&self) -> Vec<Agent> {
        self.agents.iter().map(|a| Agent::clone(a)).collect()
    }

    /// Whether any agent has a hotkey (`hotkey` or `new_session_hotkey`).
    pub(crate) fn has_hotkeys(&self) -> bool {
        self.agents
            .iter()
            .any(|a| a.hotkey.is_some() || a.new_session_hotkey.is_some())
    }

    fn agent(&self, name: &str) -> Option<Arc<Agent>> {
        self.agents.iter().find(|a| a.name == name).cloned()
    }

    fn driver(&self, harness: Harness) -> Arc<dyn Driver> {
        Arc::clone(
            self.drivers
                .get(&harness)
                .expect("every resolvable harness has a driver"),
        )
    }

    /// Write a snapshot of the store off the async runtime (the write
    /// fsyncs). Snapshots are numbered and a write older than the newest
    /// one on disk is skipped, so the file always ends at the newest state.
    fn save(&self, inner: &Inner) {
        let seq = self.save_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = inner.store.clone();
        let path = self.store_path.clone();
        let saved = Arc::clone(&self.saved_seq);
        tokio::task::spawn_blocking(move || {
            let mut last = saved.lock().unwrap_or_else(|e| e.into_inner());
            if *last >= seq {
                return;
            }
            match store::save(&path, &snapshot) {
                Ok(()) => *last = seq,
                Err(e) => warn!("agents: cannot write {}: {e:#}", path.display()),
            }
        });
    }

    /// Queue `text` to be spoken when the daemon is Idle (TTS on only).
    fn say(&self, agent: &str, text: String) {
        self.enqueue(Utterance {
            agent: agent.to_string(),
            text,
            notice: None,
            interim: None,
        });
    }

    /// Queue an interim line the agent's gate admitted as `kind` at `at`:
    /// spoken under the same holds as a reply if it is still worth saying by
    /// then ([`InterimKind::ttl`]). Never notified.
    fn say_interim(&self, agent: &str, text: String, kind: InterimKind, at: Instant) {
        self.enqueue(Utterance {
            agent: agent.to_string(),
            text,
            notice: None,
            interim: Some(Interim {
                kind,
                expires: at + kind.ttl(),
            }),
        });
    }

    /// Queue a busy notice, named after the agent when there are several.
    /// Not queued again while the same notice for this agent is queued or
    /// playing: compared as the notice, not the spoken line.
    fn say_notice(&self, agent: &str, notice: &'static str) {
        self.say_keyed(agent, notice, notice.to_string());
    }

    /// Queue the refusal notice `text` under `key`, named after the agent
    /// when there are several: one of that key per agent at a time. One
    /// queued already takes the newer text (a newer permission prompt); one
    /// playing plays on, and a different text is queued once after it.
    fn say_keyed(&self, agent: &str, key: &'static str, text: String) {
        self.enqueue(Utterance {
            agent: agent.to_string(),
            text: self.line_for(agent, text),
            notice: Some(key),
            interim: None,
        });
    }

    fn enqueue(&self, utterance: Utterance) {
        if !self.speak {
            return;
        }
        if let Ok(mut queue) = self.utterances.lock() {
            let same_notice = |u: &Utterance| {
                utterance.notice.is_some()
                    && u.agent == utterance.agent
                    && u.notice == utterance.notice
            };
            if let Some(queued) = queue.iter_mut().find(|u| same_notice(u)) {
                debug!(
                    "agent '{}': that notice is already queued; it says the newer text",
                    utterance.agent
                );
                queued.text = utterance.text;
                return;
            }
            let playing = self
                .playing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|u| same_notice(u) && u.text == utterance.text);
            if playing {
                debug!("agent '{}': that notice is playing", utterance.agent);
                return;
            }
            queue.push_back(utterance);
        }
        self.pending_changed();
        self.utterance_ready.notify_one();
    }

    /// The next utterance to speak. An interim that expired while it waited
    /// (a hold, a long reply before it) is dropped on the way.
    fn next_utterance(&self) -> Option<Utterance> {
        let now = now();
        let mut queue = self.utterances.lock().ok()?;
        while let Some(utterance) = queue.pop_front() {
            match utterance.interim {
                Some(interim) if interim.expires <= now => info!(
                    "agent '{}': {:?} expired before it could be spoken",
                    utterance.agent, interim.kind
                ),
                _ => return Some(utterance),
            }
        }
        None
    }

    /// Put a stopped utterance back at the front of the queue (a recording
    /// start stopped it, or the queue could not speak it yet). A notice of
    /// the same key and agent queued while it played is newer: that one
    /// takes the front instead, so only the newest of a key is said.
    fn requeue_front(&self, mut utterance: Utterance) {
        if let Ok(mut queue) = self.utterances.lock() {
            if utterance.notice.is_some() {
                let newer = queue
                    .iter()
                    .position(|u| u.agent == utterance.agent && u.notice == utterance.notice)
                    .and_then(|i| queue.remove(i));
                if let Some(newer) = newer {
                    debug!(
                        "agent '{}': a newer notice of that kind replaces the one put back",
                        utterance.agent
                    );
                    utterance = newer;
                }
            }
            queue.push_front(utterance);
        }
        self.pending_changed();
    }

    /// Tell the board ([`BoardEvent`]) when there is one.
    fn board(&self, event: BoardEvent) {
        if let Some(tx) = &self.board_tx {
            let _ = tx.send(event);
        }
    }

    /// The board's receiver, for the one task that owns the board. `None`
    /// without agents, or once taken.
    fn take_board_events(&self) -> Option<mpsc::UnboundedReceiver<BoardEvent>> {
        self.board_rx.lock().ok()?.take()
    }

    /// Tell the board which agents have an utterance queued or playing, when
    /// that changed: a Done row holds while its agent's reply waits or
    /// plays. Under `last_pending`, so two callers cannot send their views
    /// out of order. Never called with the queue or `playing` locked.
    fn pending_changed(&self) {
        if self.board_tx.is_none() {
            return;
        }
        let mut last = self.last_pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut agents: Vec<String> = self
            .utterances
            .lock()
            .map(|q| q.iter().map(|u| u.agent.clone()).collect())
            .unwrap_or_default();
        if let Some(playing) = self.playing_agent() {
            agents.push(playing);
        }
        agents.sort();
        agents.dedup();
        if *last == agents {
            return;
        }
        *last = agents.clone();
        self.board(BoardEvent::Pending { agents, at: now() });
    }

    /// Run `stop`, which stops an agent reply or notice being synthesized or
    /// played (the caller holds the dictation lock), and when it stopped
    /// one, put that utterance back at the front of the queue, so it plays
    /// after the recording. `true` when one was stopped.
    ///
    /// The utterance is copied before `stop` runs: the stop broadcasts Idle,
    /// and the reply queue, woken by it on another worker, clears `playing`
    /// ([`utterance_loop`]) perhaps before `stop` returns. The queue only
    /// clears it and never puts back a reply that ran, so the copy is
    /// queued exactly once.
    #[cfg(feature = "recording")]
    fn requeue_if_stopped(&self, stop: impl FnOnce() -> bool) -> bool {
        let playing = self
            .playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !stop() {
            return false;
        }
        match playing {
            // An acknowledgement or update is not worth saying after the
            // recording: it is dropped.
            Some(utterance) if utterance.interim.is_some() => info!(
                "agent '{}': the recording cut off an interim; it is not said again",
                utterance.agent
            ),
            Some(utterance) => {
                info!(
                    "agent '{}': the reply plays again after the recording",
                    utterance.agent
                );
                self.requeue_front(utterance);
            }
            None => {}
        }
        true
    }

    /// Hold agent replies until the guard drops ([`ReplyHold`]).
    pub(crate) fn hold_replies(&self) -> ReplyHold<'_> {
        self.reply_holds.fetch_add(1, Ordering::SeqCst);
        ReplyHold { rt: self }
    }

    fn has_utterances(&self) -> bool {
        self.utterances.lock().is_ok_and(|q| !q.is_empty())
    }

    fn set_playing(&self, utterance: Option<Utterance>) {
        *self.playing.lock().unwrap_or_else(|e| e.into_inner()) = utterance;
        self.pending_changed();
    }

    /// The agent whose utterance is being spoken (`speak_text` records it
    /// as the voice's owner).
    pub(crate) fn playing_agent(&self) -> Option<String> {
        self.playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|u| u.agent.clone())
    }

    /// Whether the utterance being spoken is `agent`'s (a reply or a notice).
    fn is_playing_for(&self, agent: &str) -> bool {
        self.playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|u| u.agent == agent)
    }

    /// Whether the queue is speaking an utterance (from just before it asks
    /// for the dictation state until its playback ended).
    #[cfg(feature = "recording")]
    fn is_speaking(&self) -> bool {
        self.playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Drop every queued reply and notice (`whisrs cancel` while one plays).
    pub(crate) fn drop_queued(&self) {
        let dropped = self
            .utterances
            .lock()
            .map(|mut q| std::mem::take(&mut *q).len())
            .unwrap_or(0);
        if dropped > 0 {
            info!("agents: cancel dropped {dropped} queued reply(s)");
            self.pending_changed();
        }
    }

    /// Drop every queued interim (`whisrs cancel`).
    pub(crate) fn drop_interims(&self) {
        let dropped = self.drop_interims_of(None);
        if dropped > 0 {
            info!("agents: cancel dropped {dropped} queued acknowledgement(s) or update(s)");
        }
    }

    /// Drop the queued interims of `agent` (every agent's with `None`), and
    /// return how many.
    fn drop_interims_of(&self, agent: Option<&str>) -> usize {
        let dropped = self
            .utterances
            .lock()
            .map(|mut q| {
                let before = q.len();
                q.retain(|u| u.interim.is_none() || agent.is_some_and(|a| a != u.agent));
                before - q.len()
            })
            .unwrap_or(0);
        if dropped > 0 {
            self.pending_changed();
        }
        dropped
    }

    /// The spoken line for a failed turn, naming the agent when there are
    /// several.
    fn error_line(&self, agent: &Agent, message: &str) -> String {
        self.line_for(&agent.name, spoken::spoken_error(message))
    }

    /// `line` named after `agent` when there are several agents.
    fn line_for(&self, agent: &str, line: String) -> String {
        if self.agents.len() > 1 {
            format!("{agent}: {line}")
        } else {
            line
        }
    }

    /// The row `whisrs agent` shows for `agent`.
    fn info(inner: &Inner, agent: &Agent) -> AgentInfo {
        let phase = inner
            .slots
            .get(&agent.name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle);
        let record = inner.store.agents.get(&agent.name);
        // Short beside a bg id (what Claude Code's commands take); whole
        // otherwise, as the harness takes it (`opencode --session <id>`,
        // `codex resume <id>`, `copilot --resume=<id>`).
        let session = record.and_then(|r| {
            let id = r.session_id.as_deref()?;
            Some(match r.bg_id {
                Some(_) => id.chars().take(8).collect(),
                None => id.to_string(),
            })
        });
        AgentInfo {
            name: agent.name.clone(),
            harness: agent.harness.to_string(),
            phase,
            session,
            bg_id: record.and_then(|r| r.bg_id.clone()),
        }
    }

    /// The session of `agent` when no whisrs turn runs and it has one: what
    /// [`Driver::session_phase`] is asked about.
    fn idle_session(inner: &Inner, agent: &Agent) -> Option<SessionRef> {
        if inner
            .slots
            .get(&agent.name)
            .is_some_and(|s| s.turn.is_some())
        {
            return None;
        }
        let session = inner.store.agents.get(&agent.name)?.session();
        session.bg_id.is_some().then_some(session)
    }

    /// Every agent's row. An agent with no whisrs turn running is shown as
    /// its harness sees it, so a session working on its own reads `working`.
    /// The harness is asked once for every such agent
    /// ([`Driver::session_phases`]).
    async fn list(&self) -> Vec<AgentInfo> {
        let (mut rows, rows_asked, sessions) = {
            let inner = self.inner.lock().await;
            let rows: Vec<AgentInfo> = self.agents.iter().map(|a| Self::info(&inner, a)).collect();
            let (rows_asked, sessions): (Vec<usize>, Vec<(Arc<Agent>, SessionRef)>) = self
                .agents
                .iter()
                .enumerate()
                .filter_map(|(i, a)| Self::idle_session(&inner, a).map(|s| (i, (Arc::clone(a), s))))
                .unzip();
            (rows, rows_asked, sessions)
        };
        let phases = self.idle_phases(&sessions).await;
        for (i, phase) in rows_asked.into_iter().zip(phases) {
            if let Some(phase) = phase {
                rows[i].phase = phase;
            }
        }
        rows
    }

    /// What each of `sessions` does while no whisrs turn runs, asked once
    /// per harness ([`Driver::session_phases`]), and followed by the rows
    /// the overlay shows for them ([`AgentRuntime::idle_phase_seen`]). One
    /// answer per entry, in order: `None` when the harness could not tell.
    async fn idle_phases(&self, sessions: &[(Arc<Agent>, SessionRef)]) -> Vec<Option<AgentPhase>> {
        let mut phases = vec![None; sessions.len()];
        for harness in Harness::ALL {
            let group: Vec<usize> = (0..sessions.len())
                .filter(|&i| sessions[i].0.harness == *harness)
                .collect();
            if group.is_empty() {
                continue;
            }
            let asked: Vec<(Arc<Agent>, SessionRef)> =
                group.iter().map(|&i| sessions[i].clone()).collect();
            let answers = self.driver(*harness).session_phases(&asked).await;
            for (i, answer) in group.into_iter().zip(answers) {
                let agent = &sessions[i].0;
                match answer {
                    Ok(phase) => {
                        phases[i] = Some(phase);
                        self.idle_phase_seen(agent, phase).await;
                    }
                    Err(e) => debug!(
                        "agent '{}': cannot tell what its session does: {e}",
                        agent.name
                    ),
                }
            }
        }
        phases
    }

    /// Show the session's own work as working, with no whisrs turn running:
    /// a turn was refused because the session works on its own, as `whisrs
    /// agent` lists it. Shown until a `Stop`, or a listing that shows it no
    /// longer works ([`idle_loop`]). A wait it showed is over: it works.
    fn show_own_work(&self, inner: &mut Inner, agent: &Agent) {
        let slot = inner.slots.entry(agent.name.clone()).or_default();
        if slot.turn.is_some() || slot.own_work {
            return;
        }
        slot.phase = AgentPhase::Idle;
        slot.own_work = true;
        self.board(BoardEvent::TurnStarted {
            agent: agent.name.clone(),
            since: now(),
        });
        self.idle_shown.notify_one();
    }

    /// What a listing showed the session of `agent` doing while no whisrs
    /// turn runs ([`Driver::session_phase`]). A row the overlay shows for
    /// its own work or a wait of it ([`Slot::shows_idle`]) follows it, and
    /// ends once the session neither works nor waits. A listing never
    /// starts such a row: a refusal or a hook does.
    async fn idle_phase_seen(&self, agent: &Agent, phase: AgentPhase) {
        let mut inner = self.inner.lock().await;
        let Some(slot) = inner.slots.get_mut(&agent.name).filter(|s| s.shows_idle()) else {
            return;
        };
        let name = agent.name.clone();
        match phase {
            AgentPhase::Idle => {
                slot.end_idle();
                info!("agent '{name}': its session no longer works or waits on its own");
                self.board(BoardEvent::Clear { agent: name });
            }
            // Answered elsewhere (in a terminal): the work goes on.
            AgentPhase::Working if slot.phase == AgentPhase::NeedsYou => {
                slot.phase = AgentPhase::Idle;
                if slot.own_work {
                    self.board(BoardEvent::Working { agent: name });
                } else {
                    slot.own_work = true;
                    self.board(BoardEvent::TurnStarted {
                        agent: name,
                        since: now(),
                    });
                }
            }
            // The hook that announces a prompt came first so far; this
            // only shows it.
            AgentPhase::NeedsYou if slot.phase != AgentPhase::NeedsYou => {
                slot.wait_for_you(WAITING_FOR_YOU);
                self.board(BoardEvent::NeedsYou {
                    agent: name,
                    at: now(),
                });
            }
            _ => {}
        }
    }

    /// The sessions [`idle_loop`] lists: those shown working or waiting on
    /// their own ([`Slot::shows_idle`]) that have one to ask about.
    async fn idle_shown_sessions(&self) -> Vec<(Arc<Agent>, SessionRef)> {
        let inner = self.inner.lock().await;
        self.agents
            .iter()
            .filter(|a| inner.slots.get(&a.name).is_some_and(Slot::shows_idle))
            .filter_map(|a| Self::idle_session(&inner, a).map(|s| (Arc::clone(a), s)))
            .collect()
    }

    #[cfg(test)]
    async fn phase(&self, name: &str) -> AgentPhase {
        let inner = self.inner.lock().await;
        inner
            .slots
            .get(name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle)
    }

    /// Whether a whisrs turn of the agent runs, also one that waits for the
    /// user (needs-you).
    #[cfg(test)]
    async fn has_turn(&self, name: &str) -> bool {
        let inner = self.inner.lock().await;
        inner.slots.get(name).is_some_and(|s| s.turn.is_some())
    }

    /// Why a press for `name` is refused before it records, if it is: the
    /// agent's turn works; with `new_session`, any turn of it runs, and one
    /// that waits for you is named by its needs-you notice (a plain press
    /// answers that one).
    async fn press_refusal(&self, name: &str, new_session: bool) -> Option<Refusal> {
        let inner = self.inner.lock().await;
        let slot = inner.slots.get(name)?;
        if slot.phase == AgentPhase::Working {
            return Some(Refusal::Busy);
        }
        if !new_session || slot.turn.is_none() {
            return None;
        }
        Some(match slot.waiting() {
            Some(notice) => Refusal::Waiting(notice.to_string()),
            None => Refusal::Busy,
        })
    }
}

/// Why a voice press is refused before it records.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    /// The agent's turn works (with `--new`: any turn of it runs).
    Busy,
    /// `--new` while the agent's turn waits for you: its needs-you notice.
    Waiting(String),
}

/// A session for the log: its id, else the bg id of a launch whose session
/// id was not looked up yet, else none.
fn describe(session: &SessionRef) -> String {
    match (&session.session_id, &session.bg_id) {
        (Some(id), _) => format!("session {id}"),
        (None, Some(bg)) => format!("session bg {bg}"),
        (None, None) => "a new session".to_string(),
    }
}

/// The `whisrs` binary the hooks run: the one beside this `whisrsd`, else
/// `whisrs` on the session's `PATH`.
fn whisrs_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("whisrs")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("whisrs"))
}

/// The record of `agent`, created (or reset, after a harness or dir change)
/// as needed.
fn record_mut<'a>(inner: &'a mut Inner, agent: &Agent) -> &'a mut AgentRecord {
    let fresh = || AgentRecord::new(agent.harness.as_str(), &agent.dir);
    let record = inner
        .store
        .agents
        .entry(agent.name.clone())
        .or_insert_with(fresh);
    if !record.matches(agent.harness.as_str(), &agent.dir) {
        *record = fresh();
    }
    record
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// What a daemon with no agents runs at startup: it still stops anything a
/// previous daemon's turns left in their scopes (that daemon died mid-turn,
/// and `[[agents]]` was emptied before this start), a background job
/// included. One `systemctl --user list-units` call when nothing is left.
pub(crate) fn sweep_without_agents() {
    tokio::spawn(async { TurnScopes::new(&store::store_path()).sweep().await });
}

/// Start the reply queue and the agent overlay's board, and watch again the
/// turns a restart interrupted. Called once, only with agents configured.
pub(crate) async fn start_background(
    context: Arc<DaemonContext>,
    daemon_state: Arc<Mutex<DaemonState>>,
) {
    let _ = context.agents.daemon_state.set(Arc::clone(&daemon_state));
    tokio::spawn(utterance_loop(
        Arc::clone(&context),
        Arc::clone(&daemon_state),
        context.state_tx.subscribe_raw(),
    ));
    if let Some(events) = context.agents.take_board_events() {
        tokio::spawn(board_loop(
            Arc::clone(&context),
            events,
            context.state_tx.subscribe_voice(),
        ));
        tokio::spawn(idle_loop(Arc::clone(&context)));
    }

    // Anything a previous daemon's process-per-turn turns left in their
    // scopes, a background job included (a process that ignored the SIGINT
    // its end sent, or one a turn that ended normally left running), stopped
    // before any turn of this daemon runs: every such turn waits for this.
    let scopes = Arc::clone(&context.agents.scopes);
    tokio::spawn(async move { scopes.sweep().await });

    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    for agent in &rt.agents {
        let Some(record) = inner.store.agents.get(&agent.name) else {
            continue;
        };
        if !record.turn_running {
            continue;
        }
        let session = record.session();
        let started_at = record.turn_started_at.unwrap_or(record.updated_at);
        info!(
            "agent '{}': a turn was running before the restart; watching {} again",
            agent.name,
            describe(&session)
        );
        let request = TurnRequest {
            text: String::new(),
            started_at,
        };
        spawn_turn(
            &context,
            &mut inner,
            Arc::clone(agent),
            session,
            request,
            true,
        );
    }
}

/// Whether a queued reply or notice may be spoken now: the daemon is Idle
/// (no dictation, command or read-aloud) and nothing else holds replies
/// ([`replies_held`]).
fn may_speak(state: State, held: bool) -> bool {
    state == State::Idle && !held
}

/// Whether agent replies wait besides the dictation state: a
/// [`ReplyHold`] (a command or read-aloud press capturing the selection, a
/// recording starting), or a recording capturing (a meeting recording does not touch
/// the dictation state, and a reply would play mid-call and land on its
/// system track). Checked under the dictation lock by `speak_text`, so a
/// hold taken under that lock is never missed.
pub(crate) fn replies_held(context: &DaemonContext) -> bool {
    context.agents.reply_holds.load(Ordering::SeqCst) > 0 || recording_captures(context)
}

fn recording_captures(_context: &DaemonContext) -> bool {
    #[cfg(feature = "recording")]
    return _context.recording.is_capturing();
    #[cfg(not(feature = "recording"))]
    false
}

/// A recording is starting: hold agent replies until the returned guard
/// drops (by then the recording captures, or its start failed), and stop an
/// agent reply or notice being synthesized or played, putting it back at
/// the front of the queue so it plays after the recording. A selection
/// read-aloud is left alone.
///
/// Never waits on the dictation lock, which a dictation start holds for
/// seconds while its mic opens: a recording start must not wait on it. The
/// hold is taken first, so a reply the queue starts after it is refused
/// under that lock (`speak_text`), and only one that already started needs
/// stopping. When the lock is busy, that stop is spawned and runs once the
/// lock is free, while replies are still held.
#[cfg(feature = "recording")]
pub(crate) fn hold_for_recording(context: &Arc<DaemonContext>) -> ReplyHold<'_> {
    let rt = &context.agents;
    let hold = rt.hold_replies();
    if !rt.is_speaking() {
        return hold;
    }
    let Some(daemon_state) = rt.daemon_state.get() else {
        return hold;
    };
    match daemon_state.try_lock() {
        Ok(mut ds) => stop_for_recording(&mut ds, context),
        Err(_) => {
            debug!(
                "recording: the dictation lock is busy; an agent reply is stopped once it is free"
            );
            let daemon_state = Arc::clone(daemon_state);
            let context = Arc::clone(context);
            tokio::spawn(async move {
                let mut ds = daemon_state.lock().await;
                // Nothing holds replies any more: the recording start
                // failed and is gone, so there is nothing to stop for.
                if replies_held(&context) {
                    stop_for_recording(&mut ds, &context);
                }
            });
        }
    }
    hold
}

/// Stop an agent reply or notice playing for a recording start and put it
/// back at the front of the queue. The caller holds the dictation lock.
#[cfg(feature = "recording")]
fn stop_for_recording(ds: &mut DaemonState, context: &DaemonContext) {
    context
        .agents
        .requeue_if_stopped(|| crate::speak::stop_agent_playback(ds, context, "recording"));
}

/// What wakes the reply queue besides the dictation state: the recording
/// status (feature `recording`).
struct RecordingHold {
    #[cfg(feature = "recording")]
    status_rx: tokio::sync::watch::Receiver<whisrs::recording::RecordingStatus>,
}

impl RecordingHold {
    fn new(_context: &DaemonContext) -> Self {
        Self {
            #[cfg(feature = "recording")]
            status_rx: _context.recording.subscribe(),
        }
    }

    /// Resolves when the recording status may have changed (never without
    /// the feature).
    async fn changed(&mut self) {
        #[cfg(feature = "recording")]
        if self.status_rx.changed().await.is_ok() {
            return;
        }
        std::future::pending::<()>().await
    }
}

/// Wait until the dictation state, the recording status or a hold may have
/// changed (a dropped [`ReplyHold`] wakes `utterance_ready`). `false` once
/// the state channel closed.
async fn wait_for_change(
    context: &DaemonContext,
    state_rx: &mut tokio::sync::watch::Receiver<State>,
    hold: &mut RecordingHold,
) -> bool {
    tokio::select! {
        changed = state_rx.changed() => changed.is_ok(),
        _ = hold.changed() => true,
        _ = context.agents.utterance_ready.notified() => true,
    }
}

/// Speak queued replies and notices one at a time, each once
/// [`may_speak`]. Watches the state channel like `hook_dispatch_loop`, and
/// the recording status.
async fn utterance_loop(
    context: Arc<DaemonContext>,
    daemon_state: Arc<Mutex<DaemonState>>,
    mut state_rx: tokio::sync::watch::Receiver<State>,
) {
    let mut hold = RecordingHold::new(&context);
    loop {
        if !context.agents.has_utterances() {
            context.agents.utterance_ready.notified().await;
            continue;
        }
        let state = *state_rx.borrow_and_update();
        let held = replies_held(&context);
        if !may_speak(state, held) {
            if held {
                debug!("agents: a recording or a command or read-aloud press holds replies");
            }
            if !wait_for_change(&context, &mut state_rx, &mut hold).await {
                return;
            }
            continue;
        }
        let Some(utterance) = context.agents.next_utterance() else {
            // Only expired interims were left.
            context.agents.pending_changed();
            continue;
        };
        // Playing from here: the board sees it pending all along, and
        // `speak_text` records whose voice it is.
        context.agents.set_playing(Some(utterance.clone()));
        let backend = match tts_backend(&context) {
            Ok(b) => b,
            Err(e) => {
                warn!("agent '{}': cannot speak the reply: {e}", utterance.agent);
                context.agents.set_playing(None);
                continue;
            }
        };
        debug!(
            "agent '{}': speaking {} chars",
            utterance.agent,
            utterance.text.len()
        );
        match speak_text(
            Arc::clone(&daemon_state),
            Arc::clone(&context),
            backend,
            &utterance.text,
            SpeakOrigin::Agent,
        )
        .await
        {
            SpeakAttempt::NotIdle(_) => {
                // Something started first, or a hold came in: wait again
                // (put back first, so the board sees it pending all along).
                context.agents.requeue_front(utterance);
                context.agents.set_playing(None);
                if !wait_for_change(&context, &mut state_rx, &mut hold).await {
                    return;
                }
            }
            SpeakAttempt::Ran(Response::Error { message }) => {
                context.agents.set_playing(None);
                warn!("agent '{}': reply not spoken: {message}", utterance.agent);
            }
            SpeakAttempt::Ran(_) => {
                // It counts as playing until the playback ends.
                let ended = state_rx
                    .wait_for(|s| !matches!(s, State::Speaking | State::Synthesizing))
                    .await
                    .is_ok();
                context.agents.set_playing(None);
                if !ended {
                    return;
                }
            }
        }
    }
}

/// Own the agent overlay's board: apply what the runtime reports and the
/// agent the voice belongs to, and publish the rows on `DaemonContext::agent_board`
/// whenever they changed, also when a fading row ends with time alone
/// ([`Board::next_change`]). Ends when the runtime or the state broadcast
/// is gone.
async fn board_loop(
    context: Arc<DaemonContext>,
    mut events: mpsc::UnboundedReceiver<BoardEvent>,
    mut voice_rx: tokio::sync::watch::Receiver<Option<AgentVoice>>,
) {
    let mut board = Board::new(context.agents.agents.iter().map(|a| a.name.clone()));
    loop {
        let at = now();
        let entries = board.compose(at);
        context.agent_board.send_if_modified(|shown| {
            if *shown == entries {
                return false;
            }
            debug!(
                "agents: overlay shows {:?}",
                entries
                    .iter()
                    .map(|e| (e.name.as_str(), e.visual.as_str()))
                    .collect::<Vec<_>>()
            );
            *shown = entries;
            true
        });
        let next_change = board.next_change(at);
        let wake = next_change.map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(86_400),
            tokio::time::Instant::from_std,
        );
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => apply_board_event(&mut board, event),
                None => return,
            },
            changed = voice_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let voice = voice_rx.borrow_and_update().clone();
                let voice = voice.as_ref().and_then(|v| {
                    AgentVisual::from_state(v.state).map(|visual| (v.name.as_str(), visual))
                });
                board.set_voice(voice, now());
            }
            _ = tokio::time::sleep_until(wake), if next_change.is_some() => {}
        }
    }
}

fn apply_board_event(board: &mut Board, event: BoardEvent) {
    match event {
        BoardEvent::TurnStarted { agent, since } => board.turn_started(&agent, since),
        BoardEvent::Activity { agent, label } => board.activity(&agent, &label),
        BoardEvent::NeedsYou { agent, at } => board.needs_you(&agent, at),
        BoardEvent::Working { agent } => board.working(&agent),
        BoardEvent::TurnEnded { agent, ok, at } => board.turn_ended(&agent, ok, at),
        BoardEvent::Pending { agents, at } => board.set_pending(&agents, at),
        BoardEvent::Clear { agent } => board.clear(&agent),
    }
}

/// List the sessions the overlay shows working or waiting on their own
/// ([`Slot::shows_idle`]), so such a row ends once the session neither
/// works nor waits, also without a `Stop` (a session stopped from a
/// terminal sends none). One listing per harness answers for all of them
/// ([`AgentRuntime::idle_phases`]).
///
/// The first listing comes [`IDLE_POLL`] after the rows shown changed, and
/// each one doubles the wait up to [`IDLE_POLL_MAX`]: a row shown all day
/// (a background dev server keeps its session working) or a listing that
/// keeps failing is not listed every ten seconds forever. The rows shown
/// changing, or a turn starting, starts the wait over. Sleeps while there
/// is none.
async fn idle_loop(context: Arc<DaemonContext>) {
    let rt = &context.agents;
    let mut shown: Vec<String> = Vec::new();
    let mut wait = IDLE_POLL;
    // When the next listing is due, kept across a wake-up so that one
    // never puts it off.
    let mut due: Option<tokio::time::Instant> = None;
    loop {
        let names: Vec<String> = rt
            .idle_shown_sessions()
            .await
            .into_iter()
            .map(|(agent, _)| agent.name.clone())
            .collect();
        if names.is_empty() {
            shown.clear();
            due = None;
            rt.idle_shown.notified().await;
            continue;
        }
        if names != shown {
            shown = names;
            wait = IDLE_POLL;
        }
        let next = tokio::time::Instant::now() + wait;
        let at = due.map_or(next, |due| due.min(next));
        tokio::select! {
            _ = tokio::time::sleep_until(at) => {}
            // A turn started, or a row started showing: the wait starts
            // over, and is never longer than it was.
            _ = rt.idle_shown.notified() => {
                wait = IDLE_POLL;
                due = Some(at);
                continue;
            }
        }
        due = None;
        let sessions = rt.idle_shown_sessions().await;
        rt.idle_phases(&sessions).await;
        wait = (wait * 2).min(IDLE_POLL_MAX);
    }
}

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

/// `{"cmd":"agent"}`.
pub(crate) async fn handle_agent(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    action: AgentAction,
    name: Option<String>,
    text: Option<String>,
    new: bool,
) -> Response {
    let rt = &context.agents;
    if action == AgentAction::List {
        return Response::Agents {
            agents: rt.list().await,
        };
    }
    if !rt.is_enabled() {
        return error(
            "no agents are configured: add an [[agents]] entry to config.toml \
             (see docs/configuration.md)",
        );
    }
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return error("which agent? Pass its name: whisrs agent <name>");
    };
    let Some(agent) = rt.agent(&name) else {
        let known: Vec<&str> = rt.agents.iter().map(|a| a.name.as_str()).collect();
        return error(&format!(
            "no agent named '{name}' (configured: {}). An entry the config warned about is \
             skipped; see the daemon log.",
            known.join(", ")
        ));
    };
    match action {
        AgentAction::List => unreachable!("answered above"),
        AgentAction::Toggle => voice_toggle(daemon_state, context, agent, new).await,
        AgentAction::Send => {
            let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
                return error("nothing to send: the text is empty");
            };
            if new {
                if let Err(message) = reset(&context, &agent).await {
                    return error(&message);
                }
            }
            let text = text.trim().to_string();
            match start_turn(&context, &agent, text.clone()).await {
                Ok(()) => agent_row(&context, &agent).await,
                Err(busy) => error(&refuse_busy(&context, &agent, busy, Some(&text))),
            }
        }
        AgentAction::Reset => match reset(&context, &agent).await {
            Ok(()) => agent_row(&context, &agent).await,
            Err(message) => error(&message),
        },
        AgentAction::Stop => match stop_turn(&context, &agent).await {
            Ok(()) => agent_row(&context, &agent).await,
            Err(message) => error(&message),
        },
    }
}

fn error(message: &str) -> Response {
    Response::Error {
        message: message.to_string(),
    }
}

async fn agent_row(context: &DaemonContext, agent: &Agent) -> Response {
    let inner = context.agents.inner.lock().await;
    Response::Agents {
        agents: vec![AgentRuntime::info(&inner, agent)],
    }
}

/// Who a hook event belongs to.
enum EventOwner {
    /// The running turn of this agent.
    Turn(String, mpsc::UnboundedSender<HookEvent>),
    /// This agent's stored session, with no whisrs turn running.
    Idle(Arc<Agent>, SessionRef),
}

/// `{"cmd":"agent-event"}`: route a hook event to the running turn whose
/// stored session it belongs to; with no turn running, speak the reply a
/// session produced on its own, or announce the permission prompt its own
/// work waits on; or keep it briefly for a turn that does not know its
/// session id yet.
pub(crate) async fn handle_agent_event(context: &DaemonContext, payload: serde_json::Value) {
    let rt = &context.agents;
    if !rt.is_enabled() {
        debug!("agent-event ignored: no agents configured");
        return;
    }
    let event = HookEvent::now(payload);
    let mut inner = rt.inner.lock().await;
    let mut owner = None;
    for agent in &rt.agents {
        let Some(record) = inner.store.agents.get(&agent.name) else {
            continue;
        };
        if !rt
            .driver(agent.harness)
            .hook_matches(&event.payload, &record.session())
        {
            continue;
        }
        owner = Some(
            match inner.slots.get(&agent.name).and_then(|s| s.turn.as_ref()) {
                Some(turn) => EventOwner::Turn(agent.name.clone(), turn.hooks.clone()),
                None => EventOwner::Idle(Arc::clone(agent), record.session()),
            },
        );
        break;
    }
    match owner {
        Some(EventOwner::Turn(name, hooks)) => {
            debug!("agent '{name}': hook event routed");
            let _ = hooks.send(event);
        }
        Some(EventOwner::Idle(agent, session)) => {
            drop(inner);
            match rt
                .driver(agent.harness)
                .idle_event(&event.payload, &session)
                .await
            {
                IdleEvent::Reply(raw) => own_reply(context, &agent, raw).await,
                IdleEvent::Ended => {
                    let mut inner = rt.inner.lock().await;
                    let slot = inner.slots.entry(agent.name.clone()).or_default();
                    if slot.end_idle() {
                        info!(
                            "agent '{}': the work its session did on its own ended",
                            agent.name
                        );
                        rt.board(BoardEvent::Clear {
                            agent: agent.name.clone(),
                        });
                    }
                }
                IdleEvent::NeedsYou(notice) => {
                    {
                        let mut inner = rt.inner.lock().await;
                        let slot = inner.slots.entry(agent.name.clone()).or_default();
                        if slot.turn.is_none() {
                            slot.wait_for_you(&notice);
                            rt.board(BoardEvent::NeedsYou {
                                agent: agent.name.clone(),
                                at: now(),
                            });
                            rt.idle_shown.notify_one();
                        }
                    }
                    info!(
                        "agent '{}': work its session does on its own waits for you",
                        agent.name
                    );
                    announce_needs_you(context, &agent, notice);
                }
                IdleEvent::Ignored => debug!(
                    "agent '{}': hook event with no turn running ignored",
                    agent.name
                ),
            }
        }
        None => {
            let now = event.received;
            inner
                .unclaimed
                .retain(|e| now.duration_since(e.received) < UNCLAIMED_TTL);
            if inner.unclaimed.len() >= UNCLAIMED_MAX {
                inner.unclaimed.pop_front();
            }
            debug!("agent-event: no running turn matches yet; kept for a moment");
            inner.unclaimed.push_back(event);
        }
    }
}

/// A reply the session produced on its own, with no whisrs turn running it
/// (a `Stop` with no turn running, or the old process's `Stop` before a
/// resumed turn's launch): spoken like a reply, unless it is the late `Stop`
/// of a turn that already ended on a poll, which carries that turn's reply
/// again.
async fn own_reply(context: &DaemonContext, agent: &Agent, raw: String) {
    {
        let mut inner = context.agents.inner.lock().await;
        let slot = inner.slots.entry(agent.name.clone()).or_default();
        // The work it showed as its own (or a wait of it) ended: Done, held
        // while the reply plays.
        if slot.end_idle() {
            context.agents.board(BoardEvent::TurnEnded {
                agent: agent.name.clone(),
                ok: true,
                at: now(),
            });
        }
        if slot
            .polled_reply
            .take()
            .is_some_and(|polled| same_reply(&polled, &raw))
        {
            debug!(
                "agent '{}': the Stop of a turn that already ended on a poll; not spoken twice",
                agent.name
            );
            return;
        }
    }
    info!("agent '{}': the session replied on its own", agent.name);
    deliver_reply(context, agent, &raw);
}

/// Speak and notify a needs-you notice ("Waiting for your approval to use
/// Bash."), under [`WAITING_KEY`]: one at a time per agent.
fn announce_needs_you(context: &DaemonContext, agent: &Agent, notice: String) {
    info!("agent '{}': needs you: {notice}", agent.name);
    if context.notify {
        send_notification_for(
            "whisrs",
            &format!("{}: {notice}", agent.name),
            REPLY_NOTIFICATION_MS,
        );
    }
    context.agents.say_keyed(&agent.name, WAITING_KEY, notice);
}

/// Speak and show an agent's reply (raw final message). Its acknowledgement
/// or update still queued is dropped: the reply says more. One playing goes
/// on, and the reply follows it.
fn deliver_reply(context: &DaemonContext, agent: &Agent, raw: &str) {
    let text = spoken::spoken_reply(raw);
    info!(
        "agent '{}': reply ({} chars, {} spoken)",
        agent.name,
        raw.chars().count(),
        text.chars().count()
    );
    let dropped = context.agents.drop_interims_of(Some(&agent.name));
    if dropped > 0 {
        info!(
            "agent '{}': the reply replaces {dropped} queued acknowledgement(s) or update(s)",
            agent.name
        );
    }
    if context.notify {
        send_notification_for(
            "whisrs",
            &format!("{}: {text}", agent.name),
            REPLY_NOTIFICATION_MS,
        );
    }
    context.agents.say(&agent.name, text);
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// Start a turn for `agent`. Refused while its turn is working
/// ([`Busy::Turn`]), and while its session works on its own outside a whisrs
/// turn ([`Busy::Background`]: stopping it would lose that work); a turn
/// that waits for the user (needs-you) is replaced, so answering it by voice
/// is one press.
async fn start_turn(
    context: &Arc<DaemonContext>,
    agent: &Arc<Agent>,
    text: String,
) -> Result<(), Busy> {
    let rt = &context.agents;
    let idle_session = {
        let inner = rt.inner.lock().await;
        if let Some(slot) = inner.slots.get(&agent.name) {
            if slot.turn.is_some() && slot.phase != AgentPhase::NeedsYou {
                return Err(Busy::Turn);
            }
        }
        AgentRuntime::idle_session(&inner, agent)
    };
    if let Some(session) = idle_session {
        match rt
            .driver(agent.harness)
            .session_phase(agent, &session)
            .await
        {
            Ok(AgentPhase::Working) => {
                info!(
                    "agent '{}': its session is working on its own; not stopped for a new turn",
                    agent.name
                );
                rt.show_own_work(&mut *rt.inner.lock().await, agent);
                return Err(Busy::Background);
            }
            Ok(_) => {}
            Err(e) => debug!(
                "agent '{}': cannot tell what its session does: {e}",
                agent.name
            ),
        }
    }
    let mut inner = rt.inner.lock().await;
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    let mut replaced = None;
    if slot.turn.is_some() {
        if slot.phase != AgentPhase::NeedsYou {
            return Err(Busy::Turn);
        }
        info!(
            "agent '{}': a new turn answers the one waiting for you",
            agent.name
        );
        if let Some(turn) = slot.turn.take() {
            replaced = turn.updates.stop();
            turn.task.abort();
        }
    }
    let now = Utc::now();
    let record = record_mut(&mut inner, agent);
    if let Some(latest) = replaced {
        record.set_session(&latest);
    }
    record.turn_running = true;
    record.turn_started_at = Some(now);
    record.updated_at = now;
    let session = record.session();
    rt.save(&inner);
    info!(
        "agent '{}': starting a turn ({} chars) in {}",
        agent.name,
        text.chars().count(),
        describe(&session)
    );
    let request = TurnRequest {
        text,
        started_at: now,
    };
    spawn_turn(
        context,
        &mut inner,
        Arc::clone(agent),
        session,
        request,
        false,
    );
    Ok(())
}

/// Spawn the task that runs (or, after a restart, watches) a turn and
/// register it in the agent's slot.
fn spawn_turn(
    context: &Arc<DaemonContext>,
    inner: &mut Inner,
    agent: Arc<Agent>,
    session: SessionRef,
    request: TurnRequest,
    rewatch: bool,
) {
    let generation = inner.next_generation;
    inner.next_generation += 1;
    let (hooks_tx, hooks_rx) = mpsc::unbounded_channel();
    let (updates, updates_rx) = TurnUpdates::channel();
    let started = Instant::now();
    // Events this session sent before the turn started belong to an earlier
    // turn (or one the session ran on its own, like a follow-up after a
    // background command): drop them so they cannot end this one.
    forward_unclaimed(context, inner, &agent, &session, &hooks_tx, started);
    let ctx = Arc::clone(context);
    let task_agent = Arc::clone(&agent);
    let io = TurnIo {
        hooks: hooks_rx,
        updates: updates.clone(),
    };
    let text = request.text.clone();
    // The card's timer counts from the turn's start: for one watched again
    // after a restart, when it first started.
    context.agents.board(BoardEvent::TurnStarted {
        agent: agent.name.clone(),
        since: instant_at(request.started_at),
    });
    // The rows of sessions working on their own are listed soon again
    // ([`idle_loop`]): the user is at it.
    context.agents.idle_shown.notify_one();
    let task = tokio::spawn(async move {
        run_turn_task(
            ctx, task_agent, generation, session, request, io, updates_rx, rewatch,
        )
        .await;
    });
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    slot.phase = AgentPhase::Working;
    slot.own_work = false;
    slot.turn = Some(RunningTurn {
        generation,
        started,
        task,
        text,
        hooks: hooks_tx,
        updates,
        gate: ProgressGate::new(agent.feedback),
    });
}

/// Take every unclaimed hook event that belongs to `session`: the ones that
/// arrived at or after `since` (the turn's start) go to `hooks`, older ones
/// are dropped as stale.
fn forward_unclaimed(
    context: &DaemonContext,
    inner: &mut Inner,
    agent: &Agent,
    session: &SessionRef,
    hooks: &mpsc::UnboundedSender<HookEvent>,
    since: Instant,
) {
    if session.session_id.is_none() || inner.unclaimed.is_empty() {
        return;
    }
    let driver = context.agents.driver(agent.harness);
    let (fresh, stale) = take_session_events(
        &mut inner.unclaimed,
        |payload| driver.hook_matches(payload, session),
        since,
    );
    if stale > 0 {
        debug!(
            "agent '{}': dropped {stale} hook event(s) from before this turn",
            agent.name
        );
    }
    for event in fresh {
        debug!("agent '{}': delivering an early hook event", agent.name);
        let _ = hooks.send(event);
    }
}

/// Remove the events `belongs` accepts from `events`: those that arrived at
/// or after `since` are returned in order (with when they arrived, for the
/// driver), older ones are counted and dropped. Everything else stays.
fn take_session_events(
    events: &mut VecDeque<HookEvent>,
    belongs: impl Fn(&serde_json::Value) -> bool,
    since: Instant,
) -> (Vec<HookEvent>, usize) {
    let mut fresh = Vec::new();
    let mut stale = 0;
    let mut kept = VecDeque::new();
    for event in events.drain(..) {
        if !belongs(&event.payload) {
            kept.push_back(event);
        } else if event.received >= since {
            fresh.push(event);
        } else {
            stale += 1;
        }
    }
    *events = kept;
    (fresh, stale)
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_task(
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    generation: u64,
    session: SessionRef,
    request: TurnRequest,
    io: TurnIo,
    mut updates_rx: mpsc::UnboundedReceiver<TurnUpdate>,
    rewatch: bool,
) {
    let driver = context.agents.driver(agent.harness);
    let turn = async {
        if rewatch {
            driver.rewatch(&agent, session, request, io).await
        } else {
            driver.run_turn(&agent, session, request, io).await
        }
    };
    tokio::pin!(turn);
    let outcome = loop {
        tokio::select! {
            outcome = &mut turn => break outcome,
            Some(update) = updates_rx.recv() => {
                apply_update(&context, &agent, generation, update).await;
            }
        }
    };
    while let Ok(update) = updates_rx.try_recv() {
        apply_update(&context, &agent, generation, update).await;
    }
    finish_turn(&context, &agent, generation, outcome).await;
}

/// Record a driver's progress for the turn `generation`.
async fn apply_update(context: &DaemonContext, agent: &Agent, generation: u64, update: TurnUpdate) {
    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    let Some((hooks, started)) = inner
        .slots
        .get(&agent.name)
        .and_then(|s| s.turn.as_ref())
        .filter(|t| t.generation == generation)
        .map(|t| (t.hooks.clone(), t.started))
    else {
        return;
    };
    let set_phase = |inner: &mut Inner, phase: AgentPhase| {
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.phase = phase;
        }
    };
    match update {
        TurnUpdate::Session(session) => {
            let record = record_mut(&mut inner, agent);
            record.set_session(&session);
            record.updated_at = Utc::now();
            rt.save(&inner);
            forward_unclaimed(context, &mut inner, agent, &session, &hooks, started);
        }
        TurnUpdate::NeedsYou(notice) => {
            if let Some(slot) = inner.slots.get_mut(&agent.name) {
                slot.wait_for_you(&notice);
            }
            rt.board(BoardEvent::NeedsYou {
                agent: agent.name.clone(),
                at: now(),
            });
            drop(inner);
            announce_needs_you(context, agent, notice);
        }
        TurnUpdate::Working => {
            set_phase(&mut inner, AgentPhase::Working);
            rt.board(BoardEvent::Working {
                agent: agent.name.clone(),
            });
        }
        TurnUpdate::SessionReply(raw) => {
            drop(inner);
            own_reply(context, agent, raw).await;
        }
        // The overlay's activity line only.
        TurnUpdate::Activity(label) => {
            debug!("agent '{}': activity: {label}", agent.name);
            rt.board(BoardEvent::Activity {
                agent: agent.name.clone(),
                label,
            });
        }
        TurnUpdate::Interim(raw) => {
            let Some(line) = spoken::interim_line(&raw) else {
                debug!(
                    "agent '{}': a message before a tool call has nothing to say",
                    agent.name
                );
                return;
            };
            let at = now();
            let kind = inner
                .slots
                .get_mut(&agent.name)
                .and_then(|s| s.turn.as_mut())
                .and_then(|t| t.gate.admit(&line, at));
            drop(inner);
            match kind {
                Some(kind) => {
                    info!(
                        "agent '{}': {kind:?} ({} chars) at feedback {}",
                        agent.name,
                        line.chars().count(),
                        agent.feedback
                    );
                    rt.say_interim(&agent.name, line, kind, at);
                }
                None => debug!(
                    "agent '{}': an interim is not spoken at feedback {}",
                    agent.name, agent.feedback
                ),
            }
        }
    }
}

/// End the turn `generation`: persist, then speak and show the reply, or
/// notify the error.
async fn finish_turn(
    context: &DaemonContext,
    agent: &Agent,
    generation: u64,
    outcome: TurnOutcome,
) {
    let rt = &context.agents;
    let text = {
        let mut inner = rt.inner.lock().await;
        let current = inner
            .slots
            .get(&agent.name)
            .and_then(|s| s.turn.as_ref())
            .is_some_and(|t| t.generation == generation);
        if !current {
            debug!("agent '{}': a replaced turn finished; ignored", agent.name);
            return;
        }
        let mut text = String::new();
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            text = slot.turn.take().map(|t| t.text).unwrap_or_default();
            slot.phase = AgentPhase::Idle;
            // Armed only by a turn that ended on a poll: its own late Stop
            // is the one duplicate to expect.
            slot.polled_reply = match &outcome {
                TurnOutcome::PolledReply(raw) => Some(raw.clone()),
                _ => None,
            };
        }
        let record = record_mut(&mut inner, agent);
        record.turn_running = false;
        record.turn_started_at = None;
        record.updated_at = Utc::now();
        rt.save(&inner);
        // Under the lock a next turn starts under, so the board sees this
        // end before that start. A turn refused because the session works
        // on its own did not fail: the row shows that work instead.
        if outcome == TurnOutcome::Busy {
            rt.show_own_work(&mut inner, agent);
        } else {
            rt.board(BoardEvent::TurnEnded {
                agent: agent.name.clone(),
                ok: matches!(outcome, TurnOutcome::Reply(_) | TurnOutcome::PolledReply(_)),
                at: now(),
            });
        }
        text
    };
    match outcome {
        TurnOutcome::Reply(raw) | TurnOutcome::PolledReply(raw) => {
            info!("agent '{}': turn done", agent.name);
            deliver_reply(context, agent, &raw);
        }
        TurnOutcome::Busy => {
            refuse_busy(context, agent, Busy::Background, Some(&text));
        }
        TurnOutcome::Failed(message) => {
            // Driver messages name the agent already. Spoken too, so the
            // error is heard with notifications off; it replaces the turn's
            // acknowledgement or update still queued, like a reply.
            error!("{message}");
            if context.notify_error() {
                send_notification_for("whisrs", &message, REPLY_NOTIFICATION_MS);
            }
            rt.drop_interims_of(Some(&agent.name));
            rt.say(&agent.name, rt.error_line(agent, &message));
        }
    }
}

/// The error for forgetting the session while a turn runs (`--reset`, or
/// `--new` while recording or sending). A turn that waits for you is named
/// by its needs-you notice (`waiting`), which a plain press answers.
fn reset_refusal(agent: &Agent, waiting: Option<&str>) -> String {
    match waiting {
        Some(notice) => format!("{notice} {WAITING_HINT} whisrs agent {} --stop", agent.name),
        None => format!(
            "{STILL_WORKING} Stop it first: whisrs agent {} --stop",
            agent.name
        ),
    }
}

/// Forget the agent's session. Refused while a turn runs. What it showed
/// of the session's own work goes with it, and so do its acknowledgements
/// and updates still queued.
async fn reset(context: &DaemonContext, agent: &Agent) -> Result<(), String> {
    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    if let Some(slot) = inner.slots.get(&agent.name).filter(|s| s.turn.is_some()) {
        return Err(reset_refusal(agent, slot.waiting()));
    }
    if let Some(slot) = inner.slots.get_mut(&agent.name) {
        slot.end_idle();
    }
    let record = record_mut(&mut inner, agent);
    // The driver forgets it too (what it remembered to resume it); it keeps
    // running in the harness.
    rt.driver(agent.harness).forget(&record.session());
    if let Some(old) = record.session_id.take() {
        info!(
            "agent '{}': forgot session {old}; the next turn starts a new one (the old session \
             stays in {})",
            agent.name,
            agent.harness.label()
        );
    }
    record.clear_session();
    record.updated_at = Utc::now();
    rt.save(&inner);
    rt.board(BoardEvent::Clear {
        agent: agent.name.clone(),
    });
    drop(inner);
    drop_interims_for(rt, agent);
    Ok(())
}

/// Drop the agent's queued acknowledgements and updates (`--stop`,
/// `--reset`): what they said is moot.
fn drop_interims_for(rt: &AgentRuntime, agent: &Agent) {
    let dropped = rt.drop_interims_of(Some(&agent.name));
    if dropped > 0 {
        info!(
            "agent '{}': dropped {dropped} queued acknowledgement(s) or update(s)",
            agent.name
        );
    }
}

/// Stop the running turn and keep the session, dropping its queued
/// acknowledgements and updates. With no whisrs turn running, stop the
/// session's own work, if it does any.
async fn stop_turn(context: &DaemonContext, agent: &Agent) -> Result<(), String> {
    let rt = &context.agents;
    let session = {
        let mut inner = rt.inner.lock().await;
        let Some(turn) = inner.slots.get_mut(&agent.name).and_then(|s| s.turn.take()) else {
            let idle_session = AgentRuntime::idle_session(&inner, agent);
            drop(inner);
            return stop_idle_session(context, agent, idle_session).await;
        };
        // Marked stopped first: a launch still running stops what it starts
        // itself, and the ids it reported already (a bg id whose session id
        // is still being looked up) are stopped here.
        let latest = turn.updates.stop();
        turn.task.abort();
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.phase = AgentPhase::Idle;
        }
        let record = record_mut(&mut inner, agent);
        if let Some(latest) = latest {
            record.set_session(&latest);
        }
        record.turn_running = false;
        record.turn_started_at = None;
        record.updated_at = Utc::now();
        let session = record.session();
        rt.save(&inner);
        rt.board(BoardEvent::Clear {
            agent: agent.name.clone(),
        });
        session
    };
    drop_interims_for(rt, agent);
    info!("agent '{}': turn stopped", agent.name);
    if let Err(e) = rt.driver(agent.harness).stop(agent, &session).await {
        warn!("agent '{}': {e}", agent.name);
        return Err(format!(
            "the turn was abandoned, but stopping the session failed: {e}"
        ));
    }
    Ok(())
}

/// `--stop` with no whisrs turn running: stop the session when it works on
/// its own (what `whisrs agent` shows as working), else report that nothing
/// runs.
async fn stop_idle_session(
    context: &DaemonContext,
    agent: &Agent,
    session: Option<SessionRef>,
) -> Result<(), String> {
    let nothing = || format!("no turn is running for '{}'", agent.name);
    let Some(session) = session else {
        return Err(nothing());
    };
    let driver = context.agents.driver(agent.harness);
    match driver.session_phase(agent, &session).await {
        Ok(AgentPhase::Working | AgentPhase::NeedsYou) => {
            info!(
                "agent '{}': stopping the work its session does on its own",
                agent.name
            );
            driver
                .stop(agent, &session)
                .await
                .map_err(|e| format!("stopping the session failed: {e}"))?;
            // A needs-you that work set (a permission prompt with no turn
            // running) ends with it, so the row printed after the stop does
            // not say needs-you. A turn started meanwhile keeps its phase.
            let mut inner = context.agents.inner.lock().await;
            if let Some(slot) = inner.slots.get_mut(&agent.name) {
                if slot.turn.is_none() {
                    slot.end_idle();
                    context.agents.board(BoardEvent::Clear {
                        agent: agent.name.clone(),
                    });
                }
            }
            Ok(())
        }
        Ok(AgentPhase::Idle) => {
            context
                .agents
                .idle_phase_seen(agent, AgentPhase::Idle)
                .await;
            Err(nothing())
        }
        Err(e) => Err(format!(
            "{}; cannot tell whether its session works: {e}",
            nothing()
        )),
    }
}

/// Speak and notify a busy refusal, and return the error for a CLI
/// request. `instruction` is the text that was not sent (transcribed or
/// typed), repeated in the notification so it can be sent again.
fn refuse_busy(
    context: &DaemonContext,
    agent: &Agent,
    busy: Busy,
    instruction: Option<&str>,
) -> String {
    let message = notify_busy(context, agent, busy, instruction);
    context.agents.say_notice(&agent.name, busy.notice());
    message
}

/// Log and notify a busy refusal (per `[general] notify`) without speaking
/// it, and return the error for a CLI request: [`refuse_busy`] minus the
/// notice, for a press that just stopped the reply or notice playing, so it
/// does not queue another one in its place.
fn notify_busy(
    context: &DaemonContext,
    agent: &Agent,
    busy: Busy,
    instruction: Option<&str>,
) -> String {
    match busy {
        Busy::Turn => info!("agent '{}': refused, a turn is running", agent.name),
        Busy::Background => info!(
            "agent '{}': refused, its session works on its own",
            agent.name
        ),
    }
    if context.notify_error() {
        send_notification_for(
            "whisrs",
            &busy_notification(&agent.name, busy, instruction),
            REPLY_NOTIFICATION_MS,
        );
    }
    busy.message(&agent.name)
}

/// Refuse a fresh session while the agent's turn waits for you: notified
/// (per `[general] notify`, repeating `instruction` when there is one) with
/// how to go on, and the needs-you notice spoken again unless `speak` is
/// off (the press just stopped this agent's playback). Returns the error
/// for a CLI request.
fn refuse_waiting(
    context: &DaemonContext,
    agent: &Agent,
    notice: &str,
    instruction: Option<&str>,
    speak: bool,
) -> String {
    info!(
        "agent '{}': refused a fresh session, its turn waits for you",
        agent.name
    );
    let message = reset_refusal(agent, Some(notice));
    if context.notify_error() {
        let mut body = format!("{}: {message}", agent.name);
        if let Some(text) = instruction.map(str::trim).filter(|t| !t.is_empty()) {
            body.push_str(&format!(
                " Not sent: \"{}\".",
                truncate_preview(text, REFUSED_INSTRUCTION_MAX)
            ));
        }
        send_notification_for("whisrs", &body, REPLY_NOTIFICATION_MS);
    }
    if speak {
        context
            .agents
            .say_keyed(&agent.name, WAITING_KEY, notice.to_string());
    }
    message
}

// ---------------------------------------------------------------------------
// Voice turns: the recording sink
// ---------------------------------------------------------------------------

/// `whisrs agent <name>` or its hotkey; with `new_session` (`--new`, or its
/// `new_session_hotkey`) the transcript starts a fresh session.
///
/// - Recording for this agent: stop and transcribe (a press of either kind;
///   the press that started the recording decides about the session).
/// - This agent's turn works (`--new`: any turn of it runs): refuse. When
///   this agent's own reply or notice plays, the press stops it and only
///   notifies, so it never queues another notice in its place; another
///   agent's reply plays on, and the notice is spoken (once) after it.
///   `--new` while the turn waits for you says so with its needs-you notice
///   (a plain press answers it).
/// - Otherwise an agent reply or notice playing (any agent's) is stopped
///   (barge-in) and a voice turn recorded (a selection read-aloud is
///   stopped for it). Anything else: busy.
async fn voice_toggle(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    new_session: bool,
) -> Response {
    let rt = &context.agents;
    // Known before the playback check: a refused press silences only this
    // agent's own playback.
    let refusal = rt.press_refusal(&agent.name, new_session).await;
    // Held from a barge-in stop until the recording started or the press
    // was refused, so the reply queue cannot start the next reply between
    // the stop and the recording.
    let mut barge_in = None;
    {
        let mut ds = daemon_state.lock().await;
        match ds.state_machine.state() {
            State::Recording => {
                let ours = ds
                    .agent_recording
                    .as_ref()
                    .is_some_and(|r| r.name == agent.name);
                if !ours {
                    return error(
                        "busy: another recording is active. Finish or cancel it first \
                         (whisrs cancel)",
                    );
                }
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                info!("agent '{}': manual stop", agent.name);
                return Response::Ok {
                    state: State::Recording,
                };
            }
            State::Transcribing => {
                return error("busy: still transcribing, try again in a moment");
            }
            State::Speaking | State::Synthesizing => {
                // Another agent's reply is not this press's to silence
                // when the press is refused: it would be lost.
                if refusal.is_none() || rt.is_playing_for(&agent.name) {
                    let reason = format!("agent '{}'", agent.name);
                    if crate::speak::stop_agent_playback(&mut ds, &context, &reason) {
                        barge_in = Some(rt.hold_replies());
                    }
                }
            }
            State::Idle => {}
        }
    }

    if let Some(refusal) = refusal {
        // A press that stopped this agent's playback asked for silence: no
        // notice is spoken in its place.
        let speak = barge_in.is_none();
        if !speak {
            info!(
                "agent '{}': the press stopped its playback; no notice is queued for the \
                 refusal",
                agent.name
            );
        }
        let message = match refusal {
            Refusal::Waiting(notice) => refuse_waiting(&context, &agent, &notice, None, speak),
            Refusal::Busy => {
                let message = if speak {
                    refuse_busy(&context, &agent, Busy::Turn, None)
                } else {
                    notify_busy(&context, &agent, Busy::Turn, None)
                };
                if new_session {
                    reset_refusal(&agent, None)
                } else {
                    message
                }
            }
        };
        return error(&message);
    }

    #[cfg(feature = "recording")]
    if let Some(refused) = refuse_while_recording(&context, DictationStart::Agent(&agent.name)) {
        return refused;
    }

    let response = start_recording(daemon_state, Arc::clone(&context), agent, new_session).await;
    drop(barge_in);
    response
}

/// Start recording a voice turn. Under one lock, so a reply that would start
/// speaking cannot slip in between stopping playback and the recording.
async fn start_recording(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    new_session: bool,
) -> Response {
    let audio_rx = {
        let mut ds = daemon_state.lock().await;
        match ds.state_machine.state() {
            State::Speaking | State::Synthesizing => {
                stop_read_aloud(&mut ds, &context);
                info!("agent '{}': barge-in, playback stopped", agent.name);
            }
            State::Idle => {}
            other => {
                return error(&format!("busy: the daemon is {other}"));
            }
        }

        if context.config.general.audio_feedback {
            feedback::play_start(context.config.general.audio_feedback_volume);
        }
        let mut capture = match AudioCaptureHandle::start_with_level_tx(
            &context.config.audio.device,
            context.overlay_level_tx.clone(),
        ) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{e}");
                let friendly = if msg.contains("no default audio input device") {
                    format_no_microphone_error(&context.config.audio.device)
                } else {
                    format!("Failed to start audio capture: {e}")
                };
                error!("{friendly}");
                return error(&friendly);
            }
        };
        let audio_rx = capture.take_receiver();
        if let Err(e) = ds.state_machine.transition(Action::Toggle) {
            return error(&format!("state transition failed: {e}"));
        }
        ds.audio_capture = Some(capture);
        ds.recording_started_at = Some(std::time::Instant::now());
        ds.agent_recording = Some(AgentRecordingContext {
            name: agent.name.clone(),
            new_session,
        });
        context.state_tx.publish(&ds);
        audio_rx
    };

    let fresh = if new_session { " in a new session" } else { "" };
    info!("agent '{}': recording a voice turn{fresh}", agent.name);
    if context.notify_agent_state() {
        send_notification(
            "whisrs",
            &format!("Speak to '{}'{fresh}... (press again to stop)", agent.name),
        );
    }

    let ds_ref = Arc::clone(&daemon_state);
    let ctx = Arc::clone(&context);
    tokio::spawn(async move {
        voice_turn_background(audio_rx, ds_ref, ctx, agent).await;
    });

    Response::Ok {
        state: State::Recording,
    }
}

/// Collect the instruction, transcribe it, return the state machine to Idle,
/// then hand the transcript to the agent.
async fn voice_turn_background(
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>>,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
) {
    let label = format!("agent '{}'", agent.name);
    let audio = collect_instruction_audio(audio_rx, &daemon_state, &context, &label).await;

    // Same claim as command mode: `whisrs cancel` (or a plain toggle taking
    // the recording over as dictation) takes the context under this lock,
    // and then the recording is discarded.
    let claimed = {
        let mut guard = daemon_state.lock().await;
        let ds = &mut *guard;
        let claimed = claim_session(&mut ds.state_machine, &mut ds.agent_recording);
        if let Some(claimed) = &claimed {
            ds.audio_capture.take();
            ds.recording_started_at.take();
            // Transcribing for this agent: its voice, not the pill's.
            ds.agent_transcribing = Some(claimed.name.clone());
            context.state_tx.publish(ds);
        }
        claimed
    };
    let Some(claimed) = claimed else {
        info!("{label}: voice turn cancelled, recording discarded");
        return;
    };

    let transcript = if audio.nothing_heard {
        // Nothing heard: discarded, with no stop cue (as a cancel).
        report_nothing_heard(&context, &BatchOrigin::Agent { name: &agent.name });
        Ok(String::new())
    } else {
        if context.config.general.audio_feedback {
            feedback::play_stop(context.config.general.audio_feedback_volume);
        }
        transcribe_batch_audio(
            &audio.samples,
            &context,
            &context.config.general.language,
            &BatchOptions::agent(&agent.name),
        )
        .await
    };

    // Back to Idle at once: the turn runs on its own task.
    {
        let mut ds = daemon_state.lock().await;
        let _ = ds.state_machine.transition(Action::TranscriptionDone);
        ds.agent_transcribing = None;
        context.state_tx.publish(&ds);
        if let Some(level_tx) = &context.overlay_level_tx {
            let _ = level_tx.send(0.0);
        }
    }

    // Both failures are spoken as well as notified per [general] notify,
    // so they are heard with notifications off.
    let rt = &context.agents;
    let text = match transcript {
        Ok(text) => text,
        Err(e) => {
            let friendly = format_api_error(&e);
            error!("{label}: transcription failed: {e:#}");
            let message = format!("Agent '{}': transcription failed: {friendly}", agent.name);
            if context.notify_error() {
                send_notification("whisrs", &message);
            }
            rt.say(&agent.name, rt.error_line(&agent, &message));
            return;
        }
    };
    if text.trim().is_empty() {
        // Gated, a prompt echo or unintelligible: the helper notified.
        info!("{label}: nothing to send");
        rt.say(
            &agent.name,
            rt.line_for(&agent.name, DIDNT_CATCH.to_string()),
        );
        return;
    }
    if claimed.new_session {
        // A typed turn may have started while this one recorded: the
        // session is not forgotten under it.
        if reset(&context, &agent).await.is_err() {
            match rt.press_refusal(&agent.name, true).await {
                Some(Refusal::Waiting(notice)) => {
                    refuse_waiting(&context, &agent, &notice, Some(&text), true);
                }
                _ => {
                    refuse_busy(&context, &agent, Busy::Turn, Some(&text));
                }
            }
            return;
        }
    }
    if let Err(busy) = start_turn(&context, &agent, text.clone()).await {
        refuse_busy(&context, &agent, busy, Some(&text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent(name: &str) -> Agent {
        Agent {
            name: name.into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::Claude,
            dir: PathBuf::from("/proj"),
            permissions: whisrs::agent::Permissions::Auto,
            command: "claude".into(),
            args: Vec::new(),
            instruction: String::new(),
            feedback: whisrs::agent::progress::Feedback::Brief,
        }
    }

    /// A transcription backend that always fails, like an unreachable API.
    struct UnreachableBackend;

    #[async_trait::async_trait]
    impl whisrs::transcription::TranscriptionBackend for UnreachableBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            anyhow::bail!("error sending request: connection refused")
        }

        fn sends_prompt(&self, _config: &whisrs::transcription::TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> whisrs::transcription::StreamText {
            whisrs::transcription::StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<whisrs::transcription::Transcript> {
            unreachable!("voice turns do not request segments")
        }

        fn max_segment_chunk_secs(
            &self,
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> Option<u32> {
            None
        }
    }

    /// A TTS backend that never gets to synthesize in these tests.
    struct NoTts;

    #[async_trait::async_trait]
    impl whisrs::tts::TtsBackend for NoTts {
        async fn synthesize(
            &self,
            _text: &str,
        ) -> Result<Box<dyn whisrs::tts::TtsAudioStream>, whisrs::WhisrsError> {
            Err(whisrs::WhisrsError::Audio("no TTS in tests".into()))
        }
    }

    /// A context whose runtime drives `names` with the Claude driver and TTS
    /// on, its store in `dir`, nothing notified and no audio cue played.
    fn test_context(names: &[&str], dir: &Path) -> Arc<DaemonContext> {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = names.iter().map(|n| Arc::new(test_agent(n))).collect();
        rt.drivers.insert(
            Harness::Claude,
            Arc::new(ClaudeDriver::new(
                Path::new("whisrs"),
                Path::new("/nonexistent.sock"),
            )),
        );
        rt.store_path = dir.join("agents.json");
        Arc::new(DaemonContext {
            // The RMS test: the voice turns' test audio is a square wave,
            // loud but no voice, which earshot would gate.
            config: toml::from_str("[audio]\nvad = \"rms\"\n[general]\naudio_feedback = false\n")
                .unwrap(),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            transcription_backend: Arc::new(UnreachableBackend),
            notify: false,
            state_tx: crate::context::StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            #[cfg(feature = "recording")]
            recording: crate::recording::RecordingRuntime::new().0,
            agents: rt,
            agent_board: tokio::sync::watch::channel(Vec::new()).0,
            agent_overlay_shown: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Put a running turn of `generation` in the agent's slot.
    async fn install_turn(context: &DaemonContext, name: &str, generation: u64, text: &str) {
        let mut inner = context.agents.inner.lock().await;
        let slot = inner.slots.entry(name.to_string()).or_default();
        slot.phase = AgentPhase::Working;
        slot.turn = Some(RunningTurn {
            generation,
            started: Instant::now(),
            task: tokio::spawn(async {}),
            text: text.to_string(),
            hooks: mpsc::unbounded_channel().0,
            updates: TurnUpdates::channel().0,
            gate: ProgressGate::new(
                context
                    .agents
                    .agent(name)
                    .map_or_else(whisrs::agent::progress::Feedback::default, |a| a.feedback),
            ),
        });
    }

    fn queued(rt: &AgentRuntime) -> Vec<String> {
        std::iter::from_fn(|| rt.next_utterance())
            .map(|u| u.text)
            .collect()
    }

    fn stop_hook(message: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": "sid-1",
            "hook_event_name": "Stop",
            "last_assistant_message": message,
        })
    }

    /// With no whisrs turn running: a permission prompt from the session's
    /// own work marks the agent needs-you and speaks the notice; its Stop is
    /// spoken and clears needs-you. The duplicate check compares raw text,
    /// is armed only by a turn that ended on a poll, and drops only that
    /// turn's late Stop: a real follow-up with the same spoken line is
    /// spoken.
    #[tokio::test]
    async fn hook_events_with_no_turn_running() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        )
        .unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        {
            let mut inner = rt.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-1".into());
            record.bg_id = Some("bg1".into());
            record.transcript_path = Some(transcript);
            inner.store.agents.insert("jarvis".into(), record);
        }
        handle_agent_event(
            &context,
            serde_json::json!({
                "session_id": "sid-1",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            }),
        )
        .await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::NeedsYou);
        assert_eq!(queued(rt), ["Waiting for your approval to use Bash."]);

        handle_agent_event(&context, stop_hook("Ran it. <spoken>Tests pass.</spoken>")).await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        handle_agent_event(&context, stop_hook("Again. <spoken>Tests pass.</spoken>")).await;
        assert_eq!(queued(rt), ["Tests pass.", "Tests pass."]);

        // Another session's events are not ours.
        handle_agent_event(
            &context,
            serde_json::json!({"session_id": "other", "hook_event_name": "Stop",
                               "last_assistant_message": "<spoken>Not ours.</spoken>"}),
        )
        .await;
        assert!(queued(rt).is_empty());

        // A turn that ended on a poll: its own late Stop is the duplicate.
        rt.inner
            .lock()
            .await
            .slots
            .get_mut("jarvis")
            .unwrap()
            .polled_reply = Some("Done.\n\n<spoken>All set.</spoken>".into());
        handle_agent_event(&context, stop_hook(" Done. <spoken>All set.</spoken>")).await;
        assert!(queued(rt).is_empty());
        handle_agent_event(&context, stop_hook("Done. <spoken>All set.</spoken>")).await;
        assert_eq!(queued(rt), ["All set."]);
    }

    /// Only a turn that ended on a poll arms the duplicate check; one that
    /// ended on its Stop clears it. A turn refused because the session
    /// works on its own speaks the background-task notice.
    #[tokio::test]
    async fn turn_ends_arm_the_duplicate_check_and_speak() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        let polled = |inner: &Inner| inner.slots["jarvis"].polled_reply.clone();

        install_turn(&context, "jarvis", 7, "check it").await;
        let raw = "Checked. <spoken>All good.</spoken>";
        finish_turn(&context, &agent, 7, TurnOutcome::PolledReply(raw.into())).await;
        assert_eq!(polled(&*rt.inner.lock().await).as_deref(), Some(raw));
        assert_eq!(queued(rt), ["All good."]);

        install_turn(&context, "jarvis", 8, "again").await;
        finish_turn(&context, &agent, 8, TurnOutcome::Reply(raw.into())).await;
        assert_eq!(polled(&*rt.inner.lock().await), None);
        assert_eq!(queued(rt), ["All good."]);

        install_turn(&context, "jarvis", 9, "add a test").await;
        finish_turn(&context, &agent, 9, TurnOutcome::Busy).await;
        assert_eq!(queued(rt), [BACKGROUND_TASK]);
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        assert!(!rt.inner.lock().await.store.agents["jarvis"].turn_running);
    }

    /// The old process's Stop before a resumed turn's launch is spoken as a
    /// reply the session produced on its own.
    #[tokio::test]
    async fn a_stop_from_the_old_process_is_spoken_as_a_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 3, "next").await;
        apply_update(
            &context,
            &agent,
            3,
            TurnUpdate::SessionReply("Finished. <spoken>The server is up.</spoken>".into()),
        )
        .await;
        assert_eq!(queued(&context.agents), ["The server is up."]);
        // The running turn is untouched.
        assert_eq!(context.agents.phase("jarvis").await, AgentPhase::Working);
    }

    /// A voice turn that had nothing to send, or whose transcription failed,
    /// is spoken, so it is heard with notifications off.
    #[tokio::test]
    async fn a_voice_turn_that_sends_nothing_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        let recording = || async {
            let ds = Arc::new(Mutex::new(DaemonState::new()));
            {
                let mut g = ds.lock().await;
                g.state_machine.transition(Action::Toggle).unwrap();
                g.agent_recording = Some(AgentRecordingContext {
                    name: "jarvis".into(),
                    new_session: false,
                });
            }
            ds
        };

        // No audio at all.
        let ds = recording().await;
        voice_turn_background(
            None,
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
        )
        .await;
        assert_eq!(queued(&context.agents), [DIDNT_CATCH]);
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);

        // Loud audio, and the backend cannot be reached.
        let ds = recording().await;
        let (tx, rx) = mpsc::unbounded_channel();
        let loud: Vec<i16> = (0..16_000)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect();
        tx.send(loud).unwrap();
        drop(tx);
        voice_turn_background(Some(rx), Arc::clone(&ds), Arc::clone(&context), agent).await;
        let lines = queued(&context.agents);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("Transcription failed: Cannot reach API"),
            "{lines:?}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        // No turn was started.
        assert!(context.agents.inner.lock().await.store.agents.is_empty());
    }

    /// A hold (a command press capturing its selection) keeps the reply
    /// queue from starting a reply: `speak_text` refuses it under the lock.
    /// Dropping the hold wakes the queue.
    #[tokio::test]
    async fn a_hold_keeps_agent_replies_from_starting() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let hold = context.agents.hold_replies();
        assert!(replies_held(&context));
        let attempt = speak_text(
            Arc::clone(&ds),
            Arc::clone(&context),
            Box::new(NoTts),
            "a reply",
            SpeakOrigin::Agent,
        )
        .await;
        assert!(matches!(attempt, SpeakAttempt::NotIdle(State::Idle)));
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert!(ds.lock().await.tts_stop.is_none());
        let second = context.agents.hold_replies();
        drop(hold);
        assert!(replies_held(&context), "one hold is still live");
        drop(second);
        assert!(!replies_held(&context));
        tokio::time::timeout(
            Duration::from_secs(1),
            context.agents.utterance_ready.notified(),
        )
        .await
        .expect("a dropped hold wakes the reply queue");
    }

    /// Command mode and `[[llm_commands]]` release the dictation lock before
    /// their recording starts (command mode captures the selection), so both
    /// take a reply hold in the locked block that stopped agent playback:
    /// the queue cannot start the next reply in between and take the Idle
    /// state from them ("state transition failed").
    #[test]
    fn command_presses_hold_replies_until_they_record() {
        let code = include_str!("command_mode.rs");
        for signature in ["fn handle_command_mode(", "fn handle_llm_command("] {
            let start = code.find(signature).expect(signature);
            let body = &code[start..start + code[start..].find("\n}").unwrap()];
            let block = &body[..body.find("match current_state").expect(signature)];
            let lock = block.find("daemon_state.lock()").expect(signature);
            let stop = block.find("stop_agent_playback(").expect(signature);
            let hold = block.find("hold_replies()").expect(signature);
            assert!(lock < stop && stop < hold, "{signature}");
            // Held while the start runs: bound for the whole handler.
            assert!(block.contains("_hold)"), "{signature}");
        }
    }

    /// A reply of `jarvis`.
    fn utterance(text: &str) -> Utterance {
        Utterance {
            agent: "jarvis".into(),
            text: text.into(),
            notice: None,
            interim: None,
        }
    }

    /// Put `ds` in a read-aloud of `origin` that plays, as `speak_text`
    /// does; returns its stop flag.
    async fn start_speaking(
        ds: &Mutex<DaemonState>,
        origin: SpeakOrigin,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut g = ds.lock().await;
        g.state_machine.transition(Action::SpeakStart).unwrap();
        g.state_machine.transition(Action::SpeakPlaying).unwrap();
        g.tts_stop = Some(Arc::clone(&stop));
        g.tts_origin = Some(origin);
        stop
    }

    /// A recording start stops an agent reply being played, which plays
    /// again after the recording (first in the queue), and holds replies
    /// until the guard drops. A selection read-aloud is left alone.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn a_recording_start_stops_an_agent_reply_and_plays_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.daemon_state.set(Arc::clone(&ds)).ok().unwrap();

        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("first")));
        rt.say("jarvis", "second".into());
        let hold = hold_for_recording(&context);
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert!(stop.load(Ordering::Acquire), "playback was not stopped");
        assert!(replies_held(&context));
        drop(hold);
        assert!(!replies_held(&context));
        assert_eq!(queued(rt), ["first", "second"]);
        // The reply queue clears `playing` once it sees the playback end.
        rt.set_playing(None);

        // A selection read-aloud keeps playing, even while the queue is
        // about to speak (it then finds the daemon busy and waits).
        let stop = start_speaking(&ds, SpeakOrigin::Selection).await;
        rt.set_playing(Some(utterance("third")));
        let hold = hold_for_recording(&context);
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert!(!stop.load(Ordering::Acquire));
        assert!(
            rt.is_speaking(),
            "only an agent reply that played is requeued"
        );
        drop(hold);
        assert!(queued(rt).is_empty());
    }

    /// The stop broadcasts Idle, and the reply queue, woken by it on another
    /// worker, can clear `playing` before the stopper puts the reply back.
    /// The reply is copied before the stop, so it still plays after the
    /// recording, once. Run here in that order: `playing` is cleared inside
    /// the stop, as the queue would between the stop and the requeue.
    #[cfg(feature = "recording")]
    #[test]
    fn a_stopped_reply_is_requeued_when_the_queue_clears_it_first() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.set_playing(Some(utterance("first")));
        rt.say("jarvis", "second".into());
        assert!(rt.requeue_if_stopped(|| {
            rt.set_playing(None);
            true
        }));
        assert_eq!(queued(&rt), ["first", "second"]);
        assert!(!rt.is_speaking());

        // Nothing was stopped: nothing is put back.
        rt.set_playing(Some(utterance("third")));
        assert!(!rt.requeue_if_stopped(|| false));
        assert!(queued(&rt).is_empty());
    }

    /// A recording start never waits on the dictation lock, which a
    /// dictation start holds while its mic opens: it holds replies at once
    /// and stops the agent reply playing once the lock is free.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn a_recording_start_does_not_wait_on_the_dictation_lock() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.daemon_state.set(Arc::clone(&ds)).ok().unwrap();
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("first")));

        // A dictation start holds the lock.
        let dictation = ds.lock().await;
        let hold = hold_for_recording(&context);
        assert!(replies_held(&context));
        assert!(!stop.load(Ordering::Acquire), "stopped without the lock");
        assert!(!rt.has_utterances());
        drop(dictation);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !rt.has_utterances() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stop runs once the lock is free");
        assert!(stop.load(Ordering::Acquire), "playback was not stopped");
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        drop(hold);
        assert_eq!(queued(rt), ["first"]);

        // A start that is gone by the time the lock frees (nothing holds
        // replies) stops nothing.
        rt.set_playing(None);
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("second")));
        let dictation = ds.lock().await;
        drop(hold_for_recording(&context));
        drop(dictation);
        // Let the spawned stop run (one runtime thread: it runs on the
        // first yield, and nothing in it waits once the lock is free).
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert!(!stop.load(Ordering::Acquire));
        assert!(!rt.has_utterances());
    }

    /// A harness whose session waits on a permission prompt it hit on its
    /// own (no whisrs turn running), and stops when asked.
    struct PromptWaitingDriver;

    #[async_trait::async_trait]
    impl Driver for PromptWaitingDriver {
        async fn run_turn(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn runs")
        }

        async fn rewatch(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn runs")
        }

        async fn stop(&self, _agent: &Agent, _session: &SessionRef) -> Result<(), String> {
            Ok(())
        }

        fn forget(&self, _session: &SessionRef) {}

        fn hook_matches(&self, payload: &serde_json::Value, _session: &SessionRef) -> bool {
            payload["session_id"] == "sid-1"
        }

        async fn session_phase(
            &self,
            _agent: &Agent,
            _session: &SessionRef,
        ) -> Result<AgentPhase, String> {
            Ok(AgentPhase::NeedsYou)
        }

        async fn session_phases(
            &self,
            sessions: &[(Arc<Agent>, SessionRef)],
        ) -> Vec<Result<AgentPhase, String>> {
            sessions.iter().map(|_| Ok(AgentPhase::NeedsYou)).collect()
        }

        async fn idle_event(
            &self,
            _payload: &serde_json::Value,
            _session: &SessionRef,
        ) -> IdleEvent {
            IdleEvent::NeedsYou("Waiting for your approval to use Bash.".into())
        }
    }

    /// `--stop` with no whisrs turn running stops the session's own work,
    /// and the needs-you its permission prompt set goes with it: the row
    /// printed after the stop says idle.
    #[tokio::test]
    async fn stopping_a_sessions_own_work_clears_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = test_context(&["jarvis"], dir.path());
        Arc::get_mut(&mut context)
            .unwrap()
            .agents
            .drivers
            .insert(Harness::Claude, Arc::new(PromptWaitingDriver));
        {
            let mut inner = context.agents.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-1".into());
            record.bg_id = Some("bg1".into());
            inner.store.agents.insert("jarvis".into(), record);
        }
        handle_agent_event(
            &context,
            serde_json::json!({
                "session_id": "sid-1",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            }),
        )
        .await;
        assert_eq!(context.agents.phase("jarvis").await, AgentPhase::NeedsYou);

        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let response = handle_agent(
            ds,
            Arc::clone(&context),
            AgentAction::Stop,
            Some("jarvis".into()),
            None,
            false,
        )
        .await;
        match response {
            Response::Agents { agents } => assert_eq!(agents[0].phase, AgentPhase::Idle),
            other => panic!("expected the agent's row, got {other:?}"),
        }
    }

    #[test]
    fn the_still_working_refusal_is_not_notified_twice() {
        assert!(error_already_notified(STILL_WORKING));
        assert!(error_already_notified(&format!(
            "{STILL_WORKING} Stop it first: whisrs agent jarvis --stop"
        )));
        // The CLI errors of both refusals are what was notified already.
        assert!(error_already_notified(&Busy::Turn.message("jarvis")));
        assert!(error_already_notified(&Busy::Background.message("jarvis")));
        // A fresh session refused while the turn waits for you.
        assert!(error_already_notified(&reset_refusal(
            &test_agent("jarvis"),
            Some("Waiting for your approval to use Bash.")
        )));
        assert!(error_already_notified(
            "A recording is in progress. Agent 'jarvis' is off until you stop it."
        ));
        assert!(!error_already_notified("no agent named 'x'"));
    }

    #[test]
    fn no_agents_means_an_inert_runtime() {
        let config: Config = toml::from_str("").unwrap();
        let rt = AgentRuntime::new(&config, Path::new("/nonexistent.sock"));
        assert!(!rt.is_enabled());
        assert!(!rt.has_hotkeys());
        assert!(rt.agents().is_empty());
        assert!(rt.drivers.is_empty());
        // Nothing is queued to speak.
        rt.say("jarvis", "x".into());
        assert!(!rt.has_utterances());
    }

    /// A session's events from before a turn started (its previous turn, or
    /// a follow-up it ran on its own) never end the new turn; events that
    /// arrived after the start (a first turn's Stop, before its session id
    /// was known) do. Other sessions' events stay.
    #[test]
    fn early_events_reach_the_turn_and_stale_ones_are_dropped() {
        let t0 = Instant::now();
        let start = t0 + Duration::from_secs(5);
        let ev = |sid: &str, n: u32| serde_json::json!({"session_id": sid, "n": n});
        let at = |received: Instant, payload| HookEvent { received, payload };
        let mut events: VecDeque<_> = [
            at(t0, ev("ours", 1)),
            at(t0, ev("other", 2)),
            at(start, ev("ours", 3)),
            at(start + Duration::from_secs(1), ev("ours", 4)),
        ]
        .into_iter()
        .collect();
        let (fresh, stale) = take_session_events(&mut events, |p| p["session_id"] == "ours", start);
        let payloads: Vec<_> = fresh.iter().map(|e| e.payload.clone()).collect();
        assert_eq!(payloads, vec![ev("ours", 3), ev("ours", 4)]);
        // The arrival time travels with the event, for the driver.
        assert_eq!(fresh[1].received, start + Duration::from_secs(1));
        assert_eq!(stale, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, ev("other", 2));
    }

    /// Replies wait while a recording captures, even with the daemon Idle,
    /// and while dictation, a command or a read-aloud runs.
    #[test]
    fn replies_wait_for_idle_and_no_recording() {
        assert!(may_speak(State::Idle, false));
        assert!(!may_speak(State::Idle, true));
        for state in [
            State::Recording,
            State::Transcribing,
            State::Synthesizing,
            State::Speaking,
        ] {
            assert!(!may_speak(state, false), "{state:?}");
            assert!(!may_speak(state, true), "{state:?}");
        }
    }

    /// One busy notice of a kind per agent at a time: not queued again while
    /// it is queued or playing. With several agents the spoken line names
    /// the agent, and the check still holds: it compares the notice, not
    /// the line. Other utterances are never deduplicated.
    #[test]
    fn a_busy_notice_is_not_queued_twice() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = vec![Arc::new(test_agent("a")), Arc::new(test_agent("b"))];
        rt.say_notice("a", STILL_WORKING);
        rt.say_notice("a", STILL_WORKING);
        rt.say_notice("b", STILL_WORKING);
        rt.say_notice("a", BACKGROUND_TASK);
        rt.say_notice("a", BACKGROUND_TASK);
        rt.say("a", "Done.".into());
        rt.say("a", "Done.".into());
        let queued: Vec<_> = std::iter::from_fn(|| rt.next_utterance())
            .map(|u| u.text)
            .collect();
        assert_eq!(
            queued,
            [
                format!("a: {STILL_WORKING}"),
                format!("b: {STILL_WORKING}"),
                format!("a: {BACKGROUND_TASK}"),
                "Done.".to_string(),
                "Done.".to_string()
            ]
        );
        // Playing counts too, until its playback ends.
        rt.set_playing(Some(Utterance {
            agent: "a".into(),
            text: format!("a: {STILL_WORKING}"),
            notice: Some(STILL_WORKING),
            interim: None,
        }));
        rt.say_notice("a", STILL_WORKING);
        assert!(!rt.has_utterances());
        rt.set_playing(None);
        rt.say_notice("a", STILL_WORKING);
        assert!(rt.has_utterances());
        // `whisrs cancel` while a reply plays drops the rest.
        rt.say("b", "Another reply.".into());
        rt.drop_queued();
        assert!(!rt.has_utterances());
        // One agent: the notice is spoken as is.
        rt.agents.truncate(1);
        rt.say_notice("a", BACKGROUND_TASK);
        assert_eq!(rt.next_utterance().unwrap().text, BACKGROUND_TASK);
    }

    /// A refusal after the instruction was transcribed or typed repeats it,
    /// so it can be sent again; the background-task one says how to end
    /// that task.
    #[test]
    fn a_refused_instruction_is_in_the_notification() {
        assert_eq!(
            busy_notification("jarvis", Busy::Turn, None),
            "jarvis: Still working on the last one."
        );
        assert_eq!(
            busy_notification("jarvis", Busy::Turn, Some(" add a test ")),
            "jarvis: Still working on the last one. Not sent: \"add a test\"."
        );
        assert_eq!(
            busy_notification("jarvis", Busy::Background, Some("add a test")),
            "jarvis: It's still running a background task. Not sent: \"add a test\". \
             `whisrs agent jarvis --stop` ends that task, then send it again."
        );
        let long = "word ".repeat(400);
        let body = busy_notification("jarvis", Busy::Turn, Some(&long));
        assert!(body.len() < REFUSED_INSTRUCTION_MAX + 100, "{}", body.len());
        assert_eq!(Busy::Turn.notice(), STILL_WORKING);
        assert_eq!(Busy::Background.notice(), BACKGROUND_TASK);
        assert!(Busy::Background
            .message("jarvis")
            .contains("whisrs agent jarvis --stop"));
    }

    #[test]
    fn errors_are_spoken_short_and_name_the_agent_when_there_are_several() {
        let mut rt = AgentRuntime::disabled();
        let agent = test_agent;
        let message = "Agent 'jarvis': That turn was interrupted, say it again.";
        rt.agents = vec![Arc::new(agent("jarvis"))];
        assert_eq!(
            rt.error_line(&agent("jarvis"), message),
            "That turn was interrupted, say it again."
        );
        rt.agents.push(Arc::new(agent("friday")));
        assert_eq!(
            rt.error_line(&agent("jarvis"), message),
            "jarvis: That turn was interrupted, say it again."
        );
    }

    /// A press while an agent reply or notice plays during this agent's
    /// running turn stops the playback, and is refused without queuing
    /// another notice in its place. With nothing playing, the refusal is
    /// spoken as before.
    #[tokio::test]
    async fn a_press_during_a_running_turn_stops_playback_without_a_new_notice() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));

        // The "Still working" notice of an earlier press plays.
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(Utterance {
            agent: "jarvis".into(),
            text: STILL_WORKING.into(),
            notice: Some(STILL_WORKING),
            interim: None,
        }));
        let response = voice_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
            false,
        )
        .await;
        assert!(
            matches!(&response, Response::Error { message } if message == STILL_WORKING),
            "{response:?}"
        );
        assert!(stop.load(Ordering::Acquire), "the notice was not stopped");
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        // The queue clears `playing` once it sees the playback end.
        rt.set_playing(None);
        assert!(queued(rt).is_empty(), "a notice was queued");
        assert!(
            !replies_held(&context),
            "the barge-in hold outlived the press"
        );

        // A follow-up reply plays: the same.
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("A reply it produced on its own.")));
        voice_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
            false,
        )
        .await;
        assert!(stop.load(Ordering::Acquire));
        rt.set_playing(None);
        assert!(queued(rt).is_empty());

        // Nothing plays: the refusal is spoken.
        voice_toggle(Arc::clone(&ds), Arc::clone(&context), agent, false).await;
        assert_eq!(queued(rt), [STILL_WORKING]);
    }

    /// A selection read-aloud is not an agent's playback: a press while the
    /// turn runs leaves it playing and is refused as before.
    #[tokio::test]
    async fn a_press_during_a_running_turn_leaves_a_selection_read_aloud() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let stop = start_speaking(&ds, SpeakOrigin::Selection).await;
        voice_toggle(Arc::clone(&ds), Arc::clone(&context), agent, false).await;
        assert!(!stop.load(Ordering::Acquire));
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert_eq!(queued(&context.agents), [STILL_WORKING]);
    }

    /// `--new` is refused while any turn of the agent runs and says how to
    /// go on. One that waits for you is named by its needs-you notice, as
    /// whisrs said it when the turn started waiting (a plain press answers
    /// it): spoken once, however often the press comes.
    #[tokio::test]
    async fn a_fresh_session_press_is_refused_while_a_turn_runs() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let press = || {
            voice_toggle(
                Arc::clone(&ds),
                Arc::clone(&context),
                Arc::clone(&agent),
                true,
            )
        };

        // Working.
        let response = press().await;
        assert!(
            matches!(&response, Response::Error { message }
                if message == "Still working on the last one. Stop it first: whisrs agent jarvis --stop"),
            "{response:?}"
        );
        assert_eq!(queued(&context.agents), [STILL_WORKING]);

        // Waiting for you: what whisrs said when it started waiting.
        apply_update(
            &context,
            &agent,
            1,
            TurnUpdate::NeedsYou("Waiting for your approval to use Bash.".into()),
        )
        .await;
        assert_eq!(
            queued(&context.agents),
            ["Waiting for your approval to use Bash."]
        );
        let waiting = "Waiting for your approval to use Bash. A plain press answers it; to start \
                       fresh, stop it first: whisrs agent jarvis --stop";
        for _ in 0..2 {
            let response = press().await;
            assert!(
                matches!(&response, Response::Error { message } if message == waiting),
                "{response:?}"
            );
        }
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert_eq!(
            queued(&context.agents),
            ["Waiting for your approval to use Bash."]
        );
        // `--new --text` and `--reset` say the same.
        for (action, text) in [
            (AgentAction::Send, Some("fresh".to_string())),
            (AgentAction::Reset, None),
        ] {
            let response = handle_agent(
                Arc::clone(&ds),
                Arc::clone(&context),
                action,
                Some("jarvis".into()),
                text,
                action == AgentAction::Send,
            )
            .await;
            assert!(
                matches!(&response, Response::Error { message } if message == waiting),
                "{action:?}: {response:?}"
            );
        }
        // A plain press is not refused for it (it answers the turn): the
        // refusal check lets it through to the recording.
        assert_eq!(context.agents.press_refusal("jarvis", false).await, None);
    }

    /// A refused press stops only this agent's own playback: another
    /// agent's reply plays on (it would be lost), and the refusal is spoken
    /// after it, once. This agent's reply is stopped and nothing is queued
    /// in its place.
    #[tokio::test]
    async fn a_refused_press_leaves_another_agents_reply_playing() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis", "friday"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let other = |text: &str| Utterance {
            agent: "friday".into(),
            text: text.into(),
            notice: None,
            interim: None,
        };
        for new_session in [false, true] {
            let ds = Arc::new(Mutex::new(DaemonState::new()));
            let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
            rt.set_playing(Some(other("friday: The build is green.")));
            for _ in 0..2 {
                let response = voice_toggle(
                    Arc::clone(&ds),
                    Arc::clone(&context),
                    Arc::clone(&agent),
                    new_session,
                )
                .await;
                assert!(matches!(response, Response::Error { .. }), "{response:?}");
            }
            assert!(!stop.load(Ordering::Acquire), "friday's reply was stopped");
            assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
            assert!(rt.is_playing_for("friday"));
            assert_eq!(queued(rt), [format!("jarvis: {STILL_WORKING}")]);
            assert!(!replies_held(&context));
            rt.set_playing(None);
        }

        // This agent's own reply: stopped, refused, nothing queued.
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("jarvis: A reply it produced on its own.")));
        let response = voice_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
            false,
        )
        .await;
        assert!(
            matches!(&response, Response::Error { message } if message == STILL_WORKING),
            "{response:?}"
        );
        assert!(
            stop.load(Ordering::Acquire),
            "its own reply was not stopped"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        rt.set_playing(None);
        assert!(queued(rt).is_empty(), "a notice was queued");
    }

    /// A transcription backend that always hears the same words.
    struct FixedBackend;

    #[async_trait::async_trait]
    impl whisrs::transcription::TranscriptionBackend for FixedBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            Ok("run the tests".into())
        }

        fn sends_prompt(&self, _config: &whisrs::transcription::TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> whisrs::transcription::StreamText {
            whisrs::transcription::StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<whisrs::transcription::Transcript> {
            unreachable!("voice turns do not request segments")
        }

        fn max_segment_chunk_secs(
            &self,
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> Option<u32> {
            None
        }
    }

    /// A harness that records the session each turn starts in and replies
    /// at once.
    #[derive(Default)]
    struct SessionLoggingDriver {
        sessions: std::sync::Mutex<Vec<SessionRef>>,
    }

    #[async_trait::async_trait]
    impl Driver for SessionLoggingDriver {
        async fn run_turn(
            &self,
            _agent: &Agent,
            session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            self.sessions.lock().unwrap().push(session);
            TurnOutcome::Reply("<spoken>Done.</spoken>".into())
        }

        async fn rewatch(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn is watched again")
        }

        async fn stop(&self, _agent: &Agent, _session: &SessionRef) -> Result<(), String> {
            Ok(())
        }

        fn forget(&self, _session: &SessionRef) {}

        fn hook_matches(&self, _payload: &serde_json::Value, _session: &SessionRef) -> bool {
            false
        }

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
            sessions.iter().map(|_| Ok(AgentPhase::Idle)).collect()
        }

        async fn idle_event(
            &self,
            _payload: &serde_json::Value,
            _session: &SessionRef,
        ) -> IdleEvent {
            IdleEvent::Ignored
        }
    }

    /// A voice turn whose press asked for a fresh session forgets the stored
    /// one when its transcript is ready and starts the turn in a new
    /// session; a plain one continues the stored session. A turn that
    /// started while the fresh one recorded keeps the session, and the
    /// instruction is refused.
    #[tokio::test]
    async fn a_fresh_session_voice_turn_starts_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(SessionLoggingDriver::default());
        let mut context = test_context(&["jarvis"], dir.path());
        {
            let ctx = Arc::get_mut(&mut context).unwrap();
            ctx.transcription_backend = Arc::new(FixedBackend);
            ctx.agents
                .drivers
                .insert(Harness::Claude, Arc::clone(&driver) as Arc<dyn Driver>);
        }
        let agent = Arc::clone(&context.agents.agents[0]);
        let stored = SessionRef {
            session_id: Some("sid-1".into()),
            bg_id: Some("bg1".into()),
            transcript_path: None,
            pending: false,
        };
        let store_session = |context: Arc<DaemonContext>| {
            let stored = stored.clone();
            async move {
                let mut inner = context.agents.inner.lock().await;
                let mut record = AgentRecord::new("claude", Path::new("/proj"));
                record.set_session(&stored);
                inner.store.agents.insert("jarvis".into(), record);
            }
        };
        let voice_turn = |new_session: bool| {
            let context = Arc::clone(&context);
            let agent = Arc::clone(&agent);
            async move {
                let ds = Arc::new(Mutex::new(DaemonState::new()));
                {
                    let mut g = ds.lock().await;
                    g.state_machine.transition(Action::Toggle).unwrap();
                    g.agent_recording = Some(AgentRecordingContext {
                        name: "jarvis".into(),
                        new_session,
                    });
                }
                let (tx, rx) = mpsc::unbounded_channel();
                let loud: Vec<i16> = (0..16_000)
                    .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
                    .collect();
                tx.send(loud).unwrap();
                drop(tx);
                voice_turn_background(Some(rx), ds, context, agent).await;
            }
        };
        let turns = |n: usize| {
            let driver = Arc::clone(&driver);
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while driver.sessions.lock().unwrap().len() < n {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the turn did not start");
                driver.sessions.lock().unwrap()[n - 1].clone()
            }
        };

        // Wait until the turn ended and its reply was queued.
        let replied = || async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !context.agents.has_utterances() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the turn did not reply");
            assert_eq!(queued(&context.agents), ["Done."]);
            assert!(!context.agents.has_turn("jarvis").await);
        };

        store_session(Arc::clone(&context)).await;
        voice_turn(false).await;
        assert_eq!(turns(1).await, stored, "a plain voice turn continues");
        replied().await;

        store_session(Arc::clone(&context)).await;
        voice_turn(true).await;
        assert_eq!(turns(2).await, SessionRef::default(), "not a fresh session");
        replied().await;

        // A turn runs by the time the fresh one is transcribed.
        store_session(Arc::clone(&context)).await;
        install_turn(&context, "jarvis", 99, "typed meanwhile").await;
        voice_turn(true).await;
        assert_eq!(queued(&context.agents), [STILL_WORKING]);
        let inner = context.agents.inner.lock().await;
        assert_eq!(inner.store.agents["jarvis"].session(), stored);
    }

    // -----------------------------------------------------------------------
    // Interims and the agent overlay's board
    // -----------------------------------------------------------------------

    use whisrs::agent::display::{AgentOverlayEntry, DONE_FADE, DONE_HOLD};
    use whisrs::agent::progress::{Feedback, ACK_TTL, PROGRESS_MIN_GAP, PROGRESS_TTL};

    /// [`test_context`] whose agents have `feedback`.
    fn feedback_context(names: &[&str], dir: &Path, feedback: Feedback) -> Arc<DaemonContext> {
        let mut context = test_context(names, dir);
        Arc::get_mut(&mut context).unwrap().agents.agents = names
            .iter()
            .map(|n| {
                Arc::new(Agent {
                    feedback,
                    ..test_agent(n)
                })
            })
            .collect();
        context
    }

    /// Give a test runtime the board channel `AgentRuntime::new` makes.
    fn with_board(context: &mut Arc<DaemonContext>) {
        let rt = &mut Arc::get_mut(context).unwrap().agents;
        let (tx, rx) = mpsc::unbounded_channel();
        rt.board_tx = Some(tx);
        rt.board_rx = std::sync::Mutex::new(Some(rx));
    }

    /// Every queued utterance, in order: its text and, for an interim, its
    /// kind.
    fn queued_kinds(rt: &AgentRuntime) -> Vec<(String, Option<InterimKind>)> {
        std::iter::from_fn(|| rt.next_utterance())
            .map(|u| (u.text, u.interim.map(|i| i.kind)))
            .collect()
    }

    fn line(text: &str, kind: Option<InterimKind>) -> (String, Option<InterimKind>) {
        (text.to_string(), kind)
    }

    async fn interim(context: &DaemonContext, agent: &Agent, generation: u64, raw: &str) {
        apply_update(
            context,
            agent,
            generation,
            TurnUpdate::Interim(raw.to_string()),
        )
        .await;
    }

    /// The gate speaks what the agent's level allows, whatever the model
    /// writes: summary nothing; brief the first line, as the
    /// acknowledgement; conversational that, then updates at least
    /// PROGRESS_MIN_GAP apart and never one said already. A message with
    /// nothing speakable says nothing. Interims are never notified (the
    /// test context notifies nothing, and nothing here asks it to).
    #[tokio::test(start_paused = true)]
    async fn interims_are_spoken_as_the_feedback_level_allows() {
        use InterimKind::{Ack, Progress};
        let dir = tempfile::tempdir().unwrap();
        for (feedback, first, later) in [
            (Feedback::Summary, vec![], vec![]),
            (Feedback::Brief, vec![line("On it.", Some(Ack))], vec![]),
            (
                Feedback::Conversational,
                vec![line("On it.", Some(Ack))],
                vec![line("Found the cause.", Some(Progress))],
            ),
        ] {
            let context = feedback_context(&["jarvis"], dir.path(), feedback);
            let rt = &context.agents;
            let agent = Arc::clone(&rt.agents[0]);
            install_turn(&context, "jarvis", 1, "fix the build").await;
            interim(&context, &agent, 1, "<spoken>On it.</spoken> Looking now.").await;
            // Too soon after the first, and nothing to say.
            interim(&context, &agent, 1, "<spoken>Still on it.</spoken>").await;
            interim(&context, &agent, 1, "Notes, no tags.").await;
            interim(&context, &agent, 1, "<spoken>https://x.io</spoken>").await;
            assert_eq!(queued_kinds(rt), first, "{feedback}");

            tokio::time::advance(PROGRESS_MIN_GAP + Duration::from_secs(1)).await;
            // Said already, then a new one.
            interim(&context, &agent, 1, "<spoken>On it.</spoken>").await;
            interim(&context, &agent, 1, "<spoken>Found the cause.</spoken>").await;
            interim(&context, &agent, 1, "<spoken>And another.</spoken>").await;
            assert_eq!(queued_kinds(rt), later, "{feedback}");

            // A turn that is not running (replaced, or ended) says nothing.
            interim(&context, &agent, 2, "<spoken>On it.</spoken>").await;
            assert!(queued_kinds(rt).is_empty(), "{feedback}");
        }
    }

    /// The reply drops its agent's acknowledgement or update still queued
    /// (another agent's stays, and so do notices), and is never dropped or
    /// deduplicated away for one, even when they say the same. A failed
    /// turn's error drops them too.
    #[tokio::test]
    async fn a_reply_drops_its_agents_queued_interims() {
        use InterimKind::Ack;
        let dir = tempfile::tempdir().unwrap();
        let context = feedback_context(&["jarvis", "friday"], dir.path(), Feedback::Brief);
        let rt = &context.agents;
        let jarvis = Arc::clone(&rt.agents[0]);
        let friday = Arc::clone(&rt.agents[1]);
        install_turn(&context, "jarvis", 1, "check it").await;
        install_turn(&context, "friday", 2, "build it").await;
        interim(&context, &jarvis, 1, "<spoken>All set.</spoken>").await;
        interim(&context, &friday, 2, "<spoken>Building.</spoken>").await;
        rt.say_notice("jarvis", BACKGROUND_TASK);
        finish_turn(
            &context,
            &jarvis,
            1,
            TurnOutcome::Reply("Checked. <spoken>All set.</spoken>".into()),
        )
        .await;
        assert_eq!(
            queued_kinds(rt),
            [
                line("Building.", Some(Ack)),
                line(&format!("jarvis: {BACKGROUND_TASK}"), None),
                line("All set.", None),
            ]
        );

        // An error replaces them as well.
        install_turn(&context, "friday", 3, "build it again").await;
        interim(&context, &friday, 3, "<spoken>Building.</spoken>").await;
        finish_turn(
            &context,
            &friday,
            3,
            TurnOutcome::Failed("Agent 'friday': the session ended before replying.".into()),
        )
        .await;
        assert_eq!(
            queued_kinds(rt),
            [line("friday: The session ended before replying.", None)]
        );
    }

    /// An interim waits in the queue only while it is worth saying: an
    /// acknowledgement ACK_TTL, an update PROGRESS_TTL, measured from when
    /// it was admitted. Replies never expire.
    #[tokio::test(start_paused = true)]
    async fn interims_expire_while_they_wait() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        let t0 = now();
        rt.say_interim("jarvis", "On it.".into(), InterimKind::Ack, t0);
        rt.say("jarvis", "A reply.".into());
        rt.say_interim("jarvis", "Halfway.".into(), InterimKind::Progress, t0);
        // Fresh until its time is up.
        tokio::time::advance(ACK_TTL - Duration::from_millis(10)).await;
        let ack = rt.next_utterance().unwrap();
        assert_eq!(ack.text, "On it.");
        rt.requeue_front(ack);
        tokio::time::advance(Duration::from_millis(10)).await;
        // The acknowledgement expired: dropped on the way to the reply. The
        // update lives longer.
        assert_eq!(rt.next_utterance().unwrap().text, "A reply.");
        let update = rt.next_utterance().unwrap();
        assert_eq!(update.text, "Halfway.");
        rt.requeue_front(update);
        tokio::time::advance(PROGRESS_TTL - ACK_TTL).await;
        assert!(
            rt.next_utterance().is_none(),
            "the update outlived its time"
        );
        // A reply waits however long.
        rt.say("jarvis", "A late reply.".into());
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(rt.next_utterance().unwrap().text, "A late reply.");
    }

    /// `whisrs cancel` drops the queued interims; replies stay unless an
    /// agent reply is playing (then everything queued goes, as before).
    #[tokio::test]
    async fn cancel_drops_queued_interims() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.say_interim("jarvis", "On it.".into(), InterimKind::Ack, now());
        rt.say("jarvis", "A reply.".into());
        crate::dictation::handle_cancel(Arc::clone(&ds), Arc::clone(&context)).await;
        assert_eq!(queued(rt), ["A reply."]);

        start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.say_interim("jarvis", "On it.".into(), InterimKind::Ack, now());
        rt.say("jarvis", "A reply.".into());
        crate::dictation::handle_cancel(Arc::clone(&ds), Arc::clone(&context)).await;
        assert!(queued(rt).is_empty());
    }

    /// A recording that stops an interim playing does not put it back: it
    /// is not worth saying after the recording. A reply it stops is.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn an_interim_a_recording_stopped_is_not_said_again() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.daemon_state.set(Arc::clone(&ds)).ok().unwrap();
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(Utterance {
            interim: Some(Interim {
                kind: InterimKind::Ack,
                expires: now() + ACK_TTL,
            }),
            ..utterance("On it.")
        }));
        rt.say("jarvis", "A reply.".into());
        let hold = hold_for_recording(&context);
        assert!(stop.load(Ordering::Acquire), "playback was not stopped");
        drop(hold);
        assert_eq!(queued(rt), ["A reply."]);
        rt.set_playing(None);
    }

    /// Holds apply to interims as to replies: one waits while a hold
    /// lives, and one that expired meanwhile is dropped, never spoken. One
    /// still worth saying is spoken once the hold is gone, as its agent's
    /// voice (the ui view stays Idle).
    #[tokio::test]
    async fn holds_apply_to_interims_and_a_held_one_expires() {
        // A TTS server that takes the request and never answers: a spoken
        // utterance stays Synthesizing until it is stopped.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut context = test_context(&["jarvis"], dir.path());
        Arc::get_mut(&mut context).unwrap().config = toml::from_str(&format!(
            "[general]\naudio_feedback = false\n[tts]\nenabled = true\nbackend = \
             \"tts-sidecar\"\nurl = \"http://{}/v1/audio/speech\"\n",
            server.local_addr().unwrap()
        ))
        .unwrap();
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let settle = || async {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let interim_for = |text: &str, ttl: Duration| Utterance {
            interim: Some(Interim {
                kind: InterimKind::Ack,
                expires: now() + ttl,
            }),
            ..utterance(text)
        };
        let hold = rt.hold_replies();
        tokio::spawn(utterance_loop(
            Arc::clone(&context),
            Arc::clone(&ds),
            context.state_tx.subscribe_raw(),
        ));
        rt.enqueue(interim_for("Soon stale.", Duration::from_millis(100)));
        settle().await;
        assert!(rt.has_utterances(), "spoken while a hold lived");
        drop(hold);
        settle().await;
        assert!(!rt.has_utterances());
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert_eq!(rt.playing_agent(), None, "an expired interim was spoken");

        let hold = rt.hold_replies();
        rt.enqueue(interim_for("On it.", ACK_TTL));
        settle().await;
        assert!(rt.has_utterances());
        drop(hold);
        settle().await;
        assert!(!rt.has_utterances());
        assert_eq!(ds.lock().await.state_machine.state(), State::Synthesizing);
        assert_eq!(rt.playing_agent().as_deref(), Some("jarvis"));
        assert_eq!(*context.state_tx.subscribe_ui().borrow(), State::Idle);
        assert_eq!(
            context.state_tx.subscribe_voice().borrow().clone(),
            Some(AgentVoice {
                name: "jarvis".into(),
                state: State::Synthesizing
            })
        );
        stop_read_aloud(&mut *ds.lock().await, &context);
        drop(server);
    }

    /// A needs-you notice is announced under the key its refusal speaks it
    /// again under: a fresh-session press while the announcement still
    /// waits (another agent's reply holds the queue) does not queue it
    /// twice.
    #[tokio::test]
    async fn a_needs_you_notice_is_queued_once() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis", "friday"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        rt.set_playing(Some(Utterance {
            agent: "friday".into(),
            ..utterance("friday: The build is green.")
        }));
        apply_update(
            &context,
            &agent,
            1,
            TurnUpdate::NeedsYou("Waiting for your approval to use Bash.".into()),
        )
        .await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let response = voice_toggle(ds, Arc::clone(&context), agent, true).await;
        assert!(matches!(response, Response::Error { .. }), "{response:?}");
        assert_eq!(
            queued(rt),
            ["jarvis: Waiting for your approval to use Bash."]
        );
        rt.set_playing(None);
    }

    /// The agent's progress toasts follow `[general] notify` unless an agent
    /// overlay shows (the pill no longer shows agent mode).
    #[test]
    fn agent_toasts_give_way_only_to_an_agent_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = test_context(&["jarvis"], dir.path());
        let ctx = Arc::get_mut(&mut context).unwrap();
        ctx.overlay_enabled = true;
        assert!(!ctx.notify_agent_state(), "notify is off");
        ctx.notify = true;
        assert!(!ctx.notify_state(), "the pill stands in for dictation's");
        assert!(ctx.notify_agent_state());
        ctx.agent_overlay_shown.store(true, Ordering::Relaxed);
        assert!(!ctx.notify_agent_state());
    }

    /// The rows of `context`'s board once `done` accepts them.
    async fn board_until(
        context: &DaemonContext,
        what: &str,
        done: impl Fn(&[AgentOverlayEntry]) -> bool,
    ) -> Vec<AgentOverlayEntry> {
        let mut rx = context.agent_board.subscribe();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let rows = rx.borrow_and_update().clone();
                if done(&rows) {
                    return rows;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the board never showed {what}"))
    }

    /// The rows of `context`'s board once they are `want` (name and
    /// visual, in order).
    async fn board_shows(
        context: &DaemonContext,
        want: &[(&str, AgentVisual)],
    ) -> Vec<AgentOverlayEntry> {
        board_until(context, &format!("{want:?}"), |rows| {
            rows.iter()
                .map(|e| (e.name.as_str(), e.visual))
                .eq(want.iter().copied())
        })
        .await
    }

    /// Publish `ds` as the dictation state.
    fn publish(context: &DaemonContext, ds: &DaemonState) {
        context.state_tx.publish(ds);
    }

    /// The board task, end to end: a turn's row works, ends Done and holds
    /// while its reply waits (however long); the voice shows while it
    /// plays; once it played the row holds DONE_HOLD, fades over DONE_FADE
    /// and is gone at that time on its own.
    #[tokio::test(start_paused = true)]
    async fn done_holds_while_the_reply_is_pending_and_fades_after() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(SessionLoggingDriver::default());
        let mut context = test_context(&["jarvis"], dir.path());
        Arc::get_mut(&mut context)
            .unwrap()
            .agents
            .drivers
            .insert(Harness::Claude, Arc::clone(&driver) as Arc<dyn Driver>);
        with_board(&mut context);
        let rt = &context.agents;
        tokio::spawn(board_loop(
            Arc::clone(&context),
            rt.take_board_events().unwrap(),
            context.state_tx.subscribe_voice(),
        ));
        let agent = Arc::clone(&rt.agents[0]);
        start_turn(&context, &agent, "run the tests".into())
            .await
            .unwrap();
        let rows = board_shows(&context, &[("jarvis", AgentVisual::Done)]).await;
        assert_eq!(rows[0].fading_from, None, "faded while its reply waits");
        assert_eq!(rows[0].activity.as_deref(), Some("Finished in 0:00"));
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        assert_eq!(context.agent_board.borrow()[0].fading_from, None);

        // The reply plays.
        let reply = rt.next_utterance().unwrap();
        assert_eq!(reply.text, "Done.");
        rt.set_playing(Some(reply));
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::SpeakStart).unwrap();
        ds.tts_stop = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
        ds.tts_origin = Some(SpeakOrigin::Agent);
        ds.tts_agent = rt.playing_agent();
        publish(&context, &ds);
        board_shows(&context, &[("jarvis", AgentVisual::Speaking)]).await;
        tokio::time::advance(Duration::from_secs(5)).await;

        // It played.
        ds.state_machine.transition(Action::SpeakDone).unwrap();
        ds.tts_stop = None;
        publish(&context, &ds);
        let played = now();
        rt.set_playing(None);
        let rows = board_shows(&context, &[("jarvis", AgentVisual::Done)]).await;
        let from = rows[0].fading_from.expect("a played reply's row fades");
        assert!(from >= played + DONE_HOLD, "fades before the hold ended");
        assert_eq!(rows[0].alpha(from), 1.0);
        // Gone at the fade's end, with no other event.
        board_shows(&context, &[]).await;
        assert!(now() >= from + DONE_FADE, "gone before the fade ended");
    }

    /// The board follows the turn: working with its activity line, waiting
    /// for you, working again, an error; `--stop` and `--reset` clear it.
    #[tokio::test]
    async fn the_board_follows_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = test_context(&["jarvis"], dir.path());
        with_board(&mut context);
        let rt = &context.agents;
        tokio::spawn(board_loop(
            Arc::clone(&context),
            rt.take_board_events().unwrap(),
            context.state_tx.subscribe_voice(),
        ));
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "check it").await;
        rt.board(BoardEvent::TurnStarted {
            agent: "jarvis".into(),
            since: now(),
        });
        apply_update(
            &context,
            &agent,
            1,
            TurnUpdate::Activity("Running a command".into()),
        )
        .await;
        board_until(&context, "the activity", |rows| {
            rows.len() == 1
                && rows[0].visual == AgentVisual::Working
                && rows[0].activity.as_deref() == Some("Running a command")
        })
        .await;
        apply_update(
            &context,
            &agent,
            1,
            TurnUpdate::NeedsYou("Waiting for your approval to use Bash.".into()),
        )
        .await;
        board_shows(&context, &[("jarvis", AgentVisual::NeedsYou)]).await;
        apply_update(&context, &agent, 1, TurnUpdate::Working).await;
        board_shows(&context, &[("jarvis", AgentVisual::Working)]).await;
        finish_turn(
            &context,
            &agent,
            1,
            TurnOutcome::Failed("Agent 'jarvis': the session failed.".into()),
        )
        .await;
        board_shows(&context, &[("jarvis", AgentVisual::Error)]).await;
        reset(&context, &agent).await.unwrap();
        board_shows(&context, &[]).await;

        // `--stop`.
        install_turn(&context, "jarvis", 2, "again").await;
        rt.board(BoardEvent::TurnStarted {
            agent: "jarvis".into(),
            since: now(),
        });
        board_shows(&context, &[("jarvis", AgentVisual::Working)]).await;
        let _ = stop_turn(&context, &agent).await;
        board_shows(&context, &[]).await;
    }

    /// A harness whose session, with no whisrs turn running, is listed in
    /// `phase` (the test sets it), and whose hooks mean what Claude's do: a
    /// Stop with a message is a reply, one without ends the work, a
    /// permission prompt waits. Every turn finds the session working on its
    /// own.
    struct IdleListingDriver {
        phase: std::sync::Mutex<AgentPhase>,
        /// How often [`Driver::session_phase`] or
        /// [`Driver::session_phases`] was asked.
        asked: AtomicUsize,
        /// When each [`Driver::session_phases`] listing ran, with how many
        /// sessions it answered for.
        listings: std::sync::Mutex<Vec<(Instant, usize)>>,
    }

    impl IdleListingDriver {
        fn new(phase: AgentPhase) -> Self {
            Self {
                phase: std::sync::Mutex::new(phase),
                asked: AtomicUsize::new(0),
                listings: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn set(&self, phase: AgentPhase) {
            *self.phase.lock().unwrap() = phase;
        }
    }

    #[async_trait::async_trait]
    impl Driver for IdleListingDriver {
        async fn run_turn(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            TurnOutcome::Busy
        }

        async fn rewatch(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn is watched again")
        }

        async fn stop(&self, _agent: &Agent, _session: &SessionRef) -> Result<(), String> {
            Ok(())
        }

        fn forget(&self, _session: &SessionRef) {}

        fn hook_matches(&self, payload: &serde_json::Value, _session: &SessionRef) -> bool {
            payload["session_id"] == "sid-1"
        }

        async fn session_phase(
            &self,
            _agent: &Agent,
            _session: &SessionRef,
        ) -> Result<AgentPhase, String> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(*self.phase.lock().unwrap())
        }

        async fn session_phases(
            &self,
            sessions: &[(Arc<Agent>, SessionRef)],
        ) -> Vec<Result<AgentPhase, String>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.listings.lock().unwrap().push((now(), sessions.len()));
            let phase = *self.phase.lock().unwrap();
            sessions.iter().map(|_| Ok(phase)).collect()
        }

        async fn idle_event(
            &self,
            payload: &serde_json::Value,
            _session: &SessionRef,
        ) -> IdleEvent {
            use whisrs::agent::claude::{classify_hook, HookKind};
            match classify_hook(payload) {
                HookKind::Stop {
                    message: Some(m), ..
                } if !m.trim().is_empty() => IdleEvent::Reply(m),
                HookKind::Stop { .. } => IdleEvent::Ended,
                HookKind::PermissionPrompt => {
                    IdleEvent::NeedsYou("Waiting for your approval to use Bash.".into())
                }
                HookKind::Other => IdleEvent::Ignored,
            }
        }
    }

    /// A context for `names` (the first is `jarvis`) on `driver`, jarvis's
    /// session stored (`sid-1`, bg `bg1`), with the board and the idle poll
    /// running.
    async fn idle_context(
        names: &[&str],
        dir: &Path,
        driver: &Arc<IdleListingDriver>,
    ) -> Arc<DaemonContext> {
        let mut context = test_context(names, dir);
        Arc::get_mut(&mut context)
            .unwrap()
            .agents
            .drivers
            .insert(Harness::Claude, Arc::clone(driver) as Arc<dyn Driver>);
        with_board(&mut context);
        {
            let mut inner = context.agents.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-1".into());
            record.bg_id = Some("bg1".into());
            inner.store.agents.insert("jarvis".into(), record);
        }
        tokio::spawn(board_loop(
            Arc::clone(&context),
            context.agents.take_board_events().unwrap(),
            context.state_tx.subscribe_voice(),
        ));
        tokio::spawn(idle_loop(Arc::clone(&context)));
        context
    }

    fn permission_prompt() -> serde_json::Value {
        serde_json::json!({
            "session_id": "sid-1",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
        })
    }

    /// A permission prompt with no whisrs turn running shows needs-you
    /// until the wait ends: on any Stop (with nothing in it too), or when a
    /// listing shows the session no longer waiting (stopped in a terminal,
    /// which sends no Stop). Answered elsewhere, the session's work shows
    /// as working, and its reply ends it as Done.
    #[tokio::test(start_paused = true)]
    async fn an_idle_wait_ends_on_any_stop_or_a_listing() {
        use AgentVisual::{Done, NeedsYou, Working};
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(IdleListingDriver::new(AgentPhase::NeedsYou));
        let context = idle_context(&["jarvis"], dir.path(), &driver).await;
        let rt = &context.agents;

        handle_agent_event(&context, permission_prompt()).await;
        board_shows(&context, &[("jarvis", NeedsYou)]).await;
        handle_agent_event(&context, stop_hook("")).await;
        board_shows(&context, &[]).await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);

        // No Stop: the idle poll sees it gone.
        handle_agent_event(&context, permission_prompt()).await;
        board_shows(&context, &[("jarvis", NeedsYou)]).await;
        driver.set(AgentPhase::Idle);
        board_shows(&context, &[]).await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);

        // Answered in a terminal: it works on, then replies.
        driver.set(AgentPhase::NeedsYou);
        handle_agent_event(&context, permission_prompt()).await;
        board_shows(&context, &[("jarvis", NeedsYou)]).await;
        driver.set(AgentPhase::Working);
        board_shows(&context, &[("jarvis", Working)]).await;
        handle_agent_event(&context, stop_hook("Ran it. <spoken>Tests pass.</spoken>")).await;
        board_shows(&context, &[("jarvis", Done)]).await;
        assert_eq!(
            queued(rt),
            ["Waiting for your approval to use Bash.", "Tests pass."]
        );
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);

        // `whisrs agent` lists it idle: that ends a wait too, at once.
        driver.set(AgentPhase::NeedsYou);
        handle_agent_event(&context, permission_prompt()).await;
        board_shows(&context, &[("jarvis", NeedsYou)]).await;
        driver.set(AgentPhase::Idle);
        let asked = driver.asked.load(Ordering::SeqCst);
        let rows = rt.list().await;
        assert_eq!(rows[0].phase, AgentPhase::Idle);
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        // The listing's own question, and no poll's.
        assert_eq!(
            driver.asked.load(Ordering::SeqCst),
            asked + 1,
            "the poll ran"
        );
        board_shows(&context, &[]).await;
    }

    /// A wait with no whisrs turn running that ends on a Stop with a reply
    /// ends Done while the reply waits, as the same work shown working
    /// first does.
    #[tokio::test(start_paused = true)]
    async fn an_idle_wait_that_replies_ends_done() {
        use AgentVisual::{Done, NeedsYou};
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(IdleListingDriver::new(AgentPhase::NeedsYou));
        let context = idle_context(&["jarvis"], dir.path(), &driver).await;
        let rt = &context.agents;

        handle_agent_event(&context, permission_prompt()).await;
        board_shows(&context, &[("jarvis", NeedsYou)]).await;
        handle_agent_event(&context, stop_hook("Ran it. <spoken>Tests pass.</spoken>")).await;
        let rows = board_shows(&context, &[("jarvis", Done)]).await;
        assert_eq!(rows[0].fading_from, None, "faded while its reply waits");
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        assert_eq!(
            queued(rt),
            ["Waiting for your approval to use Bash.", "Tests pass."]
        );
        // Spoken (the queue tells the board when a playback ends): nothing
        // is pending, so it fades and is gone.
        rt.pending_changed();
        board_shows(&context, &[]).await;
    }

    /// The driver's listings once there are `n` of them.
    async fn listings(driver: &IdleListingDriver, n: usize) -> Vec<(Instant, usize)> {
        loop {
            let listings = driver.listings.lock().unwrap().clone();
            if listings.len() >= n {
                return listings;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// The time from `from` to the first listing, then between each two.
    fn gaps(from: Instant, listings: &[(Instant, usize)]) -> Vec<Duration> {
        let mut last = from;
        listings
            .iter()
            .map(|&(at, _)| {
                let gap = at.duration_since(last);
                last = at;
                gap
            })
            .collect()
    }

    fn secs(list: &[u64]) -> Vec<Duration> {
        list.iter().copied().map(Duration::from_secs).collect()
    }

    /// While a session works on its own all along (a dev server), its row's
    /// listing backs off: ten seconds, then twice as long each time, up to
    /// two minutes. A turn starting, or another row starting to show,
    /// starts the wait over; one listing answers for every row shown.
    #[tokio::test(start_paused = true)]
    async fn the_idle_listing_backs_off() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(IdleListingDriver::new(AgentPhase::Working));
        let context = idle_context(&["jarvis", "friday", "kitt"], dir.path(), &driver).await;
        let rt = &context.agents;
        {
            let mut inner = rt.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-2".into());
            record.bg_id = Some("bg2".into());
            inner.store.agents.insert("friday".into(), record);
        }
        let [jarvis, friday, kitt] = [0, 1, 2].map(|i| Arc::clone(&rt.agents[i]));

        let refused = start_turn(&context, &jarvis, "add a test".into()).await;
        assert_eq!(refused, Err(Busy::Background));
        let t0 = now();
        let seen = listings(&driver, 6).await;
        assert_eq!(gaps(t0, &seen), secs(&[10, 20, 40, 80, 120, 120]));
        assert!(seen.iter().all(|&(_, n)| n == 1), "{seen:?}");

        // Kitt's turn starts (it has no session yet, so its own work shows
        // no row to list): the wait starts over.
        start_turn(&context, &kitt, "check it".into())
            .await
            .unwrap();
        let t1 = now();
        let seen = listings(&driver, 8).await;
        assert_eq!(gaps(t1, &seen[6..]), secs(&[10, 20]));

        // Friday's session works on its own too: its row starts the wait
        // over, and each listing answers for both rows.
        let refused = start_turn(&context, &friday, "ship it".into()).await;
        assert_eq!(refused, Err(Busy::Background));
        let t2 = now();
        let seen = listings(&driver, 10).await;
        assert_eq!(gaps(t2, &seen[8..]), secs(&[10, 20]));
        assert!(seen[8..].iter().all(|&(_, n)| n == 2), "{seen:?}");
    }

    /// A turn refused because the session works on its own shows that work
    /// as working, as `whisrs agent` lists it (not as a failed turn), until
    /// the work ends: refused at the start, or by the driver.
    #[tokio::test(start_paused = true)]
    async fn a_turn_refused_for_the_sessions_own_work_shows_it_working() {
        use AgentVisual::Working;
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(IdleListingDriver::new(AgentPhase::Working));
        let context = idle_context(&["jarvis"], dir.path(), &driver).await;
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);

        let refused = start_turn(&context, &agent, "add a test".into()).await;
        assert_eq!(refused, Err(Busy::Background));
        let rows = board_shows(&context, &[("jarvis", Working)]).await;
        assert_eq!(rows[0].activity, None);
        handle_agent_event(&context, stop_hook("")).await;
        board_shows(&context, &[]).await;

        // The start's check saw it idle; the driver finds it working.
        driver.set(AgentPhase::Idle);
        start_turn(&context, &agent, "add a test".into())
            .await
            .unwrap();
        while rt.has_turn("jarvis").await {
            tokio::task::yield_now().await;
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let shown: Vec<_> = context
            .agent_board
            .borrow()
            .iter()
            .map(|e| (e.name.clone(), e.visual))
            .collect();
        assert_eq!(shown, [("jarvis".to_string(), Working)]);
        assert_eq!(queued(rt), [BACKGROUND_TASK]);
        // Listed idle by the poll: gone.
        board_shows(&context, &[]).await;
        assert!(driver.asked.load(Ordering::SeqCst) >= 3);
    }

    /// A newer permission prompt replaces the text of its notice still
    /// queued; one playing plays on, and a newer text is queued once after
    /// it.
    #[test]
    fn a_newer_permission_prompt_replaces_the_queued_notice() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = vec![Arc::new(test_agent("jarvis"))];
        let bash = "Waiting for your approval to use Bash.";
        let edit = "Waiting for your approval to use Edit.";
        let write = "Waiting for your approval to use Write.";
        rt.say_keyed("jarvis", WAITING_KEY, bash.into());
        rt.say_keyed("jarvis", WAITING_KEY, edit.into());
        assert_eq!(queued(&rt), [edit]);

        rt.set_playing(Some(Utterance {
            agent: "jarvis".into(),
            text: bash.into(),
            notice: Some(WAITING_KEY),
            interim: None,
        }));
        rt.say_keyed("jarvis", WAITING_KEY, bash.into());
        assert!(!rt.has_utterances(), "the one playing says it");
        rt.say_keyed("jarvis", WAITING_KEY, edit.into());
        rt.say_keyed("jarvis", WAITING_KEY, write.into());
        assert_eq!(queued(&rt), [write]);
        rt.set_playing(None);
    }

    /// A notice put back at the front (a recording start stopped it, or the
    /// queue could not start it) gives way to a newer one of its key queued
    /// while it played: only the newest is said, first. With none, it comes
    /// back as it was, and other keys and agents are left alone.
    #[test]
    fn a_notice_put_back_gives_way_to_a_newer_one() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = vec![Arc::new(test_agent("jarvis"))];
        let bash = "Waiting for your approval to use Bash.";
        let edit = "Waiting for your approval to use Edit.";
        rt.say_keyed("jarvis", WAITING_KEY, bash.into());
        let playing = rt.next_utterance().unwrap();
        rt.set_playing(Some(playing.clone()));
        rt.say_keyed("jarvis", WAITING_KEY, edit.into());
        rt.say("friday", "A reply.".into());
        rt.requeue_front(playing);
        rt.set_playing(None);
        assert_eq!(queued(&rt), [edit, "A reply."]);

        rt.say_keyed("jarvis", WAITING_KEY, bash.into());
        let playing = rt.next_utterance().unwrap();
        rt.set_playing(Some(playing.clone()));
        rt.say_notice("jarvis", STILL_WORKING);
        rt.say_keyed("friday", WAITING_KEY, edit.into());
        rt.requeue_front(playing);
        rt.set_playing(None);
        assert_eq!(queued(&rt), [bash, STILL_WORKING, edit]);
    }

    /// `--stop` and `--reset` drop the agent's acknowledgements and updates
    /// still queued; its replies and other agents' interims stay.
    #[tokio::test]
    async fn stop_and_reset_drop_the_agents_interims() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis", "friday"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "check it").await;
        rt.say_interim("jarvis", "On it.".into(), InterimKind::Ack, now());
        rt.say_interim("friday", "Let me check.".into(), InterimKind::Ack, now());
        rt.say("jarvis", "jarvis: An earlier reply.".into());
        stop_turn(&context, &agent).await.unwrap();
        assert_eq!(queued(rt), ["Let me check.", "jarvis: An earlier reply."]);

        rt.say_interim("jarvis", "On it.".into(), InterimKind::Ack, now());
        rt.say("jarvis", "jarvis: Another reply.".into());
        reset(&context, &agent).await.unwrap();
        assert_eq!(queued(rt), ["jarvis: Another reply."]);
    }

    /// No agents: no board channel for a task to own, and nothing is ever
    /// sent to one.
    #[test]
    fn no_agents_starts_no_board() {
        let config: Config = toml::from_str("").unwrap();
        let rt = AgentRuntime::new(&config, Path::new("/nonexistent.sock"));
        assert!(rt.take_board_events().is_none());
        assert!(rt.board_tx.is_none());
        rt.board(BoardEvent::Clear {
            agent: "jarvis".into(),
        });
        rt.pending_changed();
        assert!(rt.last_pending.lock().unwrap().is_empty());
    }

    #[test]
    fn utterances_are_spoken_in_order_and_requeued_at_the_front() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.say("a", "one".into());
        rt.say("b", "two".into());
        let first = rt.next_utterance().unwrap();
        assert_eq!(first.text, "one");
        rt.requeue_front(first);
        assert_eq!(rt.next_utterance().unwrap().text, "one");
        assert_eq!(rt.next_utterance().unwrap().text, "two");
        assert!(rt.next_utterance().is_none());
    }
}
