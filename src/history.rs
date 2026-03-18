//! Transcription history — append-only JSONL storage.
//!
//! Each successful transcription is saved as a single JSON line in
//! `$XDG_DATA_HOME/whisrs/history.jsonl` (typically `~/.local/share/whisrs/history.jsonl`).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// A single transcription history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// When the transcription completed.
    pub timestamp: DateTime<Local>,
    /// The transcribed text.
    pub text: String,
    /// Which backend produced the transcription (e.g. "groq", "openai-realtime").
    pub backend: String,
    /// Language code used (e.g. "en", "auto").
    pub language: String,
    /// Duration of the recording in seconds (approximate).
    #[serde(default)]
    pub duration_secs: f64,
}

/// Return the path to the history file.
pub fn history_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("whisrs")
        .join("history.jsonl")
}

/// Append a single entry to the history file.
pub fn append_entry(entry: &HistoryEntry) -> anyhow::Result<()> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

    let line = serde_json::to_string(entry)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Read the most recent `limit` entries from the history file.
///
/// Returns entries in reverse-chronological order (newest first).
pub fn read_entries(limit: usize) -> anyhow::Result<Vec<HistoryEntry>> {
    let path = history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);

    let mut entries: Vec<HistoryEntry> = reader
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            if line.trim().is_empty() {
                return None;
            }
            match serde_json::from_str(&line) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    warn!("skipping malformed history entry: {e}");
                    None
                }
            }
        })
        .collect();

    // Newest first.
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}

/// Clear all history entries.
pub fn clear_history() -> anyhow::Result<()> {
    let path = history_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Run tests with an isolated history file.
    fn with_temp_history<F: FnOnce()>(f: F) {
        let dir = env::temp_dir().join(format!("whisrs-history-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        env::set_var("XDG_DATA_HOME", &dir);
        f();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_and_read_entries() {
        with_temp_history(|| {
            let entry = HistoryEntry {
                timestamp: Local::now(),
                text: "hello world".to_string(),
                backend: "groq".to_string(),
                language: "en".to_string(),
                duration_secs: 2.5,
            };

            append_entry(&entry).unwrap();
            append_entry(&entry).unwrap();

            let entries = read_entries(10).unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].text, "hello world");
        });
    }

    #[test]
    fn read_entries_respects_limit() {
        with_temp_history(|| {
            for i in 0..5 {
                let entry = HistoryEntry {
                    timestamp: Local::now(),
                    text: format!("entry {i}"),
                    backend: "groq".to_string(),
                    language: "en".to_string(),
                    duration_secs: 1.0,
                };
                append_entry(&entry).unwrap();
            }

            let entries = read_entries(3).unwrap();
            assert_eq!(entries.len(), 3);
            // Newest first.
            assert_eq!(entries[0].text, "entry 4");
        });
    }

    #[test]
    fn read_empty_history() {
        with_temp_history(|| {
            let entries = read_entries(10).unwrap();
            assert!(entries.is_empty());
        });
    }

    #[test]
    fn clear_history_removes_file() {
        with_temp_history(|| {
            let entry = HistoryEntry {
                timestamp: Local::now(),
                text: "test".to_string(),
                backend: "groq".to_string(),
                language: "en".to_string(),
                duration_secs: 1.0,
            };
            append_entry(&entry).unwrap();
            assert!(history_path().exists());

            clear_history().unwrap();
            assert!(!history_path().exists());

            let entries = read_entries(10).unwrap();
            assert!(entries.is_empty());
        });
    }
}
