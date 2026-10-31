//! On-disk recording store: one directory per recording under the recordings root.
//!
//! Layout: `<root>/<id>/recording.json`, `<root>/<id>/audio/{mic,system}.wav`
//! and `<root>/<id>/<id>.md`. Directories are 0700 and `recording.json` is
//! 0600, since both hold private conversation content.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use chrono::{DateTime, Local};
use tracing::{debug, warn};

use serde::Deserialize;

use super::{
    RecordingMeta, RecordingState, Speaker, SummaryRequest, TitleSource, TrackKind, TrackMeta,
};

const META_FILE: &str = "recording.json";
const META_TMP: &str = ".recording.json.tmp";
const AUDIO_DIR: &str = "audio";

/// Per-chunk results of finished requests, under the recording folder, so a
/// retry or a recovery after a crash resumes instead of starting over.
/// Dropped once the transcript is written, except the voices a diarization
/// sidecar found (`diarization-*.json`), which a later retry reuses; a failed
/// recording keeps all of it until its audio is deleted, since a retry needs
/// both.
pub const CACHE_DIR: &str = ".cache";
const SLUG_MAX: usize = 40;

/// The default recordings root, ignoring any `[recording] dir` override.
pub fn recordings_dir() -> PathBuf {
    recordings_dir_from(None)
}

/// The recordings root: `[recording] dir` (with a leading `~/` expanded) or
/// `$XDG_DATA_HOME/whisrs/recordings`.
pub fn recordings_dir_from(cfg_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = cfg_dir.map(str::trim).filter(|d| !d.is_empty()) {
        return expand_tilde(dir);
    }
    dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("whisrs")
        .join("recordings")
}

/// The recordings root `config.toml` gives ([`recordings_dir_from`]),
/// reading only `[recording] dir`, so a config the daemon would refuse for
/// an unrelated reason still finds the recordings. For the readers that
/// work without the daemon: `whisrs recording list/show/path` and
/// `whisrs mcp`.
pub fn configured_recordings_dir() -> PathBuf {
    let dir = fs::read_to_string(crate::config_path())
        .ok()
        .and_then(|text| recording_dir_from_toml(&text));
    recordings_dir_from(dir.as_deref())
}

/// `[recording] dir` from config.toml text, if set.
pub fn recording_dir_from_toml(text: &str) -> Option<String> {
    let table: toml::Table = text.parse().ok()?;
    table
        .get("recording")?
        .get("dir")?
        .as_str()
        .map(str::to_string)
}

fn expand_tilde(dir: &str) -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        if dir == "~" {
            return home;
        }
        if let Some(rest) = dir.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(dir)
}

/// Turn a title into an id suffix: lowercase ASCII alphanumerics joined by
/// single dashes, at most [`SLUG_MAX`] chars. Empty if nothing survives.
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(SLUG_MAX);
    slug.trim_end_matches('-').to_string()
}

fn base_id(started: DateTime<Local>, title: Option<&str>) -> String {
    let stamp = started.format("%Y-%m-%d_%H%M").to_string();
    match title.map(slugify).filter(|s| !s.is_empty()) {
        Some(slug) => format!("{stamp}_{slug}"),
        None => stamp,
    }
}

fn private_dir_builder() -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// Create a new recording directory (with `audio/`) and return its id and path.
///
/// The id is `YYYY-MM-DD_HHMM`, plus `_<slug>` when a title is given; a
/// collision appends `-2`, `-3`, ... The directory is claimed with
/// `create_dir`, so two concurrent callers never get the same one. The root
/// is created 0700 if missing; an existing root keeps its permissions.
pub fn create(
    root: &Path,
    started: DateTime<Local>,
    title: Option<&str>,
) -> anyhow::Result<(String, PathBuf)> {
    if !root.exists() {
        private_dir_builder()
            .recursive(true)
            .create(root)
            .with_context(|| format!("failed to create recordings dir {}", root.display()))?;
        // `mode` is masked by the umask; set it explicitly.
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }

    let base = base_id(started, title);
    for n in 1u32.. {
        let id = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let dir = root.join(&id);
        match private_dir_builder().create(&dir) {
            Ok(()) => {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                let audio = dir.join(AUDIO_DIR);
                private_dir_builder()
                    .create(&audio)
                    .with_context(|| format!("failed to create {}", audio.display()))?;
                fs::set_permissions(&audio, fs::Permissions::from_mode(0o700))?;
                return Ok((id, dir));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to create {}", dir.display())),
        }
    }
    unreachable!("u32 range exhausted while picking a recording id")
}

/// Read `recording.json` from a recording directory.
pub fn read_meta(dir: &Path) -> anyhow::Result<RecordingMeta> {
    let path = dir.join(META_FILE);
    let data = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("failed to parse {}", path.display()))
}

/// Write `recording.json` atomically (temp file in the same dir, fsync,
/// rename), mode 0600.
pub fn write_meta(dir: &Path, meta: &RecordingMeta) -> anyhow::Result<()> {
    let tmp = dir.join(META_TMP);
    let path = dir.join(META_FILE);
    let json = serde_json::to_vec_pretty(meta)?;
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("failed to open {}", tmp.display()))?;
        // A stale tmp from a crash may have other permissions.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename {} into place", tmp.display()))?;
    // Persist the rename itself; failure here is not worth failing the write.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// The fields of `recording.json` a listing needs, read without the words
/// of the transcript ([`read_header`]): serde skips what the struct leaves
/// out, so a six-hour transcript costs a scan, not its text in memory.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RecordingHeader {
    /// Required as [`read_meta`] requires it, like `sample_rate`.
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_source: Option<TitleSource>,
    pub status: RecordingState,
    pub started_at: DateTime<Local>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Local>>,
    pub sample_rate: u32,
    #[serde(default)]
    pub tracks: Vec<TrackMeta>,
    #[serde(default)]
    pub speakers: Vec<Speaker>,
    /// Who speaks in each segment, without the words.
    #[serde(default, rename = "segments")]
    pub segment_speakers: Vec<SegmentSpeaker>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_stale: bool,
    #[serde(default)]
    pub summary_pending: Option<SummaryRequest>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub audio_deleted: bool,
}

/// The speaker of one segment, as a [`RecordingHeader`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SegmentSpeaker {
    pub speaker: String,
}

impl RecordingHeader {
    /// The names of the speakers who have a line in the transcript, in the
    /// order of `speakers`, as the Markdown front matter lists them.
    pub fn heard_names(&self) -> Vec<String> {
        let heard: std::collections::BTreeSet<&str> = self
            .segment_speakers
            .iter()
            .map(|s| s.speaker.as_str())
            .collect();
        self.speakers
            .iter()
            .filter(|s| heard.contains(s.id.as_str()))
            .map(|s| s.name.clone())
            .collect()
    }
}

/// Read the [`RecordingHeader`] of the recording in `dir`.
pub fn read_header(dir: &Path) -> anyhow::Result<RecordingHeader> {
    let path = dir.join(META_FILE);
    let data = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("failed to parse {}", path.display()))
}

/// All recordings under `root`, newest first by `started_at`. Directories
/// without a readable `recording.json` are skipped. A missing root is empty.
pub fn list(root: &Path) -> anyhow::Result<Vec<(PathBuf, RecordingMeta)>> {
    list_by(root, read_meta, |meta| (meta.started_at, meta.id.as_str()))
}

/// [`list`] by headers ([`read_header`]): what a listing or a lookup reads,
/// without holding every transcript in memory at once.
pub fn list_headers(root: &Path) -> anyhow::Result<Vec<(PathBuf, RecordingHeader)>> {
    list_headers_by(root, read_header)
}

/// [`list_headers`] with each header read by `read`, which may keep the
/// ones it read before: the TUI reads the folder every 2 s and parses
/// again only a `recording.json` that changed.
pub fn list_headers_by(
    root: &Path,
    read: impl FnMut(&Path) -> anyhow::Result<RecordingHeader>,
) -> anyhow::Result<Vec<(PathBuf, RecordingHeader)>> {
    list_by(root, read, |header| (header.started_at, header.id.as_str()))
}

/// The recordings under `root` as `read` gives them, newest first by the
/// start time and then the id `key` gives.
fn list_by<T>(
    root: &Path,
    mut read: impl FnMut(&Path) -> anyhow::Result<T>,
    key: impl Fn(&T) -> (DateTime<Local>, &str),
) -> anyhow::Result<Vec<(PathBuf, T)>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", root.display())),
    };
    let mut recordings = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        match read(&dir) {
            Ok(item) => recordings.push((dir, item)),
            Err(e) => debug!("skipping {}: {e:#}", dir.display()),
        }
    }
    recordings.sort_by(|(_, a), (_, b)| {
        let (a_start, a_id) = key(a);
        let (b_start, b_id) = key(b);
        b_start.cmp(&a_start).then_with(|| b_id.cmp(a_id))
    });
    Ok(recordings)
}

/// Whether `query` can only name a folder right under the root: not empty,
/// and no `/`, `.` or `..` to name the root or climb out of it. No id has a
/// dot.
fn is_plain_folder_name(query: &str) -> bool {
    !query.is_empty() && !query.contains('/') && !query.contains('.') && !query.contains('\0')
}

/// Find a recording by `"last"` (the newest), an exact id, or a unique id
/// prefix ([`resolve_meta`]).
pub fn resolve(root: &Path, query: &str) -> anyhow::Result<PathBuf> {
    resolve_meta(root, query).map(|(dir, _)| dir)
}

/// [`resolve`], with the recording read whole. An exact id that is also
/// the name of a real folder right under the root (not a symlink) is read
/// directly; anything else is found by the headers ([`list_headers`]),
/// and only the candidates are read whole: `last` is the newest recording
/// that reads, a prefix must match exactly one that reads, and an exact id
/// whose `recording.json` does not read is that error.
pub fn resolve_meta(root: &Path, query: &str) -> anyhow::Result<(PathBuf, RecordingMeta)> {
    let query = query.trim();
    if query != "last" && is_plain_folder_name(query) {
        let dir = root.join(query);
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            if let Ok(meta) = read_meta(&dir) {
                if meta.id == query {
                    return Ok((dir, meta));
                }
            }
        }
    }
    let recordings = list_headers(root)?;
    if recordings.is_empty() {
        bail!("no recordings found in {}", root.display());
    }
    if query == "last" {
        return recordings
            .iter()
            .find_map(|(dir, _)| read_meta(dir).ok().map(|meta| (dir.clone(), meta)))
            .with_context(|| format!("no readable recording in {}", root.display()));
    }
    if let Some((dir, _)) = recordings.iter().find(|(_, h)| h.id == query) {
        return read_meta(dir).map(|meta| (dir.clone(), meta));
    }
    let matches: Vec<&(PathBuf, RecordingHeader)> = if query.is_empty() {
        Vec::new()
    } else {
        recordings
            .iter()
            .filter(|(_, h)| h.id.starts_with(query))
            .collect()
    };
    // Only the ones that read count; the first one read is kept, the
    // others are dropped as soon as they are read.
    let mut found = None;
    let mut ids: Vec<&str> = Vec::new();
    let mut unreadable = 0usize;
    for (dir, header) in &matches {
        match read_meta(dir) {
            Ok(meta) => {
                ids.push(header.id.as_str());
                found.get_or_insert((dir.clone(), meta));
            }
            Err(_) => unreadable += 1,
        }
    }
    match (found, ids.len()) {
        (Some(found), 1) => Ok(found),
        (None, _) => bail!("no recording matches '{query}'"),
        (Some(_), count) => {
            let more = match unreadable {
                0 => String::new(),
                n => format!(" ({n} more could not be read)"),
            };
            bail!(
                "'{query}' matches {count} recordings: {}{more}",
                ids.join(", ")
            )
        }
    }
}

/// Path of `recording.json` in a recording's folder.
pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join(META_FILE)
}

/// Path of the rendered transcript: `<dir>/<id>.md`.
pub fn transcript_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.md"))
}

/// Path of a track's WAV file: `<dir>/audio/{mic,system}.wav`.
pub fn audio_path(dir: &Path, kind: TrackKind) -> PathBuf {
    dir.join(AUDIO_DIR)
        .join(format!("{}.wav", kind.file_stem()))
}

/// Bytes available to an unprivileged user on the filesystem holding
/// `path`, or its nearest existing ancestor (the recordings root may not
/// exist yet).
pub fn free_space(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or_else(|| Path::new("/"));
    let c_path = std::ffi::CString::new(existing.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` a valid
    // out-pointer for the duration of the call.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Both fields are 64-bit on Linux but 32-bit on some 32-bit targets.
    #[allow(clippy::useless_conversion)]
    let (blocks, size) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
    Ok(blocks.saturating_mul(size))
}

/// Recordings left mid-capture or mid-processing (e.g. by a crash).
pub fn interrupted(root: &Path) -> Vec<PathBuf> {
    match list(root) {
        Ok(recordings) => recordings
            .into_iter()
            .filter(|(_, m)| {
                matches!(
                    m.status,
                    RecordingState::Recording | RecordingState::Processing
                )
            })
            .map(|(dir, _)| dir)
            .collect(),
        Err(e) => {
            warn!("failed to scan recordings for interrupted ones: {e:#}");
            Vec::new()
        }
    }
}

/// Recordings waiting for the network, oldest first.
pub fn waiting(root: &Path) -> Vec<(PathBuf, RecordingMeta)> {
    match list(root) {
        Ok(recordings) => recordings
            .into_iter()
            .rev()
            .filter(|(_, m)| m.status == RecordingState::Waiting)
            .collect(),
        Err(e) => {
            warn!("failed to scan recordings for ones waiting for the network: {e:#}");
            Vec::new()
        }
    }
}

/// Delete the `audio/` dir of `Done` recordings that ended at least `days`
/// days before `now` (by `ended_at`, else `started_at`), mark them
/// `audio_deleted`, and return their dirs. `0` sweeps every `Done` recording;
/// a negative value keeps audio forever. `Failed`, `Waiting`, `Recording` and
/// `Processing` recordings are never touched, so their audio stays retryable.
pub fn sweep_retention(root: &Path, days: i64, now: DateTime<Local>) -> Vec<PathBuf> {
    sweep_retention_guarded(root, days, now, |_| Some(()))
}

/// [`sweep_retention`], taking `guard(id)` before a recording is touched and
/// holding it until its `recording.json` is written. A recording `guard`
/// refuses (another writer has it, a rename) is skipped until the next
/// sweep, and the meta is read again under the guard, so what that writer
/// wrote is kept.
pub fn sweep_retention_guarded<G>(
    root: &Path,
    days: i64,
    now: DateTime<Local>,
    mut guard: impl FnMut(&str) -> Option<G>,
) -> Vec<PathBuf> {
    if days < 0 {
        return Vec::new();
    }
    let recordings = match list(root) {
        Ok(recordings) => recordings,
        Err(e) => {
            warn!("failed to scan recordings for audio retention: {e:#}");
            return Vec::new();
        }
    };
    let max_age = chrono::Duration::days(days);
    let mut swept = Vec::new();
    for (dir, listed) in recordings {
        let due = |meta: &RecordingMeta| {
            let reference = meta.ended_at.unwrap_or(meta.started_at);
            meta.status == RecordingState::Done
                && !meta.audio_deleted
                && now.signed_duration_since(reference) >= max_age
        };
        if !due(&listed) {
            continue;
        }
        let Some(_held) = guard(&listed.id) else {
            debug!("recording {} is busy; its audio is swept later", listed.id);
            continue;
        };
        let mut meta = match read_meta(&dir) {
            Ok(meta) if due(&meta) => meta,
            Ok(_) => continue,
            // Deleted since the listing (`whisrs recording delete`).
            Err(_) if !dir.exists() => {
                debug!("{} is gone; nothing to sweep", dir.display());
                continue;
            }
            Err(e) => {
                warn!(
                    "failed to read {} for audio retention: {e:#}",
                    dir.display()
                );
                continue;
            }
        };
        let audio = dir.join(AUDIO_DIR);
        match fs::remove_dir_all(&audio) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                warn!("failed to delete {}: {e}", audio.display());
                continue;
            }
        }
        // What a done recording keeps there: a diarization sidecar's voices,
        // which go with the audio.
        let cache = dir.join(CACHE_DIR);
        match fs::remove_dir_all(&cache) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => warn!("failed to delete {}: {e}", cache.display()),
        }
        meta.audio_deleted = true;
        if let Err(e) = write_meta(&dir, &meta) {
            warn!("failed to mark audio deleted in {}: {e:#}", dir.display());
            continue;
        }
        swept.push(dir);
    }
    swept
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Create a recording with a meta of the given status and times.
    fn seed(
        root: &Path,
        started: DateTime<Local>,
        title: Option<&str>,
        status: RecordingState,
        ended: Option<DateTime<Local>>,
    ) -> PathBuf {
        let (id, dir) = create(root, started, title).unwrap();
        let mut meta = RecordingMeta::new(id, title.map(String::from), started);
        meta.status = status;
        meta.ended_at = ended;
        write_meta(&dir, &meta).unwrap();
        fs::write(audio_path(&dir, TrackKind::Mic), b"wav").unwrap();
        dir
    }

    #[test]
    fn slug_rules() {
        assert_eq!(
            slugify("Weekly Sync: Q4 / Planning!"),
            "weekly-sync-q4-planning"
        );
        assert_eq!(slugify("  --  "), "");
        assert_eq!(slugify("Café déjà"), "caf-d-j");
        let long = slugify(&"ab ".repeat(40));
        assert!(long.len() <= SLUG_MAX && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn create_ids_and_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("recordings");
        let t = at(2026, 10, 2, 14, 5);

        let (id1, dir1) = create(&root, t, None).unwrap();
        assert_eq!(id1, "2026-10-02_1405");
        let (id2, _) = create(&root, t, None).unwrap();
        assert_eq!(id2, "2026-10-02_1405-2");
        let (id3, _) = create(&root, t, None).unwrap();
        assert_eq!(id3, "2026-10-02_1405-3");
        let (id4, _) = create(&root, t, Some("Team Standup")).unwrap();
        assert_eq!(id4, "2026-10-02_1405_team-standup");
        let (id5, _) = create(&root, t, Some("team standup")).unwrap();
        assert_eq!(id5, "2026-10-02_1405_team-standup-2");
        let (id6, _) = create(&root, t, Some("!!!")).unwrap();
        assert_eq!(id6, "2026-10-02_1405-4");

        assert!(dir1.join("audio").is_dir());
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&dir1), 0o700);
        assert_eq!(mode(&dir1.join("audio")), 0o700);
    }

    #[test]
    fn meta_write_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, dir) = create(tmp.path(), at(2026, 1, 1, 9, 0), Some("x")).unwrap();
        let mut meta = RecordingMeta::new(id, Some("x".into()), at(2026, 1, 1, 9, 0));
        write_meta(&dir, &meta).unwrap();
        assert_eq!(read_meta(&dir).unwrap(), meta);
        assert_eq!(mode(&dir.join(META_FILE)), 0o600);
        assert!(!dir.join(META_TMP).exists());

        meta.status = RecordingState::Done;
        meta.errors.push("system audio unavailable".into());
        write_meta(&dir, &meta).unwrap();
        assert_eq!(read_meta(&dir).unwrap(), meta);
    }

    #[test]
    fn list_is_newest_first_and_skips_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = seed(root, at(2026, 1, 1, 9, 0), None, RecordingState::Done, None);
        let new = seed(root, at(2026, 3, 1, 9, 0), None, RecordingState::Done, None);
        let mid = seed(root, at(2026, 2, 1, 9, 0), None, RecordingState::Done, None);
        fs::create_dir(root.join("not-a-recording")).unwrap();
        fs::write(root.join("stray.txt"), b"x").unwrap();

        let dirs: Vec<PathBuf> = list(root).unwrap().into_iter().map(|(d, _)| d).collect();
        assert_eq!(dirs, vec![new, mid, old]);
        assert!(list(&root.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn resolve_queries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(resolve(root, "last").is_err());

        let a = seed(
            root,
            at(2026, 1, 1, 9, 0),
            Some("alpha"),
            RecordingState::Done,
            None,
        );
        let b = seed(
            root,
            at(2026, 1, 1, 10, 0),
            Some("beta"),
            RecordingState::Done,
            None,
        );
        let c = seed(
            root,
            at(2026, 2, 5, 11, 0),
            None,
            RecordingState::Done,
            None,
        );

        assert_eq!(resolve(root, "last").unwrap(), c);
        assert_eq!(resolve(root, "2026-01-01_0900_alpha").unwrap(), a);
        assert_eq!(resolve(root, "2026-01-01_10").unwrap(), b);
        assert_eq!(resolve(root, "2026-02").unwrap(), c);

        let ambiguous = resolve(root, "2026-01-01").unwrap_err().to_string();
        assert!(ambiguous.contains("2026-01-01_0900_alpha"), "{ambiguous}");
        assert!(ambiguous.contains("2026-01-01_1000_beta"), "{ambiguous}");

        let none = resolve(root, "2025").unwrap_err().to_string();
        assert!(none.contains("no recording matches"), "{none}");
        assert!(resolve(root, "").is_err());
    }

    #[test]
    fn resolve_prefers_exact_over_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let t = at(2026, 1, 1, 9, 0);
        let first = seed(root, t, None, RecordingState::Done, None);
        let _second = seed(root, t, None, RecordingState::Done, None);
        assert_eq!(resolve(root, "2026-01-01_0900").unwrap(), first);
    }

    #[test]
    fn interrupted_finds_recording_and_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let rec = seed(
            root,
            at(2026, 1, 1, 9, 0),
            None,
            RecordingState::Recording,
            None,
        );
        let proc_ = seed(
            root,
            at(2026, 1, 2, 9, 0),
            None,
            RecordingState::Processing,
            None,
        );
        seed(root, at(2026, 1, 3, 9, 0), None, RecordingState::Done, None);
        seed(
            root,
            at(2026, 1, 4, 9, 0),
            None,
            RecordingState::Failed,
            None,
        );
        let later = seed(
            root,
            at(2026, 1, 6, 9, 0),
            None,
            RecordingState::Waiting,
            None,
        );
        let earlier = seed(
            root,
            at(2026, 1, 5, 9, 0),
            None,
            RecordingState::Waiting,
            None,
        );

        // Waiting for the network is not interrupted: recovery leaves it to
        // the automatic retries.
        assert_eq!(interrupted(root), vec![proc_, rec]);
        let waiting: Vec<PathBuf> = waiting(root).into_iter().map(|(dir, _)| dir).collect();
        assert_eq!(waiting, vec![earlier, later], "oldest first");
    }

    /// The sweep skips a recording its guard refuses, and reads the meta
    /// again under the guard: what another writer wrote between the
    /// listing and the guard is kept.
    #[test]
    fn the_sweep_skips_what_is_busy_and_keeps_what_was_written() {
        let tmp = tempfile::tempdir().unwrap();
        let old = at(2026, 1, 1, 9, 0);
        let busy = seed(
            tmp.path(),
            old,
            Some("busy"),
            RecordingState::Done,
            Some(old),
        );
        let renamed = seed(
            tmp.path(),
            old,
            Some("renamed"),
            RecordingState::Done,
            Some(old),
        );
        let busy_id = read_meta(&busy).unwrap().id;
        let swept = sweep_retention_guarded(tmp.path(), 0, at(2026, 2, 1, 9, 0), |id| {
            if id == busy_id {
                return None;
            }
            // A rename lands just before the guard is taken.
            let mut meta = read_meta(&renamed).unwrap();
            meta.speaker_names
                .insert("S1".into(), crate::recording::SpeakerName::plain("Sarah"));
            write_meta(&renamed, &meta).unwrap();
            Some(())
        });
        assert_eq!(swept, vec![renamed.clone()]);
        assert!(audio_path(&busy, TrackKind::Mic).exists(), "skipped");
        assert!(!read_meta(&busy).unwrap().audio_deleted);
        let meta = read_meta(&renamed).unwrap();
        assert!(meta.audio_deleted);
        assert_eq!(meta.speaker_names["S1"].name, "Sarah", "the rename is kept");
    }

    #[test]
    fn sweep_respects_age_and_status() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = at(2026, 10, 10, 12, 0);
        // Ended 8 days ago, started long before: swept at 7 days.
        let old = seed(
            root,
            at(2026, 9, 1, 9, 0),
            Some("old"),
            RecordingState::Done,
            Some(at(2026, 10, 2, 12, 0)),
        );
        // Ended 3 days ago: kept at 7 days.
        let recent = seed(
            root,
            at(2026, 10, 7, 11, 0),
            Some("recent"),
            RecordingState::Done,
            Some(at(2026, 10, 7, 12, 0)),
        );
        // No ended_at: falls back to started_at (20 days ago).
        let no_end = seed(
            root,
            at(2026, 9, 20, 9, 0),
            Some("noend"),
            RecordingState::Done,
            None,
        );
        let failed = seed(
            root,
            at(2026, 1, 1, 9, 0),
            Some("f"),
            RecordingState::Failed,
            None,
        );
        let rec = seed(
            root,
            at(2026, 1, 2, 9, 0),
            Some("r"),
            RecordingState::Recording,
            None,
        );
        let proc_ = seed(
            root,
            at(2026, 1, 3, 9, 0),
            Some("p"),
            RecordingState::Processing,
            None,
        );
        let waiting = seed(
            root,
            at(2026, 1, 4, 9, 0),
            Some("w"),
            RecordingState::Waiting,
            Some(at(2026, 1, 4, 10, 0)),
        );

        assert!(sweep_retention(root, -1, now).is_empty());
        assert!(old.join("audio").exists());

        let mut swept = sweep_retention(root, 7, now);
        swept.sort();
        let mut expected = vec![old.clone(), no_end.clone()];
        expected.sort();
        assert_eq!(swept, expected);
        assert!(!old.join("audio").exists());
        assert!(read_meta(&old).unwrap().audio_deleted);
        assert!(recent.join("audio").exists());
        assert!(!read_meta(&recent).unwrap().audio_deleted);

        // Already swept recordings are not reported again.
        assert_eq!(sweep_retention(root, 0, now), vec![recent.clone()]);
        assert!(!recent.join("audio").exists());

        for dir in [&failed, &rec, &proc_, &waiting] {
            assert!(dir.join("audio").exists(), "{}", dir.display());
            assert!(!read_meta(dir).unwrap().audio_deleted);
        }
    }

    #[test]
    fn free_space_reads_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(free_space(tmp.path()).unwrap() > 0);
        // A root that does not exist yet is measured at its parent.
        let missing = tmp.path().join("not/yet/created");
        assert!(free_space(&missing).unwrap() > 0);
    }

    #[test]
    fn recordings_dir_override_and_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(recordings_dir_from(Some("~/m")), home.join("m"));
        assert_eq!(recordings_dir_from(Some("/srv/m")), PathBuf::from("/srv/m"));
        assert!(recordings_dir_from(None).ends_with("whisrs/recordings"));
        assert_eq!(recordings_dir_from(Some("  ")), recordings_dir());
    }

    /// Headers list like the metas, newest first, with who speaks but
    /// without the words.
    #[test]
    fn headers_list_like_the_metas() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(
            root,
            at(2026, 1, 1, 9, 0),
            Some("a"),
            RecordingState::Done,
            None,
        );
        let dir = seed(
            root,
            at(2026, 3, 1, 9, 0),
            None,
            RecordingState::Failed,
            None,
        );
        let mut meta = read_meta(&dir).unwrap();
        meta.speakers = vec![
            crate::recording::Speaker {
                id: "me".into(),
                name: "Yosif".into(),
                source: "self".into(),
                evidence: None,
                talk_ms: 0,
                channel: None,
                minor: false,
            },
            crate::recording::Speaker {
                id: "others".into(),
                name: "Others".into(),
                source: "channel".into(),
                evidence: None,
                talk_ms: 0,
                channel: None,
                minor: false,
            },
        ];
        meta.segments = vec![crate::recording::TranscriptSegment {
            start_ms: 0,
            end_ms: 1,
            speaker: "me".into(),
            channel: TrackKind::Mic,
            text: "hello".into(),
        }];
        meta.summary = Some("S.".into());
        meta.error = Some("boom".into());
        write_meta(&dir, &meta).unwrap();
        fs::create_dir(root.join("junk")).unwrap();

        let headers = list_headers(root).unwrap();
        let metas = list(root).unwrap();
        let ids = |v: Vec<String>| v;
        assert_eq!(
            ids(headers.iter().map(|(_, h)| h.id.clone()).collect()),
            ids(metas.iter().map(|(_, m)| m.id.clone()).collect())
        );
        let header = &headers[0].1;
        assert_eq!(
            header.heard_names(),
            ["Yosif"],
            "the silent speaker is not heard"
        );
        assert_eq!(header.segment_speakers.len(), 1);
        assert_eq!(
            (header.summary.as_deref(), header.error.as_deref()),
            (Some("S."), Some("boom"))
        );
        assert_eq!(header.status, RecordingState::Failed);
    }

    /// An exact id that is its folder's name is read directly; a query
    /// with `/` or `..` never reads a folder outside the root, whatever its
    /// `recording.json` says.
    #[test]
    fn resolve_reads_only_folders_under_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("recordings");
        let inside = seed(
            &root,
            at(2026, 1, 1, 9, 0),
            Some("in"),
            RecordingState::Done,
            None,
        );
        assert_eq!(resolve(&root, "2026-01-01_0900_in").unwrap(), inside);
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let meta = RecordingMeta::new("../outside".into(), None, at(2026, 2, 1, 9, 0));
        write_meta(&outside, &meta).unwrap();
        let err = resolve(&root, "../outside").unwrap_err().to_string();
        assert!(err.contains("no recording matches"), "{err}");
        // `.` is the root itself, never read as a recording.
        write_meta(
            &root,
            &RecordingMeta::new(".".into(), None, at(2026, 3, 1, 9, 0)),
        )
        .unwrap();
        let err = resolve(&root, ".").unwrap_err().to_string();
        assert!(err.contains("no recording matches"), "{err}");
        fs::remove_file(root.join(META_FILE)).unwrap();
        // A folder whose recording.json names another id is not that id.
        fs::rename(&inside, root.join("2026-09-09_0000")).unwrap();
        assert!(resolve(&root, "2026-09-09_0000").is_err());
        assert_eq!(
            resolve(&root, "2026-01-01_0900_in").unwrap(),
            root.join("2026-09-09_0000"),
            "found by its id"
        );
    }

    /// An exact id that names a symlink is not read through it: the link
    /// may point anywhere, and the listing does not show it either.
    #[test]
    fn resolve_never_follows_a_symlinked_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("recordings");
        seed(
            &root,
            at(2026, 1, 1, 9, 0),
            Some("in"),
            RecordingState::Done,
            None,
        );
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let meta = RecordingMeta::new("linked".into(), None, at(2026, 2, 1, 9, 0));
        write_meta(&outside, &meta).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let err = resolve(&root, "linked").unwrap_err().to_string();
        assert!(err.contains("no recording matches 'linked'"), "{err}");
        assert_eq!(list_headers(&root).unwrap().len(), 1, "not listed");
    }

    /// A recording whose header reads but whose recording.json does not
    /// is never `last` nor a prefix's one match; named exactly, its error
    /// says why. A header needs what read_meta needs.
    #[test]
    fn resolve_lands_only_on_recordings_that_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let good = seed(
            root,
            at(2026, 1, 1, 9, 0),
            Some("good"),
            RecordingState::Done,
            None,
        );
        let bad = seed(
            root,
            at(2026, 1, 1, 10, 0),
            Some("bad"),
            RecordingState::Done,
            None,
        );
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(bad.join(META_FILE)).unwrap()).unwrap();
        json["segments"] = serde_json::json!([{"start_ms": 0, "end_ms": 1, "speaker": "me",
            "channel": "mic", "text": 42}]);
        fs::write(bad.join(META_FILE), json.to_string()).unwrap();
        assert!(read_meta(&bad).is_err() && read_header(&bad).is_ok());

        assert_eq!(
            resolve(root, "last").unwrap(),
            good,
            "the newest that reads"
        );
        assert_eq!(
            resolve(root, "2026-01-01").unwrap(),
            good,
            "the one match that reads"
        );
        let err = resolve(root, "2026-01-01_1000_bad").unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse"), "{err:#}");
        // Too many: only the ones that read are counted and listed.
        seed(
            root,
            at(2026, 1, 1, 11, 0),
            Some("also"),
            RecordingState::Done,
            None,
        );
        let err = resolve(root, "2026-01-01").unwrap_err().to_string();
        assert_eq!(
            err,
            "'2026-01-01' matches 2 recordings: 2026-01-01_1100_also, \
             2026-01-01_0900_good (1 more could not be read)"
        );

        let mut no_rate = json.clone();
        no_rate.as_object_mut().unwrap().remove("sample_rate");
        fs::write(bad.join(META_FILE), no_rate.to_string()).unwrap();
        assert!(read_header(&bad).is_err(), "a header needs sample_rate too");
    }

    #[test]
    fn recording_dir_comes_from_the_recording_section_only() {
        assert_eq!(
            recording_dir_from_toml("[recording]\ndir = \"~/notes/recordings\"\n").as_deref(),
            Some("~/notes/recordings")
        );
        assert_eq!(
            recording_dir_from_toml("[general]\nbackend = \"groq\"\n"),
            None
        );
        assert_eq!(recording_dir_from_toml("not toml ["), None);
    }

    #[test]
    fn transcript_and_audio_paths() {
        let dir = Path::new("/m/2026-01-01_0900");
        assert_eq!(
            transcript_path(dir, "2026-01-01_0900"),
            dir.join("2026-01-01_0900.md")
        );
        assert_eq!(audio_path(dir, TrackKind::Mic), dir.join("audio/mic.wav"));
        assert_eq!(
            audio_path(dir, TrackKind::System),
            dir.join("audio/system.wav")
        );
    }
}
