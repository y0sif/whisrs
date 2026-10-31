//! Profiles: `base` and every profile in `profiles/`, the active one `●`,
//! each with what its file sets, and at 90 columns and up the focused one's
//! keys in a pane beside the list. The folder, and each profile's merge over
//! config.toml, are read on a worker ([`read_folder`]) when the screen opens
//! and after every switch, new, delete and edit, never in a draw. `Enter`
//! switches through the daemon, `l` opens the profile in Config, `n` makes
//! one, `d` deletes one, `e` edits its file.

use std::collections::BTreeSet;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use toml::{Table, Value as Toml};
use whisrs::config::profile::{self, Check, Paths, BASE};
use whisrs::InfoAnswer;

use super::home::LIST_WIDTH;
use crate::tui::app::{App, Dialog};
use crate::tui::keys::{Action, Mode};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::input::Input;
use crate::tui::widgets::row::{Row, Value};
use crate::tui::widgets::{fit, scroll, width, wrap, Notice, Tone, Ui};
use crate::tui::worker::DaemonView;

/// How far `PgUp` and `PgDn` move the list.
const PAGE: usize = 10;

/// Before a profile's name: its dot, and a space.
const MARK: usize = 2;

/// Under the list when there is no profile yet.
const NONE_YET: &str = "No profiles yet: n makes one. A profile holds only what differs from \
                        config.toml, and whisrs runs on base plus the active one.";

/// One profile, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    /// What its file sets, a line each, and how it reads: `general.backend =
    /// "local-whisper"`; a base-only key with no value (`groq.api_key (base
    /// only, ignored)`), never an API key's. None for a file not TOML.
    pub keys: Vec<(String, Tone)>,
    /// Why it does not load over config.toml, if it does not: its file is
    /// not TOML, the merge does not deserialize or does not validate, as a
    /// client checks it (`Check::Client`): the daemon's dry run decides.
    pub error: Option<String>,
    /// What the config it makes warns of that config.toml alone does not:
    /// a key whisrsd has to find in its own environment, say.
    pub notes: Vec<String>,
    /// The base-only keys it sets, dropped.
    pub dropped: Vec<String>,
    /// The dotted paths it sets that apply.
    pub set: BTreeSet<String>,
    /// The backend and the language of the config it makes, when it loads:
    /// what Home shows while it is in use.
    pub backend: Option<String>,
    pub language: Option<String>,
}

impl Item {
    /// What its file sets, in a word: `3 keys`, `1 key`, `empty`.
    fn count(&self) -> String {
        match self.keys.len() {
            0 => "empty".to_string(),
            1 => "1 key".to_string(),
            n => format!("{n} keys"),
        }
    }

    /// The row's note, in parts: what it sets, or why it is broken, and the
    /// base-only keys it sets.
    fn note(&self, ui: &Ui) -> Value {
        if let Some(why) = &self.error {
            return Value::of(format!("broken: {why}"), Tone::Error);
        }
        let mut parts = vec![(self.count(), Tone::Muted)];
        if !self.dropped.is_empty() {
            parts.push((ignored(&self.dropped), Tone::Warn));
        }
        parts.extend(self.notes.first().map(|note| (note.clone(), Tone::Warn)));
        Value::joined(parts, ui.glyphs.separator)
    }

    /// The note in a word, beside the pane that has the rest: `broken`, or
    /// what it sets, `!` after it when it sets base-only keys.
    fn short_note(&self, ui: &Ui) -> Value {
        if self.error.is_some() {
            return Value::of("broken", Tone::Error);
        }
        let value = Value::muted(self.count());
        if self.dropped.is_empty() && self.notes.is_empty() {
            value
        } else {
            value.then(ui.glyphs.warn, Tone::Warn)
        }
    }
}

/// The base-only keys a profile sets, as a note.
fn ignored(dropped: &[String]) -> String {
    format!("base only, ignored: {}", dropped.join(", "))
}

/// The profiles folder, as the screen and Home show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The profiles, by name.
    pub items: Vec<Item>,
    /// The profile the state file names: `None` is base.
    pub active: Option<String>,
    /// What to say about it: files that are not profiles, a state file
    /// that names none, config.toml not loading.
    pub warnings: Vec<String>,
}

/// The profiles at `paths`, each merged over config.toml as the daemon
/// would and checked as a client checks it (a key that may live in
/// whisrsd's environment alone is a note, not a failure), and the state
/// file's name; as `whisrs profile list` reads them. `Err` when the folder
/// cannot be read.
pub fn read_folder(paths: &Paths) -> Result<Folder, String> {
    let listing =
        profile::list(&paths.profiles).map_err(|e| format!("cannot read profiles/: {e}"))?;
    let mut warnings: Vec<String> = listing.skipped.iter().map(|s| s.warning()).collect();
    let active = match profile::read_active(&paths.state) {
        Ok(active) => active,
        Err(why) => {
            warnings.push(why);
            None
        }
    };
    if let Some(name) = active
        .as_deref()
        .filter(|a| !listing.names.iter().any(|n| n == a))
    {
        warnings.push(format!(
            "the active profile {name} has no file ({}): whisrsd starts on base",
            profile::label(name)
        ));
    }
    let base = match std::fs::read_to_string(&paths.config) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(e) => {
            warnings.push(format!(
                "the profiles were not checked: cannot read config.toml: {e}"
            ));
            None
        }
    };
    let alone = base.as_deref().map(warnings_of).unwrap_or_default();
    let mut unchecked = None;
    let items = listing
        .names
        .iter()
        .map(|name| {
            let label = profile::label(name);
            let text = match std::fs::read_to_string(paths.profile(name)) {
                Ok(text) => text,
                Err(e) => {
                    return Item {
                        name: name.clone(),
                        keys: Vec::new(),
                        error: Some(format!("{label} cannot be read: {e}")),
                        notes: Vec::new(),
                        dropped: Vec::new(),
                        set: BTreeSet::new(),
                        backend: None,
                        language: None,
                    }
                }
            };
            let eff = base
                .as_deref()
                .map(|base| profile::effective_checked(base, name, Some(&text), Check::Client));
            let mut runs = None;
            let mut notes = Vec::new();
            let (error, dropped, set, unknown) = match eff {
                Some(Ok(eff)) => {
                    if eff.applied() {
                        let general = &eff.config.general;
                        runs = Some((general.backend.clone(), general.language.clone()));
                        notes = warned(&eff.config)
                            .into_iter()
                            .filter(|note| !alone.contains(note))
                            .collect();
                    }
                    (eff.error, eff.dropped, eff.set, eff.unknown_profile)
                }
                Some(Err(why)) => {
                    unchecked.get_or_insert(why);
                    Default::default()
                }
                None => Default::default(),
            };
            let (backend, language) = runs.unzip();
            let keys = profile::read_table(&text)
                .map(|table| key_lines(&table, &unknown))
                .unwrap_or_default();
            Item {
                name: name.clone(),
                keys,
                error,
                notes,
                dropped,
                set,
                backend,
                language,
            }
        })
        .collect();
    if let Some(why) = unchecked {
        warnings.push(format!("the profiles were not checked: {why}"));
    }
    Ok(Folder {
        items,
        active,
        warnings,
    })
}

/// What `config` warns of when written, each on one line.
fn warned(config: &whisrs::Config) -> Vec<String> {
    config
        .validate_to_write()
        .map(|warnings| {
            warnings
                .into_iter()
                .map(|w| w.message.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect()
        })
        .unwrap_or_default()
}

/// What config.toml, `text`, warns of on its own.
fn warnings_of(text: &str) -> Vec<String> {
    profile::read_table(text)
        .ok()
        .and_then(|table| Toml::Table(table).try_into::<whisrs::Config>().ok())
        .map(|config| warned(&config))
        .unwrap_or_default()
}

/// A profile's keys as the pane lists them: `section.key = value`, in the
/// file's order. A key the merge drops ([`profile::is_dropped`]: a
/// base-only key, or a section that is no table over one) is named with no
/// value, so an API key is never shown, and so is one the config does not
/// know, which says so.
fn key_lines(table: &Table, unknown: &[String]) -> Vec<(String, Tone)> {
    fn walk(
        table: &Table,
        at: &mut Vec<String>,
        unknown: &[String],
        out: &mut Vec<(String, Tone)>,
    ) {
        for (key, value) in table {
            at.push(key.clone());
            let path = at.join(".");
            match value {
                _ if profile::is_dropped(&path, value) => {
                    out.push((format!("{path} (base only, ignored)"), Tone::Muted));
                }
                Toml::Table(inner) => walk(inner, at, unknown, out),
                // Named, not shown: a key spelled wrong may be one.
                _ if unknown.contains(&path) => {
                    out.push((format!("{path} (unknown, ignored)"), Tone::Warn));
                }
                _ => out.push((format!("{path} = {value}"), Tone::Plain)),
            }
            at.pop();
        }
    }
    let mut out = Vec::new();
    walk(table, &mut Vec::new(), unknown, &mut out);
    out
}

/// Make profile `name`: an empty 0600 file, never over one that is there.
pub fn create(paths: &Paths, name: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let label = profile::label(name);
    profile::check_name(name)?;
    std::fs::create_dir_all(&paths.profiles).map_err(|e| format!("cannot make profiles/: {e}"))?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(paths.profile(name))
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("{label} is there already"))
        }
        Err(e) => Err(format!("cannot make {label}: {e}")),
    }
}

/// Delete profile `name`'s file.
pub fn delete(paths: &Paths, name: &str) -> Result<(), String> {
    std::fs::remove_file(paths.profile(name))
        .map_err(|e| format!("cannot delete {}: {e}", profile::label(name)))
}

/// What a key on the screen asks of the app beyond the screen itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Switch whisrsd to it (`base`: the base config).
    Switch(String),
    /// Open it in Config (`base`: plain Config).
    Open(String),
    /// Make it, then open it.
    Create(String),
    /// Its file in `$EDITOR` (`base`: config.toml).
    Edit(String),
}

/// What a key on the screen asks of the app.
#[derive(Debug, Default)]
pub struct Reply {
    pub leave: bool,
    pub status: Option<Notice>,
    pub dialog: Option<Dialog>,
    pub ask: Option<Ask>,
}

impl Reply {
    fn say(tone: Tone, text: impl Into<String>) -> Reply {
        Reply {
            status: Some(Notice::new(tone, text)),
            ..Reply::default()
        }
    }

    fn ask(ask: Ask) -> Reply {
        Reply {
            ask: Some(ask),
            ..Reply::default()
        }
    }
}

#[derive(Debug, Default)]
pub struct Profiles {
    /// The folder as last read; `None` until the first read.
    pub folder: Option<Result<Folder, String>>,
    /// The row the cursor is on: 0 is base.
    cursor: usize,
    /// The name under the cursor, kept there across reads.
    focus: Option<String>,
    /// The new profile's name, being typed.
    naming: Option<Input>,
    /// A switch was sent to whisrsd, not answered yet.
    pub asked: Option<String>,
    /// A switch whisrsd took and restarts for.
    pub switching: Option<Switching>,
}

/// A switch whisrsd took, followed until its `info` comes from a new start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switching {
    /// The profile asked for (`base` too).
    pub name: String,
    /// The start time its `info` had when it took the switch, if known.
    pub before: Option<u64>,
    /// The profile it was on then.
    pub from: String,
    /// When it took the switch.
    pub since: Instant,
}

impl Profiles {
    /// The screen opens: on the list, the cursor on the active profile once
    /// the folder is read.
    pub fn opened(&mut self) {
        self.naming = None;
        self.focus = None;
    }

    /// A read of the folder came: the cursor stays on its profile, and
    /// starts on the active one.
    pub fn read(&mut self, folder: Result<Folder, String>) {
        self.folder = Some(folder);
        if self.focus.is_none() {
            self.focus = Some(self.active().unwrap_or(BASE).to_string());
        }
        let rows = self.rows();
        if let Some(at) = self
            .focus
            .as_ref()
            .and_then(|name| rows.iter().position(|r| r == name))
        {
            self.cursor = at;
        }
        self.select(self.cursor);
    }

    /// Back on Profiles from profile `name`, or after making it: the cursor
    /// on its row now when there is one, and kept there by the next read
    /// (which brings a new one's row).
    pub fn back_on(&mut self, name: &str) {
        self.focus = Some(name.to_string());
        if let Some(at) = self.rows().iter().position(|row| row == name) {
            self.cursor = at;
        }
    }

    pub fn mode(&self) -> Mode {
        if self.naming.is_some() {
            Mode::Text
        } else {
            Mode::Profiles
        }
    }

    fn items(&self) -> &[Item] {
        match &self.folder {
            Some(Ok(folder)) => &folder.items,
            _ => &[],
        }
    }

    /// The rows: base, then the profiles by name.
    fn rows(&self) -> Vec<String> {
        std::iter::once(BASE.to_string())
            .chain(self.items().iter().map(|item| item.name.clone()))
            .collect()
    }

    /// Profile `name`, as last read.
    pub fn item(&self, name: &str) -> Option<&Item> {
        self.items().iter().find(|item| item.name == name)
    }

    /// The profile the state file names, as last read: `None` is base.
    pub fn active(&self) -> Option<&str> {
        match &self.folder {
            Some(Ok(folder)) => folder.active.as_deref(),
            _ => None,
        }
    }

    /// How many profiles there are, once read.
    pub fn count(&self) -> Option<usize> {
        match &self.folder {
            Some(Ok(folder)) => Some(folder.items.len()),
            _ => None,
        }
    }

    fn focused(&self) -> String {
        self.rows()
            .get(self.cursor)
            .cloned()
            .unwrap_or_else(|| BASE.to_string())
    }

    fn select(&mut self, to: usize) {
        let rows = self.rows();
        self.cursor = to.min(rows.len().saturating_sub(1));
        self.focus = rows.get(self.cursor).cloned();
    }

    /// Whether `name` can be a new profile's: the name rule, and no profile
    /// has it already.
    fn check_new(&self, name: &str) -> Result<(), String> {
        profile::check_name(name)?;
        if self.items().iter().any(|item| item.name == name) {
            return Err(format!("there is a profile named {name} already"));
        }
        Ok(())
    }

    /// What the name typed so far says, as it is typed.
    fn checked(&self, name: &str) -> Notice {
        if name.is_empty() {
            return Notice::new(
                Tone::Muted,
                format!("a name: 1 to {} of a-z, 0-9, - and _", profile::NAME_MAX),
            );
        }
        match self.check_new(name) {
            Ok(()) => Notice::new(Tone::Muted, format!("enter makes {}", profile::label(name))),
            Err(why) => Notice::new(Tone::Error, why),
        }
    }

    /// Handle `action`; `running` is the profile whisrsd runs on, when it
    /// answers `info`.
    pub fn key(&mut self, action: Action, running: Option<&str>) -> Reply {
        if self.naming.is_some() {
            return self.naming_key(action);
        }
        let last = self.rows().len().saturating_sub(1);
        let at = self.cursor;
        match action {
            Action::Back => Reply {
                leave: true,
                ..Reply::default()
            },
            Action::Up => self.moved(at.saturating_sub(1)),
            Action::Down => self.moved((at + 1).min(last)),
            Action::First => self.moved(0),
            Action::Last => self.moved(last),
            Action::PageUp => self.moved(at.saturating_sub(PAGE)),
            Action::PageDown => self.moved((at + PAGE).min(last)),
            Action::Enter => self.switch(),
            Action::Right => Reply::ask(Ask::Open(self.focused())),
            Action::Editor => Reply::ask(Ask::Edit(self.focused())),
            Action::Delete => self.ask_delete(running),
            Action::New => {
                if !matches!(self.folder, Some(Ok(_))) {
                    return Reply::say(Tone::Warn, "the profiles folder is not read yet");
                }
                self.naming = Some(Input::new("", false));
                Reply {
                    status: Some(self.checked("")),
                    ..Reply::default()
                }
            }
            _ => Reply::default(),
        }
    }

    fn moved(&mut self, to: usize) -> Reply {
        self.select(to);
        Reply::default()
    }

    /// `Enter`: switch to it, unless it is broken (the daemon would refuse
    /// it after a round trip).
    fn switch(&self) -> Reply {
        let name = self.focused();
        if let Some(why) = self.item(&name).and_then(|item| item.error.as_ref()) {
            return Reply::say(Tone::Error, format!("cannot switch to {name}: {why}"));
        }
        Reply::ask(Ask::Switch(name))
    }

    /// `d`: asked first; base, the active profile and the one whisrsd runs
    /// on (switched to elsewhere since the read) are refused at once.
    fn ask_delete(&self, running: Option<&str>) -> Reply {
        let name = self.focused();
        if name == BASE {
            return Reply::say(
                Tone::Warn,
                "base is config.toml itself, so it is not deleted",
            );
        }
        if self.active() == Some(name.as_str()) {
            return Reply::say(
                Tone::Warn,
                format!("{name} is the active profile: switch to another before deleting it"),
            );
        }
        if running == Some(name.as_str()) {
            return Reply::say(
                Tone::Warn,
                format!(
                    "{name} is the profile whisrsd runs on: switch to another before deleting it"
                ),
            );
        }
        Reply {
            dialog: Some(Dialog::DeleteProfile { name }),
            ..Reply::default()
        }
    }

    fn naming_key(&mut self, action: Action) -> Reply {
        let mut input = self.naming.take().expect("naming");
        match action {
            Action::Enter => {
                let name = input.text().trim().to_string();
                match self.check_new(&name) {
                    Ok(()) => Reply::ask(Ask::Create(name)),
                    Err(why) => {
                        self.naming = Some(input);
                        Reply::say(Tone::Error, why)
                    }
                }
            }
            Action::Back => Reply::say(Tone::Muted, "no profile made"),
            other => {
                input.apply(other);
                let status = self.checked(input.text().trim());
                self.naming = Some(input);
                Reply {
                    status: Some(status),
                    ..Reply::default()
                }
            }
        }
    }
}

/// Over the list: the daemon's line when it runs on another profile than
/// the state file names, or why the active one did not load.
fn daemon_note(app: &App) -> Option<(String, Tone)> {
    let DaemonView::Up(InfoAnswer::Info(info)) = &app.daemon else {
        return None;
    };
    if let Some(error) = &info.profile_error {
        return Some((error.clone(), Tone::Warn));
    }
    let Some(Ok(folder)) = &app.profiles.folder else {
        return None;
    };
    (info.profile != folder.active).then(|| {
        (
            format!(
                "whisrsd runs on {}; {} applies when it restarts",
                info.profile.as_deref().unwrap_or(BASE),
                folder.active.as_deref().unwrap_or(BASE)
            ),
            Tone::Muted,
        )
    })
}

/// The pane beside the list: the focused profile's note, then its keys.
fn pane_lines(screen: &Profiles, max: usize, ui: &Ui) -> Vec<(String, Tone)> {
    let name = screen.focused();
    let active = screen.active().unwrap_or(BASE) == name;
    let mut lines = Vec::new();
    let mut head = vec![name.clone()];
    if active {
        head.push("active".to_string());
    }
    let wrapped = |text: &str, tone: Tone, lines: &mut Vec<(String, Tone)>| {
        lines.extend(wrap(text, max.max(1)).into_iter().map(|line| (line, tone)));
    };
    let Some(item) = screen.item(&name) else {
        head.push("config.toml".to_string());
        lines.push((
            head.join(&format!(" {} ", ui.glyphs.separator)),
            Tone::Muted,
        ));
        lines.push((String::new(), Tone::Plain));
        wrapped(
            "The base config, which every profile goes over. l opens Config on it, e the file.",
            Tone::Muted,
            &mut lines,
        );
        return lines;
    };
    if item.error.is_none() {
        head.push(item.count());
    }
    lines.push((
        head.join(&format!(" {} ", ui.glyphs.separator)),
        Tone::Muted,
    ));
    if let Some(why) = &item.error {
        wrapped(&format!("broken: {why}"), Tone::Error, &mut lines);
    }
    if !item.dropped.is_empty() {
        wrapped(&ignored(&item.dropped), Tone::Warn, &mut lines);
    }
    for note in &item.notes {
        wrapped(note, Tone::Warn, &mut lines);
    }
    lines.push((String::new(), Tone::Plain));
    if item.keys.is_empty() && item.error.is_none() {
        wrapped(
            "Nothing yet: l sets keys in Config, e in the file.",
            Tone::Muted,
            &mut lines,
        );
    }
    for (line, tone) in &item.keys {
        lines.push((fit(line, max, ui.glyphs.ellipsis), *tone));
    }
    lines
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let screen = &app.profiles;
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let two = area.width >= frame::TWO_PANES;
    let mut notices = app.notices.clone();
    if let Some(Ok(folder)) = &screen.folder {
        notices.extend(
            folder
                .warnings
                .iter()
                .map(|w| Notice::new(Tone::Warn, w.clone())),
        );
    }
    let messages = frame::messages(&notices, app.status.as_ref(), ui, max);
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    let note: Vec<(String, Tone)> = daemon_note(app)
        .map(|(text, tone)| {
            wrap(&text, max.max(1))
                .into_iter()
                .map(|line| (line, tone))
                .collect()
        })
        .unwrap_or_default();
    let top = if note.is_empty() { 0 } else { note.len() + 1 };
    let text: Option<Vec<(String, Tone)>> = match &screen.folder {
        None => Some(vec![("looking...".to_string(), Tone::Muted)]),
        Some(Err(why)) => Some(
            wrap(&format!("cannot read the profiles: {why}"), max.max(1))
                .into_iter()
                .map(|line| (line, Tone::Error))
                .collect(),
        ),
        Some(Ok(_)) => None,
    };
    let rows = screen.rows();
    let listed = rows.len() + usize::from(screen.naming.is_some());
    let pane_width =
        usize::from(frame::body_width(area.width)).saturating_sub(usize::from(LIST_WIDTH) + 2);
    let pane = if two && text.is_none() {
        pane_lines(screen, pane_width, ui)
    } else {
        Vec::new()
    };
    let hint: Vec<String> = if text.is_none() && screen.items().is_empty() {
        wrap(NONE_YET, max.max(1))
    } else {
        Vec::new()
    };
    let hint_rows = if hint.is_empty() { 0 } else { hint.len() + 1 };
    let wanted = match &text {
        Some(lines) => top + lines.len() + foot,
        None => top + listed.max(pane.len()) + hint_rows + foot,
    };
    let Some(placed) = frame::place(area, Some(wanted as u16)) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: "profiles",
            profile: app.header_profile().as_deref(),
            daemon: app.daemon_up(),
            live: None,
        },
    );
    let room = usize::from(body.height).saturating_sub(foot);
    let line_at = |f: &mut Frame, x: u16, y: usize, width: u16, text: String, tone: Tone| {
        if y < room {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, tone.style(ui.theme)))),
                Rect::new(x, body.y + y as u16, width, 1),
            );
        }
    };
    let text_x = body.x + 2;
    let text_width = body.width.saturating_sub(2);
    for (y, (line, tone)) in note.into_iter().enumerate() {
        line_at(f, text_x, y, text_width, line, tone);
    }
    if let Some(text) = text {
        for (y, (line, tone)) in text.into_iter().enumerate() {
            line_at(f, text_x, top + y, text_width, line, tone);
        }
    } else {
        let left = room.saturating_sub(top + hint_rows);
        let list = Rect::new(
            body.x,
            body.y + top as u16,
            if two {
                LIST_WIDTH.min(body.width)
            } else {
                body.width
            },
            listed.min(left) as u16,
        );
        draw_list(screen, &rows, two, f, list, ui);
        let pane_len = pane.len();
        if two {
            let x = body.x + LIST_WIDTH + 2;
            let width = body.width.saturating_sub(LIST_WIDTH + 2);
            for (y, (line, tone)) in pane.into_iter().take(left).enumerate() {
                line_at(f, x, top + y, width, line, tone);
            }
        }
        // Under the list, and under the pane beside it.
        let shown = if two { listed.max(pane_len) } else { listed };
        let below = top + shown.min(left);
        for (y, line) in hint.into_iter().enumerate() {
            line_at(f, text_x, below + 1 + y, text_width, line, Tone::Muted);
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
}

/// The list, scrolled to the cursor, in `area`: a dot before the active
/// profile, and each one's note, in a word beside the pane.
fn draw_list(screen: &Profiles, rows: &[String], two: bool, f: &mut Frame, area: Rect, ui: &Ui) {
    let height = usize::from(area.height);
    let total = rows.len() + usize::from(screen.naming.is_some());
    let cursor = if screen.naming.is_some() {
        total - 1
    } else {
        screen.cursor
    };
    let offset = scroll(total, cursor, height);
    let active = screen.active().unwrap_or(BASE);
    let label_width = rows
        .iter()
        .map(|name| width(name))
        .max()
        .unwrap_or(0)
        .max(width("name"))
        + MARK
        + 2;
    let naming = screen.naming.is_some();
    for (i, name) in rows.iter().enumerate().skip(offset).take(height) {
        let value = match (two, screen.item(name)) {
            (true, Some(item)) => item.short_note(ui),
            (true, None) => Value::default(),
            (false, Some(item)) => item.note(ui),
            (false, None) => Value::muted("config.toml"),
        };
        let label = format!("{}{name}", " ".repeat(MARK));
        let row = Row {
            focused: i == screen.cursor && !naming,
            selected: i == screen.cursor,
            ..Row::new(&label, value)
        };
        let y = area.y + (i - offset) as u16;
        let rect = Rect::new(area.x, y, area.width, 1);
        row.draw(rect, f.buffer_mut(), ui, label_width);
        if name == active && area.width > 3 {
            f.buffer_mut()[(area.x + 2, y)]
                .set_symbol(ui.glyphs.up)
                .set_style(ui.theme.ok);
        }
    }
    if let Some(input) = &screen.naming {
        let i = total - 1;
        if i >= offset && i < offset + height {
            let label = format!("{}name", " ".repeat(MARK));
            let row = Row {
                focused: true,
                input: Some(input),
                ..Row::new(&label, Value::default())
            };
            let y = area.y + (i - offset) as u16;
            row.draw(
                Rect::new(area.x, y, area.width, 1),
                f.buffer_mut(),
                ui,
                label_width,
            );
        }
    }
}

fn footer(screen: &Profiles, f: &mut Frame, rect: Rect, ui: &Ui) {
    // `? help` last, as the one always shown, and `esc back` before it, the
    // first to go: Esc is back everywhere.
    let hints: &[(&str, &str)] = if screen.naming.is_some() {
        &[("enter", "make"), ("esc", "cancel"), ("ctrl+u", "clear")]
    } else if !matches!(screen.folder, Some(Ok(_))) {
        &[("esc", "back"), ("?", "help")]
    } else {
        &[
            ("enter", "switch"),
            ("l", "edit"),
            ("n", "new"),
            ("d", "delete"),
            ("e", "$EDITOR"),
            ("esc", "back"),
            ("?", "help"),
        ]
    };
    frame::footer(f, rect, ui, hints);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use whisrs::{DaemonInfo, State};

    use super::*;
    use crate::tui::app::{After, Effect, HelpFor, Msg, Screen};
    use crate::tui::testing::{
        assert_snapshot, chord, draw, mockup_profiles, open_profile, open_profiles, press,
        read_profiles, rows, type_text, Fixture, PROFILES,
    };
    use crate::tui::worker::Switched;

    /// Profiles open on [`PROFILES`], `offline` active.
    fn three() -> Fixture {
        let mut fixture = mockup_profiles(PROFILES, Some("offline"));
        open_profiles(&mut fixture);
        fixture
    }

    fn said(fixture: &Fixture) -> Notice {
        fixture.app.status.clone().expect("a status")
    }

    fn drawn(fixture: &Fixture, w: u16, h: u16) -> Vec<String> {
        rows(&draw(&fixture.app, w, h))
    }

    /// The cursor onto the row of `name`.
    fn to(fixture: &mut Fixture, name: &str) {
        let at = fixture
            .app
            .profiles
            .rows()
            .iter()
            .position(|row| row == name)
            .unwrap_or_else(|| panic!("no row {name}"));
        press(&mut fixture.app, &format!("g{}", "j".repeat(at)));
    }

    /// A daemon that answers `info`, on `profile`, started at `at`.
    fn up(fixture: &mut Fixture, profile: Option<&str>, at: u64) -> Option<Effect> {
        let info = DaemonInfo {
            version: "0.2.0".into(),
            state: Some(State::Idle),
            profile: profile.map(String::from),
            started_at: at,
            ..Default::default()
        };
        fixture
            .app
            .update(Msg::Daemon(DaemonView::Up(InfoAnswer::Info(info))))
    }

    /// The folder: base first, then the names sorted, the state file's one
    /// active, each one's note; a Groq key in a profile is never shown.
    #[test]
    fn the_folder_is_read_with_each_profiles_note() {
        let fixture = three();
        let screen = &fixture.app.profiles;
        assert_eq!(screen.rows(), ["base", "broken", "offline", "work"]);
        assert_eq!(screen.active(), Some("offline"));
        let offline = screen.item("offline").unwrap();
        assert_eq!(offline.count(), "3 keys");
        assert!(offline.error.is_none(), "{:?}", offline.error);
        assert_eq!(
            offline.set.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "general.backend",
                "general.language",
                "local-whisper.model_path"
            ]
        );
        let work = screen.item("work").unwrap();
        assert_eq!(work.dropped, ["groq.api_key", "hotkeys"]);
        assert!(work
            .keys
            .contains(&("groq.api_key (base only, ignored)".to_string(), Tone::Muted)));
        let broken = screen.item("broken").unwrap();
        assert!(broken
            .error
            .as_deref()
            .unwrap()
            .starts_with("profiles/broken.toml is not valid TOML"));

        let one = drawn(&fixture, 89, 30);
        assert!(one
            .iter()
            .any(|r| r.contains("\u{25CF} offline") && r.contains("3 keys")));
        assert!(one
            .iter()
            .any(|r| r.contains("work") && r.contains("base only, ignored: groq.api_key")));
        assert!(one
            .iter()
            .any(|r| r.contains("broken: profiles/broken.toml")));
        // The pane lists the focused one's keys.
        let mut on_work = three();
        to(&mut on_work, "work");
        let two = drawn(&on_work, 90, 30);
        assert!(two.iter().any(|r| r.contains("general.language = \"fr\"")));
        assert!(two
            .iter()
            .any(|r| r.contains("groq.api_key (base only, ignored)")));
        for (w, h) in [(60, 18), (89, 30), (90, 30), (200, 60)] {
            assert!(
                !drawn(&on_work, w, h).iter().any(|r| r.contains("gsk_")),
                "{w}x{h}"
            );
        }
        // A key the config does not know is named, its value not shown.
        let table = profile::read_table("[groq]\napikey = \"gsk_typo\"\n").unwrap();
        assert_eq!(
            key_lines(&table, &["groq.apikey".to_string()]),
            [("groq.apikey (unknown, ignored)".to_string(), Tone::Warn)]
        );
    }

    /// Enter switches through the daemon; the status follows it to the new
    /// daemon's `info`, which a nudged poll brings sooner.
    #[test]
    fn enter_switches_and_the_status_follows_the_restart() {
        let mut fixture = mockup_profiles(PROFILES, None);
        open_profiles(&mut fixture);
        to(&mut fixture, "offline");
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::SwitchProfile("offline".into())]
        );
        assert_eq!(said(&fixture).text, "asking whisrsd to switch to offline");
        // One at a time.
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert!(said(&fixture)
            .text
            .starts_with("still switching to offline"));
        assert_eq!(
            fixture.app.update(Msg::Switched {
                name: "offline".into(),
                answer: Ok(Switched::Restarting),
            }),
            Some(Effect::All(vec![Effect::ReadProfiles, Effect::PollSoon]))
        );
        assert_eq!(said(&fixture).text, "switching to offline...");
        // The old daemon, and none, say nothing yet.
        assert_eq!(up(&mut fixture, None, 1), None);
        fixture.app.update(Msg::Daemon(DaemonView::Down));
        assert_eq!(said(&fixture).text, "switching to offline...");
        assert_eq!(
            up(&mut fixture, Some("offline"), 2),
            Some(Effect::ReadProfiles)
        );
        assert_eq!(said(&fixture), Notice::new(Tone::Ok, "on offline"));
        assert!(fixture.app.profiles.switching.is_none());
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("profiles \u{00B7} profile offline \u{00B7} \u{25CF}")));

        // Base is a switch too; the daemon's answers are the status.
        to(&mut fixture, "base");
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::SwitchProfile("base".into())]
        );
        let busy = "cannot switch profile while a recording is being transcribed";
        assert_eq!(
            fixture.app.update(Msg::Switched {
                name: "base".into(),
                answer: Err(busy.into()),
            }),
            None
        );
        assert_eq!(said(&fixture), Notice::new(Tone::Error, busy));
        press(&mut fixture.app, "\n");
        fixture.app.update(Msg::Switched {
            name: "base".into(),
            answer: Ok(Switched::Already),
        });
        assert_eq!(said(&fixture).text, "already on base");
        // With no daemon the state file was written, and says so.
        press(&mut fixture.app, "\n");
        let line = "whisrsd is not running; base applies when it starts";
        assert_eq!(
            fixture.app.update(Msg::Switched {
                name: "base".into(),
                answer: Ok(Switched::Offline(line.into())),
            }),
            Some(Effect::ReadProfiles)
        );
        assert_eq!(said(&fixture).text, line);
    }

    /// A restart that comes back on another profile says what it runs on.
    #[test]
    fn a_switch_that_came_back_on_another_profile_says_so() {
        let mut fixture = mockup_profiles(PROFILES, None);
        open_profiles(&mut fixture);
        up(&mut fixture, None, 5);
        to(&mut fixture, "work");
        press(&mut fixture.app, "\n");
        fixture.app.update(Msg::Switched {
            name: "work".into(),
            answer: Ok(Switched::Restarting),
        });
        up(&mut fixture, None, 6);
        assert_eq!(
            said(&fixture),
            Notice::new(Tone::Warn, "whisrsd came back on base, not work")
        );
    }

    /// A broken profile is refused before anything is sent, with why.
    #[test]
    fn a_broken_profile_is_refused_before_sending() {
        let mut fixture = three();
        to(&mut fixture, "broken");
        assert!(press(&mut fixture.app, "\n").is_empty());
        let status = said(&fixture);
        assert_eq!(status.tone, Tone::Error);
        assert!(
            status
                .text
                .starts_with("cannot switch to broken: profiles/broken.toml is not valid TOML"),
            "{}",
            status.text
        );
    }

    /// `n` takes a name checked as it is typed: the rule, the reserved
    /// words, a name there already; Enter makes an empty 0600 file and
    /// opens it in Config.
    #[test]
    fn n_makes_an_empty_private_profile() {
        let mut fixture = three();
        press(&mut fixture.app, "n");
        assert_eq!(fixture.app.profiles.mode(), Mode::Text);
        assert!(said(&fixture).text.starts_with("a name: 1 to 32"));
        let typed = |fixture: &mut Fixture, text: &str| {
            chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
            type_text(&mut fixture.app, text);
            said(fixture)
        };
        let bad = typed(&mut fixture, "Meeting");
        assert_eq!(bad.tone, Tone::Error);
        assert!(bad.text.contains("not a profile name"), "{}", bad.text);
        assert!(typed(&mut fixture, "next").text.contains("reserved"));
        assert_eq!(
            typed(&mut fixture, "offline").text,
            "there is a profile named offline already"
        );
        // Enter on a bad name keeps the input.
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert_eq!(fixture.app.profiles.mode(), Mode::Text);
        assert_eq!(
            typed(&mut fixture, "meeting").text,
            "enter makes profiles/meeting.toml"
        );
        assert!(drawn(&fixture, 90, 30).iter().any(|r| r.contains("name")));
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::CreateProfile("meeting".into())]
        );
        assert_eq!(fixture.app.profiles.mode(), Mode::Profiles);

        let paths = fixture.app.paths.profile_paths();
        assert_eq!(create(&paths, "meeting"), Ok(()));
        let file = paths.profile("meeting");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            create(&paths, "meeting"),
            Err("profiles/meeting.toml is there already".into())
        );
        assert_eq!(
            fixture
                .app
                .update(Msg::ProfileCreated("meeting".into(), Ok(()))),
            Some(Effect::All(vec![
                Effect::ReadProfiles,
                Effect::OpenProfile("meeting".into())
            ]))
        );
        assert_eq!(said(&fixture).text, "made profiles/meeting.toml");
        open_profile(&mut fixture, "meeting");
        assert_eq!(fixture.app.screen(), Screen::Profile);

        // Esc while typing makes nothing.
        let mut fixture = three();
        press(&mut fixture.app, "n");
        type_text(&mut fixture.app, "x");
        assert!(press(&mut fixture.app, "\x1b").is_empty());
        assert_eq!(said(&fixture).text, "no profile made");
        assert_eq!(fixture.app.profiles.mode(), Mode::Profiles);
    }

    /// Back from profile mode the cursor is on its profile: one `n` just
    /// made, or one `l` opened, wherever the cursor went meanwhile.
    #[test]
    fn back_from_profile_mode_the_cursor_is_on_its_profile() {
        let mut profiles = PROFILES.to_vec();
        profiles.push(("quiet", ""));
        let mut fixture = mockup_profiles(&profiles, Some("offline"));
        open_profiles(&mut fixture);
        to(&mut fixture, "base");
        press(&mut fixture.app, "n");
        type_text(&mut fixture.app, "meeting");
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::CreateProfile("meeting".into())]
        );
        let paths = fixture.app.paths.profile_paths();
        assert_eq!(create(&paths, "meeting"), Ok(()));
        fixture
            .app
            .update(Msg::ProfileCreated("meeting".into(), Ok(())));
        read_profiles(&mut fixture.app);
        assert_eq!(
            fixture.app.profiles.focused(),
            "meeting",
            "the read lands on it"
        );
        open_profile(&mut fixture, "meeting");
        assert_eq!(fixture.app.screen(), Screen::Profile);
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        assert_eq!(fixture.app.profiles.focused(), "meeting");

        to(&mut fixture, "quiet");
        assert_eq!(
            press(&mut fixture.app, "l"),
            [Effect::OpenProfile("quiet".into())]
        );
        open_profile(&mut fixture, "quiet");
        fixture.app.profiles.select(0);
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        assert_eq!(fixture.app.profiles.focused(), "quiet");
        read_profiles(&mut fixture.app);
        assert_eq!(fixture.app.profiles.focused(), "quiet", "and stays there");
    }

    /// `d` asks, and only `y` deletes; base and the active profile are
    /// refused at once.
    #[test]
    fn d_deletes_after_asking() {
        let mut fixture = three();
        to(&mut fixture, "base");
        press(&mut fixture.app, "d");
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(
            said(&fixture).text,
            "base is config.toml itself, so it is not deleted"
        );
        to(&mut fixture, "offline");
        press(&mut fixture.app, "d");
        assert_eq!(fixture.app.dialog, None);
        assert!(said(&fixture)
            .text
            .starts_with("offline is the active profile"));

        to(&mut fixture, "work");
        let paths = fixture.app.paths.profile_paths();
        for keep in ["n", "\x1b", "\n"] {
            assert!(press(&mut fixture.app, "d").is_empty());
            assert_eq!(
                fixture.app.dialog,
                Some(Dialog::DeleteProfile {
                    name: "work".into()
                })
            );
            assert!(drawn(&fixture, 90, 30)
                .iter()
                .any(|r| r.contains("Delete profile work? profiles/work.toml goes for good.")));
            assert!(press(&mut fixture.app, keep).is_empty(), "{keep:?}");
            assert_eq!(said(&fixture).text, "kept work", "{keep:?}");
            assert!(paths.profile("work").is_file());
        }
        press(&mut fixture.app, "d");
        assert_eq!(
            press(&mut fixture.app, "y"),
            [Effect::DeleteProfile("work".into())]
        );
        assert_eq!(delete(&paths, "work"), Ok(()));
        assert!(!paths.profile("work").exists());
        assert_eq!(
            fixture
                .app
                .update(Msg::ProfileDeleted("work".into(), Ok(()))),
            Some(Effect::ReadProfiles)
        );
        assert_eq!(said(&fixture).text, "deleted profiles/work.toml");
        read_profiles(&mut fixture.app);
        assert_eq!(fixture.app.profiles.rows(), ["base", "broken", "offline"]);
    }

    /// `l` opens a profile in Config, base as plain Config over Profiles;
    /// Config's unsaved edits are asked about first. `e` is the file.
    #[test]
    fn l_opens_config_and_e_the_file() {
        let mut fixture = three();
        to(&mut fixture, "base");
        assert!(press(&mut fixture.app, "l").is_empty());
        assert_eq!(fixture.app.screen(), Screen::Config);
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        assert_eq!(press(&mut fixture.app, "e"), [Effect::OpenEditor]);

        to(&mut fixture, "offline");
        assert_eq!(
            press(&mut fixture.app, "l"),
            [Effect::OpenProfile("offline".into())]
        );
        assert_eq!(
            press(&mut fixture.app, "e"),
            [Effect::EditProfile("offline".into())]
        );
        assert_eq!(
            fixture
                .app
                .update(Msg::ProfileEdited("offline".into(), Ok(()))),
            Some(Effect::ReadProfiles)
        );

        // Config has an edit: it is asked about, and d drops it.
        fixture
            .app
            .config
            .form
            .set("general.notify", Some(toml::Value::Boolean(false)))
            .unwrap();
        assert!(press(&mut fixture.app, "l").is_empty());
        assert_eq!(
            fixture.app.dialog,
            Some(Dialog::Unsaved(After::Profile("offline".into())))
        );
        assert_eq!(
            press(&mut fixture.app, "d"),
            [Effect::OpenProfile("offline".into())]
        );
        assert!(!fixture.app.config.form.is_dirty());
    }

    #[test]
    fn help_and_esc() {
        let mut fixture = three();
        press(&mut fixture.app, "?");
        assert_eq!(fixture.app.dialog, Some(Dialog::Help(HelpFor::Profiles)));
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("profiles keys")));
        press(&mut fixture.app, "\x1b");
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), Screen::Home);
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("\u{258C} Profiles")));
    }

    /// The daemon on another profile than the state file names is said
    /// over the list, as `whisrs profile list` says it.
    #[test]
    fn the_daemon_on_another_profile_is_said() {
        let mut fixture = three();
        up(&mut fixture, None, 1);
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("whisrsd runs on base; offline applies when it restarts")));
        up(&mut fixture, Some("offline"), 1);
        assert!(!drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("applies when it restarts")));
    }

    /// A profile is judged as a client judges it: a cloud backend whose key
    /// may be in whisrsd's environment alone is a note, never broken, and
    /// Enter leaves the call to the daemon's dry run.
    #[test]
    fn a_key_whisrsd_may_have_is_a_note_not_a_failure() {
        let mut fixture = mockup_profiles(&[("cloud", "[general]\nbackend = \"openai\"\n")], None);
        open_profiles(&mut fixture);
        let cloud = fixture.app.profiles.item("cloud").unwrap().clone();
        assert_eq!(cloud.error, None);
        to(&mut fixture, "cloud");
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::SwitchProfile("cloud".into())]
        );
        // The note comes only when this shell has no key either.
        if std::env::var_os("WHISRS_OPENAI_API_KEY").is_none() {
            assert!(
                cloud
                    .notes
                    .iter()
                    .any(|n| n.contains("WHISRS_OPENAI_API_KEY")),
                "{:?}",
                cloud.notes
            );
            assert!(drawn(&fixture, 89, 30)
                .iter()
                .any(|r| r.contains("cloud") && r.contains("no API key in config.toml")));
            assert!(drawn(&fixture, 90, 30)
                .iter()
                .any(|r| r.contains("WHISRS_OPENAI_API_KEY")));
        }
    }

    /// What the merge drops is named and never shown, whatever its shape: a
    /// section given as an array, an array of tables or a string, over a
    /// base-only key; on the list, the pane, the switch, profile mode.
    #[test]
    fn a_dropped_value_is_never_shown() {
        let shapes = [
            ("array", "groq = [\"gsk_x1\"]\n"),
            ("tables", "[[groq]]\napi_key = \"gsk_x2\"\n"),
            ("string", "groq = \"gsk_x3\"\n"),
            ("rec", "recording = [\"gsk_x4\"]\n"),
        ];
        let mut fixture = mockup_profiles(&shapes, None);
        open_profiles(&mut fixture);
        let clean = |fixture: &Fixture, at: &str| {
            for (w, h) in [(60, 18), (89, 30), (90, 30), (200, 60)] {
                for row in drawn(fixture, w, h) {
                    assert!(!row.contains("gsk_x"), "{at} {w}x{h}: {row}");
                }
            }
            let status = fixture.app.status.as_ref().map(|s| s.text.clone());
            assert!(!status.unwrap_or_default().contains("gsk_x"), "{at}");
            for notice in fixture
                .app
                .notices
                .iter()
                .chain(&fixture.app.profile_notices)
            {
                assert!(!notice.text.contains("gsk_x"), "{at}: {}", notice.text);
            }
        };
        for (name, _) in shapes {
            let item = fixture.app.profiles.item(name).unwrap();
            let section = if name == "rec" { "recording" } else { "groq" };
            assert_eq!(
                item.keys,
                [(format!("{section} (base only, ignored)"), Tone::Muted)],
                "{name}"
            );
            to(&mut fixture, name);
            clean(&fixture, name);
            press(&mut fixture.app, "\n");
            clean(&fixture, name);
            fixture.app.profiles.asked = None;
            if press(&mut fixture.app, "l") == [Effect::OpenProfile(name.into())] {
                open_profile(&mut fixture, name);
                clean(&fixture, name);
                if fixture.app.screen() == Screen::Profile {
                    press(&mut fixture.app, "\x1b");
                }
            }
            assert_eq!(fixture.app.screen(), Screen::Profiles, "{name}");
        }
    }

    /// A switch whisrsd took but did not restart for: once the quick polls
    /// are over with the same start, or after 10 s at the latest, the status
    /// says so and the next switch can go.
    #[test]
    fn a_switch_with_no_new_start_gives_up() {
        let mut fixture = mockup_profiles(PROFILES, None);
        open_profiles(&mut fixture);
        to(&mut fixture, "offline");
        press(&mut fixture.app, "\n");
        fixture.app.update(Msg::Switched {
            name: "offline".into(),
            answer: Ok(Switched::Restarting),
        });
        assert!(fixture.app.spinning(), "it ticks while it waits");
        let back = |fixture: &mut Fixture, by: u64| {
            let switching = fixture.app.profiles.switching.as_mut().unwrap();
            switching.since -= std::time::Duration::from_secs(by);
        };
        // Within the quick polls, the same start says nothing yet.
        assert_eq!(up(&mut fixture, None, 1), None);
        assert!(fixture.app.profiles.switching.is_some());
        back(&mut fixture, 6);
        assert_eq!(
            fixture.app.update(Msg::Daemon(fixture.app.daemon.clone())),
            Some(Effect::ReadProfiles)
        );
        assert_eq!(
            said(&fixture),
            Notice::new(
                Tone::Warn,
                "the restart did not happen: whisrsd is still on base"
            )
        );
        assert!(fixture.app.profiles.switching.is_none());
        assert!(!fixture.app.spinning());

        // No answer at all: the ticks give up after 10 s.
        press(&mut fixture.app, "\n");
        fixture.app.update(Msg::Switched {
            name: "offline".into(),
            answer: Ok(Switched::Restarting),
        });
        fixture.app.update(Msg::Daemon(DaemonView::Down));
        assert_eq!(fixture.app.update(Msg::Tick), None);
        back(&mut fixture, 11);
        assert_eq!(fixture.app.update(Msg::Tick), Some(Effect::ReadProfiles));
        assert_eq!(
            said(&fixture).text,
            "whisrsd is not back since the switch to offline"
        );
        assert!(fixture.app.profiles.switching.is_none());

        // Restarting with no answer back: nothing was written, and it says so.
        press(&mut fixture.app, "\n");
        assert_eq!(
            fixture.app.update(Msg::Switched {
                name: "offline".into(),
                answer: Ok(Switched::Busy),
            }),
            None
        );
        assert_eq!(said(&fixture), Notice::new(Tone::Warn, profile::RESTARTING));
    }

    /// A switch made elsewhere (a hotkey, the tray) shows in `info`: the
    /// folder is read again, and the profile whisrsd runs on is not deleted.
    #[test]
    fn a_switch_made_elsewhere_is_followed() {
        let mut fixture = three();
        assert_eq!(
            up(&mut fixture, Some("work"), 1),
            Some(Effect::ReadProfiles)
        );
        assert_eq!(up(&mut fixture, Some("work"), 1), None);
        assert_eq!(
            up(&mut fixture, Some("work"), 2),
            Some(Effect::ReadProfiles)
        );
        to(&mut fixture, "work");
        assert!(press(&mut fixture.app, "d").is_empty());
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(
            said(&fixture).text,
            "work is the profile whisrsd runs on: switch to another before deleting it"
        );
        // Once it is on another, work goes as any.
        up(&mut fixture, None, 3);
        press(&mut fixture.app, "d");
        assert_eq!(
            fixture.app.dialog,
            Some(Dialog::DeleteProfile {
                name: "work".into()
            })
        );
    }

    #[test]
    fn profiles_snapshots() {
        let fixture = three();
        for (w, h) in [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)] {
            assert_snapshot(&format!("profiles-{w}x{h}"), &drawn(&fixture, w, h));
        }
        let mut empty = mockup_profiles(&[], None);
        open_profiles(&mut empty);
        for (w, h) in [(60, 18), (90, 30)] {
            assert_snapshot(&format!("profiles-empty-{w}x{h}"), &drawn(&empty, w, h));
        }
        let mut deleting = three();
        to(&mut deleting, "work");
        press(&mut deleting.app, "d");
        assert_snapshot("profiles-delete-90x30", &drawn(&deleting, 90, 30));
    }
}
