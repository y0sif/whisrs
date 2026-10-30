//! Recordings driven by keys through the pure update, on recordings made in
//! a temp folder and read the way the worker reads them, at a fixed time.

use std::cell::Cell as Flag;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use whisrs::recording::render::render_markdown;
use whisrs::recording::{
    store, RecordingAction, RecordingMeta, RecordingOptions, RecordingPhase, RecordingState,
    RecordingStatus, Speaker, SummaryRequest, TrackKind, TrackMeta, TranscriptSegment,
};

use super::*;
use crate::tui::app::{Dialog, Effect, Msg, Screen};
use crate::tui::testing::{
    assert_snapshot, chord, draw, key, mockup, mockup_with, press, rows, type_text, Fixture,
};
use crate::tui::worker::DaemonView;

const SIZES: [(u16, u16); 5] = [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)];

fn at(month: u32, day: u32, h: u32, m: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, month, day, h, m, 0).unwrap()
}

/// When every read in these tests happens.
fn now() -> DateTime<Local> {
    at(10, 30, 12, 0)
}

fn voice(id: &str, name: &str, channel: TrackKind) -> Speaker {
    Speaker {
        id: id.into(),
        name: name.into(),
        source: if id == "me" { "self" } else { "none" }.into(),
        evidence: None,
        talk_ms: 0,
        channel: Some(channel),
        minor: false,
    }
}

fn said(start_s: u64, speaker: &str, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        start_ms: start_s * 1000,
        end_ms: start_s * 1000 + 2000,
        speaker: speaker.into(),
        channel: if speaker == "me" {
            TrackKind::Mic
        } else {
            TrackKind::System
        },
        text: text.into(),
    }
}

/// A recording in `root` that started at `started`, `edit` making it what
/// the test needs; its mic track is on disk with `audio`, and a done one's
/// transcript beside it.
fn make(
    root: &Path,
    started: DateTime<Local>,
    title: Option<&str>,
    audio: bool,
    edit: impl FnOnce(&mut RecordingMeta),
) -> PathBuf {
    let (id, dir) = store::create(root, started, title).unwrap();
    let mut meta = RecordingMeta::new(id.clone(), title.map(String::from), started);
    meta.tracks = vec![TrackMeta {
        kind: TrackKind::Mic,
        file: "audio/mic.wav".into(),
        device: "default".into(),
        offset_ms: 0,
        samples: 0,
    }];
    edit(&mut meta);
    store::write_meta(&dir, &meta).unwrap();
    if audio {
        std::fs::write(store::audio_path(&dir, TrackKind::Mic), b"RIFF").unwrap();
    }
    if meta.status == RecordingState::Done {
        std::fs::write(store::transcript_path(&dir, &id), render_markdown(&meta)).unwrap();
    }
    dir
}

/// The mockup's three recordings: a standup with a summary and three
/// speakers, a 1:1 with two, and an untitled one that failed with its
/// audio left.
fn three(root: &Path) -> [PathBuf; 3] {
    let standup = make(root, at(10, 27, 14, 2), Some("Standup"), true, |meta| {
        meta.status = RecordingState::Done;
        meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(32));
        meta.backend = Some("deepgram".into());
        meta.speakers = vec![
            voice("me", "Yosif", TrackKind::Mic),
            voice("S1", "Sarah", TrackKind::System),
            voice("S2", "Speaker 2", TrackKind::System),
        ];
        meta.segments = vec![
            said(3, "me", "I think we can ship the TUI this week."),
            said(9, "S1", "Do we need the profiles bit first?"),
            said(14, "S2", "No, profiles come after it, in their own branch."),
            said(20, "me", "Then the recordings screen goes in today."),
        ];
        meta.summary = Some(
            "The TUI ships this week; profiles follow.\n\n### Next steps\n- [ ] Merge the \
             recordings screen"
                .into(),
        );
    });
    let planning = make(
        root,
        at(10, 26, 10, 15),
        Some("1:1 planning"),
        true,
        |meta| {
            meta.status = RecordingState::Done;
            meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(48));
            meta.backend = Some("groq".into());
            meta.speakers = vec![
                voice("me", "Yosif", TrackKind::Mic),
                voice("others", "Omar", TrackKind::System),
            ];
            meta.segments = vec![
                said(1, "me", "What is left for the release?"),
                said(5, "others", "The docs, and the man page."),
            ];
        },
    );
    let failed = make(root, at(10, 25, 9, 0), None, true, |meta| {
        meta.status = RecordingState::Failed;
        meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(5));
        meta.error = Some("the groq API answered 401: invalid API key".into());
    });
    [standup, planning, failed]
}

/// The folder as the worker reads it, with the daemon's `status`.
fn listing(root: &Path, status: Option<RecordingStatus>) -> Result<Vec<Item>, String> {
    read::folder(root, now(), || status, &mut Default::default())
}

/// The options every action of the TUI carries: the whole id only.
fn exact() -> RecordingOptions {
    RecordingOptions {
        exact: true,
        ..RecordingOptions::default()
    }
}

/// Feed the folder read to the app, and the preview it asks for.
fn refresh(fixture: &mut Fixture, root: &Path) {
    let effect = fixture.app.update(Msg::Recordings(listing(root, None)));
    answer(fixture, effect);
}

/// Answer the reads `effect` asks for, as the worker would.
fn answer(fixture: &mut Fixture, effect: Option<Effect>) {
    let effects = match effect {
        Some(Effect::All(effects)) => effects,
        Some(effect) => vec![effect],
        None => Vec::new(),
    };
    for effect in effects {
        if let Effect::ReadRecording(wanted) = effect {
            let detail = read::detail(&wanted.dir);
            fixture.app.update(Msg::RecordingRead {
                id: wanted.id,
                stamp: wanted.stamp,
                detail,
            });
        }
    }
}

/// Press `keys`, answering the reads they ask for; the other effects.
fn keys(fixture: &mut Fixture, keys: &str) -> Vec<Effect> {
    let mut left = Vec::new();
    for effect in press(&mut fixture.app, keys) {
        match effect {
            Effect::ReadRecording(_) => answer(fixture, Some(effect)),
            other => left.push(other),
        }
    }
    left
}

/// Recordings open from Home on the mockup's config, `root` read.
fn open(root: &Path) -> Fixture {
    open_with(root, "")
}

/// [`open`] on the mockup's config with `extra` after it.
fn open_with(root: &Path, extra: &str) -> Fixture {
    let mut fixture = mockup_with(extra);
    press(&mut fixture.app, "\x1bjj");
    assert_eq!(press(&mut fixture.app, "\n"), [Effect::WatchRecordings]);
    assert_eq!(fixture.app.screen(), Screen::Recordings);
    refresh(&mut fixture, root);
    fixture
}

fn status(fixture: &Fixture) -> Notice {
    fixture.app.status.clone().expect("a status")
}

fn ids(fixture: &Fixture) -> Vec<String> {
    let screen = &fixture.app.recordings;
    screen
        .visible()
        .into_iter()
        .map(|i| screen.entries()[i].id.clone())
        .collect()
}

fn focused_id(fixture: &Fixture) -> String {
    fixture.app.recordings.focused().unwrap().id.clone()
}

/// The ids of `three`, newest first.
fn three_ids(dirs: &[PathBuf; 3]) -> [String; 3] {
    dirs.clone()
        .map(|dir| dir.file_name().unwrap().to_str().unwrap().to_string())
}

/// Newest first, each with its status, its duration and one note; a
/// recording in flight is what the daemon says it is.
#[test]
fn the_list_is_newest_first_with_a_note_each() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = three(tmp.path());
    let items = listing(tmp.path(), None).unwrap();
    let ids = three_ids(&dirs);
    assert_eq!(
        items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(items[0].date, "Oct 27 14:02");
    assert_eq!(items[0].secs, Some(32 * 60));
    assert_eq!(items[0].heard, 3);
    assert!(items[0].transcript && items[0].audio);
    let notes: Vec<Option<String>> = items.iter().map(|i| i.note().map(|n| n.0)).collect();
    assert_eq!(
        notes,
        [
            Some("summary".to_string()),
            Some("2 speakers".to_string()),
            Some("retry available".to_string())
        ]
    );
    assert_eq!(items[2].phase, Phase::Failed);
    assert_eq!(duration_label(45), "45s");
    assert_eq!(duration_label(3725), "1h 02m");
    assert_eq!(date_label(at(10, 5, 9, 0), now()), "Oct  5 09:00");
    let last_year = Local.with_ymd_and_hms(2025, 10, 5, 9, 0, 0).unwrap();
    assert_eq!(date_label(last_year, now()), "Oct  5 2025");

    // Nothing in flight: the daemon is not asked.
    let asked = Flag::new(false);
    read::folder(
        tmp.path(),
        now(),
        || {
            asked.set(true);
            None
        },
        &mut Default::default(),
    )
    .unwrap();
    assert!(!asked.get());

    // A live recording counts up; two stopped ones are processing and
    // queued; a done one has its summary made.
    let live = make(tmp.path(), at(10, 30, 11, 20), Some("Live"), true, |_| {});
    let processing = make(tmp.path(), at(10, 30, 9, 0), Some("Busy"), true, |meta| {
        meta.status = RecordingState::Processing;
        meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(10));
    });
    let queued = make(tmp.path(), at(10, 30, 8, 0), Some("Next"), true, |meta| {
        meta.status = RecordingState::Processing;
        meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(10));
    });
    let mut done = store::read_meta(&dirs[1]).unwrap();
    done.summary_pending = Some(SummaryRequest::Asked);
    store::write_meta(&dirs[1], &done).unwrap();
    let id = |dir: &Path| store::read_meta(dir).unwrap().id;
    let status = RecordingStatus {
        phase: RecordingPhase::Recording,
        id: Some(id(&live)),
        queued: vec![id(&queued), done.id.clone()],
        ..RecordingStatus::default()
    };
    let items = listing(tmp.path(), Some(status)).unwrap();
    let find = |want: &str| items.iter().find(|i| i.id == want).unwrap();
    assert_eq!(find(&id(&live)).phase, Phase::Recording);
    assert_eq!(find(&id(&live)).secs, Some(40 * 60), "counts up to now");
    assert_eq!(find(&id(&processing)).phase, Phase::Processing);
    assert_eq!(find(&id(&queued)).phase, Phase::Queued);
    let planning = find(&done.id);
    assert_eq!(planning.phase, Phase::Done);
    assert_eq!(planning.note().unwrap().0, "summary queued");
    // The daemon down: what the disk says.
    let items = listing(tmp.path(), None).unwrap();
    assert_eq!(
        items.iter().find(|i| i.id == id(&queued)).unwrap().phase,
        Phase::Processing
    );
}

#[test]
fn the_delete_question_names_what_is_lost() {
    let all = Holds {
        audio: true,
        transcript: true,
        summary: true,
    };
    assert_eq!(
        delete_question(Some("Standup"), "2026-10-27_1402_standup", Some("32m"), all),
        "Delete \"Standup\" (32m): audio, transcript and summary. This cannot be undone."
    );
    let audio = Holds {
        audio: true,
        transcript: false,
        summary: false,
    };
    assert_eq!(
        delete_question(None, "2026-10-25_0900", Some("5m"), audio),
        "Delete 2026-10-25_0900 (5m): audio. This cannot be undone."
    );
    let nothing = Holds {
        audio: false,
        ..audio
    };
    assert_eq!(
        delete_question(Some(" "), "2026-10-25_0900", None, nothing),
        "Delete 2026-10-25_0900: its folder. This cannot be undone."
    );
}

/// `/` finds a title, an id, a speaker's name or the date as shown, in any
/// case; Esc clears the filter first, then leaves.
#[test]
fn the_filter_finds_titles_ids_speakers_and_dates() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    let filtered = |fixture: &mut Fixture, typed: &str| {
        keys(fixture, "/");
        assert_eq!(fixture.app.recordings.mode(), Mode::Text);
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, typed);
        let shown = ids(fixture);
        keys(fixture, "\n");
        shown
    };
    assert_eq!(filtered(&mut fixture, "SARAH"), [ids_of[0].clone()]);
    assert_eq!(filtered(&mut fixture, "oct 26"), [ids_of[1].clone()]);
    assert_eq!(filtered(&mut fixture, "2026-10-25"), [ids_of[2].clone()]);
    assert_eq!(filtered(&mut fixture, "planning omar"), [ids_of[1].clone()]);
    assert!(filtered(&mut fixture, "nobody").is_empty());
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(drawn.iter().any(|r| r.contains("nothing matches /nobody")));
    assert!(drawn.iter().any(|r| r.contains("recordings /nobody")));

    // Esc while typing clears it.
    keys(&mut fixture, "/x\x1b");
    assert!(fixture.app.recordings.filter.is_empty());
    filtered(&mut fixture, "standup");
    assert_eq!(keys(&mut fixture, "\x1b"), Vec::<Effect>::new());
    assert!(fixture.app.recordings.filter.is_empty());
    assert_eq!(ids(&fixture).len(), 3);
    assert_eq!(fixture.app.screen(), Screen::Recordings);
    assert_eq!(keys(&mut fixture, "\x1b"), [Effect::StopRecordings]);
    assert_eq!(fixture.app.screen(), Screen::Home);
}

/// `d` asks first, keep by default: Enter, n and Esc all keep; y sends the
/// delete for the recording's full id.
#[test]
fn a_delete_asks_and_keeps_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    for keep in ["\n", "n", "\x1b", "j"] {
        assert!(keys(&mut fixture, "d").is_empty());
        let Some(Dialog::DeleteRecording { question, .. }) = &fixture.app.dialog else {
            panic!("no delete dialog: {:?}", fixture.app.dialog);
        };
        assert_eq!(
            question,
            "Delete \"Standup\" (32m): audio, transcript and summary. This cannot be undone."
        );
        assert!(keys(&mut fixture, keep).is_empty(), "{keep:?}");
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(status(&fixture).text, "kept \"Standup\"");
    }
    assert_eq!(focused_id(&fixture), ids_of[0], "the cursor did not move");
    keys(&mut fixture, "d");
    let sent = keys(&mut fixture, "y");
    assert_eq!(
        sent,
        [Effect::Recording(Act {
            action: RecordingAction::Delete,
            id: ids_of[0].clone(),
            options: exact(),
            label: "\"Standup\"".into(),
        })]
    );
    assert_eq!(status(&fixture).text, "deleting \"Standup\"");
    // A second d before the answer is held back.
    assert!(keys(&mut fixture, "d").is_empty());
    assert_eq!(fixture.app.dialog, None);
    assert_eq!(
        status(&fixture).text,
        "still working on \"Standup\": wait for whisrsd's answer"
    );
    // Once done, the row goes at once, the next one in its place and read
    // whole, and the list is read again.
    let Effect::Recording(act) = sent[0].clone() else {
        unreachable!()
    };
    std::fs::remove_dir_all(tmp.path().join(&ids_of[0])).unwrap();
    let after = fixture.app.update(Msg::Acted {
        act,
        answer: Ok(()),
    });
    let Some(Effect::All(effects)) = &after else {
        panic!("{after:?}");
    };
    assert_eq!(effects[0], Effect::RefreshRecordings);
    assert!(matches!(&effects[1], Effect::ReadRecording(w) if w.id == ids_of[1]));
    assert_eq!(status(&fixture).text, "deleted \"Standup\"");
    assert_eq!(ids(&fixture), ids_of[1..], "gone before the refresh");
    assert_eq!(
        focused_id(&fixture),
        ids_of[1],
        "the next one takes its place"
    );
    refresh(&mut fixture, tmp.path());
    assert_eq!(ids(&fixture), ids_of[1..]);
}

/// `r` on a failed recording retries at once; on a transcribed one it asks
/// first, keep by default, and says what a retry replaces and keeps.
#[test]
fn a_retry_asks_first_only_when_there_is_a_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    assert!(keys(&mut fixture, "r").is_empty());
    let Some(Dialog::RetryRecording { lines, .. }) = &fixture.app.dialog else {
        panic!("no retry dialog");
    };
    assert_eq!(
        lines[0],
        "Transcribe \"Standup\" (32m) again from its audio?"
    );
    assert_eq!(
        lines[1],
        "This replaces its transcript and the speakers found in it. Names you gave follow \
         their voices; one whose voice is not found again is dropped, and whisrs says so."
    );
    assert_eq!(
        lines[2],
        "The summary stays, marked stale until you summarize it again."
    );
    assert!(keys(&mut fixture, "\n").is_empty());
    assert_eq!(status(&fixture).text, "\"Standup\" keeps its transcript");
    keys(&mut fixture, "r");
    assert_eq!(
        keys(&mut fixture, "y"),
        [Effect::Recording(Act {
            action: RecordingAction::Retry,
            id: ids_of[0].clone(),
            options: exact(),
            label: "\"Standup\"".into(),
        })]
    );

    // A recording without a summary has nothing to say of one.
    let item = fixture.app.recordings.entries()[1].clone();
    assert_eq!(retry_question(&item, false).len(), 2);

    // The failed one: at once.
    keys(&mut fixture, "G");
    assert_eq!(focused_id(&fixture), ids_of[2]);
    assert_eq!(
        keys(&mut fixture, "r"),
        [Effect::Recording(Act {
            action: RecordingAction::Retry,
            id: ids_of[2].clone(),
            options: exact(),
            label: ids_of[2].clone(),
        })]
    );
    assert_eq!(fixture.app.dialog, None);
}

/// `n` picks a speaker, then types its name in an input that starts on
/// the name it has; Enter sends both, and an empty name clears the one
/// the user gave.
#[test]
fn the_name_flow_sends_the_speaker_and_the_name() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "n");
    assert_eq!(fixture.app.recordings.mode(), Mode::Picker);
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(drawn
        .iter()
        .any(|r| r.contains("S1") && r.contains("Sarah")));
    key(&mut fixture.app, KeyCode::Down);
    assert!(keys(&mut fixture, "\n").is_empty());
    assert_eq!(fixture.app.recordings.mode(), Mode::Text);
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(drawn.iter().any(|r| r.contains("name S1")), "{drawn:#?}");
    assert!(drawn.iter().any(|r| r.contains("\u{203A} Sarah")));
    type_text(&mut fixture.app, " Smith");
    let sent = keys(&mut fixture, "\n");
    assert_eq!(
        sent,
        [Effect::Recording(Act {
            action: RecordingAction::Name,
            id: ids_of[0].clone(),
            options: RecordingOptions {
                speaker: Some("S1".into()),
                name: Some("Sarah Smith".into()),
                exact: true,
                ..RecordingOptions::default()
            },
            label: "\"Standup\"".into(),
        })]
    );
    let Effect::Recording(act) = sent[0].clone() else {
        unreachable!()
    };
    fixture.app.update(Msg::Acted {
        act,
        answer: Ok(()),
    });
    assert_eq!(status(&fixture).text, "S1 is now Sarah Smith");

    // Typing to filter the picker, then an empty name: cleared.
    keys(&mut fixture, "n");
    type_text(&mut fixture.app, "speaker");
    keys(&mut fixture, "\n");
    chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let sent = keys(&mut fixture, "\n");
    let [Effect::Recording(act)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(act.options.speaker.as_deref(), Some("S2"));
    assert_eq!(act.options.name.as_deref(), Some(""));
    fixture.app.update(Msg::Acted {
        act: act.clone(),
        answer: Ok(()),
    });
    assert_eq!(status(&fixture).text, "S2 has no name of yours now");

    // The name it has, or Esc: nothing is sent.
    keys(&mut fixture, "n\n");
    assert!(keys(&mut fixture, "\n").is_empty());
    assert_eq!(status(&fixture).text, "me keeps its name");
    keys(&mut fixture, "n\x1b");
    assert_eq!(fixture.app.recordings.mode(), Mode::Read);
    keys(&mut fixture, "n\n\x1b");
    assert_eq!(fixture.app.recordings.mode(), Mode::Read);

    // Only on a transcribed recording.
    keys(&mut fixture, "G");
    assert!(keys(&mut fixture, "n").is_empty());
    assert_eq!(fixture.app.recordings.mode(), Mode::Read);
    let said = status(&fixture);
    assert_eq!(said.tone, Tone::Warn);
    assert!(said
        .text
        .contains("name its speakers once it is transcribed"));
}

/// Every read keeps the cursor on its recording: a newer one coming in
/// does not move it, and one deleted elsewhere gives its place to the next.
#[test]
fn a_refresh_keeps_the_cursor_on_its_id() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "j");
    assert_eq!(focused_id(&fixture), ids_of[1]);
    make(tmp.path(), at(10, 30, 9, 0), Some("Newer"), true, |meta| {
        meta.status = RecordingState::Failed;
    });
    refresh(&mut fixture, tmp.path());
    assert_eq!(ids(&fixture).len(), 4);
    assert_eq!(focused_id(&fixture), ids_of[1], "still on the 1:1");
    assert_eq!(fixture.app.recordings.cursor, 2);
    std::fs::remove_dir_all(tmp.path().join(&ids_of[1])).unwrap();
    refresh(&mut fixture, tmp.path());
    assert_eq!(focused_id(&fixture), ids_of[2]);
    // The preview is read again when its recording.json changes.
    keys(&mut fixture, "k");
    let wanted = fixture
        .app
        .update(Msg::Recordings(listing(tmp.path(), None)));
    assert_eq!(wanted, None, "read once at its stamp");
}

/// The daemon's refusal is the status line, as it says it, and the list
/// is read again after any answer.
#[test]
fn a_refused_action_shows_the_daemons_message() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "G");
    let message = format!(
        "recording {} has no transcript yet (status failed). Retry with: whisrs recording \
         retry {}",
        ids_of[2], ids_of[2]
    );
    for (letter, action) in [
        ("c", RecordingAction::Copy),
        ("S", RecordingAction::Summarize),
    ] {
        let sent = keys(&mut fixture, letter);
        let [Effect::Recording(act)] = sent.as_slice() else {
            panic!("{letter}: {sent:?}");
        };
        assert_eq!((act.action, act.id.as_str()), (action, ids_of[2].as_str()));
        assert!(act.options.exact, "by its whole id");
        assert_eq!(
            fixture.app.update(Msg::Acted {
                act: act.clone(),
                answer: Err(message.clone()),
            }),
            Some(Effect::RefreshRecordings)
        );
        let said = status(&fixture);
        assert_eq!((said.tone, said.text), (Tone::Error, message.clone()));
    }
}

/// One action at a time a recording: a second one while whisrsd has not
/// answered the first says it is still working, and goes once it answered;
/// another recording is not held back meanwhile.
#[test]
fn a_second_action_waits_for_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    let sent = keys(&mut fixture, "c");
    let [Effect::Recording(act)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    for letter in ["c", "S", "d", "r"] {
        assert!(keys(&mut fixture, letter).is_empty(), "{letter}");
        assert_eq!(fixture.app.dialog, None, "{letter}");
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Warn);
        assert_eq!(
            said.text,
            "still working on \"Standup\": wait for whisrsd's answer"
        );
    }
    keys(&mut fixture, "j");
    assert_eq!(keys(&mut fixture, "c").len(), 1, "another recording goes");
    keys(&mut fixture, "k");
    fixture.app.update(Msg::Acted {
        act: act.clone(),
        answer: Ok(()),
    });
    assert_eq!(keys(&mut fixture, "c").len(), 1, "answered: it goes again");
    assert_eq!(ids_of.len(), 3);
}

/// `d` on a recording still recording or being transcribed says why not,
/// with no question and nothing sent.
#[test]
fn a_busy_recording_is_not_asked_about() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let live = make(tmp.path(), at(10, 30, 11, 20), Some("Live"), true, |_| {});
    let busy = make(tmp.path(), at(10, 30, 11, 0), Some("Busy"), true, |meta| {
        meta.status = RecordingState::Processing;
        meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(10));
    });
    let id = |dir: &Path| store::read_meta(dir).unwrap().id;
    let mut fixture = open(tmp.path());
    let status_now = RecordingStatus {
        phase: RecordingPhase::Recording,
        id: Some(id(&live)),
        queued: vec![id(&busy)],
        ..RecordingStatus::default()
    };
    let effect = fixture
        .app
        .update(Msg::Recordings(listing(tmp.path(), Some(status_now))));
    answer(&mut fixture, effect);
    for (at, phase, why) in [
        (
            "g",
            Phase::Recording,
            "\"Live\" is being recorded: stop it first, or cancel it (whisrs recording \
             cancel), which deletes it",
        ),
        (
            "j",
            Phase::Queued,
            "\"Busy\" is being transcribed: delete it once that is done",
        ),
    ] {
        keys(&mut fixture, at);
        assert_eq!(fixture.app.recordings.focused().unwrap().phase, phase);
        assert!(keys(&mut fixture, "d").is_empty());
        assert_eq!(fixture.app.dialog, None);
        let said = status(&fixture);
        assert_eq!((said.tone, said.text.as_str()), (Tone::Warn, why));
    }
}

/// A recording with no audio left is told so at `r`, done or failed, with
/// no question and nothing sent.
#[test]
fn a_retry_without_audio_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = three(tmp.path());
    for dir in [&dirs[0], &dirs[2]] {
        std::fs::remove_file(dir.join("audio/mic.wav")).unwrap();
    }
    let mut fixture = open(tmp.path());
    assert!(!fixture.app.recordings.entries()[0].audio);
    for at in ["", "G"] {
        keys(&mut fixture, at);
        assert!(keys(&mut fixture, "r").is_empty(), "{at:?}");
        assert_eq!(fixture.app.dialog, None);
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Warn);
        assert!(
            said.text
                .ends_with("has no audio left, so it cannot be transcribed again"),
            "{}",
            said.text
        );
    }
    let failed = fixture.app.recordings.focused().unwrap().clone();
    assert_eq!(failed.note().unwrap().0, "no audio left");
}

/// The retry question reads `[recording] summary` as saved, not as edited
/// and unsaved: the daemon runs on the file.
#[test]
fn the_retry_question_follows_the_saved_config() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let mut fixture = open(tmp.path());
    fixture
        .app
        .config
        .form
        .set("recording.summary", Some(toml::Value::Boolean(true)))
        .unwrap();
    assert!(fixture.app.config.form.is_dirty());
    keys(&mut fixture, "r");
    let Some(Dialog::RetryRecording { lines, .. }) = &fixture.app.dialog else {
        panic!("no retry dialog");
    };
    assert_eq!(
        lines[2],
        "The summary stays, marked stale until you summarize it again."
    );

    let mut on = open_with(tmp.path(), "\n[recording]\nsummary = true\n");
    keys(&mut on, "r");
    let Some(Dialog::RetryRecording { lines, .. }) = &on.app.dialog else {
        panic!("no retry dialog");
    };
    assert_eq!(
        lines[2],
        "[recording] summary is on, so a new summary follows; it may retitle the recording \
         unless you gave the title."
    );
}

/// A recording waiting for the network shows `waiting` with its note while
/// nothing retries it, and `queued` without one while the daemon does.
#[test]
fn a_waiting_recording_being_retried_is_queued() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = make(
        tmp.path(),
        at(10, 29, 9, 0),
        Some("Offline"),
        true,
        |meta| {
            meta.status = RecordingState::Waiting;
            meta.ended_at = Some(meta.started_at + chrono::Duration::minutes(3));
        },
    );
    let id = store::read_meta(&dir).unwrap().id;
    let items = listing(tmp.path(), None).unwrap();
    assert_eq!(items[0].phase, Phase::Waiting);
    assert_eq!(items[0].note().unwrap().0, "retry available");
    let asked = Flag::new(false);
    let items = read::folder(
        tmp.path(),
        now(),
        || {
            asked.set(true);
            Some(RecordingStatus {
                phase: RecordingPhase::Processing,
                id: Some(id.clone()),
                queued: vec![id.clone()],
                ..RecordingStatus::default()
            })
        },
        &mut Default::default(),
    )
    .unwrap();
    assert!(asked.get(), "a waiting one is asked about");
    assert_eq!(items[0].phase, Phase::Queued);
    assert_eq!(items[0].note(), None);
}

/// The folder read every 2 s parses a `recording.json` again only when its
/// modification time or length changed, and forgets a folder that is gone.
#[test]
fn headers_are_parsed_again_only_when_their_file_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = three(tmp.path());
    let mut parsed = Parsed::default();
    let read = |parsed: &mut Parsed| {
        read::folder(tmp.path(), now(), || None, &mut parsed.headers).unwrap()
    };
    assert_eq!(read(&mut parsed).len(), 3);
    assert_eq!(parsed.headers.len(), 3);
    // Same length, same time: the file is not read, so garbage in it goes
    // unseen.
    let file = dirs[1].join("recording.json");
    let was = std::fs::metadata(&file).unwrap();
    std::fs::write(&file, " ".repeat(was.len() as usize)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(was.modified().unwrap())
        .unwrap();
    assert_eq!(read(&mut parsed).len(), 3, "kept from the read before");
    // A new time: read again, and it no longer parses.
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(was.modified().unwrap() + std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(read(&mut parsed).len(), 2);
    assert_eq!(parsed.headers.len(), 2);
    std::fs::remove_dir_all(&dirs[0]).unwrap();
    assert_eq!(read(&mut parsed).len(), 1);
    assert_eq!(parsed.headers.len(), 1, "a folder that is gone is dropped");
}

/// With the daemon down the list still reads, and an action says the
/// daemon is not running and how to start it, without asking first.
#[test]
fn an_action_with_the_daemon_down_says_how_to_start_it() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let mut fixture = open(tmp.path());
    fixture.app.update(Msg::Daemon(DaemonView::Down));
    for letter in ["c", "d", "r", "S"] {
        assert!(keys(&mut fixture, letter).is_empty(), "{letter}");
        assert_eq!(fixture.app.dialog, None, "{letter}");
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Error);
        assert!(
            said.text.contains("whisrsd is not running. Start it with systemctl --user enable --now whisrs.service"),
            "{letter}: {}",
            said.text
        );
    }
    assert!(status(&fixture).text.starts_with("cannot summarize:"));
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(drawn.iter().any(|r| r.contains("Standup")));
}

/// Enter opens the transcript, the summary first; it scrolls, its actions
/// work on it, and Esc goes back to the list on the same row.
#[test]
fn the_transcript_opens_scrolls_and_goes_back_to_its_row() {
    let tmp = tempfile::tempdir().unwrap();
    let ids_of = three_ids(&three(tmp.path()));
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "\n");
    let drawn = rows(&draw(&fixture.app, 60, 18));
    assert!(drawn.iter().any(|r| r.contains("Summary")));
    assert!(drawn
        .iter()
        .any(|r| r.contains("[00:00:03] Yosif: I think we can ship")));
    // Down to the end and back.
    keys(&mut fixture, "G");
    let bottom = rows(&draw(&fixture.app, 60, 18));
    assert!(bottom.iter().any(|r| r.contains("goes in today.")));
    assert!(!bottom.iter().any(|r| r.contains("Summary")));
    keys(&mut fixture, "g");
    assert!(rows(&draw(&fixture.app, 60, 18))
        .iter()
        .any(|r| r.contains("Summary")));
    chord(&mut fixture.app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    let sent = keys(&mut fixture, "c");
    let [Effect::Recording(act)] = sent.as_slice() else {
        panic!("copy is not sent");
    };
    assert_eq!(act.id, ids_of[0]);
    keys(&mut fixture, "\x1b");
    assert_eq!(fixture.app.screen(), Screen::Recordings);
    assert!(fixture.app.recordings.open.is_none());
    assert_eq!(focused_id(&fixture), ids_of[0]);

    // A delete from the transcript closes it once done.
    keys(&mut fixture, "j\nd");
    let Some(Dialog::DeleteRecording { act, .. }) = fixture.app.dialog.clone() else {
        panic!("no delete dialog");
    };
    keys(&mut fixture, "y");
    fixture.app.update(Msg::Acted {
        act,
        answer: Ok(()),
    });
    assert!(fixture.app.recordings.open.is_none());
}

/// The failed one's transcript says why there is none.
#[test]
fn a_failed_recording_says_why_it_has_no_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "G\n");
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(
        drawn
            .iter()
            .any(|r| r.contains("Transcription failed: the groq API answered 401")),
        "{drawn:#?}"
    );
}

#[test]
fn recordings_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let fixture = open(tmp.path());
    for (w, h) in SIZES {
        assert_snapshot(
            &format!("recordings-{w}x{h}"),
            &rows(&draw(&fixture.app, w, h)),
        );
    }

    let mut reading = open(tmp.path());
    keys(&mut reading, "\n");
    for (w, h) in [(60, 18), (90, 30)] {
        assert_snapshot(
            &format!("recordings-transcript-{w}x{h}"),
            &rows(&draw(&reading.app, w, h)),
        );
    }

    let mut deleting = open(tmp.path());
    keys(&mut deleting, "d");
    for (w, h) in [(60, 18), (90, 30)] {
        assert_snapshot(
            &format!("recordings-delete-{w}x{h}"),
            &rows(&draw(&deleting.app, w, h)),
        );
    }

    let empty = tempfile::tempdir().unwrap();
    let fixture = open(empty.path());
    for (w, h) in [(60, 18), (90, 30)] {
        assert_snapshot(
            &format!("recordings-empty-{w}x{h}"),
            &rows(&draw(&fixture.app, w, h)),
        );
    }

    let mut missing = mockup();
    missing.app.recordings.built = false;
    press(&mut missing.app, "\x1bjj");
    assert!(press(&mut missing.app, "\n").is_empty(), "nothing to read");
    assert_snapshot(
        "recordings-missing-90x30",
        &rows(&draw(&missing.app, 90, 30)),
    );
    assert_eq!(press(&mut missing.app, "\x1b"), [Effect::StopRecordings]);
}

/// No size panics on the list, the transcript, a dialog or the picker.
#[test]
fn no_size_panics() {
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let list = open(tmp.path());
    let mut reading = open(tmp.path());
    keys(&mut reading, "\n");
    let mut naming = open(tmp.path());
    keys(&mut naming, "n");
    let mut typing = open(tmp.path());
    keys(&mut typing, "n\n");
    for fixture in [&list, &reading, &naming, &typing] {
        for w in [1, 2, 10, 40, 59, 60, 61, 70, 89, 90, 91, 120, 200, 250] {
            for h in [1, 2, 5, 17, 18, 19, 25, 30, 60] {
                draw(&fixture.app, w, h);
            }
        }
    }
}

/// Only the focused row is bold, its gutter in the accent.
#[test]
fn only_the_focused_recording_is_bold() {
    use ratatui::style::Modifier;
    let tmp = tempfile::tempdir().unwrap();
    three(tmp.path());
    let mut fixture = open(tmp.path());
    keys(&mut fixture, "j");
    let buf = draw(&fixture.app, 90, 30);
    let drawn = rows(&buf);
    let y = drawn
        .iter()
        .position(|r| r.contains("\u{258C} Oct 26 10:15"))
        .expect("the focused row") as u16;
    let bold: Vec<u16> = (0..buf.area.height)
        .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::BOLD)))
        .collect();
    assert_eq!(bold, [y]);
    let x = drawn[usize::from(y)].find('\u{258C}').unwrap();
    let x = drawn[usize::from(y)][..x].chars().count() as u16;
    assert_eq!(
        buf[(x, y)].fg,
        crate::tui::testing::theme().accent.fg.unwrap()
    );
}

/// The folder is `[recording] dir` of the config on disk, as `whisrs
/// recording list` finds it, else the one under the data folder; a blank
/// dir is no dir.
#[test]
fn the_folder_is_the_configs() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let data = tmp.path().join("data");
    let default = data.join("whisrs").join("recordings");
    assert_eq!(read::root(&config, &data), default, "no config");
    std::fs::write(&config, "[recording]\ndir = \"  \"\n").unwrap();
    assert_eq!(read::root(&config, &data), default, "blank");
    let notes = tmp.path().join("notes");
    std::fs::write(
        &config,
        format!("[recording]\ndir = \"{}\"\n", notes.display()),
    )
    .unwrap();
    assert_eq!(read::root(&config, &data), notes);
    // A folder that is not there yet is no recordings, not an error.
    assert_eq!(
        read_folder(&config, &data, now(), || None, &mut Parsed::default()),
        Ok(Vec::new())
    );
}
