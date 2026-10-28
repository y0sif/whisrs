use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::watch;

use whisrs::agent::display::AgentOverlayEntry;
use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::llm;
use whisrs::state::StateMachine;
use whisrs::transcription::TranscriptionBackend;
use whisrs::window::WindowTracker;
use whisrs::{Config, State};

use crate::agent::AgentRuntime;
#[cfg(feature = "recording")]
use crate::recording::RecordingRuntime;
use crate::speak::SpeakOrigin;

/// Context saved when command mode starts recording.
pub(crate) struct CommandModeContext {
    pub(crate) selected_text: String,
    pub(crate) llm_config: llm::LlmConfig,
}

/// Context saved when a named LLM command (`[[llm_commands]]`) starts recording.
///
/// Simpler than [`CommandModeContext`]: there's no selection or clipboard
/// involved — this is a toggle-recording flavor of plain dictation where the
/// transcribed text is run through a fixed instruction before typing.
pub(crate) struct LlmCommandContext {
    pub(crate) name: String,
    pub(crate) instruction: String,
    pub(crate) llm_config: llm::LlmConfig,
}

/// Context saved when an agent's hotkey (or `whisrs agent <name>`) starts
/// recording a voice turn. The transcript goes to the agent instead of an
/// LLM rewrite; there is no selection.
pub(crate) struct AgentRecordingContext {
    pub(crate) name: String,
    /// The press that started it asked for a fresh session (`whisrs agent
    /// <name> --new`, or its `new_session_hotkey`): the session is forgotten
    /// when the transcript is ready, and the turn starts a new one.
    pub(crate) new_session: bool,
}

/// Shared daemon state protected by a mutex.
pub(crate) struct DaemonState {
    pub(crate) state_machine: StateMachine,
    pub(crate) audio_capture: Option<AudioCaptureHandle>,
    /// The window that was focused when recording started.
    pub(crate) recording_window_id: Option<String>,
    /// Handle to the background streaming pipeline (if active).
    pub(crate) streaming_task: Option<tokio::task::JoinHandle<Result<String>>>,
    /// Cancel flag for the active streaming pipeline.
    ///
    /// Aborting `streaming_task` only drops the outer pipeline future — the
    /// spawned backend and typing tasks are detached JoinHandles that keep
    /// running, and the backend treats the dropped audio channel as a normal
    /// end-of-stream (it flushes and the typing task types the trailing
    /// phrase). Setting this flag makes the typing loop discard everything
    /// instead, so `cancel` never types text. Created per recording.
    pub(crate) streaming_cancel: Option<Arc<AtomicBool>>,
    /// When recording started (for duration tracking).
    pub(crate) recording_started_at: Option<std::time::Instant>,
    /// The language for the active dictation session, resolved at the
    /// Idle→Recording transition (per-toggle override or config default).
    /// Consumed when the session ends so an override never leaks into a
    /// later recording.
    pub(crate) session_language: Option<String>,
    /// Active command mode context (set when command mode is recording).
    pub(crate) command_mode: Option<CommandModeContext>,
    /// Active named LLM command context (set when an `[[llm_commands]]`
    /// hotkey is recording).
    pub(crate) llm_command: Option<LlmCommandContext>,
    /// Active agent voice turn context (set while recording for an agent).
    pub(crate) agent_recording: Option<AgentRecordingContext>,
    /// Runtime instruction overrides for `[[llm_commands]]`, keyed by command
    /// name. Written by the `set_hotkey` path (`SetLlmInstruction`) so a
    /// reprogrammed instruction takes effect immediately — the loaded
    /// `Config` is an immutable `Arc`, so this map is the live source of truth
    /// (and the change is also persisted to disk for the next start).
    pub(crate) llm_instruction_overrides: std::collections::HashMap<String, String>,
    /// Stop flag for in-progress TTS playback (read-selection-aloud).
    ///
    /// Installed when a `Speak` session enters `Synthesizing` (before the
    /// synthesis request is sent), so it also interrupts the request, the
    /// body reader and playback; cleared when the session finishes. `Cancel`
    /// and a repeat `Speak` both flip it to `true`. Its identity
    /// (`Arc::ptr_eq`) marks which session owns the read-aloud state; whether
    /// read-aloud is active is decided by the `Synthesizing`/`Speaking` states,
    /// not by this flag.
    pub(crate) tts_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// What the read-aloud owning `tts_stop` speaks. Set with it; only
    /// meaningful while `tts_stop` is set.
    pub(crate) tts_origin: Option<SpeakOrigin>,
    /// The agent whose reply, acknowledgement or notice that read-aloud
    /// speaks (origin [`SpeakOrigin::Agent`]). Set with `tts_stop`; only
    /// meaningful while it is set.
    pub(crate) tts_agent: Option<String>,
    /// The agent whose voice turn is being transcribed: set when its
    /// recording is claimed, cleared at its `TranscriptionDone` (and by
    /// `discard_command_sessions`). Only meaningful while Transcribing.
    pub(crate) agent_transcribing: Option<String>,
}

impl DaemonState {
    pub(crate) fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            audio_capture: None,
            recording_window_id: None,
            streaming_task: None,
            streaming_cancel: None,
            recording_started_at: None,
            session_language: None,
            command_mode: None,
            llm_command: None,
            agent_recording: None,
            llm_instruction_overrides: std::collections::HashMap::new(),
            tts_stop: None,
            tts_origin: None,
            tts_agent: None,
            agent_transcribing: None,
        }
    }

    /// The agent the voice belongs to now, and what it does: its voice turn
    /// records (`agent_recording`) or is transcribed (`agent_transcribing`),
    /// or its utterance is synthesized or played (`tts_agent`, only for a
    /// read-aloud of origin [`SpeakOrigin::Agent`] that owns `tts_stop`).
    /// Each state reads only its own field, so one left over from an
    /// earlier session is ignored. `None` while Idle, and for dictation,
    /// command mode, a selection read-aloud and `speak --text`.
    pub(crate) fn agent_voice(&self) -> Option<AgentVoice> {
        let state = self.state_machine.state();
        let name = match state {
            State::Idle => None,
            State::Recording => self.agent_recording.as_ref().map(|r| r.name.clone()),
            State::Transcribing => self.agent_transcribing.clone(),
            State::Synthesizing | State::Speaking => self
                .tts_agent
                .clone()
                .filter(|_| self.tts_stop.is_some() && self.tts_origin == Some(SpeakOrigin::Agent)),
        }?;
        Some(AgentVoice { name, state })
    }

    /// End a streaming session's claim on `streaming_cancel`: clear the slot
    /// only if it still holds `flag`, so a newer session's flag survives.
    /// Called by whichever side sees the session end first (the pipeline's
    /// tail, or `handle_toggle` once the pipeline task has been awaited,
    /// which also covers an early error or panic). After it, `cancel` can no
    /// longer reach this session.
    pub(crate) fn release_streaming_cancel(&mut self, flag: &Arc<AtomicBool>) {
        if self
            .streaming_cancel
            .as_ref()
            .is_some_and(|f| Arc::ptr_eq(f, flag))
        {
            self.streaming_cancel = None;
        }
    }

    /// A stopped streaming session is still draining its last deltas (#154).
    /// `streaming_cancel` is `Some` only from a streaming session's start
    /// until it ends (see `release_streaming_cancel`), and command mode never
    /// installs one, so this cannot match a command-mode `Transcribing`.
    pub(crate) fn streaming_session_draining(&self) -> bool {
        self.state_machine.state() == State::Transcribing && self.streaming_cancel.is_some()
    }

    /// Consume the active session's language, falling back to the config
    /// default. Taking (not reading) the value is what ends the language
    /// session — a per-toggle override applies to exactly one recording.
    pub(crate) fn take_session_language(&mut self, default_lang: &str) -> String {
        self.session_language
            .take()
            .unwrap_or_else(|| default_lang.to_string())
    }
}

/// The agent the voice belongs to, and what its voice does
/// ([`DaemonState::agent_voice`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentVoice {
    pub(crate) name: String,
    /// Recording, Transcribing, Synthesizing or Speaking.
    pub(crate) state: State,
}

/// The dictation state broadcast, in three views of one [`DaemonState`]
/// computed under its lock by [`StateBus::publish`], the only way to send:
///
/// - **raw:** the state machine as it is. The recording-lifecycle hooks
///   (media pause fires for an agent's voice turn too) and the agent reply
///   queue.
/// - **ui:** the same, but Idle while the voice belongs to an agent
///   ([`DaemonState::agent_voice`]): the tray, the pill overlay and the
///   GNOME pill signal. Agent mode shows on its own overlay.
/// - **voice:** that agent, for the agent overlay's board.
///
/// Without `[[agents]]` the voice is always `None`, so ui is raw.
#[derive(Clone)]
pub(crate) struct StateBus {
    raw: watch::Sender<State>,
    ui: watch::Sender<State>,
    voice: watch::Sender<Option<AgentVoice>>,
}

impl StateBus {
    pub(crate) fn new() -> Self {
        Self {
            raw: watch::channel(State::Idle).0,
            ui: watch::channel(State::Idle).0,
            voice: watch::channel(None).0,
        }
    }

    /// The three views of `ds` ([`StateBus`]).
    pub(crate) fn views(ds: &DaemonState) -> (State, State, Option<AgentVoice>) {
        let raw = ds.state_machine.state();
        let voice = ds.agent_voice();
        let ui = if voice.is_some() { State::Idle } else { raw };
        (raw, ui, voice)
    }

    /// Broadcast `ds`. The caller holds the dictation lock. raw and ui wake
    /// their receivers on every publish, as the single channel did; voice
    /// only when it changed. Stored even with no receiver, so a later
    /// subscriber starts from the newest state.
    pub(crate) fn publish(&self, ds: &DaemonState) {
        let (raw, ui, voice) = Self::views(ds);
        self.raw.send_replace(raw);
        self.ui.send_replace(ui);
        self.voice.send_if_modified(|current| {
            if *current == voice {
                return false;
            }
            *current = voice;
            true
        });
    }

    pub(crate) fn subscribe_raw(&self) -> watch::Receiver<State> {
        self.raw.subscribe()
    }

    pub(crate) fn subscribe_ui(&self) -> watch::Receiver<State> {
        self.ui.subscribe()
    }

    pub(crate) fn subscribe_voice(&self) -> watch::Receiver<Option<AgentVoice>> {
        self.voice.subscribe()
    }
}

/// Resources shared across all connections (not behind the per-request mutex).
pub(crate) struct DaemonContext {
    pub(crate) config: Config,
    pub(crate) window_tracker: Arc<dyn WindowTracker>,
    pub(crate) transcription_backend: Arc<dyn TranscriptionBackend>,
    pub(crate) notify: bool,
    /// The state broadcast ([`StateBus`]): hooks and the reply queue, the
    /// tray and pill overlay, the agent overlay's board.
    pub(crate) state_tx: StateBus,
    /// Normalized microphone (and TTS playback) level for speech-reactive
    /// overlays: set when the pill overlay or the agent overlay is enabled.
    pub(crate) overlay_level_tx: Option<tokio::sync::watch::Sender<f32>>,
    /// `true` when the visual overlay is enabled. State-transition toasts
    /// are suppressed in that case to avoid duplicate signaling — the
    /// overlay already shows recording/transcribing state visually.
    /// Error notifications still fire.
    pub(crate) overlay_enabled: bool,
    /// The recording feature: its session, its status broadcast (tray only)
    /// and the processing queue. Kept out of [`DaemonState`] so a recording
    /// never waits on, or blocks, the dictation lock.
    #[cfg(feature = "recording")]
    pub(crate) recording: RecordingRuntime,
    /// Agent mode (`[[agents]]`): sessions, running turns and the reply
    /// queue, behind its own lock. Empty, and inert, without agents.
    pub(crate) agents: AgentRuntime,
    /// What the agent overlay shows, one entry per active agent in config
    /// order: composed by the agent board task (`crate::agent`), which
    /// re-publishes when a row changes or a fade ends. The agent overlay
    /// hosts and the GNOME broadcaster subscribe. Stays empty without
    /// agents.
    pub(crate) agent_board: watch::Sender<Vec<AgentOverlayEntry>>,
    /// An agent overlay is on screen (set by its host once it shows): the
    /// agent's progress toasts are then left out ([`Self::notify_agent_state`]).
    pub(crate) agent_overlay_shown: AtomicBool,
}

impl DaemonContext {
    /// Should we surface a state/progress notification (not an error)?
    /// Suppressed when the overlay is on so we don't double-signal the
    /// same event.
    pub(crate) fn notify_state(&self) -> bool {
        self.notify && !self.overlay_enabled
    }

    /// Should an agent's progress toast ("Speak to 'jarvis'...", "Sending
    /// to 'jarvis'...") show? The pill overlay never shows agent mode, so
    /// only an agent overlay on screen stands in for it.
    pub(crate) fn notify_agent_state(&self) -> bool {
        self.notify && !self.agent_overlay_shown.load(Ordering::Relaxed)
    }

    /// Should we surface an error notification? Always yes when the user
    /// has notifications enabled — errors are critical and the overlay
    /// can't carry their detail.
    pub(crate) fn notify_error(&self) -> bool {
        self.notify
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whisrs::state::Action;

    /// [`StateBus::views`], with the voice as a pair.
    fn views(ds: &DaemonState) -> (State, State, Option<(String, State)>) {
        let (raw, ui, voice) = StateBus::views(ds);
        (raw, ui, voice.map(|v| (v.name, v.state)))
    }

    fn jarvis(state: State) -> Option<(String, State)> {
        Some(("jarvis".to_string(), state))
    }

    /// Start a read-aloud of `origin` as `speak_text` does.
    fn start_speaking(ds: &mut DaemonState, origin: SpeakOrigin, agent: Option<&str>) {
        ds.state_machine.transition(Action::SpeakStart).unwrap();
        ds.tts_stop = Some(Arc::new(AtomicBool::new(false)));
        ds.tts_origin = Some(origin);
        ds.tts_agent = agent.map(str::to_string);
    }

    /// End it as `finish_session` does.
    fn finish_speaking(ds: &mut DaemonState) {
        ds.state_machine.transition(Action::SpeakDone).unwrap();
        ds.tts_stop = None;
    }

    /// Every state, for dictation and for an agent's voice: an agent's voice
    /// turn, its transcription and its utterances keep the ui view Idle and
    /// show on the voice view; dictation, a selection read-aloud and `speak
    /// --text` show on the ui view as before. The raw view is the state
    /// machine throughout.
    #[test]
    fn an_agents_voice_stays_off_the_ui_view() {
        use State::*;
        let mut ds = DaemonState::new();
        assert_eq!(views(&ds), (Idle, Idle, None));

        ds.state_machine.transition(Action::Toggle).unwrap();
        assert_eq!(views(&ds), (Recording, Recording, None));
        ds.state_machine.transition(Action::Toggle).unwrap();
        assert_eq!(views(&ds), (Transcribing, Transcribing, None));
        ds.state_machine
            .transition(Action::TranscriptionDone)
            .unwrap();

        // An agent's voice turn, claimed as `voice_turn_background` does.
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.agent_recording = Some(AgentRecordingContext {
            name: "jarvis".into(),
            new_session: false,
        });
        assert_eq!(views(&ds), (Recording, Idle, jarvis(Recording)));
        let claimed =
            crate::command_mode::claim_session(&mut ds.state_machine, &mut ds.agent_recording);
        ds.agent_transcribing = claimed.map(|c| c.name);
        assert_eq!(views(&ds), (Transcribing, Idle, jarvis(Transcribing)));
        ds.state_machine
            .transition(Action::TranscriptionDone)
            .unwrap();
        ds.agent_transcribing = None;
        assert_eq!(views(&ds), (Idle, Idle, None));

        // Its reply (or acknowledgement, or notice).
        start_speaking(&mut ds, SpeakOrigin::Agent, Some("jarvis"));
        assert_eq!(views(&ds), (Synthesizing, Idle, jarvis(Synthesizing)));
        ds.state_machine.transition(Action::SpeakPlaying).unwrap();
        assert_eq!(views(&ds), (Speaking, Idle, jarvis(Speaking)));
        finish_speaking(&mut ds);
        assert_eq!(views(&ds), (Idle, Idle, None));

        // A selection read-aloud and `speak --text`.
        for origin in [SpeakOrigin::Selection, SpeakOrigin::Text] {
            start_speaking(&mut ds, origin, None);
            assert_eq!(views(&ds), (Synthesizing, Synthesizing, None), "{origin:?}");
            ds.state_machine.transition(Action::SpeakPlaying).unwrap();
            assert_eq!(views(&ds), (Speaking, Speaking, None), "{origin:?}");
            finish_speaking(&mut ds);
        }
    }

    /// Each state reads only its own field, so one an earlier session left
    /// is ignored: an agent read-aloud's name with no read-aloud owning the
    /// state, or under a selection read-aloud; a voice turn's name while
    /// dictation records or a read-aloud plays.
    #[test]
    fn stale_agent_fields_are_ignored() {
        use State::*;
        let mut ds = DaemonState::new();
        ds.agent_transcribing = Some("jarvis".into());
        ds.tts_agent = Some("jarvis".into());
        ds.tts_origin = Some(SpeakOrigin::Agent);
        assert_eq!(views(&ds), (Idle, Idle, None));

        // Dictation records.
        ds.state_machine.transition(Action::Toggle).unwrap();
        assert_eq!(views(&ds), (Recording, Recording, None));
        ds.state_machine.transition(Action::Cancel).unwrap();

        // A selection read-aloud, with the last agent's name left over.
        start_speaking(&mut ds, SpeakOrigin::Selection, None);
        ds.tts_agent = Some("jarvis".into());
        ds.agent_recording = Some(AgentRecordingContext {
            name: "jarvis".into(),
            new_session: false,
        });
        assert_eq!(views(&ds), (Synthesizing, Synthesizing, None));
        // An agent read-aloud whose stop flag is gone is not one.
        ds.tts_origin = Some(SpeakOrigin::Agent);
        ds.tts_stop = None;
        assert_eq!(views(&ds), (Synthesizing, Synthesizing, None));
    }

    /// A barge-in (the agent's press while its reply plays) goes from its
    /// reply straight to its voice turn: the ui view is Idle at every
    /// broadcast, and the voice moves from speaking to recording.
    #[test]
    fn a_barge_in_keeps_the_ui_idle() {
        let bus = StateBus::new();
        let ui = bus.subscribe_ui();
        let voice = bus.subscribe_voice();
        let mut ds = DaemonState::new();
        let mut seen = Vec::new();
        let mut publish = |ds: &DaemonState| {
            bus.publish(ds);
            seen.push((
                *ui.borrow(),
                voice.borrow().clone().map(|v| (v.name, v.state)),
            ));
        };
        start_speaking(&mut ds, SpeakOrigin::Agent, Some("jarvis"));
        ds.state_machine.transition(Action::SpeakPlaying).unwrap();
        publish(&ds);
        // `stop_read_aloud`.
        ds.tts_stop.take();
        ds.state_machine.transition(Action::Cancel).unwrap();
        publish(&ds);
        // `start_recording`.
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.agent_recording = Some(AgentRecordingContext {
            name: "jarvis".into(),
            new_session: false,
        });
        publish(&ds);
        assert_eq!(
            seen,
            [
                (State::Idle, jarvis(State::Speaking)),
                (State::Idle, None),
                (State::Idle, jarvis(State::Recording)),
            ]
        );
    }

    /// A plain toggle that takes an agent's voice turn over transcribes it
    /// as dictation, so the tray and the pill show it: the state the stop
    /// arm broadcasts once it discarded the voice turn.
    #[test]
    fn a_toggle_takeover_shows_as_dictation() {
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.agent_recording = Some(AgentRecordingContext {
            name: "jarvis".into(),
            new_session: false,
        });
        ds.state_machine.transition(Action::Toggle).unwrap();
        crate::dictation::discard_command_sessions(&mut ds, "toggle-stop");
        assert_eq!(views(&ds), (State::Transcribing, State::Transcribing, None));
        // `handle_toggle` broadcasts only after that discard.
        let code = include_str!("dictation.rs");
        let start = code
            .find("        State::Recording => {")
            .expect("the stop arm");
        let arm = &code[start..];
        let discard = arm
            .find("discard_command_sessions(&mut ds, \"toggle-stop\")")
            .expect("the discard");
        let publish = arm.find("state_tx.publish(").expect("the broadcast");
        assert!(discard < publish);
    }

    /// raw and ui wake their receivers on every broadcast, as the single
    /// channel did (the tray and hooks are used to it); voice only when it
    /// changed. A receiver subscribed later starts from the newest state.
    #[test]
    fn the_voice_wakes_only_on_a_change() {
        let bus = StateBus::new();
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.agent_recording = Some(AgentRecordingContext {
            name: "jarvis".into(),
            new_session: false,
        });
        bus.publish(&ds);
        let mut raw = bus.subscribe_raw();
        let mut ui = bus.subscribe_ui();
        let mut voice = bus.subscribe_voice();
        assert_eq!(*raw.borrow(), State::Recording);
        assert_eq!(*ui.borrow(), State::Idle);
        assert!(voice.borrow().is_some());
        bus.publish(&ds);
        assert!(raw.has_changed().unwrap());
        assert!(ui.has_changed().unwrap());
        assert!(!voice.has_changed().unwrap());
        raw.borrow_and_update();
        ui.borrow_and_update();
        ds.state_machine.transition(Action::Cancel).unwrap();
        bus.publish(&ds);
        assert!(voice.has_changed().unwrap());
        assert_eq!(*voice.borrow_and_update(), None);
    }
}
