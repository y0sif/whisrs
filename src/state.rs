//! Daemon state machine.
//!
//! Dictation: Idle → Recording → Transcribing → Idle.
//! Read-aloud: Idle → Synthesizing → Speaking → Idle.
//! Recovery retry: Idle → Transcribing → Idle (saved audio, no recording).
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
    /// `whisrs transcribe-recovery`: transcribe saved audio, with no
    /// recording before it. Its `TranscriptionDone` returns to Idle.
    RetryStart,
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
            Action::RetryStart => write!(f, "retry_start"),
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
    /// - RetryStart:        Idle → Transcribing
    ///
    /// Recording is intentionally refused while read-aloud is active
    /// (Toggle from Synthesizing/Speaking is invalid).
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
            // Recovery retry.
            (State::Idle, Action::RetryStart) => State::Transcribing,
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
    use super::Action::*;
    use super::*;
    use crate::State::{Idle, Recording, Speaking, Synthesizing, Transcribing};

    /// Every valid transition, walked as the flows run: a new machine is
    /// Idle, and each step returns the new state, which the machine holds.
    #[test]
    fn valid_transitions() {
        let flows: &[(&str, &[(Action, State)])] = &[
            (
                "dictation",
                &[
                    (Toggle, Recording),
                    (Toggle, Transcribing),
                    (TranscriptionDone, Idle),
                ],
            ),
            (
                "cancel, then record again",
                &[(Toggle, Recording), (Cancel, Idle), (Toggle, Recording)],
            ),
            (
                "read-aloud, then another",
                &[
                    (SpeakStart, Synthesizing),
                    (SpeakPlaying, Speaking),
                    (SpeakDone, Idle),
                    (SpeakStart, Synthesizing),
                ],
            ),
            (
                "synthesis that plays nothing",
                &[(SpeakStart, Synthesizing), (SpeakDone, Idle)],
            ),
            (
                "cancel while synthesizing",
                &[(SpeakStart, Synthesizing), (Cancel, Idle)],
            ),
            (
                "cancel while speaking",
                &[
                    (SpeakStart, Synthesizing),
                    (SpeakPlaying, Speaking),
                    (Cancel, Idle),
                ],
            ),
            (
                "recovery retry",
                &[(RetryStart, Transcribing), (TranscriptionDone, Idle)],
            ),
        ];
        for (flow, steps) in flows {
            let mut sm = StateMachine::new();
            assert_eq!(sm.state(), Idle, "{flow}: a new machine");
            for (step, &(action, to)) in steps.iter().enumerate() {
                let got = sm
                    .transition(action)
                    .unwrap_or_else(|e| panic!("{flow}, step {step}: {e}"));
                assert_eq!(got, to, "{flow}, step {step}");
                assert_eq!(sm.state(), to, "{flow}, step {step}");
            }
        }
    }

    /// The refusals the flows meet: an `InvalidTransition` from the state the
    /// machine was in, which it keeps. Recording and read-aloud refuse each
    /// other, and a transcription, a retry's too, ends only with its own
    /// `TranscriptionDone`: cancel cannot cut it short.
    #[test]
    fn invalid_transitions_leave_the_state() {
        let cases: &[(&str, &[Action], Action, State)] = &[
            (
                "toggle while transcribing",
                &[Toggle, Toggle],
                Toggle,
                Transcribing,
            ),
            ("cancel while idle", &[], Cancel, Idle),
            (
                "cancel while transcribing",
                &[Toggle, Toggle],
                Cancel,
                Transcribing,
            ),
            (
                "transcription done while idle",
                &[],
                TranscriptionDone,
                Idle,
            ),
            // A task that finishes after a cancel cannot move the machine.
            (
                "a late transcription done after cancel",
                &[Toggle, Cancel],
                TranscriptionDone,
                Idle,
            ),
            // A task that fires Toggle blindly after a cancel starts a new
            // recording, whose TranscriptionDone is then refused: the
            // daemon's tasks claim their session first (`claim_session`,
            // daemon/command_mode.rs).
            (
                "transcription done while recording",
                &[Toggle, Cancel, Toggle],
                TranscriptionDone,
                Recording,
            ),
            (
                "toggle while synthesizing",
                &[SpeakStart],
                Toggle,
                Synthesizing,
            ),
            (
                "toggle while speaking",
                &[SpeakStart, SpeakPlaying],
                Toggle,
                Speaking,
            ),
            ("speak while recording", &[Toggle], SpeakStart, Recording),
            ("retry while recording", &[Toggle], RetryStart, Recording),
            (
                "retry while transcribing",
                &[Toggle, Toggle],
                RetryStart,
                Transcribing,
            ),
            (
                "retry while synthesizing",
                &[SpeakStart],
                RetryStart,
                Synthesizing,
            ),
            (
                "retry while speaking",
                &[SpeakStart, SpeakPlaying],
                RetryStart,
                Speaking,
            ),
            ("cancel during a retry", &[RetryStart], Cancel, Transcribing),
            ("toggle during a retry", &[RetryStart], Toggle, Transcribing),
        ];
        for &(case, setup, action, before) in cases {
            let mut sm = StateMachine::new();
            for &step in setup {
                sm.transition(step)
                    .unwrap_or_else(|e| panic!("{case}: setup {step}: {e}"));
            }
            let err = sm.transition(action).expect_err(case);
            assert!(
                matches!(err, WhisrsError::InvalidTransition { from, .. } if from == before),
                "{case}: {err}"
            );
            assert_eq!(sm.state(), before, "{case}");
        }
    }
}
