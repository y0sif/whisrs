//! Audio capture, silence detection, and recovery.

pub mod capture;
pub mod feedback;
pub mod recovery;
pub mod silence;

/// A chunk of 16-bit PCM audio samples.
pub type AudioChunk = Vec<i16>;
