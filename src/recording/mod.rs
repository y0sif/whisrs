//! The recording feature: record mic + system audio to disk, transcribe
//! afterwards.
//!
//! Each recording lives in its own directory under the recordings root (see
//! `store`): `recording.json` holds [`RecordingMeta`], `audio/` holds one WAV
//! per track, and `<id>.md` is the rendered transcript.
//!
//! This file holds the data types, which every build has: the IPC wire
//! (`Command::Recording`, [`RecordingStatus`]) and `recording.json`, so a CLI
//! and a daemon built with and without the `recording` feature still talk.
//! The submodules are the feature itself and exist only with it.

#[cfg(feature = "recording")]
pub mod diarize;
#[cfg(feature = "recording")]
pub mod echo;
#[cfg(feature = "recording")]
pub mod llm_names;
#[cfg(feature = "recording")]
pub mod naming;
#[cfg(feature = "recording")]
pub mod process;
#[cfg(feature = "recording")]
pub mod recorder;
#[cfg(feature = "recording")]
pub mod render;
#[cfg(feature = "recording")]
pub mod speakers;
#[cfg(feature = "recording")]
pub mod speech;
#[cfg(feature = "recording")]
pub mod split;
#[cfg(feature = "recording")]
pub mod store;
#[cfg(feature = "recording")]
pub mod summary;
#[cfg(feature = "recording")]
pub mod wav_repair;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "recording")]
use crate::ipc::validate_language_override;

/// Label of the system track in a transcript when the recording names none.
pub const DEFAULT_OTHERS_LABEL: &str = "Others";

/// Longest label of the system track (`--others`), in characters.
pub const OTHERS_LABEL_MAX: usize = 64;

/// Most voices a recording may ask the diarization sidecar to find
/// (`--num-speakers`, `[recording] num_speakers`).
pub const NUM_SPEAKERS_MAX: u32 = 20;

/// Check `--num-speakers`: 0 (let the sidecar decide) to
/// [`NUM_SPEAKERS_MAX`].
pub fn validate_num_speakers(n: u32) -> Result<u32, String> {
    if n > NUM_SPEAKERS_MAX {
        return Err(format!(
            "num_speakers is {n}: use 0 (let the sidecar decide) to {NUM_SPEAKERS_MAX}"
        ));
    }
    Ok(n)
}

/// Validate and normalize the label of the system track (`--others`):
/// trimmed, non-empty, one line, at most [`OTHERS_LABEL_MAX`] characters.
pub fn validate_others_label(label: &str) -> Result<String, String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err("the others label is empty: name the system track, e.g. 'Video'".to_string());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("the others label must be a single line of text".to_string());
    }
    let len = trimmed.chars().count();
    if len > OTHERS_LABEL_MAX {
        return Err(format!(
            "the others label is {len} characters long: use at most {OTHERS_LABEL_MAX}"
        ));
    }
    Ok(trimmed.to_string())
}

/// Per-recording settings a `recording` command may carry, all optional.
/// The languages, `num_speakers`, `shared_mic` and `participants` apply to
/// `start`, a `toggle` that starts, and `retry`; `others` to `start` and
/// `toggle`; `speaker` and `name` to `name`. What is left out keeps the
/// config default, or for a retry what the recording stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingOptions {
    /// Language of both tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Language of the mic track, over `language`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_language: Option<String>,
    /// Language of the system track, over `language`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_language: Option<String>,
    /// Label of the system track in the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub others: Option<String>,
    /// Several people share the mic: its voices are told apart too
    /// (`[recording] shared_mic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_mic: Option<bool>,
    /// How many voices the diarization sidecar should find; 0 lets it
    /// decide (`[recording] num_speakers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_speakers: Option<u32>,
    /// Who the recording is with (`--with "Sarah, Omar"`), for naming its
    /// speakers ([`naming`]). An empty list names nobody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<Vec<String>>,
    /// The speaker `name` renames: its id (`S2`), `Speaker N`, or the name
    /// it has now ([`naming::resolve_speaker`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// Its new name; blank clears the name the user gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The id is a recording's whole id: no prefix, no `last`. The TUI
    /// sends it with every action, since the prefix of a recording deleted
    /// meanwhile can name its sibling (`..._1402` and `..._1402-2`). Left
    /// out by the CLI, which resolves what was typed itself, so its
    /// messages are as before; a daemon older than it ignores it. A delete
    /// always takes the whole id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exact: bool,
}

impl RecordingOptions {
    /// Every given value checked and normalized: the languages by the `-l`
    /// rule ([`validate_language_override`]), `others` by
    /// [`validate_others_label`], the participants by
    /// [`naming::validate_participants`]. The daemon runs this on every
    /// request: the socket is open to any client, not only the CLI that
    /// checks first.
    #[cfg(feature = "recording")]
    pub fn validated(self) -> Result<Self, String> {
        let language =
            |value: Option<String>| value.map(|v| validate_language_override(&v)).transpose();
        Ok(Self {
            language: language(self.language)?,
            mic_language: language(self.mic_language)?,
            system_language: language(self.system_language)?,
            others: self.others.map(|o| validate_others_label(&o)).transpose()?,
            shared_mic: self.shared_mic,
            num_speakers: self.num_speakers.map(validate_num_speakers).transpose()?,
            participants: self
                .participants
                .map(naming::validate_participants)
                .transpose()?,
            speaker: self
                .speaker
                .map(|s| match s.trim() {
                    "" => {
                        Err("which speaker? Give its id (S2), \"Speaker N\" or its name"
                            .to_string())
                    }
                    s => Ok(s.to_string()),
                })
                .transpose()?,
            name: self
                .name
                .map(|n| match n.trim() {
                    "" => Ok(String::new()),
                    n => naming::validate_person_name(n),
                })
                .transpose()?,
            exact: self.exact,
        })
    }

    /// The language these options give track `kind`: its own, else
    /// `language`, else `None`.
    pub fn language_for(&self, kind: TrackKind) -> Option<&str> {
        let own = match kind {
            TrackKind::Mic => &self.mic_language,
            TrackKind::System => &self.system_language,
        };
        own.as_deref().or(self.language.as_deref())
    }
}

/// What a `recording` IPC command asks the daemon to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingAction {
    Start,
    Stop,
    Cancel,
    Status,
    Toggle,
    Retry,
    Copy,
    /// Name a speaker of a transcribed recording (`whisrs recording name`).
    Name,
    /// Summarize a transcribed recording now (`whisrs recording summarize`),
    /// without transcribing it again.
    Summarize,
    /// Delete a recording's folder for good (`whisrs recording delete`): its
    /// audio, transcript and summary. Takes its id, never `last`.
    Delete,
}

/// Daemon-level recording phase, reported over IPC and to the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingPhase {
    #[default]
    Idle,
    Recording,
    Processing,
}

/// Snapshot of the daemon's recording activity.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub phase: RecordingPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Local>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// Ids of recordings waiting for processing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<String>,
    /// Ids of recordings waiting for the network ([`RecordingState::Waiting`]),
    /// not counting one that is queued or being processed right now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<String>,
    /// The recording shown as processing is being summarized
    /// ([`RecordingAction::Summarize`]), not transcribed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summarizing: bool,
}

/// Lifecycle of a single recording, stored as `status` in `recording.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingState {
    Recording,
    Processing,
    Done,
    Failed,
    /// Transcription could not reach the backend: the daemon tries again
    /// on its own ([`NetworkWait`]) until [`RecordingMeta::network_deadline`].
    Waiting,
}

/// How often the daemon tries a recording waiting for the network again.
pub const NETWORK_RETRY_EVERY: Duration = Duration::from_secs(10 * 60);

/// How long after its first failed try a recording keeps waiting for the
/// network before it becomes a plain failure. Counted from the try, not the
/// stop: a recording left untranscribed over a weekend still gets its day.
/// A manual retry that finds no network starts a new one.
pub const NETWORK_RETRY_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Most automatic attempts one recording gets: one every
/// [`NETWORK_RETRY_EVERY`] for [`NETWORK_RETRY_FOR`]. Daemon starts add an
/// attempt each, so this also bounds a daemon that keeps restarting.
pub const NETWORK_RETRY_MAX: u32 = 144;

/// The automatic retries of a recording waiting for the network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkWait {
    /// When transcription first failed for lack of network.
    pub since: DateTime<Local>,
    /// Automatic attempts made so far. Separate from
    /// [`RecordingMeta::attempts`], which they never raise.
    pub attempts: u32,
    /// When the last attempt (automatic or not) failed for lack of network.
    pub last_attempt_at: DateTime<Local>,
}

/// Which audio source a track was captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Mic,
    System,
}

impl TrackKind {
    /// File stem used for this track under `audio/`.
    pub fn file_stem(self) -> &'static str {
        match self {
            TrackKind::Mic => "mic",
            TrackKind::System => "system",
        }
    }
}

/// Why a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Stopped,
    MaxDuration,
    Shutdown,
    Recovered,
    /// Stopped because the disk was almost full.
    DiskFull,
}

/// One recorded audio track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub kind: TrackKind,
    /// Path relative to the recording directory, e.g. `audio/mic.wav`.
    pub file: String,
    pub device: String,
    /// Time of the first captured chunk relative to `started_at`.
    pub offset_ms: i64,
    pub samples: u64,
}

/// A timed piece of transcribed text from a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<Word>>,
}

/// A single timed word, when the backend reports word timings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// A speaker in the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    /// `"me"` and `"others"` for a track whose voices are not told apart;
    /// `S1`, `S2`, ... for the voices of the system track and `M1`, `M2`,
    /// ... for those of a shared mic, numbered by first appearance
    /// ([`speakers`]). `"mic"` holds a shared mic's speech no voice was found
    /// for, as `"others"` does on the system track.
    pub id: String,
    pub name: String,
    /// How the name was found ([`naming`]): `"user"` (`whisrs recording
    /// name`), `"self"` (the user), `"with"` (the one participant), `"llm"`
    /// (the LLM, with `evidence`), `"last"` (the last participant left),
    /// `"channel"` (the track's label: the others label), or `"none"`
    /// (unnamed: `Speaker N`).
    pub source: String,
    /// The words that named the speaker, when a name came from the
    /// transcript: the LLM's quote (`source` `"llm"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// How long the speaker talks in the transcript, in milliseconds.
    #[serde(default)]
    pub talk_ms: u64,
    /// The track the speaker was heard on. `None` in a recording processed
    /// before this was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<TrackKind>,
    /// A voice with little talk time ([`speakers::is_minor`]): kept in the
    /// transcript, but never named after a participant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub minor: bool,
}

/// A name the user gave a speaker, in `recording.json` `speaker_names`.
/// Written as `{"name": "Sarah", "anchor": {"channel": "system",
/// "first_ms": 4000}}`; a plain string (a file written before anchors were
/// stored) loads as a name without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SpeakerNameEntry")]
pub struct SpeakerName {
    pub name: String,
    /// Where the voice was heard first when it was named. `None` for a
    /// track's own speaker (`me`, `others`, `mic`) and in an older file:
    /// then the id alone says whose name it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<VoiceAnchor>,
}

impl SpeakerName {
    /// A name tied to its speaker's id alone.
    pub fn plain(name: impl Into<String>) -> Self {
        SpeakerName {
            name: name.into(),
            anchor: None,
        }
    }
}

/// The track and the time a voice first speaks ([`SpeakerName::anchor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceAnchor {
    pub channel: TrackKind,
    pub first_ms: u64,
}

/// The two shapes a [`SpeakerName`] loads from.
#[derive(Deserialize)]
#[serde(untagged)]
enum SpeakerNameEntry {
    Plain(String),
    Full {
        name: String,
        #[serde(default)]
        anchor: Option<VoiceAnchor>,
    },
}

impl From<SpeakerNameEntry> for SpeakerName {
    fn from(entry: SpeakerNameEntry) -> Self {
        match entry {
            SpeakerNameEntry::Plain(name) => SpeakerName::plain(name),
            SpeakerNameEntry::Full { name, anchor } => SpeakerName { name, anchor },
        }
    }
}

/// Where a recording's title came from, stored as `title_source` in
/// `recording.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleSource {
    /// Given by the user (`-t`). Never replaced.
    User,
    /// None given: the transcript shows a dated default.
    Default,
    /// Made by the LLM with the summary ([`summary`]). A later summary may
    /// replace it.
    Llm,
}

/// One line of the final transcript, on the recording timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    /// A [`Speaker::id`].
    pub speaker: String,
    pub channel: TrackKind,
    pub text: String,
}

/// Contents of `recording.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingMeta {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Where `title` came from. `None` in a recording made before this was
    /// stored: see [`Self::title_source`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    pub status: RecordingState,
    pub started_at: DateTime<Local>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Local>>,
    #[serde(default)]
    pub end_reason: Option<EndReason>,
    pub sample_rate: u32,
    #[serde(default)]
    pub tracks: Vec<TrackMeta>,
    /// Language the mic track is transcribed in, set at start (a retry may
    /// change it). `None` in a recording made before this was stored, until
    /// processing fills it from the config.
    #[serde(default)]
    pub mic_language: Option<String>,
    /// Language the system track is transcribed in, like `mic_language`.
    #[serde(default)]
    pub system_language: Option<String>,
    /// Label of the system track in the transcript, set at start. `None`
    /// (a recording made before this was stored) is [`DEFAULT_OTHERS_LABEL`].
    #[serde(default)]
    pub others_label: Option<String>,
    /// Several people share the mic, so its voices are told apart too. Set
    /// at start (a retry may change it); `None` in a recording made before
    /// this was stored, until processing fills it from the config.
    #[serde(default)]
    pub shared_mic: Option<bool>,
    /// How many voices the diarization sidecar is asked to find, 0 to let
    /// it decide. Set at start (a retry may change it); `None` in a
    /// recording made before this was stored, until processing fills it
    /// from the config.
    #[serde(default)]
    pub num_speakers: Option<u32>,
    /// Who the recording is with (`--with`), set at start (a retry given
    /// them replaces them). Empty when nobody was named.
    #[serde(default)]
    pub participants: Vec<String>,
    /// The names the user gave speakers (`whisrs recording name`), by
    /// speaker id. Kept apart from `speakers`, which processing computes
    /// again, so a retry keeps them; they win over every other name. A
    /// voice's name carries where it first spoke, so it follows the voice
    /// when a retry numbers the voices differently ([`naming`]).
    #[serde(default)]
    pub speaker_names: BTreeMap<String, SpeakerName>,
    /// How the voices were told apart, set by processing: `"off"`, or the
    /// method and model, e.g. `"deepgram:latest"`. `None` in a recording
    /// processed before speakers were separated.
    #[serde(default)]
    pub diarization: Option<String>,
    /// Transcription backend, set by processing.
    #[serde(default)]
    pub backend: Option<String>,
    /// Model the backend transcribed with, set by processing: the request's
    /// model, or the model file for local whisper. Both tracks use the same
    /// one; only their language differs.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub speakers: Vec<Speaker>,
    #[serde(default)]
    pub segments: Vec<TranscriptSegment>,
    /// Non-fatal notes, e.g. "system audio unavailable".
    #[serde(default)]
    pub errors: Vec<String>,
    /// Fatal processing error when `status` is `Failed`.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub audio_deleted: bool,
    /// Processing attempts started since the last manual retry. Written
    /// before each attempt, so a recording whose processing keeps crashing the
    /// daemon is not requeued on every start forever.
    #[serde(default)]
    pub attempts: u32,
    /// Set once transcription failed for lack of network, cleared by a
    /// manual retry or a finished transcript.
    #[serde(default)]
    pub network_wait: Option<NetworkWait>,
    /// The summary of the transcript, in Markdown ([`summary`]), or
    /// [`summary::TOO_SHORT`]. A retry keeps it unless it makes a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The `[llm]` model that wrote `summary`; `None` when no LLM was asked
    /// (a transcript too short to summarize).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// When `summary` was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarized_at: Option<DateTime<Local>>,
    /// `summary` was made before the transcript or the speaker names last
    /// changed: a retry transcribed the recording again, or a speaker was
    /// named, since. The next summary clears it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary_stale: bool,
    /// A summary is queued or running for this recording, and who wanted
    /// it. Set when the job is queued and cleared when it ends or a new
    /// transcript is written, so a summary that a shutdown or a crash
    /// interrupted is queued again at the next start (an automatic one only
    /// while `[recording] summary` is still on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_pending: Option<SummaryRequest>,
}

/// Who wanted a summary that is pending ([`RecordingMeta::summary_pending`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryRequest {
    /// `[recording] summary`, after a transcript.
    Auto,
    /// `whisrs recording summarize`.
    Asked,
}

/// Whether `text` holds more than whitespace.
fn has_text(text: Option<&str>) -> bool {
    text.is_some_and(|t| !t.trim().is_empty())
}

/// Current `recording.json` schema version.
pub const META_VERSION: u32 = 1;

/// Sample rate of recording tracks.
pub const SAMPLE_RATE: u32 = 16_000;

impl RecordingMeta {
    /// A fresh meta for a recording that just started.
    pub fn new(id: String, title: Option<String>, started_at: DateTime<Local>) -> Self {
        let title_source = if has_text(title.as_deref()) {
            TitleSource::User
        } else {
            TitleSource::Default
        };
        Self {
            version: META_VERSION,
            id,
            title,
            title_source: Some(title_source),
            status: RecordingState::Recording,
            started_at,
            ended_at: None,
            end_reason: None,
            sample_rate: SAMPLE_RATE,
            tracks: Vec::new(),
            mic_language: None,
            system_language: None,
            others_label: None,
            shared_mic: None,
            num_speakers: None,
            participants: Vec::new(),
            speaker_names: BTreeMap::new(),
            diarization: None,
            backend: None,
            model: None,
            speakers: Vec::new(),
            segments: Vec::new(),
            errors: Vec::new(),
            error: None,
            audio_deleted: false,
            attempts: 0,
            network_wait: None,
            summary: None,
            summary_model: None,
            summarized_at: None,
            summary_stale: false,
            summary_pending: None,
        }
    }

    /// Where the title came from: the stored `title_source`, else (a
    /// recording made before it was stored, when only `-t` set a title) the
    /// user's when there is a title.
    pub fn title_source(&self) -> TitleSource {
        self.title_source
            .unwrap_or(if has_text(self.title.as_deref()) {
                TitleSource::User
            } else {
                TitleSource::Default
            })
    }

    /// When a recording waiting for the network gives up:
    /// [`NETWORK_RETRY_FOR`] after its first failed try
    /// ([`NetworkWait::since`]), however long ago it stopped. `None` when it
    /// is not waiting.
    pub fn network_deadline(&self) -> Option<DateTime<Local>> {
        let wait = self.network_wait.as_ref()?;
        let span = chrono::Duration::from_std(NETWORK_RETRY_FOR).ok()?;
        Some(wait.since + span)
    }

    /// The stored language of track `kind`.
    pub fn language(&self, kind: TrackKind) -> Option<&str> {
        match kind {
            TrackKind::Mic => self.mic_language.as_deref(),
            TrackKind::System => self.system_language.as_deref(),
        }
    }

    /// Replace the stored language of each track `options` gives one
    /// ([`RecordingOptions::language_for`]); the others keep theirs.
    pub fn apply_languages(&mut self, options: &RecordingOptions) {
        if let Some(language) = options.language_for(TrackKind::Mic) {
            self.mic_language = Some(language.to_string());
        }
        if let Some(language) = options.language_for(TrackKind::System) {
            self.system_language = Some(language.to_string());
        }
    }

    /// Replace what a retry's `options` give: the languages
    /// ([`Self::apply_languages`]), `shared_mic`, `num_speakers` and the
    /// participants. The rest keep theirs.
    pub fn apply_options(&mut self, options: &RecordingOptions) {
        self.apply_languages(options);
        if let Some(shared) = options.shared_mic {
            self.shared_mic = Some(shared);
        }
        if let Some(n) = options.num_speakers {
            self.num_speakers = Some(n);
        }
        if let Some(participants) = &options.participants {
            self.participants = participants.clone();
        }
    }

    /// How many voices the diarization sidecar is asked to find on track
    /// `kind`: the stored `num_speakers` on the system track, and on the mic
    /// only when the recording has no system track (an in-person meeting),
    /// since the people on a call and those in the room are not counted
    /// together. 0 lets the sidecar decide.
    pub fn num_speakers_for(&self, kind: TrackKind) -> u32 {
        let has_system = self.tracks.iter().any(|t| t.kind == TrackKind::System);
        match kind {
            TrackKind::System => self.num_speakers.unwrap_or(0),
            TrackKind::Mic if !has_system => self.num_speakers.unwrap_or(0),
            TrackKind::Mic => 0,
        }
    }

    /// The label of the system track in the transcript.
    pub fn others_label(&self) -> &str {
        self.others_label.as_deref().unwrap_or(DEFAULT_OTHERS_LABEL)
    }
}

// Tests of `recording.json` and of the options' validation, which only the
// feature writes and runs.
#[cfg(all(test, feature = "recording"))]
mod tests {
    use super::*;

    #[test]
    fn enums_use_contract_wire_names() {
        assert_eq!(
            serde_json::to_string(&EndReason::MaxDuration).unwrap(),
            r#""max_duration""#
        );
        assert_eq!(
            serde_json::to_string(&EndReason::DiskFull).unwrap(),
            r#""disk_full""#
        );
        assert_eq!(
            serde_json::to_string(&RecordingState::Done).unwrap(),
            r#""done""#
        );
        assert_eq!(
            serde_json::to_string(&TrackKind::System).unwrap(),
            r#""system""#
        );
        assert_eq!(
            serde_json::to_string(&RecordingState::Waiting).unwrap(),
            r#""waiting""#
        );
        assert_eq!(RecordingStatus::default().phase, RecordingPhase::Idle);
    }

    #[test]
    fn meta_json_roundtrip() {
        let mut meta = RecordingMeta::new("2026-10-02_1400".into(), None, Local::now());
        meta.tracks.push(TrackMeta {
            kind: TrackKind::Mic,
            file: "audio/mic.wav".into(),
            device: "default".into(),
            offset_ms: 12,
            samples: 16_000,
        });
        meta.end_reason = Some(EndReason::Stopped);
        meta.mic_language = Some("de".into());
        meta.system_language = Some("auto".into());
        meta.others_label = Some("Video".into());
        meta.shared_mic = Some(true);
        meta.num_speakers = Some(3);
        meta.participants = vec!["Sarah".into(), "Omar".into()];
        meta.speaker_names = BTreeMap::from([
            ("me".to_string(), SpeakerName::plain("Yo")),
            (
                "S2".to_string(),
                SpeakerName {
                    name: "Dana".into(),
                    anchor: Some(VoiceAnchor {
                        channel: TrackKind::System,
                        first_ms: 4200,
                    }),
                },
            ),
        ]);
        meta.diarization = Some("deepgram:latest".into());
        meta.speakers = vec![Speaker {
            id: "S1".into(),
            name: "Speaker 1".into(),
            source: "none".into(),
            evidence: None,
            talk_ms: 4200,
            channel: Some(TrackKind::System),
            minor: true,
        }];
        meta.model = Some("ggml-large-v3-turbo.bin".into());
        meta.title = Some("Planning the move".into());
        meta.title_source = Some(TitleSource::Llm);
        meta.summary = Some("Moving on Friday.\n\n### Next steps\n- [ ] Book a van".into());
        meta.summary_model = Some("gpt-4o-mini".into());
        meta.summarized_at = Some(meta.started_at);
        meta.summary_stale = true;
        meta.summary_pending = Some(SummaryRequest::Asked);
        meta.network_wait = Some(NetworkWait {
            since: meta.started_at,
            attempts: 2,
            last_attempt_at: meta.started_at,
        });
        let json = serde_json::to_string(&meta).unwrap();
        let back: RecordingMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, meta);
    }

    /// A `recording.json` written before the languages, the others label
    /// and the model were stored still loads, with all four unset.
    #[test]
    fn meta_without_languages_still_loads() {
        let old = r#"{"version":1,"id":"2026-10-02_1400","title":null,
            "status":"done","started_at":"2026-10-02T14:00:00+02:00",
            "sample_rate":16000,"tracks":[],"backend":"groq"}"#;
        let meta: RecordingMeta = serde_json::from_str(old).unwrap();
        assert_eq!(meta.backend.as_deref(), Some("groq"));
        assert_eq!(meta.language(TrackKind::Mic), None);
        assert_eq!(meta.language(TrackKind::System), None);
        assert_eq!(meta.model, None);
        assert_eq!(meta.others_label(), DEFAULT_OTHERS_LABEL);
        assert_eq!(meta.network_wait, None);
        assert_eq!(meta.network_deadline(), None);
        assert_eq!(meta.shared_mic, None);
        assert_eq!(meta.num_speakers, None);
        assert_eq!(meta.diarization, None);
        assert!(meta.participants.is_empty());
        assert!(meta.speaker_names.is_empty());
        assert_eq!(meta.summary, None);
        assert_eq!(meta.title_source, None);
        assert_eq!(meta.title_source(), TitleSource::Default);
    }

    /// A title is the user's when `-t` gave one; an older file without
    /// `title_source` says so by having a title. A title the LLM made is
    /// not the user's.
    #[test]
    fn the_title_source_follows_the_title() {
        let now = Local::now();
        let titled = RecordingMeta::new("m".into(), Some("Standup".into()), now);
        assert_eq!(titled.title_source, Some(TitleSource::User));
        let untitled = RecordingMeta::new("m".into(), None, now);
        assert_eq!(untitled.title_source, Some(TitleSource::Default));
        let blank = RecordingMeta::new("m".into(), Some("  ".into()), now);
        assert_eq!(blank.title_source(), TitleSource::Default);

        let old: RecordingMeta = serde_json::from_str(
            r#"{"version":1,"id":"m","title":"Standup","status":"done",
                "started_at":"2026-10-02T14:00:00+02:00","sample_rate":16000}"#,
        )
        .unwrap();
        assert_eq!(old.title_source(), TitleSource::User);
        let mut llm = old.clone();
        llm.title_source = Some(TitleSource::Llm);
        assert_eq!(llm.title_source(), TitleSource::Llm);
        assert_eq!(
            serde_json::to_string(&TitleSource::Default).unwrap(),
            r#""default""#
        );
    }

    /// A user's name loads as written before anchors were stored (a plain
    /// string) and as written now; a name without an anchor is written
    /// back without one.
    #[test]
    fn speaker_names_load_old_and_new() {
        let meta: RecordingMeta = serde_json::from_str(
            r#"{"version":1,"id":"m","title":null,"status":"done",
                "started_at":"2026-10-02T14:00:00+02:00","sample_rate":16000,"tracks":[],
                "speaker_names":{"S1":"Sarah",
                    "S2":{"name":"Omar","anchor":{"channel":"system","first_ms":9000}}}}"#,
        )
        .unwrap();
        assert_eq!(meta.speaker_names["S1"], SpeakerName::plain("Sarah"));
        assert_eq!(
            meta.speaker_names["S2"].anchor,
            Some(VoiceAnchor {
                channel: TrackKind::System,
                first_ms: 9000
            })
        );
        assert_eq!(
            serde_json::to_string(&meta.speaker_names["S1"]).unwrap(),
            r#"{"name":"Sarah"}"#
        );
    }

    /// A `recording.json` processed before speakers were separated loads:
    /// its speakers have no talk time, channel or minor mark.
    #[test]
    fn meta_with_old_speakers_still_loads() {
        let old = r#"{"version":1,"id":"m","title":null,"status":"done",
            "started_at":"2026-10-02T14:00:00+02:00","sample_rate":16000,"tracks":[],
            "speakers":[{"id":"me","name":"Yosif","source":"self"},
                        {"id":"others","name":"Others","source":"channel"}],
            "segments":[{"start_ms":0,"end_ms":900,"speaker":"others",
                         "channel":"system","text":"hi"}]}"#;
        let meta: RecordingMeta = serde_json::from_str(old).unwrap();
        assert_eq!(meta.speakers.len(), 2);
        let others = &meta.speakers[1];
        assert_eq!(
            (others.id.as_str(), others.name.as_str()),
            ("others", "Others")
        );
        assert_eq!(others.talk_ms, 0);
        assert_eq!(others.channel, None);
        assert!(!others.minor);
        assert_eq!(others.evidence, None);
        // Written back without the fields an old file never had.
        let json = serde_json::to_string(&meta.speakers[0]).unwrap();
        assert_eq!(
            json,
            r#"{"id":"me","name":"Yosif","source":"self","talk_ms":0}"#
        );
    }

    /// The number of voices goes to the system track, and to the mic only
    /// in a recording without system audio.
    #[test]
    fn num_speakers_go_to_one_track() {
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.num_speakers = Some(3);
        let track = |kind| TrackMeta {
            kind,
            file: String::new(),
            device: String::new(),
            offset_ms: 0,
            samples: 0,
        };
        meta.tracks = vec![track(TrackKind::Mic)];
        assert_eq!(meta.num_speakers_for(TrackKind::Mic), 3, "mic only");
        meta.tracks.push(track(TrackKind::System));
        assert_eq!(meta.num_speakers_for(TrackKind::Mic), 0);
        assert_eq!(meta.num_speakers_for(TrackKind::System), 3);
        meta.num_speakers = None;
        assert_eq!(meta.num_speakers_for(TrackKind::System), 0);
    }

    /// A retry replaces the languages, `shared_mic` and `num_speakers` it
    /// gives and keeps the rest.
    #[test]
    fn retry_options_replace_what_they_give() {
        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.mic_language = Some("en".into());
        meta.shared_mic = Some(true);
        meta.apply_options(&RecordingOptions {
            language: Some("de".into()),
            ..Default::default()
        });
        assert_eq!(meta.language(TrackKind::Mic), Some("de"));
        assert_eq!(meta.shared_mic, Some(true), "kept");
        meta.apply_options(&RecordingOptions {
            shared_mic: Some(false),
            num_speakers: Some(4),
            ..Default::default()
        });
        assert_eq!(meta.shared_mic, Some(false));
        assert_eq!(meta.num_speakers, Some(4));
        assert_eq!(meta.language(TrackKind::Mic), Some("de"), "kept");
        meta.apply_options(&RecordingOptions::default());
        assert_eq!(meta.num_speakers, Some(4), "kept");

        // Participants given replace the stored ones, none keeps them, and
        // an empty list clears them.
        meta.participants = vec!["Sarah".into()];
        meta.apply_options(&RecordingOptions::default());
        assert_eq!(meta.participants, ["Sarah"], "kept");
        meta.apply_options(&RecordingOptions {
            participants: Some(vec!["Omar".into(), "Dana".into()]),
            ..Default::default()
        });
        assert_eq!(meta.participants, ["Omar", "Dana"]);
        meta.apply_options(&RecordingOptions {
            participants: Some(Vec::new()),
            ..Default::default()
        });
        assert!(meta.participants.is_empty());
    }

    /// A waiting recording gives up a day after its first failed try,
    /// however long before that it stopped.
    #[test]
    fn network_deadline_counts_from_the_first_failed_try() {
        let started = Local::now() - chrono::Duration::days(4);
        let mut meta = RecordingMeta::new("m".into(), None, started);
        let since = Local::now() - chrono::Duration::hours(2);
        meta.network_wait = Some(NetworkWait {
            since,
            attempts: 0,
            last_attempt_at: since,
        });
        let day_after_the_try = Some(since + chrono::Duration::hours(24));
        assert_eq!(meta.network_deadline(), day_after_the_try);
        // Stopped three days before its first try (a laptop shut over a
        // weekend): the stop does not shorten the wait.
        meta.ended_at = Some(started + chrono::Duration::hours(1));
        assert_eq!(meta.network_deadline(), day_after_the_try);
    }

    #[test]
    fn options_are_validated_and_normalized() {
        let options = RecordingOptions {
            language: Some(" DE ".into()),
            mic_language: Some("en_us".into()),
            system_language: Some("Auto".into()),
            others: Some("  Video  ".into()),
            shared_mic: Some(true),
            num_speakers: Some(NUM_SPEAKERS_MAX),
            participants: Some(vec![" Sarah ".into(), "Omar".into()]),
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(
            options,
            RecordingOptions {
                language: Some("de".into()),
                mic_language: Some("en-US".into()),
                system_language: Some("auto".into()),
                others: Some("Video".into()),
                shared_mic: Some(true),
                num_speakers: Some(NUM_SPEAKERS_MAX),
                participants: Some(vec!["Sarah".into(), "Omar".into()]),
                ..Default::default()
            }
        );
        assert_eq!(
            RecordingOptions::default().validated(),
            Ok(RecordingOptions::default())
        );

        for bad in [
            RecordingOptions {
                language: Some("english".into()),
                ..Default::default()
            },
            RecordingOptions {
                mic_language: Some("".into()),
                ..Default::default()
            },
            RecordingOptions {
                system_language: Some("1".into()),
                ..Default::default()
            },
        ] {
            let err = bad.clone().validated().unwrap_err();
            assert!(err.starts_with("invalid language"), "{bad:?}: {err}");
        }
        for (others, want) in [
            ("   ", "empty"),
            ("Video\nme", "single line"),
            (&"x".repeat(OTHERS_LABEL_MAX + 1), "at most"),
        ] {
            let err = RecordingOptions {
                others: Some(others.to_string()),
                ..Default::default()
            }
            .validated()
            .unwrap_err();
            assert!(err.contains(want), "{others:?}: {err}");
        }
        let longest = "é".repeat(OTHERS_LABEL_MAX);
        assert_eq!(validate_others_label(&longest), Ok(longest.clone()));
        // The daemon refuses a number of voices the CLI would not send.
        let err = RecordingOptions {
            num_speakers: Some(NUM_SPEAKERS_MAX + 1),
            ..Default::default()
        }
        .validated()
        .unwrap_err();
        assert!(err.contains("num_speakers is 21"), "{err}");
        // And participants it would not send.
        for (names, want) in [
            (vec![" ".to_string()], "a name is empty"),
            (vec!["Sarah, Omar".to_string()], "comma"),
            (
                vec!["Sarah".to_string(), "SARAH".to_string()],
                "listed twice",
            ),
            (vec!["x".to_string(); 21], "at most 20"),
        ] {
            let err = RecordingOptions {
                participants: Some(names.clone()),
                ..Default::default()
            }
            .validated()
            .unwrap_err();
            assert!(err.contains(want), "{names:?}: {err}");
        }
        // The speaker and the name of `name`: trimmed; a blank name clears.
        let named = RecordingOptions {
            speaker: Some(" S2 ".into()),
            name: Some(" Sarah Smith ".into()),
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(
            (named.speaker.as_deref(), named.name.as_deref()),
            (Some("S2"), Some("Sarah Smith"))
        );
        let cleared = RecordingOptions {
            name: Some("  ".into()),
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(cleared.name.as_deref(), Some(""));
        for bad in [
            RecordingOptions {
                speaker: Some(" ".into()),
                ..Default::default()
            },
            RecordingOptions {
                name: Some("Sarah\u{7}".into()),
                ..Default::default()
            },
            RecordingOptions {
                name: Some("x".repeat(65)),
                ..Default::default()
            },
        ] {
            assert!(bad.clone().validated().is_err(), "{bad:?}");
        }
        // An empty list is nobody: a retry clears the participants with it.
        assert_eq!(
            RecordingOptions {
                participants: Some(Vec::new()),
                ..Default::default()
            }
            .validated()
            .unwrap()
            .participants,
            Some(Vec::new())
        );
    }

    /// A track's own language wins over `language`, which covers the
    /// other track; tracks the options leave out keep what is stored.
    #[test]
    fn track_languages_from_options() {
        let both = RecordingOptions {
            language: Some("de".into()),
            ..Default::default()
        };
        let split = RecordingOptions {
            language: Some("de".into()),
            system_language: Some("auto".into()),
            ..Default::default()
        };
        assert_eq!(both.language_for(TrackKind::Mic), Some("de"));
        assert_eq!(both.language_for(TrackKind::System), Some("de"));
        assert_eq!(split.language_for(TrackKind::Mic), Some("de"));
        assert_eq!(split.language_for(TrackKind::System), Some("auto"));
        assert_eq!(
            RecordingOptions::default().language_for(TrackKind::Mic),
            None
        );

        let mut meta = RecordingMeta::new("m".into(), None, Local::now());
        meta.mic_language = Some("en".into());
        meta.system_language = Some("en".into());
        meta.apply_languages(&RecordingOptions {
            system_language: Some("fr".into()),
            ..Default::default()
        });
        assert_eq!(meta.language(TrackKind::Mic), Some("en"));
        assert_eq!(meta.language(TrackKind::System), Some("fr"));
        meta.apply_languages(&RecordingOptions::default());
        assert_eq!(meta.language(TrackKind::System), Some("fr"));
        meta.apply_languages(&both);
        assert_eq!(meta.language(TrackKind::Mic), Some("de"));
        assert_eq!(meta.language(TrackKind::System), Some("de"));
    }
}
