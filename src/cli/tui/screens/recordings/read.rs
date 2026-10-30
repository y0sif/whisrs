//! The recordings folder as the Recordings screen lists it, read on a
//! worker: headers only for the list (no transcript in memory), each parsed
//! again only when its `recording.json` changed, and one recording whole
//! for its preview and transcript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use whisrs::recording::render::render_text;
use whisrs::recording::store::{self, RecordingHeader};
use whisrs::recording::{RecordingPhase, RecordingState, RecordingStatus};

use super::{date_label, Detail, Item, Phase, Voice};

/// A header parsed at one read, and the `recording.json` it came from: its
/// modification time and length. The next read parses the file again only
/// when either changed.
pub struct Kept {
    mtime: Option<SystemTime>,
    len: u64,
    header: RecordingHeader,
}

/// The recordings folder `[recording] dir` in config.toml at `config`
/// names, as `whisrs recording list` finds it, else the default one under
/// the data folder `data`.
pub fn root(config: &Path, data: &Path) -> PathBuf {
    let dir = std::fs::read_to_string(config)
        .ok()
        .and_then(|text| store::recording_dir_from_toml(&text));
    match dir.as_deref().map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => store::recordings_dir_from(Some(dir)),
        None => data.join("whisrs").join("recordings"),
    }
}

/// Every recording under `root`, newest first, at `now`. `parsed` holds
/// the headers the read before parsed, by folder: one whose file did not
/// change is taken from there, and one whose folder is gone is dropped.
/// `status` is the daemon's word on what it queues and processes, asked
/// only when a recording may be one of them (recording, processing or
/// waiting for the network on disk, or a summary pending).
pub fn folder(
    root: &Path,
    now: DateTime<Local>,
    status: impl FnOnce() -> Option<RecordingStatus>,
    parsed: &mut HashMap<PathBuf, Kept>,
) -> Result<Vec<Item>, String> {
    let mut kept = HashMap::new();
    let headers = store::list_headers_by(root, |dir| {
        let file = std::fs::metadata(store::meta_path(dir))?;
        let (mtime, len) = (file.modified().ok(), file.len());
        let header = match parsed.remove(dir) {
            Some(before)
                if before.mtime.is_some() && before.mtime == mtime && before.len == len =>
            {
                before.header
            }
            _ => store::read_header(dir)?,
        };
        kept.insert(
            dir.to_path_buf(),
            Kept {
                mtime,
                len,
                header: header.clone(),
            },
        );
        Ok(header)
    })
    .map_err(|e| format!("{e:#}"))?;
    *parsed = kept;
    let in_flight = headers.iter().any(|(_, header)| {
        matches!(
            header.status,
            RecordingState::Recording | RecordingState::Processing | RecordingState::Waiting
        ) || header.summary_pending.is_some()
    });
    let status = if in_flight { status() } else { None };
    Ok(headers
        .iter()
        .map(|(dir, header)| {
            let stamp = parsed.get(dir).and_then(|kept| kept.mtime);
            item(dir, header, stamp, status.as_ref(), now)
        })
        .collect())
}

/// One row: `header` and what is on disk beside it, with the daemon's
/// `status` (when it was asked) saying what is queued or running now.
fn item(
    dir: &Path,
    header: &RecordingHeader,
    stamp: Option<SystemTime>,
    status: Option<&RecordingStatus>,
    now: DateTime<Local>,
) -> Item {
    let id = header.id.as_str();
    // `status.id` is the recording captured or processed, or with neither
    // the first queued one: the queue is what tells a queued one apart.
    let shown = status.filter(|s| s.id.as_deref() == Some(id));
    let queued = status.is_some_and(|s| s.queued.iter().any(|q| q == id));
    let live = shown.is_some_and(|s| s.phase == RecordingPhase::Recording);
    let running = !queued && shown.is_some_and(|s| s.phase == RecordingPhase::Processing);
    // A recording waiting for the network that the daemon tries again now
    // is queued or processing, not waiting.
    let phase = match header.status {
        RecordingState::Done => Phase::Done,
        _ if queued => Phase::Queued,
        _ if running => Phase::Processing,
        RecordingState::Recording => Phase::Recording,
        RecordingState::Processing => Phase::Processing,
        RecordingState::Failed => Phase::Failed,
        RecordingState::Waiting => Phase::Waiting,
    };
    let secs = match header.ended_at {
        Some(ended) => Some((ended - header.started_at).num_seconds().max(0)),
        None if live => Some((now - header.started_at).num_seconds().max(0)),
        None => None,
    };
    Item {
        id: header.id.clone(),
        dir: dir.to_path_buf(),
        title: header.title.clone(),
        date: date_label(header.started_at, now),
        secs,
        phase,
        speakers: header
            .speakers
            .iter()
            .map(|s| Voice {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect(),
        heard: header.heard_names().len(),
        summary: header
            .summary
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        stale: header.summary_stale,
        summary_queued: header.summary_pending.is_some(),
        summarizing: header.status == RecordingState::Done
            && running
            && shown.is_some_and(|s| s.summarizing),
        audio: !header.audio_deleted && header.tracks.iter().any(|t| dir.join(&t.file).exists()),
        transcript: store::transcript_path(dir, id).exists(),
        stamp,
    }
}

/// The recording in `dir` read whole: its summary and its transcript as
/// `whisrs recording show -f txt` prints it.
pub fn detail(dir: &Path) -> Result<Detail, String> {
    let meta = store::read_meta(dir).map_err(|e| format!("{e:#}"))?;
    Ok(Detail {
        backend: meta.backend.clone(),
        summary: meta.summary.clone().filter(|s| !s.trim().is_empty()),
        stale: meta.summary_stale,
        lines: render_text(&meta).lines().map(str::to_string).collect(),
        error: meta.error.clone(),
    })
}
