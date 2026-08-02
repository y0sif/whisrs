//! Audio capture and recovery.

pub mod capture;
pub mod feedback;
pub mod playback;
pub mod recovery;
pub mod wav;

/// A chunk of 16-bit PCM audio samples.
pub type AudioChunk = Vec<i16>;
