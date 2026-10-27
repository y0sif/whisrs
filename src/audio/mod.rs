//! Audio capture and recovery.

pub mod capture;
pub mod device;
pub mod feedback;
pub mod playback;
pub mod recovery;
pub mod wav;

/// A chunk of 16-bit PCM audio samples.
pub type AudioChunk = Vec<i16>;

// The speech detector (`[audio] vad = "earshot"`) runs at 16 kHz only; at any
// other rate it silently takes the RMS test. Capture and recording tracks
// must stay at its rate.
const _: () = {
    assert!(capture::SAMPLE_RATE == audio_silence_gate::VAD_SAMPLE_RATE);
    assert!(crate::recording::SAMPLE_RATE == audio_silence_gate::VAD_SAMPLE_RATE);
};

/// Run a blocking `f` without stalling a tokio worker: on the multi-thread
/// runtime the worker hands its tasks off first. `block_in_place` panics on a
/// current-thread runtime, where `f` simply runs inline, as it does with no
/// runtime at all.
pub fn block_off_runtime<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::block_off_runtime;

    #[test]
    fn block_off_runtime_without_a_runtime() {
        assert_eq!(block_off_runtime(|| 7), 7);
    }

    #[tokio::test]
    async fn block_off_runtime_on_current_thread_runtime() {
        assert_eq!(block_off_runtime(|| 7), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_off_runtime_on_multi_thread_runtime() {
        assert_eq!(block_off_runtime(|| 7), 7);
    }
}
