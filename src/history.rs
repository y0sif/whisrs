//! Transcription history — append-only JSONL storage.
//!
//! Each successful transcription is saved as a single JSON line in
//! `$XDG_DATA_HOME/whisrs/history.jsonl` (typically `~/.local/share/whisrs/history.jsonl`).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    append_entry_to(&history_path(), entry)
}

/// Append a single entry to a specific history file.
fn append_entry_to(path: &Path, entry: &HistoryEntry) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    let line = serde_json::to_string(entry)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Read the most recent `limit` entries from the history file.
///
/// Returns entries in reverse-chronological order (newest first).
pub fn read_entries(limit: usize) -> anyhow::Result<Vec<HistoryEntry>> {
    read_entries_from(&history_path(), limit)
}

/// Read the most recent `limit` entries from a specific history file.
pub fn read_entries_from(path: &Path, limit: usize) -> anyhow::Result<Vec<HistoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = fs::File::open(path)?;
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
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Create a unique temp history file path for each test.
    fn temp_history_path() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("whisrs-history-test-{}-{id}", std::process::id()))
            .join("history.jsonl")
    }

    fn make_entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now(),
            text: text.to_string(),
            backend: "groq".to_string(),
            language: "en".to_string(),
            duration_secs: 1.0,
        }
    }

    #[test]
    fn append_and_read_entries() {
        let path = temp_history_path();
        let entry = make_entry("hello world");

        append_entry_to(&path, &entry).unwrap();
        append_entry_to(&path, &entry).unwrap();

        let entries = read_entries_from(&path, 10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello world");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_entries_respects_limit() {
        let path = temp_history_path();

        for i in 0..5 {
            append_entry_to(&path, &make_entry(&format!("entry {i}"))).unwrap();
        }

        let entries = read_entries_from(&path, 3).unwrap();
        assert_eq!(entries.len(), 3);
        // Newest first.
        assert_eq!(entries[0].text, "entry 4");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_empty_history() {
        let path = temp_history_path();
        let entries = read_entries_from(&path, 10).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn clear_history_removes_file() {
        let path = temp_history_path();
        append_entry_to(&path, &make_entry("test")).unwrap();
        assert!(path.exists());

        fs::remove_file(&path).unwrap();
        assert!(!path.exists());

        let entries = read_entries_from(&path, 10).unwrap();
        assert!(entries.is_empty());
    }
}
