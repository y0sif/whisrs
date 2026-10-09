//! Shared OpenAI-compatible realtime transcription protocol helpers.

mod engine;
mod profile;
mod wire;

pub use engine::{OpenAiRealtimeProtocolEngine, RealtimeEngineConfig};
pub use profile::{
    openai_model_supports_languages, openai_model_supports_prompt,
    openai_turn_detection_mode_for_model, OpenAiRealtimeProfile, TurnDetectionMode,
};
