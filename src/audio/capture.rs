//! Audio capture using the `cpal` crate.
//!
//! Opens the default input device at 16kHz mono 16-bit and pushes audio
//! chunks into a tokio mpsc channel for downstream processing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::AudioChunk;

/// Desired sample rate for speech recognition.
const SAMPLE_RATE: u32 = 16_000;

/// Number of channels (mono).
const CHANNELS: u16 = 1;

/// A handle to a running audio capture session.
///
/// The actual `cpal::Stream` lives on a dedicated thread (since it's not Send).
/// This handle provides only the receiver and a stop signal.
pub struct AudioCaptureHandle {
    /// Receiver end of the audio channel.
    receiver: Option<mpsc::UnboundedReceiver<AudioChunk>>,
    /// Signal to stop the capture thread.
    stop_signal: Arc<AtomicBool>,
    /// Join handle for the capture thread.
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: The AudioCaptureHandle itself only contains Send types.
// The non-Send cpal::Stream lives on its own thread.
unsafe impl Send for AudioCaptureHandle {}

impl Drop for AudioCaptureHandle {
    fn drop(&mut self) {
        // Signal the capture thread to stop.
        self.stop_signal.store(true, Ordering::Release);
        // Wait for the thread to finish (non-async, best-effort).
        if let Some(handle) = self.thread_handle.take() {
            handle.join().ok();
        }
    }
}

impl AudioCaptureHandle {
    /// Start capturing audio from the default input device.
    ///
    /// The capture runs on a dedicated thread. Audio chunks are sent through
    /// the internal channel; call `take_receiver()` to get the receiving end.
    pub fn start() -> anyhow::Result<Self> {
        Self::start_with_level_tx(None)
    }

    /// Start capturing audio and optionally publish a normalized volume level.
    pub fn start_with_level_tx(
        level_tx: Option<tokio::sync::watch::Sender<f32>>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_signal);

        // Channel to send back any initialization error from the thread.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();

        let thread_handle = std::thread::Builder::new()
            .name("whisrs-audio".into())
            .spawn(move || {
                run_capture(tx, stop_clone, init_tx, level_tx);
            })
            .context("failed to spawn audio capture thread")?;

        // Wait for initialization result.
        let init_result = init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("audio capture thread exited unexpectedly"))?;
        init_result?;

        Ok(Self {
            receiver: Some(rx),
            stop_signal,
            thread_handle: Some(thread_handle),
        })
    }

    /// Take the receiver end of the audio channel.
    pub fn take_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<AudioChunk>> {
        self.receiver.take()
    }

    /// Signal the capture thread to stop (async-friendly).
    /// The channel will close once the thread exits. Callers reading
    /// from the receiver will see `None` after remaining chunks drain.
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::Release);
    }

    /// Stop the audio capture and return all accumulated samples from the channel.
    pub async fn stop_and_collect(mut self) -> anyhow::Result<Vec<i16>> {
        // Signal the capture thread to stop.
        self.stop_signal.store(true, Ordering::Release);

        // Wait for the thread to finish.
        if let Some(handle) = self.thread_handle.take() {
            // Use spawn_blocking to avoid blocking the tokio runtime.
            tokio::task::spawn_blocking(move || {
                handle.join().ok();
            })
            .await?;
        }

        let mut all_samples = Vec::new();

        if let Some(mut rx) = self.receiver.take() {
            // Drain all remaining chunks from the channel.
            rx.close();
            while let Ok(chunk) = rx.try_recv() {
                all_samples.extend_from_slice(&chunk);
            }
        }

        info!("captured {} audio samples", all_samples.len());
        Ok(all_samples)
    }
}

/// Run the audio capture on the current thread.
///
/// Sends the initialization result through `init_tx`, then blocks until the
/// stop signal is set. The cpal Stream lives on this thread (it's not Send).
fn run_capture(
    tx: mpsc::UnboundedSender<AudioChunk>,
    stop_signal: Arc<AtomicBool>,
    init_tx: std::sync::mpsc::Sender<anyhow::Result<()>>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
) {
    let result = setup_and_run(tx, stop_signal, &init_tx, level_tx);
    if let Err(e) = result {
        // If init_tx hasn't been used yet, send the error.
        init_tx.send(Err(e)).ok();
    }
}

fn setup_and_run(
    tx: mpsc::UnboundedSender<AudioChunk>,
    stop_signal: Arc<AtomicBool>,
    init_tx: &std::sync::mpsc::Sender<anyhow::Result<()>>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default audio input device found"))?;

    let device_name = device.name().unwrap_or_else(|_| "unknown".into());
    info!("using audio input device: {device_name}");

    let config = StreamConfig {
        channels: CHANNELS,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Verify device support.
    let supported = device
        .supported_input_configs()
        .context("failed to query supported input configs")?;

    let mut found_match = false;
    for range in supported {
        if range.channels() == CHANNELS
            && range.min_sample_rate().0 <= SAMPLE_RATE
            && range.max_sample_rate().0 >= SAMPLE_RATE
            && range.sample_format() == SampleFormat::I16
        {
            found_match = true;
            break;
        }
    }

    if !found_match {
        warn!(
            "device may not natively support {SAMPLE_RATE}Hz mono i16; \
             cpal will attempt conversion"
        );
    }

    let err_callback = |err: cpal::StreamError| {
        error!("audio stream error: {err}");
    };

    let callback_level_tx = level_tx.clone();
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                if let Some(level_tx) = &callback_level_tx {
                    let _ = level_tx.send(audio_level(data));
                }
                if tx.send(data.to_vec()).is_err() {
                    // Channel closed — capture is stopping.
                }
            },
            err_callback,
            None,
        )
        .context("failed to build audio input stream")?;

    stream.play().context("failed to start audio stream")?;
    debug!("audio capture started at {SAMPLE_RATE}Hz mono i16");

    // Signal successful initialization.
    init_tx.send(Ok(())).ok();

    // Block until stop is signaled. Keep the stream alive.
    while !stop_signal.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    debug!("audio capture stopping");
    if let Some(level_tx) = &level_tx {
        let _ = level_tx.send(0.0);
    }
    drop(stream);

    Ok(())
}

fn audio_level(data: &[i16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }

    let sum_squares: f32 = data
        .iter()
        .map(|sample| {
            let normalized = *sample as f32 / i16::MAX as f32;
            normalized * normalized
        })
        .sum();
    let rms = (sum_squares / data.len() as f32).sqrt();

    // Soft compressor: 1 - exp(-k*rms). k=18 maps typical speech RMS
    // (~0.05–0.15) to the 0.6–0.95 range, so the visualizer reaches the
    // top of its dynamic range during normal speech instead of hovering
    // around 30 % deflection.
    (1.0 - (-rms * 18.0).exp()).clamp(0.0, 1.0)
}

/// Encode raw PCM samples (16kHz, mono, i16) to a WAV byte buffer.
pub fn encode_wav(samples: &[i16]) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).context("failed to create WAV writer")?;

        for &sample in samples {
            writer
                .write_sample(sample)
                .context("failed to write WAV sample")?;
        }

        writer.finalize().context("failed to finalize WAV")?;
    }

    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_produces_valid_output() {
        let samples: Vec<i16> = (0..1600).map(|i| (i % 256) as i16).collect();
        let wav = encode_wav(&samples).unwrap();

        // WAV files start with "RIFF".
        assert_eq!(&wav[..4], b"RIFF");

        // Verify we can read it back with hound.
        let cursor = std::io::Cursor::new(&wav);
        let reader = hound::WavReader::new(cursor).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);

        let read_samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_samples.len(), 1600);
        assert_eq!(read_samples, samples);
    }

    #[test]
    fn encode_wav_empty_samples() {
        let wav = encode_wav(&[]).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
