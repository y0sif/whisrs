//! Audio recovery: save unprocessed audio to disk when transcription fails.
//!
//! When a batch dictation's backend call fails, its PCM audio is saved as a
//! WAV file in `~/.cache/whisrs/recovery/` (the newest [`RECOVERY_KEEP`] are
//! kept), and `whisrs transcribe-recovery` transcribes one later.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use tracing::{info, warn};

use super::wav::{encode_wav, WAV_SPEC};

/// How many recovery files are kept: each save removes the oldest beyond it.
pub const RECOVERY_KEEP: usize = 10;

/// Return the recovery directory path (`~/.cache/whisrs/recovery/`).
pub fn recovery_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("whisrs")
        .join("recovery")
}

/// Whether `name` is a recovery file's name, as [`save_recovery_audio`]
/// writes it (`recovery_<time>.wav`). The cleanup removes and
/// `transcribe-recovery` reads only files named so.
pub fn is_recovery_file_name(name: &str) -> bool {
    name.starts_with("recovery_") && name.ends_with(".wav")
}

/// Save raw PCM samples as a timestamped WAV file in the recovery directory.
///
/// Returns the path to the saved file on success.
pub fn save_recovery_audio(samples: &[i16]) -> anyhow::Result<PathBuf> {
    save_recovery_audio_to(&recovery_dir(), samples)
}

fn save_recovery_audio_to(dir: &Path, samples: &[i16]) -> anyhow::Result<PathBuf> {
    if samples.is_empty() {
        anyhow::bail!("no audio samples to save for recovery");
    }

    std::fs::create_dir_all(dir)
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

/// The recovery files in `dir`, oldest first: by modification time, then by
/// name (which holds the save time, to the second). Only regular files with
/// a recovery name ([`is_recovery_file_name`]): never a symlink or a folder.
fn recovery_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)?.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_recovery_file_name(name) || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((modified, name.to_string(), entry.path()));
    }
    files.sort();
    Ok(files.into_iter().map(|(_, _, path)| path).collect())
}

/// Clean up old recovery files, keeping only the most recent `keep` files.
pub fn cleanup_old_recoveries(keep: usize) {
    cleanup_old_recoveries_in(&recovery_dir(), keep);
}

/// [`cleanup_old_recoveries`] in `dir`. Files that are no recovery file
/// ([`is_recovery_file_name`]) are left alone.
pub fn cleanup_old_recoveries_in(dir: &Path, keep: usize) {
    let files = match recovery_files(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => {
            warn!("failed to read recovery directory {}: {e}", dir.display());
            return;
        }
    };
    let excess = files.len().saturating_sub(keep);
    for path in files.into_iter().take(excess) {
        match std::fs::remove_file(&path) {
            Ok(()) => info!("removed old recovery file {}", path.display()),
            Err(e) => warn!("failed to remove old recovery file {}: {e}", path.display()),
        }
    }
}

/// The recovery file `whisrs transcribe-recovery` asked for: `None` is the
/// newest, a path must name a recovery file in the recovery directory.
pub fn resolve_recovery_file(arg: Option<&Path>) -> anyhow::Result<PathBuf> {
    resolve_recovery_file_in(&recovery_dir(), arg)
}

/// [`resolve_recovery_file`] in `dir`. A path is refused unless it resolves
/// (symlinks followed) to a regular file directly in `dir` with a recovery
/// name, so a file anywhere else is never read, let alone removed. Returns
/// the resolved path.
pub fn resolve_recovery_file_in(dir: &Path, arg: Option<&Path>) -> anyhow::Result<PathBuf> {
    let Some(path) = arg else {
        let files = match recovery_files(dir) {
            Ok(files) => files,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", dir.display())),
        };
        return files
            .into_iter()
            .next_back()
            .ok_or_else(|| anyhow!("no recovery audio in {}", dir.display()));
    };
    let refused = || {
        anyhow!(
            "not a recovery file: {} (recovery audio lives in {})",
            path.display(),
            dir.display()
        )
    };
    let file =
        std::fs::canonicalize(path).with_context(|| format!("cannot open {}", path.display()))?;
    let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
        return Err(refused());
    };
    let in_dir = file.parent() == Some(canonical_dir.as_path());
    let named = file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_recovery_file_name);
    let regular = std::fs::metadata(&file).is_ok_and(|m| m.is_file());
    if in_dir && named && regular {
        Ok(file)
    } else {
        Err(refused())
    }
}

/// Read a recovery file's samples. Refused unless it holds the format
/// [`encode_wav`] writes (16 kHz mono 16-bit PCM), the only one a recovery
/// file is saved in.
pub fn load_recovery_audio(path: &Path) -> anyhow::Result<Vec<i16>> {
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let spec = reader.spec();
    if spec != WAV_SPEC {
        bail!(
            "{} is not recovery audio: expected {}, found {}",
            path.display(),
            describe_spec(&WAV_SPEC),
            describe_spec(&spec)
        );
    }
    reader
        .into_samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read {}", path.display()))
}

/// A WAV format in words: "16000 Hz, 1 channel, 16-bit integer".
fn describe_spec(spec: &hound::WavSpec) -> String {
    let channels = match spec.channels {
        1 => "1 channel".to_string(),
        n => format!("{n} channels"),
    };
    let format = match spec.sample_format {
        hound::SampleFormat::Int => "integer",
        hound::SampleFormat::Float => "float",
    };
    format!(
        "{} Hz, {channels}, {}-bit {format}",
        spec.sample_rate, spec.bits_per_sample
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, SystemTime};

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
        let dir = tempfile::tempdir().unwrap();
        let samples: Vec<i16> = (0..1600).map(|i| (i % 256) as i16).collect();
        let path = save_recovery_audio_to(dir.path(), &samples).unwrap();

        assert!(path.exists());
        assert!(is_recovery_file_name(
            path.file_name().unwrap().to_str().unwrap()
        ));
        assert_eq!(load_recovery_audio(&path).unwrap(), samples);
    }

    /// A recovery file in `dir` named `name`, modified `age_secs` ago.
    fn recovery_file(dir: &Path, name: &str, age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, encode_wav(&[1, 2, 3]).unwrap()).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
        path
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn recovery_file_names() {
        assert!(is_recovery_file_name("recovery_2026-11-02T10-00-00.wav"));
        for other in [
            "recovery_2026-11-02T10-00-00.wav.part",
            "notes.wav",
            "recovery.wav",
            "Recovery_2026.wav",
            "recovery_2026.mp3",
        ] {
            assert!(!is_recovery_file_name(other), "{other}");
        }
    }

    /// The cleanup keeps the newest `keep` by modification time, whatever
    /// the names say, and never touches a file that is no recovery file.
    #[test]
    fn cleanup_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        recovery_file(dir.path(), "recovery_c.wav", 400);
        recovery_file(dir.path(), "recovery_a.wav", 300);
        recovery_file(dir.path(), "recovery_d.wav", 200);
        recovery_file(dir.path(), "recovery_b.wav", 100);
        std::fs::write(dir.path().join("notes.wav"), b"mine").unwrap();
        std::fs::create_dir(dir.path().join("recovery_dir.wav")).unwrap();

        cleanup_old_recoveries_in(dir.path(), 2);
        assert_eq!(
            names(dir.path()),
            [
                "notes.wav",
                "recovery_b.wav",
                "recovery_d.wav",
                "recovery_dir.wav"
            ]
        );

        // At or under the limit, nothing goes.
        cleanup_old_recoveries_in(dir.path(), 2);
        assert_eq!(names(dir.path()).len(), 4);
        cleanup_old_recoveries_in(dir.path(), 0);
        assert_eq!(names(dir.path()), ["notes.wav", "recovery_dir.wav"]);

        // A missing folder is nothing to clean.
        cleanup_old_recoveries_in(&dir.path().join("missing"), 0);
    }

    #[test]
    fn no_argument_resolves_to_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        recovery_file(dir.path(), "recovery_b.wav", 300);
        let newest = recovery_file(dir.path(), "recovery_a.wav", 100);
        recovery_file(dir.path(), "recovery_c.wav", 200);
        // A newer file that is no recovery file is passed over.
        std::fs::write(dir.path().join("notes.wav"), b"mine").unwrap();
        assert_eq!(resolve_recovery_file_in(dir.path(), None).unwrap(), newest);

        // The same time: the later name (the later save) wins.
        let dir = tempfile::tempdir().unwrap();
        recovery_file(dir.path(), "recovery_2026-11-02T10-00-00.wav", 100);
        let later = recovery_file(dir.path(), "recovery_2026-11-02T10-00-01.wav", 100);
        assert_eq!(resolve_recovery_file_in(dir.path(), None).unwrap(), later);
    }

    #[test]
    fn no_recovery_audio_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.wav"), b"mine").unwrap();
        let err = resolve_recovery_file_in(dir.path(), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("no recovery audio in {}", dir.path().display())
        );
        let missing = dir.path().join("missing");
        let err = resolve_recovery_file_in(&missing, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("no recovery audio in {}", missing.display())
        );
    }

    #[test]
    fn a_named_file_in_the_folder_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = recovery_file(dir.path(), "recovery_a.wav", 100);
        recovery_file(dir.path(), "recovery_b.wav", 10);
        let resolved = resolve_recovery_file_in(dir.path(), Some(&path)).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&path).unwrap());
    }

    /// Only a recovery file directly in the folder: a file elsewhere, one
    /// named otherwise, one in a subfolder or a symlink out of the folder
    /// is refused, and left as it was.
    #[test]
    fn anything_else_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("recovery");
        std::fs::create_dir(&dir).unwrap();
        let outside = recovery_file(root.path(), "recovery_x.wav", 100);
        let notes = dir.join("notes.wav");
        std::fs::write(&notes, encode_wav(&[1]).unwrap()).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let nested = recovery_file(&dir.join("sub"), "recovery_y.wav", 100);
        let link = dir.join("recovery_link.wav");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        for path in [&outside, &notes, &nested, &link] {
            let err = resolve_recovery_file_in(&dir, Some(path)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "not a recovery file: {} (recovery audio lives in {})",
                    path.display(),
                    dir.display()
                )
            );
        }
        // The symlink is no candidate for the newest either.
        assert!(resolve_recovery_file_in(&dir, None).is_err());
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            encode_wav(&[1, 2, 3]).unwrap()
        );

        let err = resolve_recovery_file_in(&dir, Some(&dir.join("recovery_gone.wav"))).unwrap_err();
        assert!(err.to_string().starts_with("cannot open "), "{err}");
    }

    #[test]
    fn audio_in_another_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery_a.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            ..WAV_SPEC
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        let err = load_recovery_audio(&path).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} is not recovery audio: expected 16000 Hz, 1 channel, 16-bit integer, found \
                 44100 Hz, 2 channels, 16-bit integer",
                path.display()
            )
        );

        let spec = hound::WavSpec {
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
            ..WAV_SPEC
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0.0f32).unwrap();
        writer.finalize().unwrap();
        let err = load_recovery_audio(&path).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("found 16000 Hz, 1 channel, 32-bit float"),
            "{err}"
        );

        std::fs::write(&path, b"not a wav").unwrap();
        let err = load_recovery_audio(&path).unwrap_err();
        assert!(err.to_string().starts_with("failed to read "), "{err}");
    }
}
