//! Audio recovery: save unprocessed audio to disk when transcription fails.
//!
//! On mid-stream failure, raw PCM audio is saved as WAV files in
//! `~/.cache/whisrs/recovery/` so users can retry transcription manually.

use std::path::PathBuf;

use anyhow::Context;
use tracing::{info, warn};

use super::capture::encode_wav;

/// Return the recovery directory path (`~/.cache/whisrs/recovery/`).
pub fn recovery_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("whisrs")
        .join("recovery")
}

/// Save raw PCM samples as a timestamped WAV file in the recovery directory.
///
/// Returns the path to the saved file on success.
pub fn save_recovery_audio(samples: &[i16]) -> anyhow::Result<PathBuf> {
    if samples.is_empty() {
        anyhow::bail!("no audio samples to save for recovery");
    }

    let dir = recovery_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create recovery directory: {}", dir.display()))?;

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S");
    let filename = format!("recovery_{timestamp}.wav");
    let path = dir.join(&filename);

    let wav_data = encode_wav(samples).context("failed to encode recovery audio as WAV")?;
    std::fs::write(&path, &wav_data)
        .with_context(|| format!("failed to write recovery file: {}", path.display()))?;

    info!(
        "saved recovery audio to {} ({} bytes)",
        path.display(),
        wav_data.len()
    );

    Ok(path)
}

/// Clean up old recovery files, keeping only the most recent `keep` files.
pub fn cleanup_old_recoveries(keep: usize) {
    let dir = recovery_dir();
    if !dir.exists() {
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(iter) => iter
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("recovery_")
                    && e.file_name().to_string_lossy().ends_with(".wav")
            })
            .collect(),
        Err(e) => {
            warn!("failed to read recovery directory: {e}");
            return;
        }
    };

    if entries.len() <= keep {
        return;
    }

    // Sort by modification time (oldest first).
    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });

    let to_remove = entries.len() - keep;
    for entry in entries.into_iter().take(to_remove) {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            warn!("failed to remove old recovery file: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_dir_is_under_cache() {
        let dir = recovery_dir();
        let dir_str = dir.to_string_lossy();
        assert!(
            dir_str.contains("whisrs") && dir_str.contains("recovery"),
            "unexpected recovery dir: {dir_str}"
        );
    }

    #[test]
    fn save_recovery_rejects_empty_samples() {
        let err = save_recovery_audio(&[]).unwrap_err();
        assert!(err.to_string().contains("no audio samples"));
    }

    #[test]
    fn save_and_read_recovery_audio() {
        let samples: Vec<i16> = (0..1600).map(|i| (i % 256) as i16).collect();
        let path = save_recovery_audio(&samples).unwrap();

        // Verify the file exists and is a valid WAV.
        assert!(path.exists());
        let cursor = std::io::Cursor::new(std::fs::read(&path).unwrap());
        let reader = hound::WavReader::new(cursor).unwrap();
        let read_samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_samples, samples);

        // Clean up.
        std::fs::remove_file(&path).ok();
    }
}
