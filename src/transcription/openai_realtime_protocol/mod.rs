//! Shared OpenAI-compatible realtime transcription protocol helpers.

mod engine;
mod profile;
mod wire;

pub use engine::{OpenAiRealtimeProtocolEngine, RealtimeEngineConfig};
pub use profile::{
    clamp_prompt, encode_pcm_base64, openai_turn_detection_mode_for_model, resample_16k_to_24k,
    DeltaMode, OpenAiRealtimeProfile, TurnDetectionMode, PROMPT_MAX_CHARS,
};
pub use wire::{AudioBufferAppend, AudioBufferCommit, ServerError, ServerMessage};
