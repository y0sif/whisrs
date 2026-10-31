//! Audio capture and recovery.

pub mod capture;
pub mod device;
pub mod feedback;
pub mod playback;
pub mod quality;
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

/// Synthetic audio for tests in several modules.
#[cfg(test)]
pub(crate) mod test_audio {
    /// A drum loop at 120 bpm whose loudest sample is at `peak_dbfs`: a
    /// kick (a sine falling from 150 to 50 Hz) on every beat, a hi-hat (a
    /// burst of noise) on every beat and off-beat, decaying to digital
    /// silence between them. No voice, but the soft threshold
    /// (`VadConfig::soft_snr_db`) calls about a quarter of it speech at
    /// -30 dBFS; without it, none.
    pub(crate) fn drum_loop(secs: usize, peak_dbfs: f64) -> Vec<i16> {
        use std::f64::consts::PI;
        let rate = super::capture::SAMPLE_RATE as usize;
        let n = secs * rate;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            2.0 * ((state >> 11) as f64 / (1u64 << 53) as f64) - 1.0
        };
        let mut out = vec![0.0f64; n];
        let beat = rate / 2;
        for start in (0..n).step_by(beat) {
            let mut phase = 0.0;
            for k in 0..(rate / 5).min(n - start) {
                let t = k as f64 / rate as f64;
                phase += (50.0 + 100.0 * (-t / 0.03).exp()) / rate as f64;
                out[start + k] += (2.0 * PI * phase).sin() * (-t / 0.08).exp();
            }
            for hat in [start, start + beat / 2] {
                for k in 0..(rate / 20).min(n.saturating_sub(hat)) {
                    let decay = (-(k as f64) / (rate as f64 * 0.01)).exp();
                    out[hat + k] += 0.4 * uniform() * decay;
                }
            }
        }
        let peak = out.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        let gain = 10f64.powf(peak_dbfs / 20.0) * f64::from(i16::MAX) / peak;
        out.iter()
            .map(|x| (x * gain).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
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
