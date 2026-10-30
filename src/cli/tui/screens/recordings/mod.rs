//! Recordings: the recordings folder as a list, newest first, the focused
//! one previewed under it at 90 columns and up, and its transcript a key
//! away. The folder is read on a worker ([`read_folder`]) when the screen
//! opens, every 2 s while it is open and after every action's answer, as
//! `whisrs recording list` reads it; the actions (retry, copy, name,
//! summarize, delete) are the daemon's.

#[cfg(feature = "recording")]
mod read;
#[cfg(all(test, feature = "recording"))]
mod tests;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use whisrs::recording::{RecordingAction, RecordingOptions, RecordingStatus};

use crate::tui::app::{App, Dialog};
use crate::tui::keys::{Action, Mode};
use crate::tui::options::{Loading, Options, Pick};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::input::Input;
use crate::tui::widgets::picker::{self, Answer, Picked, Picker};
use crate::tui::widgets::row::{Row, Value};
use crate::tui::widgets::{dialog, fit, width, wrap, Notice, Tone, Ui};

/// What the screen says in a whisrs built without the recording feature.
pub const MISSING: &str = "This whisrs was built without the recording feature, so there are \
     no recordings to list. Use a release build, or rebuild with `cargo install whisrs \
     --features recording`.";

/// How far `PgUp` and `PgDn` move the list.
const PAGE: usize = 10;

/// The turns a preview shows.
const PREVIEW_TURNS: usize = 5;

/// The list's date column (`Oct 27 14:02`) and the gap after it.
const DATE_WIDTH: usize = 14;

/// The duration column, right-aligned.
const DURATION_WIDTH: usize = 6;

/// The status column, `processing` the longest.
const STATUS_WIDTH: usize = 10;

/// The note column, `retry available` the longest.
const NOTE_WIDTH: usize = 15;

/// The narrowest title column beside a note: below it the note goes.
const TITLE_MIN: usize = 14;

/// How many recordings read whole are kept.
const CACHED: usize = 16;

/// Where a recording is, as its row says.
// Made by the folder read ([`read`]), which only the recording feature
// builds.
#[cfg_attr(not(feature = "recording"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Recording,
    Queued,
    Processing,
    Done,
    Failed,
    /// Waiting for the network; the daemon tries again on its own.
    Waiting,
}

impl Phase {
    fn word(self) -> &'static str {
        match self {
            Phase::Recording => "recording",
            Phase::Queued => "queued",
            Phase::Processing => "processing",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Waiting => "waiting",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Phase::Recording => Tone::Ok,
            Phase::Queued | Phase::Processing => Tone::Muted,
            Phase::Done => Tone::Plain,
            Phase::Failed => Tone::Error,
            Phase::Waiting => Tone::Warn,
        }
    }
}

/// A speaker of a recording: its id (`S2`) and the name it has now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

/// One recording as the list shows it, read from its `recording.json`
/// header on a worker.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub dir: PathBuf,
    pub title: Option<String>,
    /// The start as the list shows it, and `/` finds it: `Oct 27 14:02`.
    pub date: String,
    /// End minus start, in seconds, counting up while it records; `None`
    /// when not known (a recording a crash left).
    pub secs: Option<i64>,
    pub phase: Phase,
    pub speakers: Vec<Voice>,
    /// How many of them have a line in the transcript.
    pub heard: usize,
    pub summary: bool,
    /// The summary was made before the transcript or a name changed.
    pub stale: bool,
    /// A summary is queued for it.
    pub summary_queued: bool,
    /// A summary of it is being made now.
    pub summarizing: bool,
    /// One of its tracks is still on disk.
    pub audio: bool,
    /// Its `<id>.md` is on disk.
    pub transcript: bool,
    /// When `recording.json` last changed: a preview read before is read
    /// again.
    pub stamp: Option<SystemTime>,
}

impl Item {
    /// The recording as messages name it: its title quoted, else its id.
    pub fn label(&self) -> String {
        match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(title) => format!("\"{title}\""),
            None => self.id.clone(),
        }
    }

    /// The label with the duration after it: `"Standup" (32m)`.
    fn named(&self) -> String {
        match self.secs {
            Some(secs) => format!("{} ({})", self.label(), duration_label(secs)),
            None => self.label(),
        }
    }

    /// The row's one note, and how it reads.
    fn note(&self) -> Option<(String, Tone)> {
        let speakers = |n: usize| match n {
            0 => None,
            1 => Some(("1 speaker".to_string(), Tone::Muted)),
            n => Some((format!("{n} speakers"), Tone::Muted)),
        };
        match self.phase {
            Phase::Done if self.summarizing => Some(("summarizing".into(), Tone::Muted)),
            Phase::Done if self.summary_queued => Some(("summary queued".into(), Tone::Muted)),
            Phase::Done if self.summary && self.stale => Some(("summary stale".into(), Tone::Warn)),
            Phase::Done if self.summary => Some(("summary".into(), Tone::Muted)),
            Phase::Done => speakers(self.heard),
            Phase::Failed | Phase::Waiting if self.audio => {
                Some(("retry available".into(), Tone::Muted))
            }
            Phase::Failed | Phase::Waiting => Some(("no audio left".into(), Tone::Faint)),
            Phase::Recording | Phase::Queued | Phase::Processing => None,
        }
    }

    /// Whether `filter` finds it: each of its words starts a word of the
    /// title, the id, a speaker's name or the date as shown, in any case,
    /// as a picker's filter does (`oct 26`, `sarah`, `2026-10-25`).
    fn matches(&self, filter: &str) -> bool {
        let mut texts = vec![
            self.title.as_deref().unwrap_or_default(),
            self.id.as_str(),
            self.date.as_str(),
        ];
        texts.extend(self.speakers.iter().map(|voice| voice.name.as_str()));
        picker::matches(filter, &texts)
    }
}

/// One recording read whole, for its preview and its transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub backend: Option<String>,
    /// The summary, Markdown as written.
    pub summary: Option<String>,
    pub stale: bool,
    /// The transcript as `whisrs recording show -f txt` prints it: a line
    /// per turn, `[00:01:02] Sarah: ...`.
    pub lines: Vec<String>,
    /// Why it failed, or what it waits for.
    pub error: Option<String>,
}

/// What the worker's folder reader keeps from one read to the next: each
/// header it parsed, by folder, with the `recording.json` it came from.
#[derive(Default)]
pub struct Parsed {
    #[cfg(feature = "recording")]
    headers: std::collections::HashMap<PathBuf, read::Kept>,
}

/// A recording to read whole on a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub id: String,
    pub dir: PathBuf,
    pub stamp: Option<SystemTime>,
}

/// A daemon action on one recording, and how its answer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Act {
    pub action: RecordingAction,
    /// The full id: the daemon resolves nothing a user did not see.
    pub id: String,
    pub options: RecordingOptions,
    /// The recording as messages name it ([`Item::label`]).
    pub label: String,
}

impl Act {
    /// `action` on `entry`, by its whole id ([`RecordingOptions::exact`]).
    fn on(entry: &Item, action: RecordingAction) -> Act {
        Act {
            action,
            id: entry.id.clone(),
            options: RecordingOptions {
                exact: true,
                ..RecordingOptions::default()
            },
            label: entry.label(),
        }
    }

    /// What it does, after "cannot".
    pub fn verb(&self) -> &'static str {
        match self.action {
            RecordingAction::Retry => "retry",
            RecordingAction::Copy => "copy",
            RecordingAction::Name => "name a speaker",
            RecordingAction::Summarize => "summarize",
            RecordingAction::Delete => "delete",
            _ => "do that",
        }
    }

    /// What the status says while the daemon is asked.
    pub fn asking(&self) -> String {
        match self.action {
            RecordingAction::Retry => format!("queueing {} for transcription", self.label),
            RecordingAction::Copy => format!("copying the transcript of {}", self.label),
            RecordingAction::Name => format!("naming a speaker of {}", self.label),
            RecordingAction::Summarize => format!("queueing {} for a summary", self.label),
            RecordingAction::Delete => format!("deleting {}", self.label),
            _ => "asking whisrsd".to_string(),
        }
    }

    /// What the status says once the daemon did it.
    pub fn done(&self) -> String {
        let speaker = self.options.speaker.as_deref().unwrap_or_default();
        match self.action {
            RecordingAction::Retry => format!("queued {} for transcription", self.label),
            RecordingAction::Copy => format!("copied the transcript of {}", self.label),
            RecordingAction::Name => match self.options.name.as_deref() {
                Some("") | None => format!("{speaker} has no name of yours now"),
                Some(name) => format!("{speaker} is now {name}"),
            },
            RecordingAction::Summarize => format!("queued {} for a summary", self.label),
            RecordingAction::Delete => format!("deleted {}", self.label),
            _ => "done".to_string(),
        }
    }
}

/// What a recording holds, for the delete's question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holds {
    pub audio: bool,
    pub transcript: bool,
    pub summary: bool,
}

/// The question a delete asks: `Delete "Standup" (32m): audio, transcript
/// and summary. This cannot be undone.` It names the recording by its
/// title, else its id, and only what of it is there to lose.
pub fn delete_question(
    title: Option<&str>,
    id: &str,
    duration: Option<&str>,
    holds: Holds,
) -> String {
    let name = match title.filter(|t| !t.trim().is_empty()) {
        Some(title) => format!("\"{title}\""),
        None => id.to_string(),
    };
    let duration = duration.map(|d| format!(" ({d})")).unwrap_or_default();
    let parts: Vec<&str> = [
        (holds.audio, "audio"),
        (holds.transcript, "transcript"),
        (holds.summary, "summary"),
    ]
    .into_iter()
    .filter_map(|(has, part)| has.then_some(part))
    .collect();
    let what = match parts.as_slice() {
        [] => "its folder".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!("Delete {name}{duration}: {what}. This cannot be undone.")
}

/// A length as the list shows it: `45s`, `32m`, `1h 05m`.
pub fn duration_label(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A start as a list shows it: `Oct 27 14:02` this year, `Oct 27 2025`
/// before it.
pub fn date_label(at: DateTime<Local>, now: DateTime<Local>) -> String {
    if at.year() == now.year() {
        at.format("%b %e %H:%M").to_string()
    } else {
        at.format("%b %e %Y").to_string()
    }
}

/// The question a retry of a transcribed recording asks first, worded from
/// what a retry does: it transcribes the audio again and replaces the
/// transcript and the speakers found in it (the names they got with them);
/// a name the user gave follows its voice, and one whose voice is not found
/// again is dropped with a note (`naming::USER_NAME_LOST`); a summary is
/// kept, marked stale, unless `[recording] summary` makes a new one, which
/// retitles any recording whose title is not the user's.
fn retry_question(entry: &Item, summary_on: bool) -> Vec<String> {
    let mut lines = vec![
        format!("Transcribe {} again from its audio?", entry.named()),
        "This replaces its transcript and the speakers found in it. Names you gave follow \
         their voices; one whose voice is not found again is dropped, and whisrs says so."
            .to_string(),
    ];
    if summary_on {
        lines.push(
            "[recording] summary is on, so a new summary follows; it may retitle the recording \
             unless you gave the title."
                .to_string(),
        );
    } else if entry.summary {
        lines.push("The summary stays, marked stale until you summarize it again.".to_string());
    }
    lines
}

/// The recordings folder `config.toml` at `config` names (`[recording]
/// dir`), else the one under the data folder `data`, read with the
/// recordings' headers only: newest first, the cursor's next recording
/// read whole only when wanted. `parsed` keeps the headers from one read to
/// the next, so only a changed `recording.json` is parsed again. `status`
/// asks the daemon which of them are queued or processed; it is called
/// only when one may be.
pub fn read_folder(
    config: &Path,
    data: &Path,
    now: DateTime<Local>,
    status: impl FnOnce() -> Option<RecordingStatus>,
    parsed: &mut Parsed,
) -> Result<Vec<Item>, String> {
    #[cfg(feature = "recording")]
    {
        read::folder(&read::root(config, data), now, status, &mut parsed.headers)
    }
    #[cfg(not(feature = "recording"))]
    {
        let _ = (config, data, now, status, parsed);
        Err(MISSING.to_string())
    }
}

/// The recording in `dir` read whole.
pub fn read_detail(dir: &Path) -> Result<Detail, String> {
    #[cfg(feature = "recording")]
    {
        read::detail(dir)
    }
    #[cfg(not(feature = "recording"))]
    {
        let _ = dir;
        Err(MISSING.to_string())
    }
}

/// What a key on the screen asks of the app.
#[derive(Debug, Default)]
pub struct Reply {
    pub leave: bool,
    pub status: Option<Notice>,
    pub dialog: Option<Dialog>,
    /// A daemon action to send.
    pub act: Option<Act>,
    /// A recording to read whole.
    pub read: Option<Wanted>,
}

impl Reply {
    fn say(tone: Tone, text: impl Into<String>) -> Reply {
        Reply {
            status: Some(Notice::new(tone, text)),
            ..Reply::default()
        }
    }
}

/// The transcript open, and how far down it is.
#[derive(Debug, Clone)]
struct Reading {
    id: String,
    scroll: usize,
}

/// `n`: the speaker picker, then the name input.
#[derive(Debug, Clone)]
enum Naming {
    Picking {
        id: String,
        label: String,
        picker: Picker,
    },
    Typing {
        id: String,
        label: String,
        speaker: String,
        current: String,
        input: Input,
    },
}

/// A recording read whole, by the stamp it was read at.
#[derive(Debug, Clone)]
struct Cached {
    id: String,
    stamp: Option<SystemTime>,
    detail: Result<Detail, String>,
}

#[derive(Debug)]
pub struct Recordings {
    /// This whisrs has the recording feature.
    pub built: bool,
    /// The folder as last read; `None` until the first read.
    pub folder: Option<Result<Vec<Item>, String>>,
    /// The id the cursor is on, kept there across reads.
    focus: Option<String>,
    cursor: usize,
    pub filter: String,
    /// The filter being typed.
    typing: Option<Input>,
    open: Option<Reading>,
    cache: Vec<Cached>,
    /// Read whole on a worker, not back yet.
    asked: Vec<(String, Option<SystemTime>)>,
    naming: Option<Naming>,
    /// The ids an action was sent for, not answered yet: one at a time
    /// each.
    busy: Vec<String>,
    /// How far the open transcript can scroll and how tall it is, as last
    /// drawn: a key moves within that.
    shown: Cell<(usize, usize)>,
}

impl Recordings {
    pub fn new(built: bool) -> Recordings {
        Recordings {
            built,
            folder: None,
            focus: None,
            cursor: 0,
            filter: String::new(),
            typing: None,
            open: None,
            cache: Vec::new(),
            asked: Vec::new(),
            naming: None,
            busy: Vec::new(),
            shown: Cell::new((usize::MAX, PAGE)),
        }
    }

    /// The screen opens: on the list, the cursor where it was, the folder
    /// as last read until the next read is back.
    pub fn opened(&mut self) {
        self.open = None;
        self.naming = None;
        self.typing = None;
        self.filter.clear();
        self.asked.clear();
    }

    pub fn mode(&self) -> Mode {
        match &self.naming {
            Some(Naming::Picking { .. }) => Mode::Picker,
            Some(Naming::Typing { .. }) => Mode::Text,
            None if self.typing.is_some() => Mode::Text,
            None => Mode::Read,
        }
    }

    fn entries(&self) -> &[Item] {
        match &self.folder {
            Some(Ok(entries)) => entries,
            _ => &[],
        }
    }

    /// The entries the filter keeps, by index.
    fn visible(&self) -> Vec<usize> {
        self.entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.filter.is_empty() || entry.matches(&self.filter))
            .map(|(i, _)| i)
            .collect()
    }

    fn entry(&self, id: &str) -> Option<&Item> {
        self.entries().iter().find(|entry| entry.id == id)
    }

    /// The entry under the cursor.
    fn focused(&self) -> Option<&Item> {
        let visible = self.visible();
        visible.get(self.cursor).map(|&i| &self.entries()[i])
    }

    /// What an action works on: the open transcript's recording, else the
    /// one under the cursor.
    fn target(&self) -> Option<&Item> {
        match &self.open {
            Some(reading) => self.entry(&reading.id),
            None => self.focused(),
        }
    }

    /// Put the cursor back on the focused id, or where it was when that
    /// recording is gone, and remember what is under it now.
    fn refocus(&mut self) {
        let visible = self.visible();
        if let Some(at) = self
            .focus
            .as_ref()
            .and_then(|id| visible.iter().position(|&i| self.entries()[i].id == *id))
        {
            self.cursor = at;
        }
        self.cursor = self.cursor.min(visible.len().saturating_sub(1));
        self.focus = visible
            .get(self.cursor)
            .map(|&i| self.entries()[i].id.clone());
    }

    fn select(&mut self, to: usize) {
        self.cursor = to;
        self.focus = None;
        let visible = self.visible();
        self.cursor = self.cursor.min(visible.len().saturating_sub(1));
        self.focus = visible
            .get(self.cursor)
            .map(|&i| self.entries()[i].id.clone());
    }

    /// The target's preview, when it is not read at its stamp yet nor
    /// asked for.
    fn want(&mut self) -> Option<Wanted> {
        let entry = self.target()?;
        let key = (entry.id.clone(), entry.stamp);
        let had = self.cache.iter().any(|c| c.id == key.0 && c.stamp == key.1);
        if had || self.asked.contains(&key) {
            return None;
        }
        let wanted = Wanted {
            id: entry.id.clone(),
            dir: entry.dir.clone(),
            stamp: entry.stamp,
        };
        self.asked.push(key);
        Some(wanted)
    }

    /// A read of the folder came: the cursor stays on its recording, a
    /// transcript or a name flow on one that is gone closes, and the
    /// recording under the cursor is read whole if it changed.
    pub fn read(&mut self, folder: Result<Vec<Item>, String>) -> Option<Wanted> {
        self.folder = Some(folder);
        self.refocus();
        if let Some(reading) = &self.open {
            if self.entry(&reading.id).is_none() {
                self.open = None;
            }
        }
        let naming = match &self.naming {
            Some(Naming::Picking { id, .. } | Naming::Typing { id, .. }) => Some(id.clone()),
            None => None,
        };
        if naming.is_some_and(|id| self.entry(&id).is_none()) {
            self.naming = None;
        }
        self.want()
    }

    /// A recording read whole came back.
    pub fn detail_read(
        &mut self,
        id: String,
        stamp: Option<SystemTime>,
        detail: Result<Detail, String>,
    ) {
        self.asked.retain(|(i, s)| !(*i == id && *s == stamp));
        self.cache.retain(|c| c.id != id);
        self.cache.push(Cached { id, stamp, detail });
        if self.cache.len() > CACHED {
            self.cache.remove(0);
        }
    }

    /// Whether an action for `id` was sent and not answered yet.
    pub fn is_busy(&self, id: &str) -> bool {
        self.busy.iter().any(|busy| busy == id)
    }

    /// An action for `id` was sent.
    pub fn sent(&mut self, id: &str) {
        self.busy.push(id.to_string());
    }

    /// The daemon answered `act`. A deleted recording leaves the list at
    /// once, its transcript with it, rather than at the next read; the one
    /// that takes its place is read whole if it is not yet.
    pub fn acted(&mut self, act: &Act, done: bool) -> Option<Wanted> {
        self.busy.retain(|busy| *busy != act.id);
        if !done || act.action != RecordingAction::Delete {
            return None;
        }
        if self.open.as_ref().is_some_and(|r| r.id == act.id) {
            self.open = None;
        }
        if let Some(Ok(items)) = &mut self.folder {
            items.retain(|item| item.id != act.id);
        }
        self.refocus();
        self.want()
    }

    /// `entry` read whole: at its stamp, else an earlier read while the new
    /// one comes.
    fn detail(&self, entry: &Item) -> Option<&Result<Detail, String>> {
        self.cache
            .iter()
            .rev()
            .find(|c| c.id == entry.id && c.stamp == entry.stamp)
            .or_else(|| self.cache.iter().rev().find(|c| c.id == entry.id))
            .map(|c| &c.detail)
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.refocus();
    }

    /// Handle `action`; `summary_on` is whether `[recording] summary`
    /// follows a retry with a new summary.
    pub fn key(&mut self, action: Action, summary_on: bool) -> Reply {
        if self.naming.is_some() {
            return self.naming_key(action);
        }
        if self.typing.is_some() {
            return self.filter_key(action);
        }
        if !self.built {
            return Reply {
                leave: action == Action::Back,
                ..Reply::default()
            };
        }
        match action {
            Action::Back if self.open.is_some() => {
                self.open = None;
                Reply::default()
            }
            Action::Back if !self.filter.is_empty() => {
                self.set_filter(String::new());
                Reply {
                    read: self.want(),
                    ..Reply::default()
                }
            }
            Action::Back => Reply {
                leave: true,
                ..Reply::default()
            },
            Action::Filter if self.open.is_none() => {
                self.typing = Some(Input::new(&self.filter, false));
                Reply::default()
            }
            Action::Up
            | Action::Down
            | Action::First
            | Action::Last
            | Action::PageUp
            | Action::PageDown
            | Action::HalfUp
            | Action::HalfDown => {
                if self.open.is_some() {
                    self.scroll(action);
                    return Reply::default();
                }
                let last = self.visible().len().saturating_sub(1);
                let at = self.cursor;
                self.select(match action {
                    Action::Up => at.saturating_sub(1),
                    Action::Down => (at + 1).min(last),
                    Action::First => 0,
                    Action::Last => last,
                    Action::PageUp => at.saturating_sub(PAGE),
                    Action::PageDown => (at + PAGE).min(last),
                    Action::HalfUp => at.saturating_sub(PAGE / 2),
                    _ => (at + PAGE / 2).min(last),
                });
                Reply {
                    read: self.want(),
                    ..Reply::default()
                }
            }
            Action::Enter if self.open.is_none() => {
                let Some(id) = self.focused().map(|e| e.id.clone()) else {
                    return Reply::default();
                };
                self.open = Some(Reading { id, scroll: 0 });
                self.shown.set((usize::MAX, PAGE));
                Reply {
                    read: self.want(),
                    ..Reply::default()
                }
            }
            Action::Retry => self.retry(summary_on),
            Action::Copy => self.act(RecordingAction::Copy),
            Action::Summarize => self.act(RecordingAction::Summarize),
            Action::Name => self.start_naming(),
            Action::Delete => self.ask_delete(),
            _ => Reply::default(),
        }
    }

    /// Scroll the open transcript, within what the last draw had.
    fn scroll(&mut self, action: Action) {
        let (max, height) = self.shown.get();
        let page = height.max(1);
        let half = (page / 2).max(1);
        let Some(reading) = self.open.as_mut() else {
            return;
        };
        let at = reading.scroll.min(max);
        reading.scroll = match action {
            Action::Up => at.saturating_sub(1),
            Action::Down => at.saturating_add(1).min(max),
            Action::First => 0,
            Action::Last => max,
            Action::PageUp => at.saturating_sub(page),
            Action::PageDown => at.saturating_add(page).min(max),
            Action::HalfUp => at.saturating_sub(half),
            _ => at.saturating_add(half).min(max),
        };
    }

    /// `/`: the filter is typed below the frame and applies as it is typed;
    /// `Enter` keeps it, `Esc` clears it.
    fn filter_key(&mut self, action: Action) -> Reply {
        let input = self.typing.as_mut().expect("typing the filter");
        match action {
            Action::Enter => {
                let kept = input.text().trim().to_string();
                self.typing = None;
                self.set_filter(kept);
            }
            Action::Back => {
                self.typing = None;
                self.set_filter(String::new());
            }
            other => {
                input.apply(other);
                let text = input.text();
                self.set_filter(text.trim().to_string());
            }
        }
        Reply {
            read: self.want(),
            ..Reply::default()
        }
    }

    fn act(&self, action: RecordingAction) -> Reply {
        match self.target() {
            Some(entry) => Reply {
                act: Some(Act::on(entry, action)),
                ..Reply::default()
            },
            None => Reply::default(),
        }
    }

    /// `r`: a recording without a transcript (failed, waiting) is retried at
    /// once; a transcribed one is asked about first, since a retry replaces
    /// its transcript. One with no audio left is told so, with nothing to
    /// ask. One in progress or queued goes to the daemon, which says why not.
    fn retry(&self, summary_on: bool) -> Reply {
        let Some(entry) = self.target() else {
            return Reply::default();
        };
        let stopped = matches!(entry.phase, Phase::Done | Phase::Failed | Phase::Waiting);
        if stopped && !entry.audio {
            return Reply::say(
                Tone::Warn,
                format!(
                    "{} has no audio left, so it cannot be transcribed again",
                    entry.label()
                ),
            );
        }
        let act = Act::on(entry, RecordingAction::Retry);
        if entry.phase == Phase::Done {
            return Reply {
                dialog: Some(Dialog::RetryRecording {
                    act,
                    lines: retry_question(entry, summary_on),
                }),
                ..Reply::default()
            };
        }
        Reply {
            act: Some(act),
            ..Reply::default()
        }
    }

    /// `d`: asked first, no by default.
    fn ask_delete(&self) -> Reply {
        let Some(entry) = self.target() else {
            return Reply::default();
        };
        let duration = entry.secs.map(duration_label);
        let question = delete_question(
            entry.title.as_deref(),
            &entry.id,
            duration.as_deref(),
            Holds {
                audio: entry.audio,
                transcript: entry.transcript,
                summary: entry.summary,
            },
        );
        Reply {
            dialog: Some(Dialog::DeleteRecording {
                act: Act::on(entry, RecordingAction::Delete),
                question,
            }),
            ..Reply::default()
        }
    }

    /// `n`: pick the speaker of a transcribed recording, then type its name.
    fn start_naming(&mut self) -> Reply {
        let Some(entry) = self.target() else {
            return Reply::default();
        };
        if entry.phase != Phase::Done {
            return Reply::say(
                Tone::Warn,
                format!(
                    "{} has no transcript yet: name its speakers once it is transcribed",
                    entry.label()
                ),
            );
        }
        if entry.speakers.is_empty() {
            return Reply::say(
                Tone::Muted,
                format!("{} has no speakers to name", entry.label()),
            );
        }
        let picks = entry
            .speakers
            .iter()
            .map(|voice| Pick {
                value: voice.id.clone(),
                label: voice.id.clone(),
                about: voice.name.clone(),
                note: None,
                refused: None,
            })
            .collect();
        let options = Options {
            picks,
            unset: None,
            open: false,
            loading: Loading::Done,
        };
        self.naming = Some(Naming::Picking {
            id: entry.id.clone(),
            label: entry.label(),
            picker: Picker::new("speaker", options, None, false),
        });
        Reply::default()
    }

    fn naming_key(&mut self, action: Action) -> Reply {
        match self.naming.take().expect("naming") {
            Naming::Picking {
                id,
                label,
                mut picker,
            } => match picker.key(action) {
                Answer::Picked(Picked::Value(speaker)) => {
                    let current = self
                        .entry(&id)
                        .and_then(|e| e.speakers.iter().find(|v| v.id == speaker))
                        .map(|v| v.name.clone())
                        .unwrap_or_default();
                    self.naming = Some(Naming::Typing {
                        id,
                        label,
                        speaker,
                        input: Input::new(&current, false),
                        current,
                    });
                    Reply::default()
                }
                Answer::Closed => Reply::default(),
                Answer::Refused(why) => {
                    self.naming = Some(Naming::Picking { id, label, picker });
                    Reply::say(Tone::Warn, why)
                }
                _ => {
                    self.naming = Some(Naming::Picking { id, label, picker });
                    Reply::default()
                }
            },
            Naming::Typing {
                id,
                label,
                speaker,
                current,
                mut input,
            } => match action {
                Action::Enter => {
                    let name = input.text().trim().to_string();
                    if name == current {
                        return Reply::say(Tone::Muted, format!("{speaker} keeps its name"));
                    }
                    Reply {
                        act: Some(Act {
                            action: RecordingAction::Name,
                            id,
                            options: RecordingOptions {
                                speaker: Some(speaker),
                                name: Some(name),
                                exact: true,
                                ..RecordingOptions::default()
                            },
                            label,
                        }),
                        ..Reply::default()
                    }
                }
                Action::Back => Reply::default(),
                other => {
                    input.apply(other);
                    self.naming = Some(Naming::Typing {
                        id,
                        label,
                        speaker,
                        current,
                        input,
                    });
                    Reply::default()
                }
            },
        }
    }
}

/// What the body holds when it is not the list.
fn body_text(app: &App, width: usize) -> Option<Vec<(String, Tone)>> {
    let screen = &app.recordings;
    let lines = |text: &str, tone: Tone| -> Vec<(String, Tone)> {
        wrap(text, width.max(1))
            .into_iter()
            .map(|line| (line, tone))
            .collect()
    };
    if !screen.built {
        return Some(lines(MISSING, Tone::Warn));
    }
    match &screen.folder {
        None => Some(lines("looking...", Tone::Muted)),
        Some(Err(why)) => Some(lines(
            &format!("cannot read the recordings: {why}"),
            Tone::Error,
        )),
        Some(Ok(entries)) if entries.is_empty() => {
            // The hotkey as saved: the daemon binds that one.
            let hotkey = app
                .config
                .form
                .saved("hotkeys.recording")
                .and_then(toml::Value::as_str)
                .filter(|h| !h.trim().is_empty())
                .map(str::to_string);
            let text = match hotkey {
                Some(hotkey) => format!(
                    "No recordings yet: whisrs recording start, or your recording hotkey \
                     ({hotkey})"
                ),
                None => "No recordings yet: whisrs recording start, or bind one with \
                         [hotkeys] recording"
                    .to_string(),
            };
            Some(lines(&text, Tone::Muted))
        }
        Some(Ok(_)) if screen.visible().is_empty() => Some(lines(
            &format!("nothing matches /{}", screen.filter),
            Tone::Muted,
        )),
        Some(Ok(_)) => None,
    }
}

/// The header's name: the screen, with the filter kept.
fn header_name(screen: &Recordings) -> String {
    if screen.filter.is_empty() || screen.typing.is_some() {
        "recordings".to_string()
    } else {
        format!("recordings /{}", screen.filter)
    }
}

/// `text` padded with spaces to `columns` columns.
fn pad(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

/// The preview's head and a transcript's: `Standup · 32m · deepgram · 3
/// speakers`.
fn summary_line(entry: &Item, detail: Option<&Detail>, ui: &Ui) -> String {
    let mut parts = vec![entry
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "(untitled)".to_string())];
    if let Some(secs) = entry.secs {
        parts.push(duration_label(secs));
    }
    if let Some(backend) = detail.and_then(|d| d.backend.clone()) {
        parts.push(backend);
    }
    match entry.heard {
        0 => {}
        1 => parts.push("1 speaker".to_string()),
        n => parts.push(format!("{n} speakers")),
    }
    parts.join(&format!(" {} ", ui.glyphs.separator))
}

/// Why `entry` shows no transcript.
fn no_transcript(entry: &Item, detail: &Detail) -> String {
    let why = detail
        .error
        .as_deref()
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    match entry.phase {
        Phase::Recording => "Still recording: the transcript comes after the stop.".to_string(),
        Phase::Queued => "Queued for transcription.".to_string(),
        Phase::Processing => "Being transcribed.".to_string(),
        Phase::Failed => format!("Transcription failed{why}. r tries again."),
        Phase::Waiting => format!("Waiting for the network{why}. r tries now."),
        Phase::Done => "The transcript is empty.".to_string(),
    }
}

/// A transcript line, `[00:01:02] Sarah: ...`, its time muted, wrapped to
/// `max` columns, the lines after the first indented.
fn turn_lines(text: &str, max: usize, ui: &Ui) -> Vec<Line<'static>> {
    let wrapped = wrap(text, max.max(1));
    let mut first = true;
    let mut lines = Vec::new();
    let mut rest = Vec::new();
    for line in &wrapped {
        if first {
            first = false;
            let (time, said) = match line.split_once("] ") {
                Some((time, said)) if line.starts_with('[') => (format!("{time}] "), said),
                _ => (String::new(), line.as_str()),
            };
            lines.push(Line::from(vec![
                Span::styled(time, ui.theme.muted),
                Span::raw(said.to_string()),
            ]));
        } else {
            rest.push(line.as_str());
        }
    }
    if !rest.is_empty() {
        for part in wrap(&rest.join(" "), max.saturating_sub(2).max(1)) {
            lines.push(Line::from(format!("  {part}")));
        }
    }
    lines
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let screen = &app.recordings;
    if screen.built && screen.open.is_some() {
        return reader(app, f, ui);
    }
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, max);
    let two = area.width >= frame::TWO_PANES;
    let text = body_text(app, max);
    let visible = screen.visible();
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    let preview = if two && text.is_none() {
        2 + PREVIEW_TURNS
    } else {
        0
    };
    let wanted = text.as_ref().map_or(visible.len(), Vec::len) + preview + foot;
    let Some(placed) = frame::place(area, Some(wanted as u16)) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: &header_name(screen),
            daemon: app.daemon_up(),
            live: None,
        },
    );
    let room = usize::from(body.height).saturating_sub(foot);
    let line_at = |f: &mut Frame, y: usize, line: Line<'static>| {
        if y < room {
            f.render_widget(
                Paragraph::new(line),
                Rect::new(
                    body.x + 2,
                    body.y + y as u16,
                    body.width.saturating_sub(2),
                    1,
                ),
            );
        }
    };

    if let Some(text) = text {
        for (y, (part, tone)) in text.into_iter().enumerate() {
            line_at(f, y, Line::from(Span::styled(part, tone.style(ui.theme))));
        }
    } else {
        // At least three rows of the list stay when the preview is cut.
        let preview = preview.min(room.saturating_sub(visible.len().min(3)));
        let list = visible.len().min(room.saturating_sub(preview));
        draw_list(
            screen,
            &visible,
            f,
            Rect::new(body.x, body.y, body.width, list as u16),
            ui,
        );
        if preview > 2 {
            if let Some(entry) = screen.focused() {
                let lines = preview_lines(screen, entry, max, preview - 1, ui);
                for (i, line) in lines.into_iter().enumerate() {
                    line_at(f, list + 1 + i, line);
                }
            }
        }
    }
    for (i, line) in messages.into_iter().enumerate() {
        let y = body.y + (room + 1 + i) as u16;
        if y < body.bottom() {
            f.render_widget(
                line,
                Rect::new(body.x + 2, y, body.width.saturating_sub(2), 1),
            );
        }
    }
    footer(screen, f, placed.footer, ui);
    draw_naming(screen, f, placed.frame, body, ui);
}

/// The list, scrolled to the cursor, in `area`.
fn draw_list(screen: &Recordings, visible: &[usize], f: &mut Frame, area: Rect, ui: &Ui) {
    let height = usize::from(area.height);
    let offset = crate::tui::widgets::scroll(visible.len(), screen.cursor, height);
    // The gutter and its space.
    let room = usize::from(area.width).saturating_sub(2 + DATE_WIDTH);
    let tail = DURATION_WIDTH + 2 + STATUS_WIDTH;
    // A space after the title, and one before the note.
    let noted = room >= TITLE_MIN + 1 + tail + 1 + NOTE_WIDTH;
    let title_width = if noted {
        room - tail - 2 - NOTE_WIDTH
    } else {
        room.saturating_sub(tail + 1)
    };
    let typing = screen.typing.is_some();
    for (row, &index) in visible.iter().enumerate().skip(offset).take(height) {
        let entry = &screen.entries()[index];
        let (title, tone) = match entry.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(title) => (title.to_string(), Tone::Plain),
            None => ("(untitled)".to_string(), Tone::Muted),
        };
        let duration = entry.secs.map_or_else(|| "-".to_string(), duration_label);
        let mut value = Value::default();
        value.parts.push((
            pad(
                &fit(&title, title_width, ui.glyphs.ellipsis),
                title_width + 1,
            ),
            tone,
        ));
        value
            .parts
            .push((format!("{duration:>DURATION_WIDTH$}  "), Tone::Plain));
        value
            .parts
            .push((pad(entry.phase.word(), STATUS_WIDTH), entry.phase.tone()));
        if noted {
            if let Some((note, tone)) = entry.note() {
                value.parts.push((format!(" {note}"), tone));
            }
        }
        let line = Row {
            focused: row == screen.cursor && !typing,
            selected: row == screen.cursor,
            ..Row::new(&entry.date, value)
        };
        let y = area.y + (row - offset) as u16;
        line.draw(
            Rect::new(area.x, y, area.width, 1),
            f.buffer_mut(),
            ui,
            DATE_WIDTH,
        );
    }
}

/// The focused recording's preview, `height` lines at most after the blank
/// one: its head, then its first turns.
fn preview_lines(
    screen: &Recordings,
    entry: &Item,
    max: usize,
    height: usize,
    ui: &Ui,
) -> Vec<Line<'static>> {
    let detail = screen.detail(entry);
    let ok = detail.and_then(|d| d.as_ref().ok());
    let mut lines = vec![Line::from(fit(
        &summary_line(entry, ok, ui),
        max,
        ui.glyphs.ellipsis,
    ))];
    match detail {
        None => lines.push(Line::from(Span::styled("reading...", ui.theme.muted))),
        Some(Err(why)) => lines.push(Line::from(Span::styled(
            fit(why, max, ui.glyphs.ellipsis),
            ui.theme.error,
        ))),
        Some(Ok(detail)) if detail.lines.is_empty() => lines.push(Line::from(Span::styled(
            fit(&no_transcript(entry, detail), max, ui.glyphs.ellipsis),
            ui.theme.muted,
        ))),
        Some(Ok(detail)) => {
            for text in &detail.lines {
                let text = fit(text, max, ui.glyphs.ellipsis);
                lines.extend(turn_lines(&text, max, ui).into_iter().take(1));
            }
        }
    }
    lines.truncate(height);
    lines
}

/// The open transcript: its head fixed, then the summary (marked stale when
/// it is) and every turn, scrolled.
fn reader(app: &App, f: &mut Frame, ui: &Ui) {
    let screen = &app.recordings;
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, max);
    let Some(placed) = frame::place(area, None) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: "recordings",
            daemon: app.daemon_up(),
            live: None,
        },
    );
    let Some(reading) = &screen.open else {
        return;
    };
    let Some(entry) = screen.entry(&reading.id) else {
        return;
    };
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    let detail = screen.detail(entry);
    let at = |y: u16| Rect::new(body.x + 2, y, body.width.saturating_sub(2), 1);
    let head = summary_line(entry, detail.and_then(|d| d.as_ref().ok()), ui);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(&head, max, ui.glyphs.ellipsis),
            ui.theme.muted,
        ))),
        at(body.y),
    );
    let lines = transcript_lines(entry, detail, max, ui);
    let height = usize::from(body.height).saturating_sub(2 + foot);
    let most = lines.len().saturating_sub(height);
    screen.shown.set((most, height));
    let offset = reading.scroll.min(most);
    for (i, line) in lines.into_iter().skip(offset).take(height).enumerate() {
        f.render_widget(Paragraph::new(line), at(body.y + 2 + i as u16));
    }
    for (i, line) in messages.into_iter().enumerate() {
        let y = body.y + (2 + height + 1 + i) as u16;
        if y < body.bottom() {
            f.render_widget(line, at(y));
        }
    }
    footer(screen, f, placed.footer, ui);
    draw_naming(screen, f, placed.frame, body, ui);
}

/// The transcript view's lines, `max` columns wide.
fn transcript_lines(
    entry: &Item,
    detail: Option<&Result<Detail, String>>,
    max: usize,
    ui: &Ui,
) -> Vec<Line<'static>> {
    let styled = |text: String, tone: Tone| Line::from(Span::styled(text, tone.style(ui.theme)));
    let detail = match detail {
        None => return vec![styled("reading...".into(), Tone::Muted)],
        Some(Err(why)) => {
            return wrap(why, max.max(1))
                .into_iter()
                .map(|line| styled(line, Tone::Error))
                .collect()
        }
        Some(Ok(detail)) => detail,
    };
    let mut lines = Vec::new();
    if let Some(summary) = &detail.summary {
        let mut head = vec![Span::styled("Summary", ui.theme.muted)];
        if detail.stale {
            head.push(Span::styled(
                format!(" {} stale", ui.glyphs.separator),
                ui.theme.warn,
            ));
        }
        lines.push(Line::from(head));
        for part in summary.lines() {
            let heading = part.trim_start().starts_with('#');
            let part = part.trim_start_matches(|c: char| c == '#' || c.is_whitespace());
            if part.trim().is_empty() {
                lines.push(Line::default());
                continue;
            }
            let tone = if heading { Tone::Muted } else { Tone::Plain };
            lines.extend(
                wrap(part, max.max(1))
                    .into_iter()
                    .map(|line| styled(line, tone)),
            );
        }
        lines.push(Line::default());
        lines.push(styled("Transcript".into(), Tone::Muted));
    }
    if detail.lines.is_empty() {
        lines.extend(
            wrap(&no_transcript(entry, detail), max.max(1))
                .into_iter()
                .map(|line| styled(line, Tone::Muted)),
        );
    }
    for text in &detail.lines {
        lines.extend(turn_lines(text, max, ui));
    }
    lines
}

/// The speaker picker or the name input, over the body, the screen behind
/// dimmed.
fn draw_naming(screen: &Recordings, f: &mut Frame, frame_rect: Rect, body: Rect, ui: &Ui) {
    let Some(naming) = &screen.naming else {
        return;
    };
    dialog::dim(f.buffer_mut(), frame_rect, ui);
    match naming {
        Naming::Picking { picker, .. } => picker.draw(f, body, ui),
        Naming::Typing {
            speaker,
            current,
            input,
            ..
        } => {
            let rect = Rect::new(body.x, body.y, body.width, 4.min(body.height));
            let inner = dialog::boxed(f, rect, ui, &format!("name {speaker}"));
            if inner.height == 0 {
                return;
            }
            let mut spans = vec![Span::styled(
                format!("{} ", ui.glyphs.prompt),
                ui.theme.accent,
            )];
            spans.extend(input.spans(ui, usize::from(inner.width.saturating_sub(2))));
            f.render_widget(Line::from(spans), Rect { height: 1, ..inner });
            if inner.height > 1 {
                let note = format!("  now {current}; empty clears the name you gave");
                f.render_widget(
                    Line::from(Span::styled(
                        fit(&note, usize::from(inner.width), ui.glyphs.ellipsis),
                        ui.theme.muted,
                    )),
                    Rect::new(inner.x, inner.y + 1, inner.width, 1),
                );
            }
        }
    }
}

fn footer(screen: &Recordings, f: &mut Frame, rect: Rect, ui: &Ui) {
    let hints: &[(&str, &str)] = match (&screen.naming, &screen.typing) {
        (Some(Naming::Picking { .. }), _) => &[
            ("type", "to filter"),
            ("enter", "pick"),
            ("esc", "cancel"),
            ("?", "help"),
        ],
        (Some(Naming::Typing { .. }), _) => {
            &[("enter", "keep"), ("esc", "cancel"), ("ctrl+u", "clear")]
        }
        (None, Some(input)) => {
            let mut spans = vec![Span::raw("  "), Span::styled("/", ui.theme.accent)];
            spans.extend(input.spans(ui, 24));
            spans.push(Span::styled(
                format!("  enter keep {} esc clear", ui.glyphs.separator),
                ui.theme.muted,
            ));
            f.render_widget(Line::from(spans), rect);
            return;
        }
        // `? help` last, as the one always shown (it lists the rest), and
        // `esc back` before it, the first to go: Esc is back everywhere.
        _ if !screen.built => &[("esc", "back"), ("q", "quit")],
        _ if screen.open.is_some() => &[
            ("j/k", "scroll"),
            ("r", "retry"),
            ("c", "copy"),
            ("n", "name"),
            ("S", "summary"),
            ("d", "delete"),
            ("ctrl+d/u", "page"),
            ("esc", "back"),
            ("?", "help"),
        ],
        _ if screen.entries().is_empty() => &[("esc", "back"), ("?", "help")],
        _ => &[
            ("enter", "open"),
            ("r", "retry"),
            ("c", "copy"),
            ("n", "name"),
            ("S", "summary"),
            ("d", "delete"),
            ("/", "filter"),
            ("esc", "back"),
            ("?", "help"),
        ],
    };
    frame::footer(f, rect, ui, hints);
}
