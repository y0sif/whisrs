//! Shared OpenAI-compatible realtime transcription protocol helpers.

mod engine;
mod profile;
mod wire;

pub use engine::{OpenAiRealtimeProtocolEngine, RealtimeEngineConfig};
pub use profile::{openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, TurnDetectionMode};
