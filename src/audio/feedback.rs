//! Audio feedback — play subtle tones on recording start, stop, and completion.
//!
//! Generates simple tones programmatically using `cpal` for playback on the
//! default output device. Each play function spawns a thread so it never
//! blocks the caller.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tracing::warn;

/// Sample rate for generated tones.
const TONE_SAMPLE_RATE: u32 = 44_100;

/// Play the "start recording" sound: a short rising tone (800Hz -> 1200Hz, ~150ms).
pub fn play_start(volume: f32) {
    std::thread::Builder::new()
        .name("whisrs-feedback".into())
        .spawn(move || {
            let samples = generate_sweep(800.0, 1200.0, 0.15, volume);
            play_samples(&samples);
        })
        .ok();
}

/// Play the "stop recording" sound: a short falling tone (1200Hz -> 800Hz, ~150ms).
pub fn play_stop(volume: f32) {
    std::thread::Builder::new()
        .name("whisrs-feedback".into())
        .spawn(move || {
            let samples = generate_sweep(1200.0, 800.0, 0.15, volume);
            play_samples(&samples);
        })
        .ok();
}

/// Play the "done" sound: a soft double beep (~200ms total).
pub fn play_done(volume: f32) {
    std::thread::Builder::new()
        .name("whisrs-feedback".into())
        .spawn(move || {
            let beep1 = generate_tone(1000.0, 0.07, volume);
            let silence = vec![0.0f32; (TONE_SAMPLE_RATE as f32 * 0.06) as usize];
            let beep2 = generate_tone(1200.0, 0.07, volume);

            let mut samples = Vec::with_capacity(beep1.len() + silence.len() + beep2.len());
            samples.extend_from_slice(&beep1);
            samples.extend_from_slice(&silence);
            samples.extend_from_slice(&beep2);
            play_samples(&samples);
        })
        .ok();
}

/// Generate a frequency sweep from `start_hz` to `end_hz` over `duration_secs`.
fn generate_sweep(start_hz: f32, end_hz: f32, duration_secs: f32, volume: f32) -> Vec<f32> {
    let num_samples = (TONE_SAMPLE_RATE as f32 * duration_secs) as usize;
    let mut samples = Vec::with_capacity(num_samples);
    let fade_samples = (TONE_SAMPLE_RATE as f32 * 0.02) as usize; // 20ms fade

    for i in 0..num_samples {
        let t = i as f32 / TONE_SAMPLE_RATE as f32;
        let progress = i as f32 / num_samples as f32;
        let freq = start_hz + (end_hz - start_hz) * progress;
        let mut sample = (2.0 * std::f32::consts::PI * freq * t).sin() * volume;

        // Apply fade-in/fade-out envelope to avoid clicks.
        if i < fade_samples {
            sample *= i as f32 / fade_samples as f32;
        } else if i > num_samples - fade_samples {
            sample *= (num_samples - i) as f32 / fade_samples as f32;
        }

        samples.push(sample);
    }

    samples
}

/// Generate a pure tone at `freq_hz` for `duration_secs`.
fn generate_tone(freq_hz: f32, duration_secs: f32, volume: f32) -> Vec<f32> {
    let num_samples = (TONE_SAMPLE_RATE as f32 * duration_secs) as usize;
    let mut samples = Vec::with_capacity(num_samples);
    let fade_samples = (TONE_SAMPLE_RATE as f32 * 0.01) as usize; // 10ms fade

    for i in 0..num_samples {
        let t = i as f32 / TONE_SAMPLE_RATE as f32;
        let mut sample = (2.0 * std::f32::consts::PI * freq_hz * t).sin() * volume;

        // Apply fade envelope.
        if i < fade_samples {
            sample *= i as f32 / fade_samples as f32;
        } else if i > num_samples - fade_samples {
            sample *= (num_samples - i) as f32 / fade_samples as f32;
        }

        samples.push(sample);
    }

    samples
}

/// Play f32 samples on the default output device (blocking until complete).
fn play_samples(samples: &[f32]) {
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            warn!("no default audio output device for feedback");
            return;
        }
    };

    let config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(TONE_SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Check if the device supports f32 output at our sample rate.
    let supports_f32 = device
        .supported_output_configs()
        .ok()
        .map(|configs| {
            configs.into_iter().any(|c| {
                c.channels() >= 1
                    && c.min_sample_rate().0 <= TONE_SAMPLE_RATE
                    && c.max_sample_rate().0 >= TONE_SAMPLE_RATE
                    && c.sample_format() == SampleFormat::F32
            })
        })
        .unwrap_or(false);

    if !supports_f32 {
        // Try anyway — cpal may do conversion.
    }

    let samples = samples.to_vec();
    let sample_idx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sample_idx_clone = std::sync::Arc::clone(&sample_idx);
    let samples_len = samples.len();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_clone = std::sync::Arc::clone(&done);

    let stream = match device.build_output_stream(
        &config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            for sample in data.iter_mut() {
                let idx = sample_idx_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if idx < samples_len {
                    *sample = samples[idx];
                } else {
                    *sample = 0.0;
                    done_clone.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        },
        |err| {
            warn!("audio feedback stream error: {err}");
        },
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!("failed to build audio feedback stream: {e}");
            return;
        }
    };

    if let Err(e) = stream.play() {
        warn!("failed to play audio feedback: {e}");
        return;
    }

    // Wait for playback to finish (with a timeout).
    let timeout = std::time::Duration::from_secs(2);
    let start = std::time::Instant::now();
    while !done.load(std::sync::atomic::Ordering::Acquire) {
        if start.elapsed() > timeout {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Small extra delay to let the last buffer drain.
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sweep_correct_length() {
        let samples = generate_sweep(800.0, 1200.0, 0.15, 0.5);
        let expected = (TONE_SAMPLE_RATE as f32 * 0.15) as usize;
        assert_eq!(samples.len(), expected);
    }

    #[test]
    fn generate_tone_correct_length() {
        let samples = generate_tone(1000.0, 0.1, 0.5);
        let expected = (TONE_SAMPLE_RATE as f32 * 0.1) as usize;
        assert_eq!(samples.len(), expected);
    }

    #[test]
    fn volume_scaling() {
        let loud = generate_tone(1000.0, 0.1, 1.0);
        let quiet = generate_tone(1000.0, 0.1, 0.25);
        // The peak of the quiet signal should be roughly 1/4 of the loud signal.
        let loud_peak = loud.iter().cloned().fold(0.0f32, |a, b| a.max(b.abs()));
        let quiet_peak = quiet.iter().cloned().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(quiet_peak < loud_peak);
        assert!((quiet_peak / loud_peak - 0.25).abs() < 0.05);
    }

    #[test]
    fn fade_envelope_no_click() {
        let samples = generate_tone(1000.0, 0.1, 0.5);
        // First and last samples should be near zero (fade in/out).
        assert!(samples[0].abs() < 0.01);
        assert!(samples[samples.len() - 1].abs() < 0.01);
    }

    #[test]
    fn sweep_fade_envelope() {
        let samples = generate_sweep(800.0, 1200.0, 0.15, 0.5);
        assert!(samples[0].abs() < 0.01);
        assert!(samples[samples.len() - 1].abs() < 0.01);
    }
}
