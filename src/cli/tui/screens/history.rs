//! History: the dictations `history.jsonl` keeps, newest first, read on a
//! worker when the screen opens. `Enter` opens one's whole text, `/`
//! filters them by their text, `C` clears them all through the daemon
//! (asking first), as `whisrs log --clear` does. With `[general] history`
//! off it says so, since the daemon then adds nothing.

use std::cell::Cell;
use std::path::Path;

use chrono::{DateTime, Local};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::recordings::date_label;
use crate::tui::app::App;
use crate::tui::keys::{Action, Mode};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::input::Input;
use crate::tui::widgets::picker::matches;
use crate::tui::widgets::row::{Row, Value};
use crate::tui::widgets::{fit, scroll, width, wrap, Tone, Ui};

/// How far `PgUp` and `PgDn` move the list.
const PAGE: usize = 10;

/// The date column (`Oct 30 14:02`) and the gap after it.
const DATE_WIDTH: usize = 14;

/// The duration column, right-aligned.
const DURATION_WIDTH: usize = 6;

/// The widest the backend column gets.
const BACKEND_MAX: usize = 18;

/// Over the list while `[general] history` is off.
const OFF_NOTE: &str = "History is off: new dictations are not kept. C clears these.";

/// In place of the list while it is off and the file holds none.
const OFF_EMPTY: &str = "History is off, so dictations are not kept. Config > General > \
                         history turns it on.";

/// One dictation, as the screen shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Dictation {
    /// When it was typed, as the list shows it: `Oct 30 14:02`.
    pub date: String,
    /// As the open entry heads it: `2026-10-30 14:02:11`.
    pub when: String,
    pub secs: f64,
    pub backend: String,
    pub language: String,
    pub text: String,
}

/// The dictations in the history file at `path`, newest first, dated at
/// `now`. A missing file is none.
pub fn read(path: &Path, now: DateTime<Local>) -> Result<Vec<Dictation>, String> {
    let entries =
        whisrs::history::read_entries_from(path, usize::MAX).map_err(|e| format!("{e:#}"))?;
    Ok(entries
        .into_iter()
        .map(|entry| Dictation {
            date: date_label(entry.timestamp, now),
            when: entry.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
            secs: entry.duration_secs,
            backend: entry.backend,
            language: entry.language,
            text: entry.text,
        })
        .collect())
}

/// A dictation's length: `4.2s`, `1m 05s`.
fn seconds(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let whole = secs.round() as u64;
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// The text on one line: its first line, spaces made one.
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a key on History asks of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Nothing,
    Leave,
    /// `C` on the list: clear the whole history, asking first.
    Clear,
}

#[derive(Debug, Default)]
pub struct History {
    /// The dictations as last read; `None` until the read is back.
    pub entries: Option<Result<Vec<Dictation>, String>>,
    cursor: usize,
    pub filter: String,
    /// The filter being typed.
    typing: Option<Input>,
    /// The open dictation, by its index in `entries`, and how far down.
    open: Option<(usize, usize)>,
    /// How far the open text can scroll and how tall it is, as last drawn.
    shown: Cell<(usize, usize)>,
    /// A clear was sent and the daemon has not answered yet.
    pub clearing: bool,
}

impl History {
    /// The screen opens: on the list, read again.
    pub fn opened(&mut self) {
        self.entries = None;
        self.cursor = 0;
        self.filter.clear();
        self.typing = None;
        self.open = None;
        self.shown.set((usize::MAX, PAGE));
    }

    pub fn read(&mut self, entries: Result<Vec<Dictation>, String>) {
        self.entries = Some(entries);
        self.clamp();
    }

    pub fn mode(&self) -> Mode {
        if self.typing.is_some() {
            Mode::Text
        } else {
            Mode::Read
        }
    }

    fn all(&self) -> &[Dictation] {
        match &self.entries {
            Some(Ok(entries)) => entries,
            _ => &[],
        }
    }

    /// The dictations the filter keeps, by index: each word of it starts a
    /// word of the text, in any case, as a picker's filter does.
    fn visible(&self) -> Vec<usize> {
        self.all()
            .iter()
            .enumerate()
            .filter(|(_, d)| self.filter.is_empty() || matches(&self.filter, &[&d.text]))
            .map(|(i, _)| i)
            .collect()
    }

    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.visible().len().saturating_sub(1));
    }

    /// Handle `action`: what it asks of the app.
    pub fn key(&mut self, action: Action) -> Asked {
        if let Some(input) = self.typing.as_mut() {
            match action {
                Action::Enter => {
                    self.filter = input.text().trim().to_string();
                    self.typing = None;
                }
                Action::Back => {
                    self.filter.clear();
                    self.typing = None;
                }
                other => {
                    input.apply(other);
                    self.filter = input.text().trim().to_string();
                }
            }
            self.cursor = 0;
            self.clamp();
            return Asked::Nothing;
        }
        if let Some((index, at)) = self.open {
            let (most, height) = self.shown.get();
            let page = height.max(1);
            let half = (page / 2).max(1);
            let at = at.min(most);
            let to = match action {
                Action::Back => {
                    self.open = None;
                    return Asked::Nothing;
                }
                Action::Up => at.saturating_sub(1),
                Action::Down => at.saturating_add(1).min(most),
                Action::First => 0,
                Action::Last => most,
                Action::PageUp => at.saturating_sub(page),
                Action::PageDown => at.saturating_add(page).min(most),
                Action::HalfUp => at.saturating_sub(half),
                Action::HalfDown => at.saturating_add(half).min(most),
                _ => at,
            };
            self.open = Some((index, to));
            return Asked::Nothing;
        }
        let last = self.visible().len().saturating_sub(1);
        let at = self.cursor;
        self.cursor = match action {
            Action::Back if !self.filter.is_empty() => {
                self.filter.clear();
                0
            }
            Action::Back => return Asked::Leave,
            Action::ClearAll => return Asked::Clear,
            Action::Filter => {
                self.typing = Some(Input::new(&self.filter, false));
                at
            }
            Action::Enter => {
                if let Some(&index) = self.visible().get(at) {
                    self.open = Some((index, 0));
                    self.shown.set((usize::MAX, PAGE));
                }
                at
            }
            Action::Up => at.saturating_sub(1),
            Action::Down => (at + 1).min(last),
            Action::First => 0,
            Action::Last => last,
            Action::PageUp => at.saturating_sub(PAGE),
            Action::PageDown => (at + PAGE).min(last),
            Action::HalfUp => at.saturating_sub(PAGE / 2),
            Action::HalfDown => (at + PAGE / 2).min(last),
            _ => at,
        };
        self.clamp();
        Asked::Nothing
    }
}

fn header_name(history: &History) -> String {
    if history.filter.is_empty() || history.typing.is_some() {
        "history".to_string()
    } else {
        format!("history /{}", history.filter)
    }
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let history = &app.history;
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, max);
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    let open = history
        .open
        .and_then(|(index, at)| Some((history.all().get(index)?, at)));
    let visible = history.visible();
    let kept = app.history_kept();
    let text: Option<(String, Tone)> = match &history.entries {
        _ if open.is_some() => None,
        None => Some(("looking...".into(), Tone::Muted)),
        Some(Err(why)) => Some((format!("cannot read the history: {why}"), Tone::Error)),
        Some(Ok(all)) if all.is_empty() && !kept => Some((OFF_EMPTY.into(), Tone::Muted)),
        Some(Ok(all)) if all.is_empty() => Some(("No dictations yet".into(), Tone::Muted)),
        Some(Ok(_)) if visible.is_empty() => {
            Some((format!("nothing matches /{}", history.filter), Tone::Muted))
        }
        Some(Ok(_)) => None,
    };
    let text = text.map(|(text, tone)| {
        wrap(&text, max.max(1))
            .into_iter()
            .map(|line| (line, tone))
            .collect::<Vec<_>>()
    });
    // Over the dictations the file still holds, a line that none is added.
    let note = match &history.entries {
        Some(Ok(all)) if !kept && !all.is_empty() && open.is_none() => wrap(OFF_NOTE, max.max(1)),
        _ => Vec::new(),
    };
    let top = if note.is_empty() { 0 } else { note.len() + 1 };
    let wanted = match (&text, open) {
        (_, Some(_)) => None,
        (Some(lines), None) => Some((top + lines.len() + foot) as u16),
        (None, None) => Some((top + visible.len() + foot) as u16),
    };
    let Some(placed) = frame::place(area, wanted) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: &header_name(history),
            profile: app.header_profile().as_deref(),
            daemon: app.daemon_up(),
            live: None,
        },
    );
    let room = usize::from(body.height).saturating_sub(foot);
    let at = |y: usize| {
        Rect::new(
            body.x + 2,
            body.y + y as u16,
            body.width.saturating_sub(2),
            1,
        )
    };

    if let Some((dictation, scrolled)) = open {
        let head = format!(
            "{when} {sep} {secs} {sep} {backend} {sep} {language}",
            when = dictation.when,
            secs = seconds(dictation.secs),
            backend = dictation.backend,
            language = dictation.language,
            sep = ui.glyphs.separator
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&head, max, ui.glyphs.ellipsis),
                ui.theme.muted,
            ))),
            at(0),
        );
        let mut lines = Vec::new();
        for part in dictation.text.lines() {
            if part.trim().is_empty() {
                lines.push(String::new());
            } else {
                lines.extend(wrap(part, max.max(1)));
            }
        }
        let height = room.saturating_sub(2);
        let most = lines.len().saturating_sub(height);
        history.shown.set((most, height));
        for (i, line) in lines
            .into_iter()
            .skip(scrolled.min(most))
            .take(height)
            .enumerate()
        {
            f.render_widget(Paragraph::new(Line::from(line)), at(2 + i));
        }
    } else {
        for (y, line) in note.into_iter().enumerate() {
            if y < room {
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(line, ui.theme.muted))),
                    at(y),
                );
            }
        }
        let room = room.saturating_sub(top);
        if let Some(text) = text {
            for (y, (line, tone)) in text.into_iter().enumerate() {
                if y < room {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(line, tone.style(ui.theme)))),
                        at(top + y),
                    );
                }
            }
        } else {
            draw_list(
                history,
                &visible,
                f,
                Rect::new(body.x, body.y + top as u16, body.width, room as u16),
                ui,
            );
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
    footer(history, f, placed.footer, ui);
}

fn draw_list(history: &History, visible: &[usize], f: &mut Frame, area: Rect, ui: &Ui) {
    let height = usize::from(area.height);
    let offset = scroll(visible.len(), history.cursor, height);
    let all = history.all();
    // The text keeps two thirds of what is left after the date and the
    // length, at the least.
    let room = usize::from(area.width).saturating_sub(2 + DATE_WIDTH + DURATION_WIDTH + 2);
    let backend_width = visible
        .iter()
        .map(|&i| width(&all[i].backend))
        .max()
        .unwrap_or(0)
        .min(BACKEND_MAX)
        .min(room / 3);
    let typing = history.typing.is_some();
    for (row, &index) in visible.iter().enumerate().skip(offset).take(height) {
        let dictation = &all[index];
        let backend = fit(&dictation.backend, backend_width, ui.glyphs.ellipsis);
        let mut value = Value::default();
        value.parts.push((
            format!("{:>DURATION_WIDTH$}  ", seconds(dictation.secs)),
            Tone::Muted,
        ));
        value.parts.push((
            format!(
                "{backend}{}  ",
                " ".repeat(backend_width.saturating_sub(width(&backend)))
            ),
            Tone::Muted,
        ));
        value.parts.push((first_line(&dictation.text), Tone::Plain));
        let line = Row {
            focused: row == history.cursor && !typing,
            selected: row == history.cursor,
            ..Row::new(&dictation.date, value)
        };
        line.draw(
            Rect::new(area.x, area.y + (row - offset) as u16, area.width, 1),
            f.buffer_mut(),
            ui,
            DATE_WIDTH,
        );
    }
}

fn footer(history: &History, f: &mut Frame, rect: Rect, ui: &Ui) {
    if let Some(input) = &history.typing {
        let mut spans = vec![Span::raw("  "), Span::styled("/", ui.theme.accent)];
        spans.extend(input.spans(ui, 24));
        spans.push(Span::styled(
            format!("  enter keep {} esc clear", ui.glyphs.separator),
            ui.theme.muted,
        ));
        f.render_widget(Line::from(spans), rect);
        return;
    }
    // `? help` last, as the one always shown, as on Recordings.
    let hints: &[(&str, &str)] = if history.open.is_some() {
        &[
            ("j/k", "scroll"),
            ("ctrl+d/u", "page"),
            ("esc", "back"),
            ("?", "help"),
        ]
    } else if history.all().is_empty() {
        &[("esc", "back"), ("?", "help")]
    } else {
        &[
            ("j/k", "move"),
            ("enter", "open"),
            ("/", "filter"),
            ("C", "clear"),
            ("esc", "back"),
            ("?", "help"),
        ]
    };
    frame::footer(f, rect, ui, hints);
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::TimeZone;

    use super::*;
    use crate::tui::app::{Dialog, Effect, Msg};
    use crate::tui::testing::{
        assert_snapshot, draw, mockup, mockup_on, press, rows, Fixture, MOCKUP,
    };
    use crate::tui::worker::DaemonView;

    fn at(month: u32, day: u32, h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, month, day, h, m, s).unwrap()
    }

    /// A history file with three dictations, the newest last as the daemon
    /// appends them.
    fn file(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("history.jsonl");
        let mut out = std::fs::File::create(&path).unwrap();
        for (when, text, backend, secs) in [
            (
                at(10, 29, 9, 15, 2),
                "Remind me to book the van for Friday.",
                "groq",
                2.4,
            ),
            (
                at(10, 30, 11, 40, 30),
                "The parser keeps the comments and the key order, so a save changes only the \
                 line you edited. We checked it against three configs.\nSecond paragraph.",
                "deepgram-streaming",
                14.8,
            ),
            (
                at(10, 30, 14, 2, 11),
                "Ship the TUI this week.",
                "local-whisper",
                1.9,
            ),
        ] {
            let entry = whisrs::history::HistoryEntry {
                timestamp: when,
                text: text.to_string(),
                backend: backend.to_string(),
                language: "en".to_string(),
                duration_secs: secs,
            };
            writeln!(out, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        }
        path
    }

    /// History open from Home, the mockup's config, the file read at a
    /// fixed time.
    fn history(read: Result<Vec<Dictation>, String>) -> Fixture {
        opened(mockup(), read)
    }

    /// [`history`] on a config with `[general] history` off.
    fn history_off(read: Result<Vec<Dictation>, String>) -> Fixture {
        let toml = MOCKUP.replace(
            "language = \"en\"\n",
            "language = \"en\"\nhistory = false\n",
        );
        assert_ne!(toml, MOCKUP);
        opened(mockup_on(&toml), read)
    }

    fn opened(mut fixture: Fixture, read: Result<Vec<Dictation>, String>) -> Fixture {
        press(&mut fixture.app, "\x1bG");
        let effects = press(&mut fixture.app, "\n");
        assert_eq!(effects, [Effect::ReadHistory]);
        fixture.app.update(Msg::HistoryRead(read));
        fixture
    }

    fn three() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let read = read(&file(dir.path()), at(10, 30, 15, 0, 0));
        history(read)
    }

    fn three_off() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let read = read(&file(dir.path()), at(10, 30, 15, 0, 0));
        history_off(read)
    }

    fn said(fixture: &Fixture) -> String {
        fixture.app.status.as_ref().expect("a status").text.clone()
    }

    fn shows(fixture: &Fixture, text: &str) -> bool {
        rows(&draw(&fixture.app, 90, 30))
            .iter()
            .any(|row| row.contains(text))
    }

    #[test]
    fn the_file_is_read_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let entries = read(&file(dir.path()), at(10, 30, 15, 0, 0)).unwrap();
        let texts: Vec<&str> = entries.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(texts[0], "Ship the TUI this week.");
        assert_eq!(texts[2], "Remind me to book the van for Friday.");
        assert_eq!(entries[0].date, "Oct 30 14:02");
        assert_eq!(entries[0].when, "2026-10-30 14:02:11");
        assert!(read(&dir.path().join("none.jsonl"), at(10, 30, 15, 0, 0))
            .unwrap()
            .is_empty());
        assert_eq!(seconds(14.84), "14.8s");
        assert_eq!(seconds(65.2), "1m 05s");
    }

    /// `/` filters by the text as it is typed; Esc while typing clears it,
    /// Enter keeps it, and Esc on the list clears a kept filter before it
    /// leaves.
    #[test]
    fn the_filter_finds_the_text() {
        let mut fixture = three();
        press(&mut fixture.app, "/");
        assert_eq!(fixture.app.history.mode(), Mode::Text);
        press(&mut fixture.app, "VAN");
        assert_eq!(fixture.app.history.visible(), [2]);
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.history.visible().len(), 3);
        press(&mut fixture.app, "/comments\n");
        assert_eq!(fixture.app.history.filter, "comments");
        assert_eq!(fixture.app.history.visible(), [1]);
        let drawn = rows(&draw(&fixture.app, 90, 30));
        assert!(drawn.iter().any(|r| r.contains("history /comments")));
        press(&mut fixture.app, "\x1b");
        assert!(fixture.app.history.filter.is_empty());
        assert_eq!(fixture.app.screen(), crate::tui::app::Screen::History);
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), crate::tui::app::Screen::Home);
    }

    /// Enter opens the whole text, wrapped; it scrolls, and Esc goes back
    /// to the same row.
    #[test]
    fn an_entry_opens_whole() {
        let mut fixture = three();
        press(&mut fixture.app, "j\n");
        let drawn = rows(&draw(&fixture.app, 60, 18));
        assert!(drawn.iter().any(|r| r.contains("Second paragraph.")));
        assert!(drawn
            .iter()
            .any(|r| r.contains("2026-10-30 11:40:30 \u{00B7} 14.8s")));
        press(&mut fixture.app, "jG");
        press(&mut fixture.app, "\x1b");
        assert!(fixture.app.history.open.is_none());
        assert_eq!(fixture.app.history.cursor, 1);
        assert_eq!(fixture.app.screen(), crate::tui::app::Screen::History);
    }

    /// With `[general] history` off, a line over the list says no new
    /// dictation is kept, and an empty file says how to turn it on. An open
    /// dictation has the screen to itself.
    #[test]
    fn history_off_is_said() {
        let off = three_off();
        assert!(shows(&off, "History is off: new dictations are not kept"));
        assert!(shows(&off, "Ship the TUI this week."));
        assert!(!shows(&three(), "History is off"));
        let empty = history_off(Ok(Vec::new()));
        assert!(shows(&empty, "History is off, so dictations are not kept"));
        assert!(!shows(&empty, "No dictations yet"));
        let mut open = three_off();
        press(&mut open.app, "\n");
        assert!(!shows(&open, "History is off"));
        assert!(shows(&open, "Ship the TUI this week."));
    }

    /// `C` asks first and only `y` sends the clear; until the daemon
    /// answers another `C` says so, and a clear that worked reads the file
    /// again.
    #[test]
    fn c_clears_after_asking() {
        let mut fixture = three();
        for keep in ["n", "\x1b", "\n"] {
            assert!(press(&mut fixture.app, "C").is_empty());
            assert_eq!(
                fixture.app.dialog,
                Some(Dialog::ClearHistory { count: Some(3) })
            );
            assert!(shows(&fixture, "All 3 dictations go for good."));
            assert!(press(&mut fixture.app, keep).is_empty(), "{keep:?}");
            assert_eq!(fixture.app.dialog, None, "{keep:?}");
            assert_eq!(said(&fixture), "kept the history", "{keep:?}");
            assert_eq!(fixture.app.screen(), crate::tui::app::Screen::History);
        }
        press(&mut fixture.app, "C");
        assert_eq!(press(&mut fixture.app, "y"), [Effect::ClearHistory]);
        assert!(fixture.app.history.clearing);
        assert_eq!(said(&fixture), "clearing the history...");
        press(&mut fixture.app, "C");
        assert_eq!(fixture.app.dialog, None);
        assert!(said(&fixture).starts_with("still clearing the history"));
        assert_eq!(
            fixture.app.update(Msg::HistoryCleared(Ok(()))),
            Some(Effect::ReadHistory)
        );
        assert!(!fixture.app.history.clearing);
        assert_eq!(said(&fixture), "history cleared");
        fixture.app.update(Msg::HistoryRead(Ok(Vec::new())));
        assert!(shows(&fixture, "No dictations yet"));
        press(&mut fixture.app, "C");
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(said(&fixture), "the history is empty");
    }

    /// Nothing is asked while the daemon is down; a clear it refused says
    /// why and reads nothing; `C` in an open dictation does nothing.
    #[test]
    fn c_is_refused_while_the_daemon_is_down() {
        let mut fixture = three();
        fixture.app.update(Msg::Daemon(DaemonView::Down));
        press(&mut fixture.app, "C");
        assert_eq!(fixture.app.dialog, None);
        assert!(said(&fixture).starts_with("cannot clear the history: whisrsd is not running"));
        assert_eq!(
            fixture.app.update(Msg::HistoryCleared(Err(
                "failed to clear history: denied".into()
            ))),
            None
        );
        assert_eq!(said(&fixture), "failed to clear history: denied");
        let mut open = three();
        press(&mut open.app, "\nC");
        assert_eq!(open.app.dialog, None);
        assert!(open.app.history.open.is_some());
        // One dictation is said as one.
        let mut one = history(Ok(three().app.history.all()[..1].to_vec()));
        press(&mut one.app, "C");
        assert!(shows(&one, "Its one dictation goes for good."));
    }

    #[test]
    fn history_snapshots() {
        let fixture = three();
        for (w, h) in [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)] {
            assert_snapshot(
                &format!("history-{w}x{h}"),
                &rows(&draw(&fixture.app, w, h)),
            );
        }
        let mut open = three();
        press(&mut open.app, "j\n");
        for (w, h) in [(60, 18), (90, 30)] {
            assert_snapshot(
                &format!("history-open-{w}x{h}"),
                &rows(&draw(&open.app, w, h)),
            );
        }
        let empty = history(Ok(Vec::new()));
        assert_snapshot("history-empty-90x30", &rows(&draw(&empty.app, 90, 30)));
        let off = three_off();
        for (w, h) in [(60, 18), (90, 30)] {
            assert_snapshot(
                &format!("history-off-{w}x{h}"),
                &rows(&draw(&off.app, w, h)),
            );
        }
        let off_empty = history_off(Ok(Vec::new()));
        assert_snapshot(
            "history-off-empty-90x30",
            &rows(&draw(&off_empty.app, 90, 30)),
        );
        let mut asking = three();
        press(&mut asking.app, "C");
        assert_snapshot("history-clear-90x30", &rows(&draw(&asking.app, 90, 30)));
    }
}
