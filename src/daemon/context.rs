use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::llm;
use whisrs::state::StateMachine;
use whisrs::transcription::TranscriptionBackend;
use whisrs::window::WindowTracker;
use whisrs::{Config, State};

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
    /// Runtime instruction overrides for `[[llm_commands]]`, keyed by command
    /// name. Written by the `set_hotkey` path (`SetLlmInstruction`) so a
    /// reprogrammed instruction takes effect immediately — the loaded
    /// `Config` is an immutable `Arc`, so this map is the live source of truth
    /// (and the change is also persisted to disk for the next start).
    pub(crate) llm_instruction_overrides: std::collections::HashMap<String, String>,
    /// Stop flag for in-progress TTS playback (read-selection-aloud).
    ///
    /// Set when a `Speak` synthesis succeeds and playback begins; cleared when
    /// playback finishes. `Cancel` and a repeat `Speak` both flip it to `true`
    /// to interrupt playback. Read-aloud runs independently of the recording
    /// state machine, so there is no dedicated `State` variant.
    pub(crate) tts_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
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
            llm_instruction_overrides: std::collections::HashMap::new(),
            tts_stop: None,
        }
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

/// Resources shared across all connections (not behind the per-request mutex).
pub(crate) struct DaemonContext {
    pub(crate) config: Config,
    pub(crate) window_tracker: Arc<dyn WindowTracker>,
    pub(crate) transcription_backend: Arc<dyn TranscriptionBackend>,
    pub(crate) notify: bool,
    /// Broadcast channel for state changes (consumed by system tray).
    pub(crate) state_tx: tokio::sync::watch::Sender<State>,
    /// Normalized microphone level for speech-reactive overlays.
    pub(crate) overlay_level_tx: Option<tokio::sync::watch::Sender<f32>>,
    /// `true` when the visual overlay is enabled. State-transition toasts
    /// are suppressed in that case to avoid duplicate signaling — the
    /// overlay already shows recording/transcribing state visually.
    /// Error notifications still fire.
    pub(crate) overlay_enabled: bool,
}

impl DaemonContext {
    /// Should we surface a state/progress notification (not an error)?
    /// Suppressed when the overlay is on so we don't double-signal the
    /// same event.
    pub(crate) fn notify_state(&self) -> bool {
        self.notify && !self.overlay_enabled
    }

    /// Should we surface an error notification? Always yes when the user
    /// has notifications enabled — errors are critical and the overlay
    /// can't carry their detail.
    pub(crate) fn notify_error(&self) -> bool {
        self.notify
    }
}
