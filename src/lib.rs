//! whisrs — shared types for CLI and daemon communication.

pub mod audio;
pub mod config;
pub mod history;
pub mod hotkey;
pub mod ipc;
pub mod llm;
pub mod overlay;
pub mod service_ctl;
pub mod state;
pub mod transcription;
pub mod tray;
pub mod tts;
pub mod window;

pub use config::types::*;
pub use ipc::*;
pub use service_ctl::*;

#[cfg(feature = "local-whisper")]
pub(crate) use config::types::default_phrase_silence_ms;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WhisrsError {
    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("transcription error: {0}")]
    Transcription(String),

    #[error("invalid state transition from {from} on {action}")]
    InvalidTransition { from: State, action: String },
}

#[cfg(test)]
mod tests {
    /// The man pages carry the version by hand and drifted for 20 releases, so
    /// assert it rather than trusting the release checklist. The revision date
    /// in the same `.TH` line stays manual — nothing in the build knows it.
    #[test]
    fn man_pages_declare_current_version() {
        let expected = format!("\"whisrs {}\"", env!("CARGO_PKG_VERSION"));
        for (name, page) in [
            ("contrib/whisrs.1", include_str!("../contrib/whisrs.1")),
            ("contrib/whisrsd.1", include_str!("../contrib/whisrsd.1")),
        ] {
            let th = page.lines().next().unwrap_or_default();
            assert!(
                th.starts_with(".TH ") && th.contains(&expected),
                "{name}: .TH should carry {expected}, got: {th}"
            );
        }
    }
}
