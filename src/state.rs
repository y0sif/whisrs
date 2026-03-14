//! Daemon state machine: Idle → Recording → Transcribing → Idle.

use crate::{State, WhisrsError};

/// Actions that can trigger state transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Cancel,
    TranscriptionDone,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Toggle => write!(f, "toggle"),
            Action::Cancel => write!(f, "cancel"),
            Action::TranscriptionDone => write!(f, "transcription_done"),
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
    pub fn transition(&mut self, action: Action) -> Result<State, WhisrsError> {
        let new_state = match (self.state, action) {
            (State::Idle, Action::Toggle) => State::Recording,
            (State::Recording, Action::Toggle) => State::Transcribing,
            (State::Recording, Action::Cancel) => State::Idle,
            (State::Transcribing, Action::TranscriptionDone) => State::Idle,
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
}
