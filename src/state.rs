//! Daemon state machine.
//!
//! Dictation: Idle → Recording → Transcribing → Idle.
//! Read-aloud: Idle → Synthesizing → Speaking → Idle.
//!
//! The two flows are mutually exclusive: recording is refused while
//! read-aloud is active, and a read-aloud request is refused while recording.

use crate::{State, WhisrsError};

/// Actions that can trigger state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Cancel,
    TranscriptionDone,
    /// Read-aloud: begin synthesizing the selection.
    SpeakStart,
    /// Read-aloud: synthesis finished, playback begins.
    SpeakPlaying,
    /// Read-aloud: playback finished (or synthesis produced nothing).
    SpeakDone,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Toggle => write!(f, "toggle"),
            Action::Cancel => write!(f, "cancel"),
            Action::TranscriptionDone => write!(f, "transcription_done"),
            Action::SpeakStart => write!(f, "speak_start"),
            Action::SpeakPlaying => write!(f, "speak_playing"),
            Action::SpeakDone => write!(f, "speak_done"),
        }
    }
}

/// Manages the daemon's state transitions.
#[derive(Debug)]
pub struct StateMachine {
    state: State,
}

impl StateMachine {
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    /// Return the current state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Attempt a state transition. Returns the new state on success.
    ///
    /// Valid transitions:
    /// - Toggle:            Idle → Recording
    /// - Toggle:            Recording → Transcribing
    /// - Cancel:            Recording → Idle
    /// - TranscriptionDone: Transcribing → Idle
    /// - SpeakStart:        Idle → Synthesizing
    /// - SpeakPlaying:      Synthesizing → Speaking
    /// - SpeakDone:         Synthesizing → Idle
    /// - SpeakDone:         Speaking → Idle
    /// - Cancel:            Synthesizing → Idle
    /// - Cancel:            Speaking → Idle
    ///
    /// Recording is intentionally refused while read-aloud is active
    /// (Toggle/CommandMode from Synthesizing/Speaking are invalid).
    pub fn transition(&mut self, action: Action) -> Result<State, WhisrsError> {
        let new_state = match (self.state, action) {
            (State::Idle, Action::Toggle) => State::Recording,
            (State::Recording, Action::Toggle) => State::Transcribing,
            (State::Recording, Action::Cancel) => State::Idle,
            (State::Transcribing, Action::TranscriptionDone) => State::Idle,
            // Read-aloud flow.
            (State::Idle, Action::SpeakStart) => State::Synthesizing,
            (State::Synthesizing, Action::SpeakPlaying) => State::Speaking,
            (State::Synthesizing, Action::SpeakDone) => State::Idle,
            (State::Speaking, Action::SpeakDone) => State::Idle,
            (State::Synthesizing, Action::Cancel) => State::Idle,
            (State::Speaking, Action::Cancel) => State::Idle,
            (from, action) => {
                return Err(WhisrsError::InvalidTransition {
                    from,
                    action: action.to_string(),
                });
            }
        };
        self.state = new_state;
        Ok(new_state)
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_idle() {
        let sm = StateMachine::new();
        assert_eq!(sm.state(), State::Idle);
    }

    #[test]
    fn toggle_idle_to_recording() {
        let mut sm = StateMachine::new();
        let s = sm.transition(Action::Toggle).unwrap();
        assert_eq!(s, State::Recording);
        assert_eq!(sm.state(), State::Recording);
    }

    #[test]
    fn toggle_recording_to_transcribing() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        let s = sm.transition(Action::Toggle).unwrap();
        assert_eq!(s, State::Transcribing);
    }

    #[test]
    fn cancel_recording_to_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        let s = sm.transition(Action::Cancel).unwrap();
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn transcription_done_to_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Toggle).unwrap(); // → Transcribing
        let s = sm.transition(Action::TranscriptionDone).unwrap();
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn invalid_toggle_while_transcribing() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Toggle).unwrap(); // → Transcribing
        let err = sm.transition(Action::Toggle).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Transcribing,
                ..
            }
        ));
    }

    #[test]
    fn invalid_cancel_while_idle() {
        let mut sm = StateMachine::new();
        let err = sm.transition(Action::Cancel).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Idle,
                ..
            }
        ));
    }

    #[test]
    fn invalid_cancel_while_transcribing() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Toggle).unwrap(); // → Transcribing
        let err = sm.transition(Action::Cancel).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Transcribing,
                ..
            }
        ));
    }

    #[test]
    fn invalid_transcription_done_while_idle() {
        let mut sm = StateMachine::new();
        let err = sm.transition(Action::TranscriptionDone).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Idle,
                ..
            }
        ));
    }

    #[test]
    fn full_cycle() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state(), State::Idle);

        sm.transition(Action::Toggle).unwrap();
        assert_eq!(sm.state(), State::Recording);

        sm.transition(Action::Toggle).unwrap();
        assert_eq!(sm.state(), State::Transcribing);

        sm.transition(Action::TranscriptionDone).unwrap();
        assert_eq!(sm.state(), State::Idle);
    }

    #[test]
    fn cancel_then_restart() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Cancel).unwrap(); // → Idle
        sm.transition(Action::Toggle).unwrap(); // → Recording again
        assert_eq!(sm.state(), State::Recording);
    }

    /// A background task finishing after `cancel` must not move the machine:
    /// its late TranscriptionDone is invalid from Idle and leaves Idle
    /// untouched (issues #81/#90).
    #[test]
    fn late_transcription_done_after_cancel_stays_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Cancel).unwrap(); // → Idle
        assert!(sm.transition(Action::TranscriptionDone).is_err());
        assert_eq!(sm.state(), State::Idle);
    }

    /// Documents the #81/#90 trap: from a post-cancel Idle, Toggle is a
    /// valid user action that starts a NEW recording, so the machine cannot
    /// reject a background task that fires Toggle blindly — the task must
    /// check its session context first (`claim_session` in
    /// src/daemon/command_mode.rs). A blind Toggle re-enters Recording and
    /// the finalizer's TranscriptionDone then wedges there.
    #[test]
    fn blind_toggle_after_cancel_reenters_recording_and_wedges() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Cancel).unwrap(); // → Idle
                                                // The old buggy background: Toggle from Idle is valid...
        assert_eq!(sm.transition(Action::Toggle).unwrap(), State::Recording);
        // ...and its finalizer's TranscriptionDone is then swallowed,
        // leaving the machine stuck in Recording. This is the wedge the
        // daemon-side context check prevents from ever being reachable.
        assert!(sm.transition(Action::TranscriptionDone).is_err());
        assert_eq!(sm.state(), State::Recording);
    }

    /// Once a claimed session is Transcribing, cancel cannot yank the state
    /// out from under the pipeline — its finalizer's TranscriptionDone is
    /// the only way back to Idle. (This is why the post-claim half of the
    /// cancel race is safe: cancel returns an error instead.)
    #[test]
    fn cancel_cannot_interrupt_transcribing_before_finalize() {
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        sm.transition(Action::Toggle).unwrap(); // claimed → Transcribing
        assert!(sm.transition(Action::Cancel).is_err());
        assert_eq!(sm.state(), State::Transcribing);
        assert_eq!(
            sm.transition(Action::TranscriptionDone).unwrap(),
            State::Idle
        );
    }

    // --- Read-aloud flow ---

    #[test]
    fn speak_full_cycle() {
        let mut sm = StateMachine::new();
        assert_eq!(
            sm.transition(Action::SpeakStart).unwrap(),
            State::Synthesizing
        );
        assert_eq!(
            sm.transition(Action::SpeakPlaying).unwrap(),
            State::Speaking
        );
        assert_eq!(sm.transition(Action::SpeakDone).unwrap(), State::Idle);
    }

    #[test]
    fn speak_done_from_synthesizing() {
        // Synthesis produced nothing / was cancelled before playback began.
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        assert_eq!(sm.transition(Action::SpeakDone).unwrap(), State::Idle);
    }

    #[test]
    fn cancel_synthesizing_to_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        assert_eq!(sm.transition(Action::Cancel).unwrap(), State::Idle);
    }

    #[test]
    fn cancel_speaking_to_idle() {
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        sm.transition(Action::SpeakPlaying).unwrap();
        assert_eq!(sm.transition(Action::Cancel).unwrap(), State::Idle);
    }

    #[test]
    fn speak_then_speak_again() {
        // After a read-aloud cycle finishes, a fresh read-aloud can start.
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        sm.transition(Action::SpeakPlaying).unwrap();
        sm.transition(Action::SpeakDone).unwrap(); // → Idle
        assert_eq!(
            sm.transition(Action::SpeakStart).unwrap(),
            State::Synthesizing
        );
    }

    #[test]
    fn invalid_toggle_while_speaking() {
        // Recording is refused while read-aloud is playing.
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        sm.transition(Action::SpeakPlaying).unwrap();
        let err = sm.transition(Action::Toggle).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Speaking,
                ..
            }
        ));
    }

    #[test]
    fn invalid_toggle_while_synthesizing() {
        let mut sm = StateMachine::new();
        sm.transition(Action::SpeakStart).unwrap();
        let err = sm.transition(Action::Toggle).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Synthesizing,
                ..
            }
        ));
    }

    #[test]
    fn invalid_speak_start_while_recording() {
        // Read-aloud is refused while recording (handler maps this to a
        // "busy" error without mutating the FSM, but the transition itself
        // is also invalid as a defence in depth).
        let mut sm = StateMachine::new();
        sm.transition(Action::Toggle).unwrap(); // → Recording
        let err = sm.transition(Action::SpeakStart).unwrap_err();
        assert!(matches!(
            err,
            WhisrsError::InvalidTransition {
                from: State::Recording,
                ..
            }
        ));
    }
}
