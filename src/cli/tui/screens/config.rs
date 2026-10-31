//! Config: every config.toml key, by section ([`schema::SECTIONS`]), on the
//! left the sections and on the right the rows of the one picked. At 90
//! columns and up both panes show; below, the list drills into a section.
//!
//! Values are picked, stepped or typed by kind ([`Kind`]): closed sets
//! cycle in place and open a picker, known values are suggested, numbers
//! step by an amount that fits, hotkeys are built in a dialog. Typing stays
//! wherever a value of the user's own is legitimate.
//!
//! Edits go into a [`Form`] and stay there until `w`, which validates the
//! config, writes it through the format-preserving writer (comments, key
//! order and unknown keys kept, 0600, atomic, `vocabulary.txt` by its own
//! rules) and restarts the daemon. An invalid config is refused with the
//! error shown and every edit kept.
//!
//! On a profile ([`load_profile`], profile mode) the rows show the config
//! the profile makes over config.toml, each tagged with whose value it is;
//! an edit goes into the profile, `x` takes a key out of it, and `w` writes
//! only the profile's file ([`save_profile`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use toml::Value;
use whisrs::config::keys::{self, KeySummary};
use whisrs::config::profile;
use whisrs::config::setup::key_test::{
    Outcome as Checked, Source as KeySource, Target as TestTarget, Tested,
};
use whisrs::config::vocabulary::{self, VocabularyBaseline};
use whisrs::config::write::{self, ExistingConfig};

use whisrs::overlay::palette::Palette;

use crate::tui::app::{App, Dialog};
use crate::tui::complete;
use crate::tui::form::{self, Form};
use crate::tui::keys::{Action, Mode};
use crate::tui::options::{self, Listed, Listing, Lists};
use crate::tui::schema::{
    self, Body, Check, Entries, Field, Kind, ListKind, Section, Steps, SECTIONS,
};
use crate::tui::theme::Rgb;
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::hotkey::{self, HotkeyDialog};
use crate::tui::widgets::input::Input;
use crate::tui::widgets::picker::{self, Picked, Picker};
use crate::tui::widgets::row::{Row, Value as Shown};
use crate::tui::widgets::{dialog, fit, scroll, width, wrap, Notice, Tone, Ui};
use crate::tui::{Env, Paths};

/// The section list's width in the two-pane layout.
const LIST_WIDTH: u16 = super::home::LIST_WIDTH;

/// The widest a label column gets.
const MAX_LABEL: usize = 24;

/// The most help lines under the rows.
const MAX_HELP: usize = 4;

/// How far `PgUp` and `PgDn` move the cursor.
const PAGE: usize = 10;

/// A config read from disk, ready to edit.
pub struct Loaded {
    pub form: Form,
    pub vocabulary: VocabularyBaseline,
    /// No file at the path: these are the defaults.
    pub fresh: bool,
    /// Unknown keys, an unreadable `vocabulary.txt`, a fresh config.
    pub notices: Vec<Notice>,
    /// Both files as read, to tell at save whether something else wrote
    /// one since, such as the daemon's `llm-command-set`.
    pub disk: OnDisk,
}

/// config.toml's and `vocabulary.txt`'s bytes (`None`: no file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnDisk {
    config: Option<Vec<u8>>,
    vocabulary: Option<Vec<u8>>,
}

impl OnDisk {
    fn read(paths: &Paths) -> OnDisk {
        OnDisk {
            config: std::fs::read(&paths.config).ok(),
            vocabulary: std::fs::read(&paths.vocabulary).ok(),
        }
    }

    /// The file that is not as it was in `self`, if one is not.
    fn changed(&self, now: &OnDisk) -> Option<&'static str> {
        if self.config != now.config {
            Some("config.toml")
        } else if self.vocabulary != now.vocabulary {
            Some("vocabulary.txt")
        } else {
            None
        }
    }
}

/// What a save did.
pub struct Saved {
    /// config.toml.
    pub path: PathBuf,
    pub warnings: Vec<String>,
    /// config.toml could not be merged into and was written from scratch.
    pub regenerated: Option<write::Regenerated>,
    /// The file as it was, kept here before the write.
    pub backup: Option<PathBuf>,
    /// Both stores as they are now on disk, to edit on from.
    pub loaded: Loaded,
}

/// Why a save did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// Refused or failed: nothing was written, or not all of it.
    Failed(String),
    /// This file changed on disk since it was read; nothing was written.
    Changed(&'static str),
    /// Written, but reading it back failed.
    Reread(String),
}

/// Load config.toml (and `vocabulary.txt`) to edit. A file that cannot be
/// used is refused with the text `whisrs config` exits with (#134): nothing
/// opens on defaults over a file the user has.
pub fn load(paths: &Paths) -> Result<Loaded, String> {
    let mut loaded = read(paths)
        .map_err(|(kind, message)| write::unusable_config_refusal(kind, &message, &paths.config))?;
    if loaded.fresh {
        loaded.notices.push(Notice::new(
            Tone::Muted,
            "No config.toml yet: these are the defaults, and w writes them. whisrs setup does \
             the whole onboarding.",
        ));
    }
    Ok(loaded)
}

/// [`load`] again after `$EDITOR`, with what validation says about the
/// result. A file that no longer loads is its error's first line.
pub fn reload(paths: &Paths) -> Result<Loaded, String> {
    let mut loaded = read(paths)
        .map_err(|(_, message)| message.lines().next().unwrap_or_default().to_string())?;
    match loaded.form.config().validate_to_write() {
        Ok(warnings) => loaded.notices.extend(
            warnings
                .into_iter()
                .map(|w| Notice::new(Tone::Warn, format!("warning: {w}"))),
        ),
        Err(e) => loaded
            .notices
            .push(Notice::new(Tone::Error, one_line(&e.to_string()))),
    }
    Ok(loaded)
}

/// What `whisrs setup` found at the config path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Missing,
    /// A config that loads.
    Usable,
    /// A file that does not deserialize, and why: setup starts fresh and
    /// writes over it, keeping it as `.bak` (#134).
    Broken(String),
}

/// [`load`] for `whisrs setup`: an unreadable file is refused as by
/// `config`, but one that does not deserialize opens on the defaults, with
/// what is wrong, for setup to write over (#134).
pub fn load_for_setup(paths: &Paths) -> Result<(Loaded, Found), String> {
    match read(paths) {
        Ok(loaded) => {
            let found = if loaded.fresh {
                Found::Missing
            } else {
                Found::Usable
            };
            Ok((loaded, found))
        }
        Err((write::UnusableKind::Unreadable, message)) => Err(write::unusable_config_refusal(
            write::UnusableKind::Unreadable,
            &message,
            &paths.config,
        )),
        Err((write::UnusableKind::Undeserializable, message)) => {
            let defaults = toml::from_str("").expect("an empty config parses");
            let mut loaded = loaded_from(paths, defaults)?;
            loaded.fresh = false;
            Ok((loaded, Found::Broken(message)))
        }
    }
}

/// `config` ready to edit as if read from `paths` now: `vocabulary.txt`
/// merged in as a load does, and both files' bytes kept to tell a change at
/// save. Setup's answers start here.
pub fn loaded_from(paths: &Paths, mut config: whisrs::Config) -> Result<Loaded, String> {
    let disk = OnDisk::read(paths);
    let (vocabulary, _) = VocabularyBaseline::load(&mut config, &paths.vocabulary);
    Ok(Loaded {
        form: Form::new(config)?,
        vocabulary,
        fresh: true,
        notices: Vec::new(),
        disk,
    })
}

fn read(paths: &Paths) -> Result<Loaded, (write::UnusableKind, String)> {
    // Before the load: a write between the two then reads as a change at
    // save, which only asks.
    let disk = OnDisk::read(paths);
    let (mut config, fresh, unknown) = match write::load_existing_config_from(&paths.config) {
        ExistingConfig::Loaded { config, unknown } => (config, false, unknown),
        ExistingConfig::Unusable { kind, message } => return Err((kind, message)),
        ExistingConfig::Missing => {
            let defaults = toml::from_str("").expect("an empty config parses");
            (defaults, true, Vec::new())
        }
    };
    // At load, not at save: the keys are about to be shown back as a config
    // that looks fine (#116).
    let mut notices: Vec<Notice> = unknown
        .iter()
        .map(|key| {
            Notice::new(
                Tone::Warn,
                format!("unknown key {} is ignored", show_key(key)),
            )
        })
        .collect();
    let (vocabulary, unreadable) = VocabularyBaseline::load(&mut config, &paths.vocabulary);
    if let Some(e) = unreadable {
        notices.push(Notice::new(
            Tone::Warn,
            format!(
                "cannot read {}: {e}. Its terms are not shown, and vocabulary edits stay in \
                 config.toml.",
                paths.vocabulary.display()
            ),
        ));
    }
    let form = Form::new(config).map_err(|e| (write::UnusableKind::Undeserializable, e))?;
    Ok(Loaded {
        form,
        vocabulary,
        fresh,
        notices,
        disk,
    })
}

/// An unknown key as `[section] key`, the way config.toml spells it.
fn show_key(path: &str) -> String {
    match path.split_once('.') {
        Some((section, key)) => format!("[{section}] {key}"),
        None => path.to_string(),
    }
}

/// Validate the edited config and write it, `vocabulary.txt` by the plan
/// `write_config_and_vocabulary` makes; then read both back. Unless
/// `overwrite`, a config.toml or `vocabulary.txt` that changed on disk since
/// it was read is left alone: writing it would undo what changed it.
pub fn save(screen: &ConfigScreen, paths: &Paths, overwrite: bool) -> Result<Saved, SaveError> {
    if !overwrite {
        if let Some(file) = screen.disk.changed(&OnDisk::read(paths)) {
            return Err(SaveError::Changed(file));
        }
    }
    write_out(screen.form.config(), &screen.vocabulary, paths, None)
}

/// Write `keys` (dotted) of `answers` onto config.toml as it is on disk now,
/// and nothing else: every other key, section and comment of the file stays
/// as it is. With `backup`, the file is first kept as `config.toml.bak`
/// (0600). Unless `overwrite`, a file that changed since `answers` read it
/// is left alone, as [`save`] does. A file that does not deserialize is
/// written over from the defaults, and kept as `.bak` (#134).
pub fn save_keys(
    answers: &ConfigScreen,
    keys: &[String],
    backup: bool,
    paths: &Paths,
    overwrite: bool,
) -> Result<Saved, SaveError> {
    if !overwrite {
        if let Some(file) = answers.disk.changed(&OnDisk::read(paths)) {
            return Err(SaveError::Changed(file));
        }
    }
    let mut base = match read(paths) {
        Ok(loaded) => loaded,
        Err((write::UnusableKind::Undeserializable, _)) => {
            let defaults = toml::from_str("").expect("an empty config parses");
            loaded_from(paths, defaults).map_err(SaveError::Failed)?
        }
        Err((write::UnusableKind::Unreadable, message)) => {
            return Err(SaveError::Failed(
                message.lines().next().unwrap_or_default().to_string(),
            ))
        }
    };
    for key in keys {
        let value = answers.form.effective(key).cloned();
        base.form
            .set(key, value)
            .map_err(|why| SaveError::Failed(format!("{key}: {why}")))?;
    }
    // Checked before the backup: a refusal writes nothing at all.
    base.form
        .config()
        .validate_to_write()
        .map_err(|e| SaveError::Failed(one_line(&e.to_string())))?;
    let kept = match (backup, std::fs::read_to_string(&paths.config)) {
        (true, Ok(text)) => {
            let bak = write::config_backup_path(&paths.config);
            write::write_private_file(&bak, &text).map_err(|e| {
                SaveError::Failed(format!("cannot keep a copy at {}: {e}", bak.display()))
            })?;
            Some(bak)
        }
        _ => None,
    };
    write_out(base.form.config(), &base.vocabulary, paths, kept)
}

/// Validate `config` for a write, write it and `vocabulary.txt` by the
/// plan `write_config_and_vocabulary` makes, and read both back. A missing
/// API key is a warning here: the daemon can take it from its env.
fn write_out(
    config: &whisrs::Config,
    vocabulary: &VocabularyBaseline,
    paths: &Paths,
    backup: Option<PathBuf>,
) -> Result<Saved, SaveError> {
    let warnings = config
        .validate_to_write()
        .map_err(|e| SaveError::Failed(one_line(&e.to_string())))?;
    let wrote = vocabulary::write_config_and_vocabulary(
        config,
        vocabulary,
        &paths.config,
        &paths.vocabulary,
    )
    .map_err(|e| SaveError::Failed(format!("{e:#}")))?;
    let loaded = read(paths).map_err(|(_, message)| {
        SaveError::Reread(message.lines().next().unwrap_or_default().to_string())
    })?;
    Ok(Saved {
        path: paths.config.clone(),
        warnings: warnings.into_iter().map(|w| w.message).collect(),
        regenerated: wrote.regenerated,
        backup,
        loaded,
    })
}

/// A message on one line: its line breaks are spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The sections a profile does not set from here: the replacements, a map
/// of their own. `e` sets them in the profile's file.
const BASE_SECTIONS: [&str; 1] = ["Replacements"];

/// The key a profile does not set from here: the TUI writes the vocabulary
/// to `vocabulary.txt`, which every profile shares. `e` sets a profile's
/// own list in its file.
const VOCABULARY: &str = "general.vocabulary";

/// Config on a profile: what it sets, as loaded and as edited, over
/// config.toml as loaded.
pub struct ProfileEdit {
    pub name: String,
    /// config.toml as loaded: what a row the profile does not set shows,
    /// and what `x` puts back.
    base: Form,
    /// The dotted paths its file sets, as loaded.
    set: BTreeSet<String>,
    /// Edited since: each is written at `w`, even at config.toml's value.
    added: BTreeSet<String>,
    /// Taken out with `x` since: removed from the file at `w`.
    removed: BTreeSet<String>,
}

impl ProfileEdit {
    /// Whether the profile sets `path`, as edited.
    pub fn sets(&self, path: &str) -> bool {
        self.added.contains(path) || (self.set.contains(path) && !self.removed.contains(path))
    }

    /// `profiles/<name>.toml`, as messages name it.
    pub fn label(&self) -> String {
        profile::label(&self.name)
    }
}

/// What [`load_profile`] read: the config profile `name` makes over
/// config.toml, ready to edit, and what its file sets.
pub struct ProfileLoad {
    pub loaded: Loaded,
    pub edit: ProfileEdit,
}

/// Load profile `name` over config.toml for profile mode, as the daemon
/// merges it (`config::profile::merge`), `vocabulary.txt` after. A profile
/// that is not TOML, or does not load over config.toml, is refused with
/// why: there is no config to show, and `e` fixes it. One that loads but
/// does not validate opens, its error a notice.
pub fn load_profile(paths: &Paths, name: &str) -> Result<ProfileLoad, String> {
    let base = read(paths).map_err(|(_, message)| {
        format!(
            "config.toml does not load: {}",
            message.lines().next().unwrap_or_default()
        )
    })?;
    let base_text = read_base(paths)?;
    let label = profile::label(name);
    let text = std::fs::read_to_string(paths.profile_paths().profile(name))
        .map_err(|e| format!("cannot read {label}: {e}"))?;
    let eff = profile::effective_checked(&base_text, name, Some(&text), profile::Check::Client)?;
    let why = || {
        eff.error
            .clone()
            .unwrap_or_else(|| format!("{label} does not load"))
    };
    let over = profile::read_table(&text).map_err(|_| why())?;
    let under = profile::read_table(&base_text).map_err(|e| e.message().to_string())?;
    let merged = profile::merge(&under, &over);
    let mut config: whisrs::Config = Value::Table(merged.table).try_into().map_err(|_| why())?;
    let (vocabulary, _) = VocabularyBaseline::load(&mut config, &paths.vocabulary);
    let mut notices: Vec<Notice> = eff
        .warnings
        .iter()
        .map(|w| Notice::new(Tone::Warn, w.clone()))
        .collect();
    if let Some(error) = &eff.error {
        notices.push(Notice::new(Tone::Error, one_line(error)));
    }
    Ok(ProfileLoad {
        loaded: Loaded {
            form: Form::new(config)?,
            vocabulary,
            fresh: false,
            notices,
            disk: base.disk,
        },
        edit: ProfileEdit {
            name: name.to_string(),
            base: base.form,
            set: merged.set,
            added: BTreeSet::new(),
            removed: BTreeSet::new(),
        },
    })
}

/// config.toml's text: empty when there is no file.
fn read_base(paths: &Paths) -> Result<String, String> {
    match std::fs::read_to_string(&paths.config) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("cannot read config.toml: {e}")),
    }
}

/// What a profile's save did.
pub struct ProfileSaved {
    pub name: String,
    /// The state file names it: the daemon runs on it once restarted.
    pub active: bool,
    pub warnings: Vec<String>,
    /// The profile as it is now on disk, to edit on from.
    pub load: ProfileLoad,
}

/// Profile mode's `w`: the keys edited set and the ones `x` took out
/// removed, onto the profile's file as it is on disk now (toml_edit:
/// comments, order and every other key kept), only where the value changed.
/// The config it then makes over config.toml is validated first: refused,
/// nothing is written and every edit stays. Written whole, 0600.
pub fn save_profile(screen: &ConfigScreen, paths: &Paths) -> Result<ProfileSaved, String> {
    let edit = screen
        .profile
        .as_ref()
        .ok_or_else(|| "not a profile".to_string())?;
    let profiles = paths.profile_paths();
    let label = edit.label();
    let text = match std::fs::read_to_string(profiles.profile(&edit.name)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {label}: {e}")),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|_| format!("{label} is not valid TOML any more: e opens it to fix it, then w"))?;
    let file = profile::read_table(&text).unwrap_or_default();
    for path in &edit.removed {
        remove_leaf(&mut doc, path);
    }
    for path in &edit.added {
        match screen.form.effective(path) {
            Some(value) if form::lookup(&file, path) == Some(value) => {}
            Some(value) => set_leaf(&mut doc, path, value)?,
            None => remove_leaf(&mut doc, path),
        }
    }
    let written = doc.to_string();
    let warnings = check_profile(&read_base(paths)?, &edit.name, &written)?;
    if written != text {
        profile::write_profile(&profiles, &edit.name, &written)
            .map_err(|e| format!("cannot write {label}: {e}"))?;
    }
    Ok(ProfileSaved {
        name: edit.name.clone(),
        active: profile::read_active(&profiles.state) == Ok(Some(edit.name.clone())),
        warnings,
        load: load_profile(paths, &edit.name)?,
    })
}

/// The config profile `name`'s file, `text`, would make over config.toml as
/// `base_text` has it, checked as a client checks it (`Check::Client`: a key
/// that may live in whisrsd's environment alone is a warning, and an error
/// config.toml alone has too is not the profile's): refused when it does
/// not load, else its warnings. No error quotes a base-only value.
fn check_profile(base_text: &str, name: &str, text: &str) -> Result<Vec<String>, String> {
    let eff = profile::effective_checked(base_text, name, Some(text), profile::Check::Client)?;
    if let Some(why) = eff.error {
        return Err(one_line(&why));
    }
    Ok(eff
        .config
        .validate_to_write()
        .map(|warnings| warnings.into_iter().map(|w| w.message).collect())
        .unwrap_or_default())
}

/// The key a profile's file spells the top-level `section` with: its own
/// name, else the alias the file has it under (`[local]`), so a write never
/// puts the section there twice.
fn spelled(doc: &toml_edit::DocumentMut, section: &str) -> String {
    if doc.contains_key(section) {
        return section.to_string();
    }
    profile::ALIASES
        .iter()
        .filter(|(at, _, name)| at.is_empty() && *name == section)
        .map(|(_, alias, _)| *alias)
        .find(|alias| doc.contains_key(alias))
        .unwrap_or(section)
        .to_string()
}

/// Set the dotted `path` of a profile's file to `value`, making the tables
/// on the way (inline ones inside an inline table).
fn set_leaf(doc: &mut toml_edit::DocumentMut, path: &str, value: &Value) -> Result<(), String> {
    // A TOML value as toml_edit holds it: parsed back from its own text.
    let parsed: toml_edit::DocumentMut = format!("v = {value}")
        .parse()
        .map_err(|_| format!("{path}: the value does not write as TOML"))?;
    let item = parsed.get("v").cloned().unwrap_or_default();
    let mut keys: Vec<String> = path.split('.').map(str::to_string).collect();
    keys[0] = spelled(doc, &keys[0]);
    put_item(doc.as_item_mut(), &keys, item).map_err(|why| format!("{path}: {why}"))
}

fn put_item(
    at: &mut toml_edit::Item,
    keys: &[String],
    item: toml_edit::Item,
) -> Result<(), String> {
    let inline = at.is_inline_table();
    let table = at
        .as_table_like_mut()
        .ok_or_else(|| "a key on the way is not a table in the file".to_string())?;
    let Some((first, rest)) = keys.split_first() else {
        return Err("empty path".to_string());
    };
    if rest.is_empty() {
        table.insert(first, item);
        return Ok(());
    }
    if !table.contains_key(first) {
        let empty = if inline {
            toml_edit::Item::Value(toml_edit::Value::InlineTable(Default::default()))
        } else {
            toml_edit::table()
        };
        table.insert(first, empty);
    }
    let next = table.get_mut(first).expect("the table was just made");
    put_item(next, rest, item)
}

/// Take the dotted `path` out of a profile's file. A table it leaves empty
/// goes too, unless a comment heads it.
fn remove_leaf(doc: &mut toml_edit::DocumentMut, path: &str) {
    let mut keys: Vec<String> = path.split('.').map(str::to_string).collect();
    keys[0] = spelled(doc, &keys[0]);
    take_item(doc.as_table_mut(), &keys);
}

fn take_item(table: &mut dyn toml_edit::TableLike, keys: &[String]) {
    let Some((first, rest)) = keys.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.remove(first);
        return;
    }
    let Some(item) = table.get_mut(first) else {
        return;
    };
    let Some(inner) = item.as_table_like_mut() else {
        return;
    };
    take_item(inner, rest);
    let emptied = inner.is_empty();
    let commented = item
        .as_table()
        .and_then(|t| t.decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| prefix.contains('#'));
    if emptied && !commented {
        table.remove(first);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sections,
    Detail,
}

/// A level of the detail pane.
#[derive(Clone)]
enum View {
    /// The section's own rows, entries or map.
    Section { cursor: usize },
    /// One entry of the section's array of tables.
    Entry { index: usize, cursor: usize },
    /// The items of a list key.
    List {
        path: String,
        field: &'static Field,
        cursor: usize,
    },
}

impl View {
    fn cursor(&self) -> usize {
        match self {
            View::Section { cursor } | View::Entry { cursor, .. } | View::List { cursor, .. } => {
                *cursor
            }
        }
    }

    fn set_cursor(&mut self, to: usize) {
        match self {
            View::Section { cursor } | View::Entry { cursor, .. } | View::List { cursor, .. } => {
                *cursor = to
            }
        }
    }
}

/// A row of the detail pane.
#[derive(Clone)]
enum Item {
    Field {
        field: &'static Field,
        path: String,
    },
    /// Entry `index` of the section's array of tables.
    Entry(usize),
    /// A key of the section's map.
    Pair(String),
    /// Item `index` of the list being edited.
    Element(usize),
    /// A group's title among the rows (the Hotkeys section's `Agents`);
    /// the cursor passes over it.
    Heading(&'static str),
}

/// What is being typed.
#[derive(Clone)]
enum Target {
    Field {
        field: &'static Field,
        path: String,
    },
    /// An item of the list at `path`: an edit of `index`, or a new one.
    Element {
        path: String,
        index: Option<usize>,
    },
    /// A new map key; its value is asked next.
    NewKey,
    /// The value of map key `key`.
    PairValue {
        key: String,
    },
    /// A new entry's name.
    EntryName,
    Filter,
}

#[derive(Clone)]
struct Editing {
    target: Target,
    input: Input,
}

/// An open picker, and what its pick sets.
struct Picking {
    picker: Picker,
    purpose: Purpose,
}

#[derive(Clone)]
enum Purpose {
    Field {
        field: &'static Field,
        path: String,
    },
    /// An item of the list at `path`: an edit of `index`, or a new one.
    Element {
        field: &'static Field,
        path: String,
        index: Option<usize>,
    },
    /// The key of the open hotkey dialog.
    HotkeyKey,
}

/// An open hotkey dialog, and the row it sets.
struct Hotkeying {
    dialog: HotkeyDialog,
    field: &'static Field,
    path: String,
}

/// How a row's value sits: arrows either side while focused, dimmed at a
/// limit.
#[derive(Debug, Clone, Copy, Default)]
struct Look {
    arrows: bool,
    at_min: bool,
    at_max: bool,
}

/// What a key on Config asks of the app.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Back to Home.
    pub leave: bool,
    pub status: Option<Notice>,
    pub flash: Option<String>,
    pub dialog: Option<Dialog>,
    /// A list to fetch on a worker.
    pub load: Option<ListKind>,
    /// `t` on a key row: test what the row at this path holds.
    pub test: Option<(String, TestTarget)>,
}

/// A row's key test: running, or what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    Running,
    Done(Tested),
}

impl Outcome {
    fn say(tone: Tone, text: impl Into<String>) -> Outcome {
        Outcome {
            status: Some(Notice::new(tone, text)),
            ..Outcome::default()
        }
    }
}

pub struct ConfigScreen {
    pub form: Form,
    pub vocabulary: VocabularyBaseline,
    pub fresh: bool,
    /// config.toml stopped loading after `$EDITOR`: `w` and `r` are refused.
    pub broken: Option<String>,
    /// Both files as last read ([`Loaded::disk`]).
    disk: OnDisk,
    section: usize,
    focus: Focus,
    /// The detail pane's levels, never empty.
    views: Vec<View>,
    editing: Option<Editing>,
    picker: Option<Picking>,
    hotkey: Option<Hotkeying>,
    /// What the workers listed: devices, model files.
    pub lists: Lists,
    /// For `PATH` lookups and `~`.
    env: Env,
    /// Rows whose label or path holds this, in any case.
    filter: String,
    /// Only these rows (each at its path), in this order, and no section
    /// list: the wizard's step, or the Mic screen's device.
    pinned: Option<Vec<(&'static Field, String)>>,
    /// Where the section list was before [`ConfigScreen::pin`].
    unpinned: Option<(usize, Focus, Vec<View>)>,
    /// The key tests of the rows, by path. A changed value drops its own.
    pub tests: BTreeMap<String, Test>,
    /// The spinner's frame, while a test runs.
    pub spin: usize,
    /// Profile mode: the profile edited, over config.toml.
    pub profile: Option<ProfileEdit>,
    /// On base, the active profile and the paths it sets: its rows say it
    /// overrides them.
    pub overrides: Option<(String, BTreeSet<String>)>,
}

impl ConfigScreen {
    /// Profile mode on what [`load_profile`] read.
    pub fn on_profile(load: ProfileLoad, env: Env) -> ConfigScreen {
        let mut screen = ConfigScreen::new(load.loaded, env);
        screen.profile = Some(load.edit);
        screen
    }

    /// Edit on from the profile as read again, where the cursor was.
    pub fn reload_profile(&mut self, load: ProfileLoad) {
        self.profile = Some(load.edit);
        self.reloaded(load.loaded);
    }

    /// Whether anything is edited and not saved: in profile mode, a key
    /// added to the profile or taken out of it too.
    pub fn is_dirty(&self) -> bool {
        self.form.is_dirty()
            || self.profile.as_ref().is_some_and(|edit| {
                !edit.removed.is_empty() || edit.added.iter().any(|p| !edit.set.contains(p))
            })
    }

    pub fn new(loaded: Loaded, env: Env) -> ConfigScreen {
        ConfigScreen {
            form: loaded.form,
            vocabulary: loaded.vocabulary,
            fresh: loaded.fresh,
            broken: None,
            disk: loaded.disk,
            section: 0,
            focus: Focus::Sections,
            views: vec![View::Section { cursor: 0 }],
            editing: None,
            picker: None,
            hotkey: None,
            lists: Lists::default(),
            env,
            filter: String::new(),
            pinned: None,
            unpinned: None,
            tests: BTreeMap::new(),
            spin: 0,
            profile: None,
            overrides: None,
        }
    }

    /// Show only `fields`, the cursor on the first, for a screen that
    /// edits a few rows of the config (setup's steps, the Mic screen);
    /// [`ConfigScreen::unpin`] puts the section list back as it was.
    pub fn pin(&mut self, fields: Vec<&'static Field>) {
        self.pin_at(
            fields
                .into_iter()
                .map(|field| (field, field.path.to_string()))
                .collect(),
        );
    }

    /// [`ConfigScreen::pin`] with each row at a path of its own: an
    /// entry's fields (`agents.1.dir`). Pinned again (a row came or went
    /// with a value), the cursor stays on its row. An agent's harness notes
    /// when it is not on `PATH`, looked up here.
    pub fn pin_at(&mut self, rows: Vec<(&'static Field, String)>) {
        let kept = match self.pinned {
            Some(_) => self.focused().map(|(_, path)| path),
            None => {
                self.unpinned = Some((self.section, self.focus, self.views.clone()));
                None
            }
        };
        if rows.iter().any(|(_, path)| path.starts_with("agents.")) {
            self.lists.on_path = Some(options::harnesses_on_path(&self.env));
        }
        let cursor = kept
            .and_then(|kept| rows.iter().position(|(_, path)| *path == kept))
            .unwrap_or(0);
        self.close();
        self.pinned = Some(rows);
        self.section = 0;
        self.focus = Focus::Detail;
        self.views = vec![View::Section { cursor }];
    }

    pub fn unpin(&mut self) {
        self.close();
        self.pinned = None;
        if let Some((section, focus, views)) = self.unpinned.take() {
            self.section = section;
            self.focus = focus;
            self.views = views;
        }
        self.clamp();
    }

    /// The row with the cursor, when it is a key's.
    pub fn focused(&self) -> Option<(&'static Field, String)> {
        match self.items().get(self.view().cursor()) {
            Some(Item::Field { field, path }) => Some((field, path.clone())),
            _ => None,
        }
    }

    /// Open the editor of the row at `path` (its picker, input or hotkey
    /// dialog), as `Enter` does on Config.
    pub fn edit(&mut self, path: &str) -> Outcome {
        let Some(at) = self
            .items()
            .iter()
            .position(|item| matches!(item, Item::Field { path: p, .. } if p == path))
        else {
            return Outcome::default();
        };
        self.view_mut().set_cursor(at);
        self.key(Action::Enter)
    }

    /// Whether a text input has the keys.
    #[cfg(test)]
    pub fn typing(&self) -> bool {
        self.editing.is_some()
    }

    /// Who has the keys: a picker, the hotkey dialog, an input, or the
    /// rows.
    pub fn mode(&self) -> Mode {
        if self.picker.is_some() {
            Mode::Picker
        } else if self.hotkey.is_some() {
            Mode::Hotkey
        } else if self.editing.is_some() {
            Mode::Text
        } else {
            Mode::Browse
        }
    }

    /// A worker's list came: keep it, and fill an open picker of that list
    /// (any other picker is left as it is).
    pub fn listed(&mut self, listing: Listing) {
        let arrived = match listing {
            Listing::Devices(found) => {
                self.lists.devices = found.map_or_else(Listed::Failed, Listed::Ready);
                ListKind::Devices
            }
            Listing::Models(found) => {
                self.lists.models = found.map_or_else(Listed::Failed, Listed::Ready);
                ListKind::Models
            }
        };
        let Some(picking) = &mut self.picker else {
            return;
        };
        if let Purpose::Field { field, path } | Purpose::Element { field, path, .. } =
            &picking.purpose
        {
            if source(&field.kind).and_then(schema::Source::list) == Some(arrived) {
                let options = options::options(field, path, &self.form, &self.lists);
                picking.picker.set_options(options);
            }
        }
    }

    pub fn discard(&mut self) {
        self.form.discard();
        if let Some(edit) = &mut self.profile {
            edit.added.clear();
            edit.removed.clear();
        }
        self.close();
        self.clamp();
    }

    /// Close any input, picker or hotkey dialog.
    fn close(&mut self) {
        self.editing = None;
        self.picker = None;
        self.hotkey = None;
    }

    /// Edit on from what a save wrote.
    pub fn saved(&mut self, loaded: Loaded) {
        self.reloaded(loaded);
    }

    /// `path` alone was written ([`save_keys`]): it is no edit any more,
    /// and `loaded` is the file as it is now. The other edits stay.
    pub fn settled(&mut self, path: &str, loaded: Loaded) {
        self.form.settle(path);
        self.disk = loaded.disk;
    }

    /// Edit on from config.toml as read again.
    pub fn reloaded(&mut self, loaded: Loaded) {
        self.form = loaded.form;
        self.vocabulary = loaded.vocabulary;
        self.fresh = loaded.fresh;
        self.disk = loaded.disk;
        self.broken = None;
        self.close();
        self.clamp();
    }

    /// Delete entry `index` of the current section's array of tables.
    pub fn delete_entry(&mut self, index: usize, name: &str) -> Notice {
        let Some(entries) = self.entries() else {
            return Notice::new(Tone::Muted, "nothing to delete");
        };
        match self.form.remove_entry(entries.path, index) {
            Ok(()) => {
                self.clamp();
                Notice::new(Tone::Muted, format!("deleted {name}; w saves it"))
            }
            Err(why) => Notice::new(Tone::Error, why),
        }
    }

    fn current(&self) -> &'static Section {
        &SECTIONS[self.section]
    }

    /// The section is in this build, or says it is not.
    fn built(&self) -> bool {
        self.current().gate.is_none_or(|gate| gate.built())
    }

    fn entries(&self) -> Option<&'static Entries> {
        match self.current().body {
            Body::Entries(entries) => Some(entries),
            _ => None,
        }
    }

    fn view(&self) -> &View {
        self.views.last().expect("the detail pane has a level")
    }

    fn view_mut(&mut self) -> &mut View {
        self.views.last_mut().expect("the detail pane has a level")
    }

    /// Whether `field` of `section` passes the filter.
    fn passes(&self, section: &Section, field: &Field) -> bool {
        let query = self.filter.to_lowercase();
        query.is_empty()
            || section.title.to_lowercase().contains(&query)
            || field.label.to_lowercase().contains(&query)
            || field.path.to_lowercase().contains(&query)
    }

    /// The sections with a row that passes the filter.
    fn visible(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        (0..SECTIONS.len())
            .filter(|&i| {
                let section = &SECTIONS[i];
                query.is_empty()
                    || section.title.to_lowercase().contains(&query)
                    || match section.body {
                        Body::Rows(fields) => {
                            fields.iter().any(|f| self.passes(section, f))
                                || (section.entry_hotkeys && !self.entry_hotkeys().is_empty())
                        }
                        Body::Entries(entries) => entries.path.contains(&query),
                        Body::Map(map) => map.path.contains(&query),
                    }
            })
            .collect()
    }

    /// The rows of the detail pane's current level.
    fn items(&self) -> Vec<Item> {
        if let Some(rows) = &self.pinned {
            return rows
                .iter()
                .map(|(field, path)| Item::Field {
                    field,
                    path: path.clone(),
                })
                .collect();
        }
        let section = self.current();
        match self.view() {
            _ if !self.built() => Vec::new(),
            View::Section { .. } => match section.body {
                Body::Rows(fields) => {
                    let mut items: Vec<Item> = fields
                        .iter()
                        .filter(|f| self.passes(section, f))
                        .map(|field| Item::Field {
                            field,
                            path: field.path.to_string(),
                        })
                        .collect();
                    if section.entry_hotkeys {
                        items.extend(self.entry_hotkeys());
                    }
                    items
                }
                Body::Entries(entries) => (0..self.form.entries(entries.path).len())
                    .map(Item::Entry)
                    .collect(),
                Body::Map(map) => self.map(map.path).into_keys().map(Item::Pair).collect(),
            },
            View::Entry { index, .. } => {
                let entries = self.entries().expect("an entry is opened from its list");
                entries
                    .fields
                    .iter()
                    .map(|field| Item::Field {
                        field,
                        path: format!("{}.{index}.{}", entries.path, field.path),
                    })
                    .collect()
            }
            View::List { path, .. } => (0..self.list(path).len()).map(Item::Element).collect(),
        }
    }

    /// Every entry's hotkey rows, a heading per list that has entries:
    /// the entries' own keys, edited here as in their forms.
    fn entry_hotkeys(&self) -> Vec<Item> {
        let query = self.filter.to_lowercase();
        let mut items = Vec::new();
        for section in SECTIONS {
            let Body::Entries(entries) = section.body else {
                continue;
            };
            let mut rows = Vec::new();
            for index in 0..self.form.entries(entries.path).len() {
                for field in entries.fields {
                    if !matches!(field.kind, Kind::Hotkey { .. }) {
                        continue;
                    }
                    let path = format!("{}.{index}.{}", entries.path, field.path);
                    let label = self.entry_label(field, &path);
                    if query.is_empty()
                        || "hotkeys".contains(&query)
                        || field.label.contains(&query)
                        || label.to_lowercase().contains(&query)
                    {
                        rows.push(Item::Field { field, path });
                    }
                }
            }
            if !rows.is_empty() {
                items.push(Item::Heading(section.title));
                items.extend(rows);
            }
        }
        items
    }

    /// An entry's hotkey row as the Hotkeys section labels it: the entry's
    /// name (`jarvis`, `jarvis new session`, `translate-de set`).
    fn entry_label(&self, field: &Field, path: &str) -> String {
        let Some((list, index)) = entry_index(path) else {
            return field.label.to_string();
        };
        let name = self.name_of(list, index);
        match field.path {
            "hotkey" => name,
            "new_session_hotkey" => format!("{name} new session"),
            "set_hotkey" => format!("{name} set"),
            other => format!("{name} {other}"),
        }
    }

    /// Whether `path` is an entry's key shown outside its form (the Hotkeys
    /// section).
    fn borrowed(&self, path: &str) -> bool {
        self.pinned.is_none()
            && matches!(self.view(), View::Section { .. })
            && entry_index(path).is_some()
    }

    fn list(&self, path: &str) -> Vec<String> {
        self.form
            .effective(path)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn map(&self, path: &str) -> BTreeMap<String, String> {
        self.form
            .value(path)
            .and_then(Value::as_table)
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The name of entry `index`, for lists and messages.
    fn entry_name(&self, index: usize) -> String {
        let Some(entries) = self.entries() else {
            return String::new();
        };
        self.form
            .value(&format!("{}.{index}.name", entries.path))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map_or_else(|| format!("{} {}", entries.noun, index + 1), str::to_string)
    }

    /// Keep every cursor on a row after the rows changed.
    fn clamp(&mut self) {
        if let Some(entries) = self.entries() {
            let count = self.form.entries(entries.path).len();
            self.views.retain(|view| match view {
                View::Entry { index, .. } => *index < count,
                _ => true,
            });
        }
        let items = self.items();
        let view = self.view_mut();
        let at = view.cursor().min(items.len().saturating_sub(1));
        view.set_cursor(past_headings(&items, at, at));
    }

    /// Show section `to`, its own rows first.
    fn select(&mut self, to: usize) {
        if to != self.section {
            self.section = to;
            self.views = vec![View::Section { cursor: 0 }];
        }
    }

    /// Handle `action`. The app has already taken quit, help, save, `e`
    /// and restart.
    pub fn key(&mut self, action: Action) -> Outcome {
        if self.picker.is_some() {
            return self.picker_key(action);
        }
        if self.hotkey.is_some() {
            return self.hotkey_key(action);
        }
        if self.editing.is_some() {
            return self.edit_key(action);
        }
        if action == Action::Filter && self.pinned.is_none() {
            self.editing = Some(Editing {
                target: Target::Filter,
                input: Input::new(&self.filter, false),
            });
            return Outcome::default();
        }
        match self.focus {
            Focus::Sections => self.sections_key(action),
            Focus::Detail => self.detail_key(action),
        }
    }

    fn sections_key(&mut self, action: Action) -> Outcome {
        let visible = self.visible();
        let at = visible.iter().position(|&s| s == self.section).unwrap_or(0);
        let last = visible.len().saturating_sub(1);
        let to = match action {
            Action::Up => at.saturating_sub(1),
            Action::Down => (at + 1).min(last),
            Action::First => 0,
            Action::Last => last,
            Action::Enter | Action::Right | Action::Pane => {
                if !visible.is_empty() {
                    self.focus = Focus::Detail;
                }
                return Outcome {
                    load: self.prefetch(),
                    ..Outcome::default()
                };
            }
            Action::Back if !self.filter.is_empty() => {
                self.set_filter(String::new());
                return Outcome::default();
            }
            Action::Back => {
                return Outcome {
                    leave: true,
                    ..Outcome::default()
                }
            }
            _ => return Outcome::default(),
        };
        if let Some(&section) = visible.get(to) {
            self.select(section);
        }
        Outcome::default()
    }

    fn detail_key(&mut self, action: Action) -> Outcome {
        let items = self.items();
        let last = items.len().saturating_sub(1);
        let cursor = self.view().cursor();
        let to = match action {
            Action::Up => cursor.saturating_sub(1),
            Action::Down => (cursor + 1).min(last),
            Action::PageUp => cursor.saturating_sub(PAGE),
            Action::PageDown => (cursor + PAGE).min(last),
            Action::First => 0,
            Action::Last => last,
            Action::Pane | Action::Back if self.pinned.is_some() => {
                return Outcome::default();
            }
            Action::Pane => {
                self.focus = Focus::Sections;
                return Outcome::default();
            }
            Action::Back => {
                if self.views.len() > 1 {
                    self.views.pop();
                } else {
                    self.focus = Focus::Sections;
                }
                return Outcome::default();
            }
            _ => return self.item_key(action, items.get(cursor).cloned()),
        };
        let to = past_headings(&items, cursor, to);
        self.view_mut().set_cursor(to);
        Outcome {
            load: self.prefetch(),
            ..Outcome::default()
        }
    }

    /// Ask, once, for the worker list behind the row the cursor is on, so
    /// that `h`/`l` and its picker usually have it.
    fn prefetch(&mut self) -> Option<ListKind> {
        if self.focus != Focus::Detail {
            return None;
        }
        let Some(Item::Field { field, .. }) = self.items().get(self.view().cursor()).cloned()
        else {
            return None;
        };
        let kind = source(&field.kind)?.list()?;
        (self.lists.not_asked(kind) && self.lists.ask(kind)).then_some(kind)
    }

    /// Open entry `index` of the section's array of tables. An agent's
    /// form notes a harness not on `PATH`, looked up once here.
    fn open_entry(&mut self, index: usize) {
        if self
            .entries()
            .is_some_and(|entries| entries.path == "agents")
        {
            self.lists.on_path = Some(options::harnesses_on_path(&self.env));
        }
        self.views.push(View::Entry { index, cursor: 0 });
    }

    fn item_key(&mut self, action: Action, item: Option<Item>) -> Outcome {
        if let Some(Item::Field { field, path }) = &item {
            return self.field_key(field, path, action);
        }
        let view = self.view().clone();
        if self.profile.is_some() {
            // An entry opens to be looked at; what would change one is not.
            let edited = match (&view, self.current().body, action) {
                (
                    View::List { path, .. },
                    _,
                    Action::Add | Action::Delete | Action::Enter | Action::Right,
                ) => Some(path.clone()),
                (View::Section { .. }, Body::Entries(entries), Action::Add | Action::Delete) => {
                    Some(entries.path.to_string())
                }
                (
                    View::Section { .. },
                    Body::Map(map),
                    Action::Add | Action::Delete | Action::Enter | Action::Right,
                ) => Some(map.path.to_string()),
                _ => None,
            };
            if let Some(why) = edited.and_then(|path| self.refusal(&path)) {
                return Outcome::say(Tone::Warn, why);
            }
        }
        match (&view, self.current().body, action) {
            (View::List { path, field, .. }, _, Action::Add) => {
                if matches!(field.kind, Kind::List { items: Some(_), .. }) {
                    return self.open_element_picker(field, path, None);
                }
                self.type_into(Target::Element {
                    path: path.clone(),
                    index: None,
                });
            }
            (View::List { path, field, .. }, _, Action::Enter | Action::Right) => {
                if let Some(Item::Element(index)) = item {
                    if matches!(field.kind, Kind::List { items: Some(_), .. }) {
                        return self.open_element_picker(field, path, Some(index));
                    }
                    let text = self.list(path).get(index).cloned().unwrap_or_default();
                    self.editing = Some(Editing {
                        target: Target::Element {
                            path: path.clone(),
                            index: Some(index),
                        },
                        input: Input::new(&text, false),
                    });
                }
            }
            (View::List { path, .. }, _, Action::Delete) => {
                if let Some(Item::Element(index)) = item {
                    let mut list = self.list(path);
                    list.remove(index);
                    let outcome = self.set_list(path, list);
                    self.clamp();
                    return outcome;
                }
            }
            (View::List { .. }, _, Action::Left) => {
                self.views.pop();
            }
            (View::Section { .. }, Body::Entries(_), Action::Add) => {
                self.type_into(Target::EntryName);
            }
            (View::Section { .. }, Body::Entries(_), Action::Enter | Action::Right) => {
                if let Some(Item::Entry(index)) = item {
                    self.open_entry(index);
                }
            }
            (View::Section { .. }, Body::Entries(_), Action::Delete) => {
                if let Some(Item::Entry(index)) = item {
                    return Outcome {
                        dialog: Some(Dialog::DeleteEntry {
                            index,
                            name: self.entry_name(index),
                        }),
                        ..Outcome::default()
                    };
                }
            }
            (View::Section { .. }, Body::Map(_), Action::Add) => self.type_into(Target::NewKey),
            (View::Section { .. }, Body::Map(map), Action::Enter | Action::Right) => {
                if let Some(Item::Pair(key)) = item {
                    let value = self.map(map.path).remove(&key).unwrap_or_default();
                    self.editing = Some(Editing {
                        target: Target::PairValue { key },
                        input: Input::new(&value, false),
                    });
                }
            }
            (View::Section { .. }, Body::Map(map), Action::Delete) => {
                if let Some(Item::Pair(key)) = item {
                    let mut pairs = self.map(map.path);
                    pairs.remove(&key);
                    let outcome = self.set_map(map.path, pairs);
                    self.clamp();
                    return outcome;
                }
            }
            (_, _, Action::Left) if self.pinned.is_none() => self.focus = Focus::Sections,
            _ => {}
        }
        Outcome::default()
    }

    fn field_key(&mut self, field: &'static Field, path: &str, action: Action) -> Outcome {
        if self.profile.is_some() {
            if changes(&field.kind, action) {
                if let Some(why) = self.refusal(path) {
                    return Outcome::say(Tone::Warn, why);
                }
            }
            if action == Action::Reset {
                return self.take_out(field, path);
            }
        }
        let current = self.form.effective(path).cloned();
        let picked = picked(&field.kind);
        match (&field.kind, action) {
            (_, Action::Test) => {
                return match key_target(path) {
                    Some(target) => Outcome {
                        test: Some((path.to_string(), target)),
                        ..Outcome::default()
                    },
                    None => Outcome::say(
                        Tone::Muted,
                        "t tests a key: it works on the api_key rows and the whisper model",
                    ),
                }
            }
            (_, Action::Reset) => {
                let result = self.form.reset(path);
                return self.apply(path, field, result);
            }
            (Kind::Bool, Action::Enter | Action::Toggle | Action::Left | Action::Right) => {
                let on = current.and_then(|v| v.as_bool()).unwrap_or(false);
                return self.set(path, field, Some(Value::Boolean(!on)));
            }
            (_, Action::Left | Action::Right) if picked => {
                let by = if action == Action::Right { 1 } else { -1 };
                return self.step(field, path, by);
            }
            // Space is the next value of a closed set, as it always was.
            (
                Kind::Choice(_) | Kind::MaybeChoice { .. } | Kind::MaybeBool { .. },
                Action::Toggle,
            ) => return self.step(field, path, 1),
            (_, Action::Enter) if picked => return self.open_picker(field, path),
            (
                Kind::Int(steps),
                Action::Left | Action::Right | Action::BigLeft | Action::BigRight,
            ) => {
                let by = match action {
                    Action::Left => -steps.step,
                    Action::Right => steps.step,
                    Action::BigLeft => -steps.big,
                    _ => steps.big,
                };
                let n = current.and_then(|v| v.as_integer()).unwrap_or(steps.min);
                let n = n.saturating_add(by).clamp(steps.min, steps.max);
                return self.set(path, field, Some(Value::Integer(n)));
            }
            (
                Kind::Float {
                    min,
                    max,
                    step,
                    big,
                    ..
                },
                Action::Left | Action::Right | Action::BigLeft | Action::BigRight,
            ) => {
                let by = match action {
                    Action::Left => -step,
                    Action::Right => *step,
                    Action::BigLeft => -big,
                    _ => *big,
                };
                let n = current.and_then(|v| v.as_float()).unwrap_or(*min);
                let n = ((n + by) * 100.0).round() / 100.0;
                return self.set(path, field, Some(Value::Float(n.clamp(*min, *max))));
            }
            (Kind::Text { .. }, Action::Enter)
                if current
                    .as_ref()
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains('\n')) =>
            {
                // A one-line input would flatten it.
                return Outcome::say(
                    Tone::Muted,
                    format!("{} spans lines: e edits it in config.toml", field.label),
                );
            }
            (
                Kind::Int(_)
                | Kind::Float { .. }
                | Kind::Text { .. }
                | Kind::Path { .. }
                | Kind::Color,
                Action::Enter,
            ) => {
                // The exact value: rounded, an untouched commit would change it.
                let text = current.map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                });
                self.editing = Some(Editing {
                    target: Target::Field {
                        field,
                        path: path.to_string(),
                    },
                    input: Input::new(text.as_deref().unwrap_or_default(), false),
                });
            }
            (Kind::Hotkey { .. }, Action::Enter) => return self.open_hotkey(field, path),
            (Kind::Secret, Action::Enter) => {
                // Starts empty: a key is typed whole, never shown back.
                self.editing = Some(Editing {
                    target: Target::Field {
                        field,
                        path: path.to_string(),
                    },
                    input: Input::new("", true),
                });
            }
            (Kind::List { .. }, Action::Enter | Action::Right) => {
                self.views.push(View::List {
                    path: path.to_string(),
                    field,
                    cursor: 0,
                });
            }
            (_, Action::Left) if self.pinned.is_none() => self.focus = Focus::Sections,
            _ => {}
        }
        Outcome::default()
    }

    /// What `field` at `path` offers now.
    fn options(&self, field: &Field, path: &str) -> options::Options {
        options::options(field, path, &self.form, &self.lists)
    }

    /// The value at `path` as a picker or a ring names it: `None` unset (or
    /// a value that means the same), `true`/`false` for an optional toggle.
    fn current_text(&self, field: &Field, path: &str) -> Option<String> {
        let value = self.form.effective(path)?;
        match field.kind {
            Kind::MaybeBool { .. } => value.as_bool().map(|on| on.to_string()),
            _ => value
                .as_str()
                .filter(|text| !means_unset(path, text))
                .map(str::to_string),
        }
    }

    /// `h`/`l` on a picked row: the next value of its ring, round. A row
    /// whose list comes from a worker is left as it is until the list is
    /// here: stepping through half a list would drop the value set.
    fn step(&mut self, field: &'static Field, path: &str, by: i64) -> Outcome {
        if let Some(kind) = source(&field.kind).and_then(schema::Source::list) {
            if !self.lists.ready(kind) {
                return Outcome {
                    load: self.lists.ask(kind).then_some(kind),
                    ..Outcome::say(Tone::Muted, LOOKING)
                };
            }
        }
        let ring = self.options(field, path).ring();
        if ring.is_empty() {
            return Outcome::default();
        }
        let now = self.current_text(field, path);
        let next = match ring.iter().position(|v| *v == now) {
            Some(at) => cycle(at, ring.len(), by),
            None if by > 0 => 0,
            None => ring.len() - 1,
        };
        let value = to_value(&field.kind, ring[next].clone());
        self.set(path, field, value)
    }

    /// `Enter` on a picked row: its picker, with the list behind it asked
    /// for (again: devices come and go).
    fn open_picker(&mut self, field: &'static Field, path: &str) -> Outcome {
        let load = source(&field.kind)
            .and_then(schema::Source::list)
            .filter(|kind| self.lists.ask(*kind));
        let options = self.options(field, path);
        let current = self.current_text(field, path);
        let paths = matches!(field.kind, Kind::Path { .. });
        self.picker = Some(Picking {
            picker: Picker::new(field.label, options, current.as_deref(), paths),
            purpose: Purpose::Field {
                field,
                path: path.to_string(),
            },
        });
        Outcome {
            load,
            ..Outcome::default()
        }
    }

    /// The picker for an item of a list whose items are suggested.
    fn open_element_picker(
        &mut self,
        field: &'static Field,
        path: &str,
        index: Option<usize>,
    ) -> Outcome {
        let current = index.and_then(|i| self.list(path).get(i).cloned());
        let options = self.options(field, path);
        self.picker = Some(Picking {
            picker: Picker::new(field.label, options, current.as_deref(), false),
            purpose: Purpose::Element {
                field,
                path: path.to_string(),
                index,
            },
        });
        Outcome::default()
    }

    fn picker_key(&mut self, action: Action) -> Outcome {
        let picking = self.picker.as_mut().expect("a picker");
        match picking.picker.key(action) {
            picker::Answer::None => Outcome::default(),
            picker::Answer::Closed => {
                self.picker = None;
                Outcome::default()
            }
            picker::Answer::Refused(why) => Outcome::say(Tone::Warn, why),
            picker::Answer::Complete => {
                let tilde = match &picking.purpose {
                    Purpose::Field { field, .. } => {
                        matches!(field.kind, Kind::Path { tilde: true, .. })
                    }
                    _ => true,
                };
                let done =
                    complete::complete(&picking.picker.filter(), self.env.get("HOME"), tilde);
                picking.picker.set_filter(&done.text);
                candidates(&done.candidates)
            }
            picker::Answer::Picked(picked) => {
                let purpose = picking.purpose.clone();
                let (outcome, done) = self.apply_pick(&purpose, picked);
                if done {
                    self.picker = None;
                }
                outcome
            }
        }
    }

    /// Set what a picker picked; `false` when it was refused and the
    /// picker stays, with why.
    fn apply_pick(&mut self, purpose: &Purpose, picked: Picked) -> (Outcome, bool) {
        match purpose {
            Purpose::HotkeyKey => {
                if let (Some(open), Picked::Value(name)) = (self.hotkey.as_mut(), &picked) {
                    open.dialog.set_key(name);
                }
                (Outcome::default(), true)
            }
            Purpose::Field { field, path } => {
                let text = match picked {
                    Picked::Unset => None,
                    Picked::Value(text) | Picked::Typed(text) => {
                        let text = self.written(&field.kind, &text);
                        match check_of(&field.kind).map_or(Ok(text.clone()), |check| check(&text)) {
                            Ok(text) => Some(text),
                            Err(why) => {
                                return (
                                    Outcome::say(Tone::Error, format!("{}: {why}", field.label)),
                                    false,
                                )
                            }
                        }
                    }
                };
                let outcome = self.set(path, field, to_value(&field.kind, text));
                let done = outcome.status.is_none();
                (outcome, done)
            }
            Purpose::Element { field, path, index } => {
                let (Picked::Value(text) | Picked::Typed(text)) = picked else {
                    return (Outcome::default(), true);
                };
                match check_of(&field.kind).map_or(Ok(text.clone()), |check| check(&text)) {
                    Ok(text) => (self.set_element(path, *index, &text), true),
                    Err(why) => (
                        Outcome::say(Tone::Error, format!("{}: {why}", field.label)),
                        false,
                    ),
                }
            }
        }
    }

    /// `Enter` on a hotkey row: the dialog, or typing for a value that does
    /// not parse.
    fn open_hotkey(&mut self, field: &'static Field, path: &str) -> Outcome {
        let value = self
            .form
            .effective(path)
            .and_then(Value::as_str)
            .map(str::to_string);
        let title = self.hotkey_title(field, path);
        match HotkeyDialog::new(&title, value.as_deref(), self.hotkeys_but(path)) {
            Some(dialog) => {
                self.hotkey = Some(Hotkeying {
                    dialog,
                    field,
                    path: path.to_string(),
                });
                Outcome::default()
            }
            None => {
                let text = value.unwrap_or_default();
                let why = schema::check_hotkey(&text).err().unwrap_or_default();
                self.editing = Some(Editing {
                    target: Target::Field {
                        field,
                        path: path.to_string(),
                    },
                    input: Input::new(&text, false),
                });
                Outcome::say(
                    Tone::Warn,
                    format!("{text} does not parse ({why}); type it"),
                )
            }
        }
    }

    fn hotkey_key(&mut self, action: Action) -> Outcome {
        let open = self.hotkey.as_mut().expect("a hotkey dialog");
        match open.dialog.key(action) {
            hotkey::Answer::None => Outcome::default(),
            hotkey::Answer::Cancel => {
                self.hotkey = None;
                Outcome::default()
            }
            hotkey::Answer::PickKey => {
                let current = open.dialog.key_name();
                self.picker = Some(Picking {
                    picker: Picker::new("key", options::hotkey_keys(), current, false),
                    purpose: Purpose::HotkeyKey,
                });
                Outcome::default()
            }
            hotkey::Answer::Invalid(why) => Outcome::say(Tone::Error, why),
            hotkey::Answer::Save(value) => {
                let clash = open.dialog.clash().map(str::to_string);
                let (field, path) = (open.field, open.path.clone());
                let outcome = self.set(&path, field, Some(Value::String(value.clone())));
                if outcome.status.is_some() {
                    return outcome;
                }
                self.hotkey = None;
                match clash {
                    Some(who) => Outcome {
                        status: Some(Notice::new(Tone::Warn, format!("{value} is also {who}"))),
                        ..outcome
                    },
                    None => outcome,
                }
            }
            hotkey::Answer::Type(text) => {
                let (field, path) = (open.field, open.path.clone());
                self.hotkey = None;
                self.editing = Some(Editing {
                    target: Target::Field { field, path },
                    input: Input::new(&text, false),
                });
                Outcome::default()
            }
            hotkey::Answer::Unset => {
                let (field, path) = (open.field, open.path.clone());
                self.hotkey = None;
                let result = self.form.reset(&path);
                self.apply(&path, field, result)
            }
        }
    }

    /// The hotkey dialog's title: `toggle hotkey`, `jarvis hotkey`. A
    /// `[hotkeys]` key is named as its section names it, whatever its row
    /// is labelled (setup's `hotkey`).
    fn hotkey_title(&self, field: &Field, path: &str) -> String {
        match entry_index(path) {
            Some((list, index)) => format!("{} {}", self.name_of(list, index), field.label),
            None => {
                let label = schema::find(path).map_or(field.label, |row| row.label);
                format!("{label} hotkey")
            }
        }
    }

    /// The name of entry `index` of the array of tables `list`.
    fn name_of(&self, list: &str, index: usize) -> String {
        let noun = SECTIONS
            .iter()
            .find_map(|s| match s.body {
                Body::Entries(entries) if entries.path == list => Some(entries.noun),
                _ => None,
            })
            .unwrap_or("entry");
        self.form
            .value(&format!("{list}.{index}.name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map_or_else(|| format!("{noun} {}", index + 1), str::to_string)
    }

    /// Every hotkey of the form but the one at `path`: who has it, and the
    /// value.
    fn hotkeys_but(&self, path: &str) -> Vec<(String, String)> {
        let mut all = Vec::new();
        let mut add = |who: String, at: String| {
            if at == path {
                return;
            }
            if let Some(value) = self.form.effective(&at).and_then(Value::as_str) {
                if !value.trim().is_empty() {
                    all.push((who, value.to_string()));
                }
            }
        };
        for section in SECTIONS {
            match section.body {
                Body::Rows(fields) => {
                    for field in fields {
                        if matches!(field.kind, Kind::Hotkey { .. }) {
                            add(field.label.to_string(), field.path.to_string());
                        }
                    }
                }
                Body::Entries(entries) => {
                    for index in 0..self.form.entries(entries.path).len() {
                        let name = self.name_of(entries.path, index);
                        for field in entries.fields {
                            if matches!(field.kind, Kind::Hotkey { .. }) {
                                add(
                                    format!("{} {name} {}", entries.noun, field.label),
                                    format!("{}.{index}.{}", entries.path, field.path),
                                );
                            }
                        }
                    }
                }
                Body::Map(_) => {}
            }
        }
        all
    }

    /// Start typing a new value for `target`.
    fn type_into(&mut self, target: Target) {
        self.editing = Some(Editing {
            target,
            input: Input::new("", false),
        });
    }

    fn set(&mut self, path: &str, field: &Field, value: Option<Value>) -> Outcome {
        // A profile's file holds values: no key in it unsets config.toml's.
        let unsets_base = self
            .profile
            .as_ref()
            .is_some_and(|edit| value.is_none() && edit.base.effective(path).is_some());
        if unsets_base {
            return Outcome::say(
                Tone::Warn,
                format!(
                    "{}: a profile cannot unset what config.toml sets; pick a value, or x for \
                     config.toml's",
                    field.label
                ),
            );
        }
        let result = self.form.set(path, value);
        self.apply(path, field, result)
    }

    fn apply(&mut self, path: &str, field: &Field, result: Result<bool, String>) -> Outcome {
        // Only a change is an edit: a commit or a pick of the value there
        // already pins nothing into a profile.
        if result == Ok(true) {
            self.tests.remove(path);
            self.mark(path);
        }
        match result {
            Ok(true) => Outcome {
                flash: Some(path.to_string()),
                ..Outcome::default()
            },
            Ok(false) => Outcome::default(),
            Err(why) => Outcome::say(Tone::Error, format!("{}: {why}", field.label)),
        }
    }

    fn set_list(&mut self, path: &str, list: Vec<String>) -> Outcome {
        let value = Value::Array(list.into_iter().map(Value::String).collect());
        match self.form.set(path, Some(value)) {
            Ok(changed) => {
                if changed {
                    self.mark(path);
                }
                Outcome::default()
            }
            Err(why) => Outcome::say(Tone::Error, why),
        }
    }

    /// In profile mode, `path` was changed: the profile sets it now, even
    /// at config.toml's value; or, left unset as config.toml leaves it, no
    /// longer does.
    fn mark(&mut self, path: &str) {
        let has = self.form.effective(path).is_some();
        let Some(edit) = self.profile.as_mut() else {
            return;
        };
        if has {
            edit.added.insert(path.to_string());
            edit.removed.remove(path);
        } else {
            edit.added.remove(path);
            if edit.set.contains(path) {
                edit.removed.insert(path.to_string());
            }
        }
    }

    /// Profile mode's `x`: the key out of the profile, its row back at
    /// config.toml's value.
    fn take_out(&mut self, field: &Field, path: &str) -> Outcome {
        let Some(edit) = self.profile.as_ref() else {
            return Outcome::default();
        };
        if !edit.sets(path) {
            return Outcome::say(Tone::Muted, format!("{} does not set this", edit.name));
        }
        let base = edit.base.value(path).cloned();
        let changed = match self.form.set(path, base) {
            Ok(changed) => changed,
            Err(why) => return Outcome::say(Tone::Error, format!("{}: {why}", field.label)),
        };
        if let Some(edit) = self.profile.as_mut() {
            edit.added.remove(path);
            if edit.set.contains(path) {
                edit.removed.insert(path.to_string());
            }
        }
        if changed {
            self.tests.remove(path);
        }
        Outcome {
            flash: Some(path.to_string()),
            ..Outcome::default()
        }
    }

    /// In profile mode, why the key at `path`, a row of this section, is not
    /// set from here, if it is not.
    fn refusal(&self, path: &str) -> Option<String> {
        let edit = self.profile.as_ref()?;
        if profile::is_base_only(path) {
            return Some(profile::BASE_ONLY_NOTE.to_string());
        }
        if path == VOCABULARY {
            return Some(format!(
                "the vocabulary is edited on base, as vocabulary.txt is every profile's: e sets \
                 a list in {}",
                edit.label()
            ));
        }
        let title = self.current().title;
        BASE_SECTIONS
            .contains(&title)
            .then(|| format!("{title} is edited on base: e sets it in {}", edit.label()))
    }

    fn set_map(&mut self, path: &str, pairs: BTreeMap<String, String>) -> Outcome {
        let value = (!pairs.is_empty()).then(|| {
            Value::Table(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            )
        });
        match self.form.set(path, value) {
            Ok(_) => Outcome::default(),
            Err(why) => Outcome::say(Tone::Error, why),
        }
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        let visible = self.visible();
        if !visible.contains(&self.section) {
            if let Some(&first) = visible.first() {
                self.section = first;
            }
            self.views = vec![View::Section { cursor: 0 }];
        }
        if visible.is_empty() {
            self.focus = Focus::Sections;
        }
        self.clamp();
    }

    fn edit_key(&mut self, action: Action) -> Outcome {
        let editing = self.editing.as_mut().expect("typing");
        match action {
            Action::Enter => self.commit(),
            Action::Complete => {
                let Target::Field { field, .. } = &editing.target else {
                    return Outcome::default();
                };
                let Kind::Path { tilde, .. } = field.kind else {
                    return Outcome::default();
                };
                let done = complete::complete(&editing.input.text(), self.env.get("HOME"), tilde);
                editing.input = Input::new(&done.text, false);
                candidates(&done.candidates)
            }
            Action::Back => {
                let editing = self.editing.take().expect("typing");
                if matches!(editing.target, Target::Filter) {
                    self.set_filter(String::new());
                }
                Outcome::default()
            }
            other => {
                editing.input.apply(other);
                if matches!(editing.target, Target::Filter) {
                    let text = editing.input.text();
                    self.set_filter(text);
                }
                Outcome::default()
            }
        }
    }

    /// `Enter` in an input. A value the field refuses keeps the input open
    /// with the reason, so it can be fixed.
    fn commit(&mut self) -> Outcome {
        let editing = self.editing.take().expect("typing");
        let text = editing.input.text();
        let trimmed = text.trim();
        match &editing.target {
            Target::Filter => {
                self.set_filter(trimmed.to_string());
                Outcome::default()
            }
            Target::Field { field, path } => {
                match parse(field, &self.written(&field.kind, trimmed)) {
                    Err(why) => {
                        self.editing = Some(editing.clone());
                        Outcome::say(Tone::Error, format!("{}: {why}", field.label))
                    }
                    Ok(None) => Outcome::default(),
                    Ok(Some(value)) => {
                        let typed = value.as_ref().and_then(Value::as_str).map(str::to_string);
                        let outcome = self.set(path, field, value);
                        if outcome.status.is_some() {
                            self.editing = Some(editing.clone());
                            return outcome;
                        }
                        // A typed hotkey another row has too: said, not refused.
                        if let (Kind::Hotkey { .. }, Some(typed)) = (&field.kind, typed) {
                            let ours = whisrs::hotkey::hotkey_parts(&typed);
                            let other = self.hotkeys_but(path).into_iter().find(|(_, value)| {
                                ours.is_some() && whisrs::hotkey::hotkey_parts(value) == ours
                            });
                            if let Some((who, _)) = other {
                                return Outcome {
                                    status: Some(Notice::new(
                                        Tone::Warn,
                                        format!("{typed} is also {who}"),
                                    )),
                                    ..outcome
                                };
                            }
                        }
                        outcome
                    }
                }
            }
            Target::Element { path, index } => self.set_element(path, *index, trimmed),
            Target::NewKey => {
                let Body::Map(map) = self.current().body else {
                    return Outcome::default();
                };
                if trimmed.is_empty() {
                    return Outcome::default();
                }
                if self.map(map.path).contains_key(trimmed) {
                    self.editing = Some(editing.clone());
                    return Outcome::say(
                        Tone::Error,
                        format!("{trimmed} is there already: enter on it edits it"),
                    );
                }
                self.editing = Some(Editing {
                    target: Target::PairValue {
                        key: trimmed.to_string(),
                    },
                    input: Input::new("", false),
                });
                Outcome::default()
            }
            Target::PairValue { key } => {
                let Body::Map(map) = self.current().body else {
                    return Outcome::default();
                };
                let mut pairs = self.map(map.path);
                pairs.insert(key.clone(), trimmed.to_string());
                let at = pairs.keys().position(|k| k == key).unwrap_or(0);
                let outcome = self.set_map(map.path, pairs);
                self.view_mut().set_cursor(at);
                outcome
            }
            Target::EntryName => {
                let Some(entries) = self.entries() else {
                    return Outcome::default();
                };
                if trimmed.is_empty() {
                    return Outcome::default();
                }
                let count = self.form.entries(entries.path).len();
                if (0..count).any(|i| self.entry_name(i) == trimmed) {
                    self.editing = Some(editing.clone());
                    return Outcome::say(
                        Tone::Error,
                        format!("there is a {} named {trimmed} already", entries.noun),
                    );
                }
                let mut entry = toml::Table::new();
                for key in entries.required {
                    entry.insert(key.to_string(), Value::from(""));
                }
                entry.insert("name".into(), Value::from(trimmed));
                match self.form.push_entry(entries.path, entry) {
                    Ok(index) => {
                        self.view_mut().set_cursor(index);
                        self.open_entry(index);
                        Outcome::default()
                    }
                    Err(why) => Outcome::say(Tone::Error, why),
                }
            }
        }
    }
}

impl ConfigScreen {
    /// `text` as a row of `kind` stores it: a path whose key the daemon
    /// does not expand `~` in gets the home folder written out.
    fn written(&self, kind: &Kind, text: &str) -> String {
        match kind {
            Kind::Path { tilde: false, .. } => complete::expand_home(text, self.env.get("HOME")),
            _ => text.to_string(),
        }
    }

    /// Set item `index` of the list at `path` to `text` (blank removes
    /// it), or add `text` as a new one.
    fn set_element(&mut self, path: &str, index: Option<usize>, text: &str) -> Outcome {
        let mut list = self.list(path);
        match (index, text.is_empty()) {
            (Some(i), _) if i >= list.len() => return Outcome::default(),
            (Some(i), true) => {
                list.remove(i);
            }
            (Some(i), false) => list[i] = text.to_string(),
            (None, true) => return Outcome::default(),
            (None, false) => list.push(text.to_string()),
        }
        let added = index.is_none();
        let len = list.len();
        let outcome = self.set_list(path, list);
        if added {
            self.view_mut().set_cursor(len - 1);
        }
        self.clamp();
        outcome
    }
}

/// What a row whose worker list is not here yet says.
const LOOKING: &str = "looking...";

/// The list and the entry an entry's key is in (`agents.0.hotkey`); `None`
/// for a key of a section (`overlay.colors.ring`).
fn entry_index(path: &str) -> Option<(&str, usize)> {
    match path.split('.').collect::<Vec<_>>().as_slice() {
        [list, index, _] => Some((list, index.parse().ok()?)),
        _ => None,
    }
}

/// What `t` tests on the row at `path`, if it tests anything.
pub fn key_target(path: &str) -> Option<TestTarget> {
    let backend = |name: &str| Some(TestTarget::Backend(name.to_string()));
    match path {
        "groq.api_key" => backend("groq"),
        "deepgram.api_key" => backend("deepgram"),
        "openai.api_key" => backend("openai"),
        "asr-sidecar.api_key" => backend("asr-sidecar"),
        "openai-compatible-realtime.api_key" => backend("openai-compatible-realtime"),
        "local-whisper.model_path" => backend("local-whisper"),
        "tts.api_key" => Some(TestTarget::Tts),
        "llm.api_key" => Some(TestTarget::Llm),
        _ => None,
    }
}

/// A key test's outcome as a row shows it: one word, and its tone.
pub fn test_word(test: &Test, spin: usize, ui: &Ui) -> (String, Tone) {
    let Test::Done(tested) = test else {
        return (
            ui.glyphs.spinner[spin % ui.glyphs.spinner.len()].to_string(),
            Tone::Muted,
        );
    };
    let (word, tone) = match &tested.outcome {
        Checked::Valid { .. } => ("valid", Tone::Ok),
        Checked::Model(whisrs::config::setup::key_test::Model::Ggml(_)) => ("a model", Tone::Ok),
        Checked::Model(_) => ("not a model", Tone::Error),
        Checked::Rejected => ("rejected", Tone::Error),
        Checked::Malformed => ("not a key", Tone::Error),
        Checked::Limited => ("limited", Tone::Warn),
        Checked::Unreachable(_) => ("unreachable", Tone::Warn),
        Checked::Unexpected(_) => ("unexpected", Tone::Warn),
        Checked::NoKey => ("no key", Tone::Muted),
        Checked::NotTestable(_) => ("not testable", Tone::Muted),
    };
    (word.to_string(), tone)
}

/// A key test's outcome in full, for the status line: what it found, and
/// whose key it was when that was the env var.
pub fn test_notice(label: &str, tested: &Tested) -> Notice {
    let tone = match &tested.outcome {
        outcome if outcome.passed() => Tone::Ok,
        Checked::Rejected | Checked::Malformed | Checked::Model(_) => Tone::Error,
        Checked::NoKey | Checked::NotTestable(_) => Tone::Muted,
        _ => Tone::Warn,
    };
    let whose = match tested.source {
        KeySource::Env(var) => format!(" ({var} from this shell, which wins over config.toml)"),
        KeySource::Config | KeySource::None => String::new(),
    };
    Notice::new(tone, format!("{label}: {}{whose}", tested.outcome))
}

/// `text` with its first letter lower case, to follow a colon.
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `to`, or past it when it is a heading: on in the way the cursor moved,
/// else back. A heading is never where the cursor stops.
fn past_headings(items: &[Item], from: usize, to: usize) -> usize {
    let row = |i: &usize| !matches!(items.get(*i), Some(Item::Heading(_)));
    if row(&to) {
        return to;
    }
    let after = (to..items.len()).find(row);
    let before = (0..to).rev().find(row);
    let found = if to >= from {
        after.or(before)
    } else {
        before.or(after)
    };
    found.unwrap_or(from)
}

/// A value that means what unset means for `path`: `system_device =
/// "default"` is the default output's monitor, as no key is.
fn means_unset(path: &str, text: &str) -> bool {
    path == "recording.system_device" && matches!(text.trim(), "default" | "")
}

/// `at` moved `step` around a ring of `len`.
fn cycle(at: usize, len: usize, step: i64) -> usize {
    (at as i64 + step).rem_euclid(len as i64) as usize
}

/// Whether `action` on a row of `kind` changes its value, or opens what
/// does; moving and opening a list are not.
fn changes(kind: &Kind, action: Action) -> bool {
    match action {
        Action::Reset => true,
        Action::Enter => !matches!(kind, Kind::List { .. }),
        Action::Toggle => matches!(
            kind,
            Kind::Bool | Kind::Choice(_) | Kind::MaybeChoice { .. } | Kind::MaybeBool { .. }
        ),
        Action::Left | Action::Right => {
            matches!(kind, Kind::Bool | Kind::Int(_) | Kind::Float { .. }) || picked(kind)
        }
        Action::BigLeft | Action::BigRight => matches!(kind, Kind::Int(_) | Kind::Float { .. }),
        _ => false,
    }
}

/// Whether a row is picked (`h`/`l` step its values, `Enter` opens its
/// picker) rather than typed.
fn picked(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Choice(_)
            | Kind::MaybeChoice { .. }
            | Kind::MaybeBool { .. }
            | Kind::Suggest { .. }
            | Kind::Path { from: Some(_), .. }
    )
}

/// Where a row's suggestions come from, if they do.
fn source(kind: &Kind) -> Option<schema::Source> {
    match kind {
        Kind::Suggest { from, .. }
        | Kind::Path {
            from: Some(from), ..
        } => Some(*from),
        Kind::List {
            items: Some((from, _)),
            ..
        } => Some(*from),
        _ => None,
    }
}

/// The check a typed or picked value of `kind` goes through.
fn check_of(kind: &Kind) -> Option<Check> {
    match kind {
        Kind::Text { check, .. } | Kind::Suggest { check, .. } => *check,
        Kind::Color => Some(schema::check_color),
        Kind::Hotkey { .. } => Some(schema::check_hotkey),
        Kind::List {
            items: Some((_, check)),
            ..
        } => Some(*check),
        _ => None,
    }
}

/// A picked value as the config stores it: a bool for an optional toggle,
/// else the string; `None` unsets.
fn to_value(kind: &Kind, value: Option<String>) -> Option<Value> {
    let value = value?;
    Some(match kind {
        Kind::MaybeBool { .. } => Value::Boolean(value == "true"),
        _ => Value::String(value),
    })
}

/// The names a `Tab` could still complete to, as a status line.
fn candidates(names: &[String]) -> Outcome {
    const SHOWN: usize = 8;
    if names.is_empty() {
        return Outcome::default();
    }
    let mut text = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join("  ");
    if names.len() > SHOWN {
        text.push_str(&format!("  and {} more", names.len() - SHOWN));
    }
    Outcome::say(Tone::Muted, text)
}

/// What typing `text` into `field` sets: `Ok(None)` changes nothing,
/// `Ok(Some(None))` unsets the key.
fn parse(field: &Field, text: &str) -> Result<Option<Option<Value>>, String> {
    // A blank string unsets an optional key, and empties a plain one.
    let blank = |unset: Option<&str>| match unset {
        Some(_) => None,
        None => Some(Value::from("")),
    };
    match &field.kind {
        Kind::Int(Steps { min, max, .. }) => {
            let n: i64 = text.parse().map_err(|_| "not a whole number".to_string())?;
            if !(*min..=*max).contains(&n) {
                return Err(format!("use {min} to {max}"));
            }
            Ok(Some(Some(Value::Integer(n))))
        }
        Kind::Float { min, max, .. } => {
            let n: f64 = text.parse().map_err(|_| "not a number".to_string())?;
            if !(*min..=*max).contains(&n) {
                return Err(format!("use {min} to {max}"));
            }
            Ok(Some(Some(Value::Float(n))))
        }
        Kind::Text { unset, .. } | Kind::Suggest { unset, .. } | Kind::Path { unset, .. } => {
            if text.is_empty() {
                return Ok(Some(blank(*unset)));
            }
            let text = match check_of(&field.kind) {
                Some(check) => check(text)?,
                None => text.to_string(),
            };
            Ok(Some(Some(Value::String(text))))
        }
        Kind::Color if text.is_empty() => Ok(Some(None)),
        Kind::Hotkey { unset } if text.is_empty() => Ok(Some(blank(*unset))),
        Kind::Color | Kind::Hotkey { .. } => {
            let check = check_of(&field.kind).expect("colours and hotkeys are checked");
            Ok(Some(Some(Value::String(check(text)?))))
        }
        // A blank key changes nothing; `x` clears one.
        Kind::Secret if text.is_empty() => Ok(None),
        Kind::Secret => Ok(Some(Some(Value::from(text)))),
        _ => Ok(None),
    }
}

/// How a row reads.
fn shown(screen: &ConfigScreen, field: &Field, path: &str, env: &Env, ui: &Ui) -> Shown {
    let value = screen.form.effective(path);
    let config = screen.form.config();
    let on_off = |on: bool| Shown::plain(if on { "on" } else { "off" });
    let text = value.and_then(Value::as_str);
    match &field.kind {
        Kind::Bool => on_off(value.and_then(Value::as_bool).unwrap_or(false)),
        Kind::MaybeBool { unset } => match value.and_then(Value::as_bool) {
            Some(on) => on_off(on),
            None => Shown::muted(*unset),
        },
        Kind::Choice(_) => noted(screen, field, path, text.unwrap_or_default()),
        Kind::MaybeChoice { unset, .. } => match text {
            Some(text) => noted(screen, field, path, text),
            None => Shown::muted(options::unset_text(path, unset, config)),
        },
        Kind::Int(steps) => Shown::plain(show_int(
            value.and_then(Value::as_integer).unwrap_or(steps.min),
            steps,
        )),
        Kind::Float { percent: true, .. } => {
            let level = value
                .and_then(Value::as_float)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let on = (level * 10.0).round() as usize;
            let bar = format!(
                "{}{}",
                ui.glyphs.bar_on.repeat(on),
                ui.glyphs.bar_off.repeat(10 - on)
            );
            Shown::plain(bar).then(format!("{:.0}%", level * 100.0), Tone::Plain)
        }
        Kind::Float { .. } => Shown::plain(format!(
            "{:.2}",
            value.and_then(Value::as_float).unwrap_or(0.0)
        )),
        // A path that must be set, and is not: say so, not `empty`.
        Kind::Path { unset: None, .. } if text.is_none_or(|text| text.trim().is_empty()) => {
            Shown::muted("not set")
        }
        Kind::Text { unset, .. } | Kind::Path { unset, .. } => typed(text, *unset, ui),
        Kind::Suggest {
            unset: Some(unset), ..
        } if text.is_some_and(|text| means_unset(path, text)) => {
            Shown::muted(options::unset_text(path, unset, config))
        }
        Kind::Suggest { unset, .. } => match text {
            Some("") => Shown::muted("empty"),
            Some(text) => Shown::plain(text),
            None => Shown::muted(options::unset_text(
                path,
                unset.unwrap_or("not set"),
                config,
            )),
        },
        Kind::Color => color(screen, path, text, ui),
        Kind::Hotkey { unset } => match text.map(str::trim) {
            None | Some("") => Shown::muted(unset.unwrap_or("not set")),
            Some(hotkey) if schema::check_hotkey(hotkey).is_err() => {
                Shown::plain(hotkey).then("does not parse", Tone::Warn)
            }
            Some(hotkey) => Shown::plain(hotkey),
        },
        Kind::Secret => secret(screen, path, env),
        Kind::List { empty, .. } => {
            let list = screen.list(path);
            if list.is_empty() {
                Shown::muted(*empty)
            } else {
                Shown::plain(list.join(", "))
            }
        }
    }
}

/// A typed string: its first line, `empty`, or what applies while unset.
fn typed(text: Option<&str>, unset: Option<&str>, ui: &Ui) -> Shown {
    match text {
        Some("") => Shown::muted("empty"),
        Some(text) => {
            let mut lines = text.lines();
            let first = lines.next().unwrap_or_default();
            if lines.next().is_some() {
                Shown::plain(format!("{first}{}", ui.glyphs.ellipsis))
            } else {
                Shown::plain(first)
            }
        }
        None => Shown::muted(unset.unwrap_or("not set")),
    }
}

/// A choice's value, with why it cannot be used here or a word about it
/// (`not built in`, `not on PATH`).
fn noted(screen: &ConfigScreen, field: &Field, path: &str, value: &str) -> Shown {
    let shown = Shown::plain(value);
    let options = screen.options(field, path);
    let refused = match options.find(value) {
        Some(pick) => {
            if let Some(note) = &pick.note {
                return shown.then(note.clone(), Tone::Warn);
            }
            pick.refused.as_ref().map(|(short, _)| short.clone())
        }
        // An alias, or a value no list has.
        None if matches!(path, "general.backend" | "recording.backend") => {
            schema::backend_refusal(value).map(|(short, _)| short.to_string())
        }
        None => None,
    };
    match refused {
        Some(short) => shown.then(short, Tone::Muted),
        None => shown,
    }
}

/// A number with its unit, or the name it has (`0` is `off`).
fn show_int(n: i64, steps: &Steps) -> String {
    if let Some((_, name)) = steps.named.iter().find(|(v, _)| *v == n) {
        return name.to_string();
    }
    match steps.unit {
        "" => n.to_string(),
        "min" if n >= 60 => match (n / 60, n % 60) {
            (hours, 0) => format!("{n} min ({hours} h)"),
            (hours, minutes) => format!("{n} min ({hours} h {minutes:02})"),
        },
        "days" if n == 1 => "1 day".to_string(),
        unit => format!("{n} {unit}"),
    }
}

/// A colour row: the hex and a swatch of it, or `theme` and a swatch of the
/// theme's colour while unset.
fn color(screen: &ConfigScreen, path: &str, text: Option<&str>, ui: &Ui) -> Shown {
    let swatch = |shown: Shown, argb: [u8; 4]| {
        if ui.theme.color {
            shown.then(
                ui.glyphs.swatch.repeat(2),
                Tone::Swatch(Rgb::from_argb(argb)),
            )
        } else {
            shown
        }
    };
    match text {
        Some(hex) => match whisrs::parse_hex_color(hex) {
            Some(argb) => swatch(Shown::plain(hex), argb),
            None => Shown::plain(hex).then("not a colour", Tone::Warn),
        },
        None => {
            let overlay = screen.form.config().overlay.clone().unwrap_or_default();
            let palette = Palette::from_config(&overlay);
            let argb = match path.rsplit('.').next().unwrap_or_default() {
                "recording" => palette.rec_bar,
                "transcribing" => palette.trans_bar,
                "speaking" => palette.speak_bar,
                "background" => palette.bg,
                "ring" => palette.ring,
                _ => palette.glow,
            };
            swatch(Shown::muted("theme"), argb)
        }
    }
}

/// Where a key comes from: the env var, which wins as in the daemon, the
/// file (masked), or nowhere (#142).
fn secret(screen: &ConfigScreen, path: &str, env: &Env) -> Shown {
    let config = screen.form.config();
    let section = path.split('.').next().unwrap_or_default();
    let backend = match section {
        "deepgram" | "groq" | "openai" | "asr-sidecar" | "openai-compatible-realtime" => section,
        _ => {
            let key = screen
                .form
                .effective(path)
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty());
            return match (key, section) {
                (Some(key), _) => Shown::plain(format!("set {}", keys::mask_api_key(key))),
                (None, "tts") => {
                    let tts = config.tts.as_ref().map_or("groq", |t| t.backend.as_str());
                    match tts {
                        "groq" | "openai" | "deepgram" => {
                            Shown::muted(format!("uses the [{tts}] key"))
                        }
                        _ => Shown::muted("not needed"),
                    }
                }
                (None, _) => Shown::muted("not set"),
            };
        }
    };
    // The summary the daemon's resolvers would give with this backend on.
    let mut as_backend = config.clone();
    as_backend.general.backend = backend.to_string();
    let active = match config.general.backend.as_str() {
        "deepgram-streaming" => "deepgram",
        "openai-realtime" => "openai",
        "asr" | "vibevoice" => "asr-sidecar",
        other => other,
    } == backend;
    match keys::key_summary_from(&as_backend, |var| env.get(var).map(str::to_string)) {
        KeySummary::File { masked } => Shown::plain(format!("set {masked}")),
        KeySummary::Env { var, .. } => Shown::plain(var).then("(this shell)", Tone::Muted),
        KeySummary::NotSet if active => Shown::of("not set", Tone::Warn),
        KeySummary::NotSet => Shown::muted("not set"),
        KeySummary::Optional(_) => Shown::muted("not set, optional"),
        KeySummary::NotNeeded => Shown::muted("not needed"),
    }
}

/// A row's label, value, and how the value sits.
fn describe(screen: &ConfigScreen, item: &Item, env: &Env, ui: &Ui) -> (String, Shown, Look) {
    match item {
        Item::Field { field, path } => {
            let value = screen.form.effective(path);
            let look = match &field.kind {
                Kind::Int(steps) => {
                    let n = value.and_then(Value::as_integer).unwrap_or(steps.min);
                    Look {
                        arrows: true,
                        at_min: n <= steps.min,
                        at_max: n >= steps.max,
                    }
                }
                Kind::Float { min, max, .. } => {
                    let n = value.and_then(Value::as_float).unwrap_or(*min);
                    Look {
                        arrows: true,
                        at_min: n <= *min,
                        at_max: n >= *max,
                    }
                }
                kind => Look {
                    arrows: picked(kind),
                    ..Look::default()
                },
            };
            let label = if screen.borrowed(path) {
                screen.entry_label(field, path)
            } else {
                field.label.to_string()
            };
            let mut value = shown(screen, field, path, env, ui);
            if let Some(test) = screen.tests.get(path) {
                let (word, tone) = test_word(test, screen.spin, ui);
                value = value.then(format!("{} {word}", ui.glyphs.separator), tone);
            }
            (label, value, look)
        }
        Item::Heading(title) => (title.to_string(), Shown::default(), Look::default()),
        Item::Entry(index) => {
            let entries = screen.entries().expect("entries");
            let parts: Vec<(String, Tone)> = entries
                .summary
                .iter()
                .filter_map(|key| {
                    let path = format!("{}.{index}.{key}", entries.path);
                    let value = screen.form.value(&path)?.as_str()?;
                    let first = value.lines().next()?;
                    (!first.is_empty()).then(|| (first.to_string(), Tone::Plain))
                })
                .collect();
            (
                screen.entry_name(*index),
                Shown::joined(parts, ui.glyphs.separator),
                Look::default(),
            )
        }
        Item::Pair(key) => {
            let path = match screen.current().body {
                Body::Map(map) => map.path,
                _ => "",
            };
            let value = screen.map(path).remove(key).unwrap_or_default();
            (key.clone(), Shown::plain(value), Look::default())
        }
        Item::Element(index) => {
            let path = match screen.view() {
                View::List { path, .. } => path.as_str(),
                _ => "",
            };
            let text = screen.list(path).get(*index).cloned().unwrap_or_default();
            (String::new(), Shown::plain(text), Look::default())
        }
    }
}

/// The word at a row's end. In profile mode, whose value the row shows: an
/// accent `profile` for a key the profile sets, `base only` for one it
/// cannot, `base` for one it leaves to config.toml. On base, `offline sets
/// this` for a key the active profile overrides.
fn tag(screen: &ConfigScreen, item: &Item) -> Option<(String, Tone)> {
    let path = match item {
        Item::Field { path, .. } => path.clone(),
        Item::Pair(key) => match screen.current().body {
            Body::Map(map) => format!("{}.{key}", map.path),
            _ => return None,
        },
        Item::Entry(_) => screen.entries()?.path.to_string(),
        Item::Element(_) | Item::Heading(_) => return None,
    };
    if let Some(edit) = &screen.profile {
        return Some(if edit.sets(&path) {
            ("profile".to_string(), Tone::Accent)
        } else if screen.refusal(&path).is_some() {
            ("base only".to_string(), Tone::Muted)
        } else {
            ("base".to_string(), Tone::Muted)
        });
    }
    let (name, paths) = screen.overrides.as_ref()?;
    paths
        .contains(&path)
        .then(|| (format!("{name} sets this"), Tone::Muted))
}

/// The key an item flashes under.
fn flash_key(item: &Item) -> Option<&str> {
    match item {
        Item::Field { path, .. } => Some(path),
        _ => None,
    }
}

/// The help of the row with the cursor, for a screen that draws it itself.
pub fn row_help(screen: &ConfigScreen) -> String {
    help(screen, screen.items().get(screen.view().cursor()))
}

/// The help under the rows, for the row with the cursor.
fn help(screen: &ConfigScreen, item: Option<&Item>) -> String {
    let section = screen.current();
    match (screen.view(), item) {
        (View::List { field, .. }, _)
            if matches!(field.kind, Kind::List { items: Some(_), .. }) =>
        {
            "a adds an item from a list, enter picks another, d deletes it.".to_string()
        }
        (View::List { .. }, _) => "a adds an item, enter edits it, d deletes it.".to_string(),
        (_, Some(Item::Field { field, path })) if screen.borrowed(path) => {
            let (list, index) = entry_index(path).unwrap_or_default();
            let (noun, title) = SECTIONS
                .iter()
                .find_map(|s| match s.body {
                    Body::Entries(entries) if entries.path == list => Some((entries.noun, s.title)),
                    _ => None,
                })
                .unwrap_or(("entry", "its section"));
            let name = screen.name_of(list, index);
            format!(
                "The {label} of {noun} {name}: {help} Its other settings are under {title}.",
                label = field.label,
                help = lower_first(field.help),
            )
        }
        (_, Some(Item::Field { field, path })) => {
            let mut text = field.help.to_string();
            text.push_str(&layer_note(screen, path));
            if let Some(gate) = field.gate.filter(|gate| !gate.built()) {
                text.push_str(&format!(
                    " This whisrs was built without the {} feature, so the daemon ignores it.",
                    gate.name()
                ));
            }
            // What the value set now is, when its list says.
            if let Some(value) = screen.current_text(field, path) {
                if let Some(pick) = screen.options(field, path).find(&value) {
                    let about = match (&pick.refused, &pick.note) {
                        (Some((_, why)), _) => why.clone(),
                        (None, Some(note)) if pick.about.is_empty() => note.clone(),
                        (None, Some(note)) => format!("{}, {note}", pick.about),
                        (None, None) => pick.about.clone(),
                    };
                    if !about.is_empty() && !matches!(field.kind, Kind::MaybeBool { .. }) {
                        text.push_str(&format!(" {}: {about}.", pick.label));
                    }
                }
            }
            text
        }
        (_, _) => match section.body {
            Body::Entries(entries) => format!(
                "a adds {} {}, enter opens one, d deletes it.",
                article(entries.noun),
                entries.noun
            ),
            Body::Map(map) => format!(
                "{} a adds one, enter edits its {}, d deletes it.",
                map.help, map.value_label
            ),
            Body::Rows(_) => String::new(),
        },
    }
}

/// What the help adds about whose value a row is: in profile mode, that
/// the profile sets it (and `x` takes it out), leaves it to config.toml,
/// or cannot set it; on base, that the active profile overrides it.
fn layer_note(screen: &ConfigScreen, path: &str) -> String {
    if let Some(edit) = &screen.profile {
        let name = &edit.name;
        return if let Some(why) = screen.refusal(path) {
            format!(" Base only: {why}.")
        } else if edit.sets(path) {
            format!(" {name} sets this; x puts back config.toml's value.")
        } else {
            format!(" From config.toml; an edit sets it in {name}.")
        };
    }
    match &screen.overrides {
        Some((name, paths)) if paths.contains(path) => {
            format!(" {name} sets this, so an edit here applies once base is active.")
        }
        _ => String::new(),
    }
}

fn article(noun: &str) -> &'static str {
    if noun.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

/// The body rows Config asks for: the longest section with its help under
/// it (or the section list, when longer), and room for `messages` lines,
/// two at least, so a status does not grow the frame.
fn body_rows(messages: usize) -> u16 {
    let longest = SECTIONS
        .iter()
        .map(|section| match section.body {
            Body::Rows(fields) => fields.len(),
            // An entry's form, under its crumb.
            Body::Entries(entries) => entries.fields.len() + 2,
            Body::Map(_) => 0,
        })
        .max()
        .unwrap_or(0);
    let panes = SECTIONS.len().max(longest + 1 + 3);
    (panes + 1 + messages.max(2)) as u16
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    draw(app, &app.config, f, ui);
}

/// Config in profile mode, on the profile being edited.
pub fn view_profile(app: &App, f: &mut Frame, ui: &Ui) {
    if let Some(screen) = &app.profile_config {
        draw(app, screen, f, ui);
    }
}

fn draw(app: &App, screen: &ConfigScreen, f: &mut Frame, ui: &Ui) {
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = if screen.profile.is_some() {
        let notices = [app.notices.as_slice(), app.profile_notices.as_slice()].concat();
        frame::messages(&notices, app.status.as_ref(), ui, max)
    } else {
        app.messages(ui, max)
    };
    let Some(placed) = frame::place(area, Some(body_rows(messages.len()))) else {
        return frame::too_small(f, ui);
    };
    let name = if screen.filter.is_empty() || screen.typing_filter() {
        "config".to_string()
    } else {
        format!("config /{}", screen.filter)
    };
    // What is edited: the profile, or base while one is active.
    let profile = match &screen.profile {
        Some(edit) => Some(format!("profile {}", edit.name)),
        None => app.header_profile().map(|_| profile::BASE.to_string()),
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: &name,
            profile: profile.as_deref(),
            daemon: app.daemon_up(),
            live: None,
        },
    );

    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() as u16 + 1
    };
    let panes = Rect {
        height: body.height.saturating_sub(foot),
        ..body
    };
    let two_panes = area.width >= frame::TWO_PANES;
    let mut detail_rect = panes;
    if two_panes {
        let list = Rect {
            width: LIST_WIDTH.min(panes.width),
            ..panes
        };
        detail_rect = Rect {
            x: panes.x + list.width,
            width: panes.width - list.width,
            ..panes
        };
        sections(screen, f, list, ui);
        let pane = Pane {
            crumb: false,
            help: true,
        };
        draw_rows(
            screen,
            &app.env,
            app.flash.as_deref(),
            f,
            detail_rect,
            ui,
            pane,
        );
    } else if screen.focus == Focus::Sections {
        sections(screen, f, panes, ui);
    } else {
        let pane = Pane {
            crumb: true,
            help: true,
        };
        draw_rows(screen, &app.env, app.flash.as_deref(), f, panes, ui, pane);
    }
    for (i, line) in messages.into_iter().enumerate() {
        let y = panes.bottom() + 1 + i as u16;
        if y < body.bottom() {
            f.render_widget(
                line,
                Rect::new(body.x + 2, y, body.width.saturating_sub(2), 1),
            );
        }
    }

    footer(screen, f, placed.footer, ui);
    draw_open(screen, f, placed.frame, detail_rect, ui);
}

/// The hotkey dialog, and a picker over `detail` (the key picker over the
/// dialog), the screen behind dimmed.
pub fn draw_open(screen: &ConfigScreen, f: &mut Frame, frame: Rect, detail: Rect, ui: &Ui) {
    if let Some(open) = &screen.hotkey {
        open.dialog.draw(f, frame, ui);
    }
    if let Some(picking) = &screen.picker {
        dialog::dim(f.buffer_mut(), frame, ui);
        picking.picker.draw(f, detail, ui);
    }
}

impl ConfigScreen {
    /// The path the current level edits as a whole: the list open, or the
    /// section's entries or map; empty for a section of rows.
    fn current_path(&self) -> &str {
        match (self.view(), self.current().body) {
            (View::List { path, .. }, _) => path,
            (_, Body::Entries(entries)) => entries.path,
            (_, Body::Map(map)) => map.path,
            (_, Body::Rows(_)) => "",
        }
    }

    fn typing_filter(&self) -> bool {
        matches!(
            self.editing,
            Some(Editing {
                target: Target::Filter,
                ..
            })
        )
    }
}

fn sections(screen: &ConfigScreen, f: &mut Frame, area: Rect, ui: &Ui) {
    let visible = screen.visible();
    if visible.is_empty() {
        let text = format!("nothing matches /{}", screen.filter);
        let line = Line::from(Span::styled(
            fit(
                &text,
                usize::from(area.width.saturating_sub(2)),
                ui.glyphs.ellipsis,
            ),
            ui.theme.muted,
        ));
        f.render_widget(
            line,
            Rect::new(area.x + 2, area.y, area.width.saturating_sub(2), 1),
        );
        return;
    }
    let at = visible
        .iter()
        .position(|&s| s == screen.section)
        .unwrap_or(0);
    let offset = scroll(visible.len(), at, usize::from(area.height));
    for (row, &index) in visible.iter().enumerate().skip(offset) {
        let y = area.y + (row - offset) as u16;
        if y >= area.bottom() {
            break;
        }
        let section = &SECTIONS[index];
        let count = match section.body {
            Body::Entries(entries) => Some(screen.form.entries(entries.path).len()),
            Body::Map(map) => Some(screen.map(map.path).len()),
            Body::Rows(_) => None,
        }
        .filter(|n| *n > 0);
        let value = count
            .map(|n| Shown::muted(n.to_string()))
            .unwrap_or_default();
        let picked = index == screen.section;
        let row = Row {
            focused: picked && screen.focus == Focus::Sections,
            selected: picked,
            ..Row::new(section.title, value)
        };
        row.draw(
            Rect::new(area.x, y, area.width, 1),
            f.buffer_mut(),
            ui,
            width(section.title) + 2,
        );
    }
}

/// How [`draw_rows`] heads and ends the rows.
#[derive(Debug, Clone, Copy)]
pub struct Pane {
    /// Where the rows are, over them.
    pub crumb: bool,
    /// The row's help, under them.
    pub help: bool,
}

/// The detail pane: the rows, with the one being typed into, and the help
/// of the row with the cursor. `crumb` heads it with where it is, for the
/// one-pane layout, where the section list is not on screen; a nested level
/// always has one. `flash` is the row that just changed.
pub fn draw_rows(
    screen: &ConfigScreen,
    env: &Env,
    flash: Option<&str>,
    f: &mut Frame,
    area: Rect,
    ui: &Ui,
    pane: Pane,
) {
    let section = screen.current();
    let Pane { crumb, help } = pane;
    let crumb = crumb && screen.pinned.is_none();
    let focused = screen.focus == Focus::Detail;
    let mut top = area.y;
    let crumbs = match screen.view() {
        View::Section { .. } => crumb.then(|| section.title.to_string()),
        View::Entry { index, .. } => Some(format!(
            "{} {} {}",
            section.title,
            ui.glyphs.separator,
            screen.entry_name(*index)
        )),
        View::List { field, .. } => Some(format!(
            "{} {} {}",
            section.title, ui.glyphs.separator, field.label
        )),
    };
    let text_width = usize::from(area.width.saturating_sub(2));
    if let Some(crumbs) = crumbs {
        let line = Line::from(Span::styled(
            fit(&crumbs, text_width, ui.glyphs.ellipsis),
            ui.theme.muted,
        ));
        f.render_widget(
            line,
            Rect::new(area.x + 2, top, area.width.saturating_sub(2), 1),
        );
        top += 2;
    }
    let text_at = |f: &mut Frame, y: u16, lines: Vec<String>, tone: Tone| {
        for (i, text) in lines.into_iter().enumerate() {
            let y = y + i as u16;
            if y < area.bottom() {
                f.render_widget(
                    Paragraph::new(Span::styled(text, tone.style(ui.theme))),
                    Rect::new(area.x + 2, y, area.width.saturating_sub(2), 1),
                );
            }
        }
    };
    if !screen.built() {
        text_at(
            f,
            top,
            wrap(schema::RECORDING_MISSING, text_width),
            Tone::Warn,
        );
        return;
    }

    let items = screen.items();
    let cursor = screen.view().cursor();
    let rows: Vec<(String, Shown, Look)> = items
        .iter()
        .map(|item| describe(screen, item, env, ui))
        .collect();
    // A new item is typed on a row of its own, after the others.
    let new_row = screen.editing.as_ref().and_then(|e| match &e.target {
        Target::Element { index: None, .. } => Some(("", &e.input)),
        Target::NewKey => Some((screen_map_label(section), &e.input)),
        Target::PairValue { key }
            if !items.iter().any(|i| matches!(i, Item::Pair(k) if k == key)) =>
        {
            Some((key.as_str(), &e.input))
        }
        Target::EntryName => Some(("name", &e.input)),
        _ => None,
    });
    // A heading is not lined up with the rows' labels.
    let label_width = rows
        .iter()
        .zip(&items)
        .filter(|(_, item)| !matches!(item, Item::Heading(_)))
        .map(|((label, _, _), _)| width(label))
        .chain(new_row.map(|(label, _)| width(label)))
        .max()
        .map_or(0, |w| if w == 0 { 0 } else { (w + 2).min(MAX_LABEL) });

    let help_text = if focused && help {
        self::help(screen, items.get(cursor))
    } else {
        String::new()
    };
    let mut help_lines = wrap(&help_text, text_width);
    help_lines.truncate(MAX_HELP);
    let total = rows.len() + usize::from(new_row.is_some());
    let room = usize::from(area.bottom().saturating_sub(top));
    let reserved = if help_lines.is_empty() {
        0
    } else {
        help_lines.len() + 1
    };
    let height = total.min(room.saturating_sub(reserved));

    if total == 0 {
        let empty = match (screen.view(), section.body) {
            (View::List { .. }, _) => "empty: a adds an item".to_string(),
            (_, Body::Entries(entries)) => format!("no {}s yet: a adds one", entries.noun),
            (_, Body::Map(map)) => format!("no {}s yet: a adds one", map.value_label),
            _ if !screen.filter.is_empty() => format!("nothing matches /{}", screen.filter),
            _ => String::new(),
        };
        text_at(f, top, vec![empty], Tone::Muted);
        if !help_lines.is_empty() {
            text_at(f, top + 2, help_lines, Tone::Muted);
        }
        return;
    }

    let shown_cursor = if new_row.is_some() { total - 1 } else { cursor };
    let offset = scroll(total, shown_cursor, height);
    let input_on = |item: &Item| -> Option<&Input> {
        let editing = screen.editing.as_ref()?;
        let on = match (&editing.target, item) {
            (Target::Field { path, .. }, Item::Field { path: row, .. }) => path == row,
            (Target::Element { index: Some(i), .. }, Item::Element(row)) => i == row,
            (Target::PairValue { key }, Item::Pair(row)) => key == row,
            _ => false,
        };
        on.then_some(&editing.input)
    };
    let tags: Vec<Option<(String, Tone)>> = items.iter().map(|item| tag(screen, item)).collect();
    for (i, (label, value, look)) in rows.into_iter().enumerate().skip(offset).take(height) {
        let item = &items[i];
        let y = top + (i - offset) as u16;
        if let Item::Heading(title) = item {
            // The title, then a rule to the edge: a group, not a row.
            let title = fit(title, text_width, ui.glyphs.ellipsis);
            let rule = text_width.saturating_sub(width(&title) + 1);
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(title, ui.theme.muted),
                Span::raw(" "),
                Span::styled(ui.glyphs.frame.horizontal_top.repeat(rule), ui.theme.faint),
            ]);
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            continue;
        }
        let row = Row {
            focused: focused && i == cursor && new_row.is_none(),
            choice: look.arrows,
            at_min: look.at_min,
            at_max: look.at_max,
            flash: flash.is_some_and(|key| Some(key) == flash_key(item)),
            input: input_on(item),
            tag: tags[i].as_ref().map(|(text, tone)| (text.as_str(), *tone)),
            ..Row::new(&label, value)
        };
        row.draw(
            Rect::new(area.x, y, area.width, 1),
            f.buffer_mut(),
            ui,
            label_width,
        );
    }
    if let Some((label, input)) = new_row {
        let i = total - 1;
        if i >= offset && i < offset + height {
            let row = Row {
                focused: true,
                input: Some(input),
                ..Row::new(label, Shown::default())
            };
            let y = top + (i - offset) as u16;
            row.draw(
                Rect::new(area.x, y, area.width, 1),
                f.buffer_mut(),
                ui,
                label_width,
            );
        }
    }
    if !help_lines.is_empty() {
        text_at(f, top + height as u16 + 1, help_lines, Tone::Muted);
    }
}

fn screen_map_label(section: &Section) -> &'static str {
    match section.body {
        Body::Map(map) => map.key_label,
        _ => "",
    }
}

/// The footer while a picker, the hotkey dialog or an input has the keys;
/// `false` when none has, and nothing was drawn.
pub fn draw_open_footer(screen: &ConfigScreen, f: &mut Frame, rect: Rect, ui: &Ui) -> bool {
    if screen.mode() == Mode::Browse {
        return false;
    }
    footer(screen, f, rect, ui);
    true
}

fn footer(screen: &ConfigScreen, f: &mut Frame, rect: Rect, ui: &Ui) {
    let hints = |pairs: &[(String, String)]| -> Vec<(String, String)> { pairs.to_vec() };
    let pair = |key: &str, what: &str| (key.to_string(), what.to_string());
    let owned: Vec<(String, String)> = if let Some(picking) = &screen.picker {
        let mut keys = vec![
            // In the order they are dropped from the end when narrow; ?
            // lists them all.
            pair("type", "to filter"),
            pair("enter", "pick"),
            pair("esc", "cancel"),
            pair(
                &format!("{}/{}", ui.glyphs.key_up, ui.glyphs.key_down),
                "move",
            ),
        ];
        if picking.picker.paths {
            keys.push(pair("tab", "complete"));
        }
        keys.push(pair("?", "help"));
        keys
    } else if screen.hotkey.is_some() {
        vec![
            pair("tab", "next"),
            pair("h/l", "change"),
            pair("enter", "key/save"),
            pair("esc", "cancel"),
            pair("t", "type it"),
            pair("x", "unset"),
            pair("?", "help"),
        ]
    } else if let Some(editing) = &screen.editing {
        if matches!(editing.target, Target::Filter) {
            let mut spans = vec![Span::raw("  "), Span::styled("/", ui.theme.accent)];
            spans.extend(editing.input.spans(ui, 24));
            spans.push(Span::styled(
                format!("  enter keep {} esc clear", ui.glyphs.separator),
                ui.theme.muted,
            ));
            f.render_widget(Line::from(spans), rect);
            return;
        }
        let path = matches!(
            &editing.target,
            Target::Field { field, .. } if matches!(field.kind, Kind::Path { .. })
        );
        let mut keys = vec![pair("enter", "keep"), pair("esc", "cancel")];
        if path {
            keys.push(pair("tab", "complete"));
        }
        keys.push(pair("ctrl+u", "clear"));
        keys
    } else {
        match screen.focus {
            Focus::Sections => hints(&[
                pair("j/k", "move"),
                pair("tab", "pane"),
                pair("enter", "open"),
                pair("w", "save"),
                pair("e", "$EDITOR"),
                pair("/", "filter"),
                pair("?", "help"),
            ]),
            Focus::Detail => {
                let items = screen.items();
                let item = items.get(screen.view().cursor());
                let testable =
                    matches!(item, Some(Item::Field { path, .. }) if key_target(path).is_some());
                let mut change = match item {
                    Some(Item::Field { path, .. }) if screen.refusal(path).is_some() => vec![],
                    Some(Item::Field { field, .. }) => verbs(field, ui),
                    Some(Item::Entry(_)) => vec![pair("enter", "open")],
                    _ if screen.refusal(screen.current_path()).is_some() => vec![],
                    _ => vec![pair("enter", "edit")],
                };
                // In profile mode, on a key the profile sets: back to base.
                if let (Some(edit), Some(Item::Field { path, .. })) = (&screen.profile, item) {
                    if edit.sets(path) {
                        change.push(pair("x", "base"));
                    }
                }
                let lists = matches!(screen.view(), View::List { .. })
                    || matches!(
                        (screen.view(), screen.current().body),
                        (View::Section { .. }, Body::Entries(_) | Body::Map(_))
                    );
                let mut keys = vec![pair("j/k", "move")];
                // A list profile mode leaves to config.toml has no a or d.
                let fixed = screen.refusal(screen.current_path()).is_some();
                if lists {
                    if !fixed {
                        keys.push(pair("a", "add"));
                    }
                    keys.extend(change);
                    if !fixed {
                        keys.push(pair("d", "delete"));
                    }
                    keys.push(pair("esc", "back"));
                    keys.extend([pair("w", "save"), pair("?", "help")]);
                } else {
                    keys.push(pair("tab", "pane"));
                    keys.extend(change);
                    keys.extend([pair("w", "save"), pair("e", "$EDITOR"), pair("/", "filter")]);
                    // Last but help: the first to go when narrow.
                    if testable {
                        keys.push(pair("t", "test"));
                    }
                    keys.push(pair("?", "help"));
                }
                keys
            }
        }
    };
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, what)| (key.as_str(), what.as_str()))
        .collect();
    frame::footer(f, rect, ui, &refs);
}

/// What the keys do on a row of `field`: `h/l change · enter pick`,
/// `h/l +/-250 · H/L +/-1000 · enter type`.
pub fn verbs(field: &Field, ui: &Ui) -> Vec<(String, String)> {
    let pm = ui.glyphs.plus_minus();
    let pair = |key: &str, what: String| (key.to_string(), what);
    let number = |n: f64| {
        let text = format!("{n:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    match &field.kind {
        Kind::Bool => vec![pair("space", "toggle".into())],
        Kind::Int(steps) => vec![
            pair("h/l", format!("{pm}{}", steps.step)),
            pair("H/L", format!("{pm}{}", steps.big)),
            pair("enter", "type".into()),
        ],
        Kind::Float {
            step,
            big,
            percent: true,
            ..
        } => vec![
            pair("h/l", format!("{pm}{}%", number(step * 100.0))),
            pair("H/L", format!("{pm}{}%", number(big * 100.0))),
            pair("enter", "type".into()),
        ],
        Kind::Float { step, big, .. } => vec![
            pair("h/l", format!("{pm}{}", number(*step))),
            pair("H/L", format!("{pm}{}", number(*big))),
            pair("enter", "type".into()),
        ],
        kind if picked(kind) => vec![pair("h/l", "change".into()), pair("enter", "pick".into())],
        Kind::Hotkey { .. } => vec![pair("enter", "set".into())],
        Kind::List { .. } => vec![pair("enter", "open".into())],
        _ => vec![pair("enter", "edit".into())],
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::tui::app::{Dialog, Effect};
    use crate::tui::testing::{app, app_on, chord, mockup, paths, press, type_text, Fixture};

    #[cfg(feature = "recording")]
    #[test]
    fn a_recording_language_is_unset_back_to_the_general_one() {
        let mut fixture = app("[general]\nlanguage = \"de\"\n");
        open(&mut fixture, "Recording");
        to_row(&mut fixture, "language");
        assert!(
            crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30))
                .iter()
                .any(|r| r.contains("[general] language (de)"))
        );
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "french");
        press(&mut fixture.app, "\n");
        let language = |f: &Fixture| {
            f.app
                .config
                .form
                .config()
                .recording
                .as_ref()
                .and_then(|r| r.language.clone())
        };
        assert_eq!(language(&fixture).as_deref(), Some("fr"));
        // h steps back to unset, first in the ring.
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Home, KeyModifiers::NONE);
        press(&mut fixture.app, "\n");
        assert_eq!(language(&fixture), None);
        // The retention names its ends.
        to_row(&mut fixture, "keep audio");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "-1");
        press(&mut fixture.app, "\n");
        assert!(
            crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30))
                .iter()
                .any(|r| r.contains("keep forever"))
        );
    }

    /// Open section `title` and put the focus on its rows.
    fn open(fixture: &mut Fixture, title: &str) {
        let index = SECTIONS.iter().position(|s| s.title == title).unwrap();
        if fixture.app.config.focus == Focus::Detail {
            press(&mut fixture.app, "\t");
        }
        press(&mut fixture.app, "g");
        press(&mut fixture.app, &"j".repeat(index));
        press(&mut fixture.app, "\n");
        assert_eq!(fixture.app.config.current().title, title);
    }

    /// Move the cursor to the row labelled `label`.
    fn to_row(fixture: &mut Fixture, label: &str) {
        let screen = &fixture.app.config;
        let at = screen
            .items()
            .iter()
            .position(|item| matches!(item, Item::Field { field, .. } if field.label == label))
            .unwrap_or_else(|| panic!("no row {label}"));
        press(&mut fixture.app, "g");
        press(&mut fixture.app, &"j".repeat(at));
    }

    fn config(fixture: &Fixture) -> &whisrs::Config {
        fixture.app.config.form.config()
    }

    #[test]
    fn a_list_is_edited_one_item_at_a_time() {
        let mut fixture = app("[general]\nvocabulary = [\"whisrs\"]\n");
        open(&mut fixture, "Vocabulary");
        to_row(&mut fixture, "vocabulary");
        press(&mut fixture.app, "\n");
        // Add.
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "NixOS");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).general.vocabulary, ["whisrs", "NixOS"]);
        // Edit: Ctrl+U clears, the new text replaces it.
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "Arch");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).general.vocabulary, ["whisrs", "Arch"]);
        // Esc cancels an edit.
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "zzz");
        press(&mut fixture.app, "\x1b");
        assert_eq!(config(&fixture).general.vocabulary, ["whisrs", "Arch"]);
        // Delete.
        press(&mut fixture.app, "gd");
        assert_eq!(config(&fixture).general.vocabulary, ["Arch"]);
        assert!(fixture.app.config.form.is_dirty());
        // Esc goes back to the rows.
        press(&mut fixture.app, "\x1b");
        assert!(matches!(fixture.app.config.view(), View::Section { .. }));
    }

    #[test]
    fn replacements_are_added_edited_and_deleted() {
        let mut fixture = app("");
        open(&mut fixture, "Replacements");
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "package build");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "PKGBUILD");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture)
                .replacements
                .get("package build")
                .map(String::as_str),
            Some("PKGBUILD")
        );
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "PKGBUILDs");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).replacements["package build"], "PKGBUILDs");
        press(&mut fixture.app, "d");
        assert!(config(&fixture).replacements.is_empty());
        assert!(!fixture.app.config.form.is_dirty());
    }

    #[test]
    fn an_agent_is_added_edited_and_deleted_after_asking() {
        let mut fixture = app("");
        open(&mut fixture, "Agents");
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "jarvis");
        press(&mut fixture.app, "\n");
        // The new entry's form opens.
        assert!(matches!(
            fixture.app.config.view(),
            View::Entry { index: 0, .. }
        ));
        to_row(&mut fixture, "harness");
        press(&mut fixture.app, "l");
        assert_eq!(config(&fixture).agents[0].name, "jarvis");
        assert_eq!(config(&fixture).agents[0].harness, "opencode");
        to_row(&mut fixture, "args");
        press(&mut fixture.app, "\na");
        type_text(&mut fixture.app, "--model");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).agents[0].args, ["--model"]);
        press(&mut fixture.app, "\x1b\x1b");
        // A second name like the first is refused.
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "jarvis");
        press(&mut fixture.app, "\n");
        assert!(fixture.app.config.typing());
        press(&mut fixture.app, "\x1b");
        // Delete asks first; Esc keeps it.
        press(&mut fixture.app, "d");
        assert!(matches!(
            fixture.app.dialog,
            Some(Dialog::DeleteEntry { .. })
        ));
        press(&mut fixture.app, "\x1b");
        assert_eq!(config(&fixture).agents.len(), 1);
        press(&mut fixture.app, "dd");
        assert!(config(&fixture).agents.is_empty());
    }

    #[test]
    fn an_llm_command_gets_the_keys_it_needs() {
        let mut fixture = app("");
        open(&mut fixture, "LLM commands");
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "translate-de");
        press(&mut fixture.app, "\n");
        let commands = &config(&fixture).llm_commands;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "translate-de");
        assert_eq!(commands[0].hotkey, "");
    }

    #[test]
    fn the_filter_keeps_the_matching_rows_and_esc_clears_it() {
        let mut fixture = app("");
        press(&mut fixture.app, "/");
        type_text(&mut fixture.app, "API_KEY");
        let titles: Vec<&str> = fixture
            .app
            .config
            .visible()
            .into_iter()
            .map(|i| SECTIONS[i].title)
            .collect();
        assert_eq!(titles, ["Backend & keys", "Read aloud (TTS)", "LLM"]);
        // Typing names letters, not commands.
        assert!(fixture.app.dialog.is_none());
        press(&mut fixture.app, "\n");
        assert!(!fixture.app.config.typing());
        assert_eq!(fixture.app.config.current().title, "Backend & keys");
        press(&mut fixture.app, "\n");
        let labels: Vec<&str> = fixture
            .app
            .config
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Field { field, .. } => Some(field.label),
                _ => None,
            })
            .collect();
        assert!(labels.iter().all(|l| l.contains("api_key")), "{labels:?}");
        // Esc leaves the rows, then clears the filter, then goes Home.
        press(&mut fixture.app, "\x1b\x1b");
        assert_eq!(fixture.app.config.visible().len(), SECTIONS.len());
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.screen(), crate::tui::app::Screen::Home);
    }

    #[test]
    fn choices_toggles_and_numbers_change_in_place() {
        let mut fixture = app("");
        open(&mut fixture, "General");
        to_row(&mut fixture, "notifications");
        press(&mut fixture.app, " ");
        assert!(!config(&fixture).general.notify);
        assert_eq!(fixture.app.flash.as_deref(), Some("general.notify"));
        to_row(&mut fixture, "silence timeout");
        press(&mut fixture.app, "l");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 2250);
        // x puts it back.
        press(&mut fixture.app, "x");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 2000);
        // A number out of range keeps the input open, with why.
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "-3");
        press(&mut fixture.app, "\n");
        assert!(fixture.app.config.typing());
        assert_eq!(fixture.app.status.as_ref().unwrap().tone, Tone::Error);
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "900");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 900);

        open(&mut fixture, "Overlay");
        to_row(&mut fixture, "theme");
        press(&mut fixture.app, "l");
        assert_eq!(fixture.app.overlay().theme, "ember");
        press(&mut fixture.app, "hh");
        assert_eq!(fixture.app.overlay().theme, "custom");
        to_row(&mut fixture, "agent overlay");
        press(&mut fixture.app, "l");
        assert_eq!(
            config(&fixture).agent_overlay.as_ref().unwrap().enabled,
            Some(true)
        );
        // h on a row that is not a choice goes to the section list.
        to_row(&mut fixture, "recording color");
        press(&mut fixture.app, "h");
        assert_eq!(fixture.app.config.focus, Focus::Sections);
    }

    #[test]
    fn a_secret_is_typed_as_bullets_and_shown_masked() {
        let mut fixture = app("");
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "groq api_key");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "gsk_secret_77ab");
        let screen = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(!screen.iter().any(|r| r.contains("gsk_secret")));
        assert!(screen
            .iter()
            .any(|r| r.contains("\u{2022}\u{2022}\u{2022}")));
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).groq.as_ref().unwrap().api_key,
            "gsk_secret_77ab"
        );
        let screen = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(screen.iter().any(|r| r.contains("set ****77ab")));
        // A blank input changes nothing; x clears the key.
        press(&mut fixture.app, "\n\n");
        assert_eq!(
            config(&fixture).groq.as_ref().unwrap().api_key,
            "gsk_secret_77ab"
        );
        press(&mut fixture.app, "x");
        // The file had no [groq], and it is back at its defaults.
        assert!(config(&fixture).groq.is_none());
        assert!(!fixture.app.config.form.is_dirty());
    }

    /// The env var wins, as in the daemon, and says "this shell" (#142).
    #[test]
    fn a_key_from_the_environment_says_so() {
        let fixture = mockup();
        let screen = &fixture.app.config;
        let ui_theme = crate::tui::testing::theme();
        let glyphs = crate::tui::glyphs::Glyphs::UNICODE;
        let ui = Ui {
            theme: &ui_theme,
            glyphs: &glyphs,
        };
        let field = |label: &str| {
            schema::SECTIONS[1..2]
                .iter()
                .flat_map(|s| match s.body {
                    Body::Rows(fields) => fields.iter(),
                    _ => [].iter(),
                })
                .find(|f| f.label == label)
                .unwrap()
        };
        let text = |label: &str| {
            let field = field(label);
            shown(screen, field, field.path, &fixture.app.env, &ui)
                .parts
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<String>()
        };
        assert_eq!(text("groq api_key"), "WHISRS_GROQ_API_KEY (this shell)");
        assert_eq!(text("deepgram api_key"), "set ****9f2c");
        assert_eq!(text("openai api_key"), "not set");
        assert_eq!(text("sidecar api_key"), "not set, optional");
    }

    // Saving.

    /// Acceptance criterion 2: a save keeps comments, key order and unknown
    /// keys, and writes 0600.
    #[test]
    fn a_save_keeps_the_file_and_writes_it_private() {
        use std::os::unix::fs::PermissionsExt;

        let toml = "# my config\n[groq]\napi_key = \"gsk_1\"\n# the model\nmodel = \"m\"\n\n\
                    [general]\nbackend = \"groq\"\nsilense_timeout_ms = 1\n";
        let mut fixture = app(toml);
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "de");
        press(&mut fixture.app, "\n");
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);

        let paths = paths(fixture.dir.path());
        let saved = save(&fixture.app.config, &paths, false).expect("saves");
        let out = std::fs::read_to_string(&paths.config).unwrap();
        assert!(out.contains("# my config\n[groq]\n"), "{out}");
        assert!(out.contains("# the model\nmodel = \"m\""), "{out}");
        assert!(out.contains("silense_timeout_ms = 1"), "{out}");
        assert!(out.contains("language = \"de\""), "{out}");
        assert!(out.find("[groq]") < out.find("[general]"), "{out}");
        let mode = std::fs::metadata(&paths.config)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // What is now on disk is the new baseline.
        assert_eq!(saved.loaded.form.config().general.language, "de");
        fixture.app.update(crate::tui::app::Msg::Saved(Ok(saved)));
        assert!(!fixture.app.config.form.is_dirty());
    }

    /// An invalid config is refused, nothing is written, and every edit
    /// stays.
    #[test]
    fn an_invalid_config_is_refused_with_its_edits_kept() {
        let toml = "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"http://127.0.0.1:8765/transcribe\"\n";
        let mut fixture = app(toml);
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "sidecar url");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut fixture.app, "\n");
        open(&mut fixture, "General");
        to_row(&mut fixture, "notifications");
        press(&mut fixture.app, " ");

        let paths = paths(fixture.dir.path());
        let why = match save(&fixture.app.config, &paths, false) {
            Err(SaveError::Failed(why)) => why,
            Err(other) => panic!("refused for another reason: {other:?}"),
            Ok(_) => panic!("saved an invalid config"),
        };
        assert!(why.contains("no sidecar URL configured"), "{why}");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), toml);
        fixture
            .app
            .update(crate::tui::app::Msg::Saved(Err(SaveError::Failed(why))));
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert!(status.text.starts_with("not saved: "), "{}", status.text);
        assert!(fixture.app.config.form.is_dirty());
        assert!(!config(&fixture).general.notify);
        assert_eq!(config(&fixture).asr_sidecar.as_ref().unwrap().url, "");
    }

    /// A sparse config, hand-commented: what the save tests below edit.
    const SPARSE: &str = "# my whisrs\n[general]\nbackend = \"groq\"\n# my language\nlanguage = \"de\"\nsilence_timeout_ms = 4500\n\n[groq]\napi_key = \"gsk_1\"\n\n[[agents]]\nname = \"jarvis\"\ndir = \"/tmp\"\n";

    /// The inode config.toml is at: a write replaces the file, a no-op does
    /// not.
    fn inode(path: &std::path::Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    /// One key changed, one line changed: nothing else of the file moves,
    /// and no default of anything else is written.
    #[test]
    fn a_save_writes_only_the_changed_line() {
        let mut fixture = app(SPARSE);
        open(&mut fixture, "General");
        to_row(&mut fixture, "silence timeout");
        press(&mut fixture.app, "l");
        let paths = paths(fixture.dir.path());
        save(&fixture.app.config, &paths, false).expect("saved");
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            SPARSE.replace("silence_timeout_ms = 4500", "silence_timeout_ms = 4750"),
            "the file differs by that one line"
        );
    }

    /// `x` on a set key removes it from the file, so its default applies
    /// again. The comment above it moves onto the key after it.
    #[test]
    fn a_reset_key_is_removed_and_its_comment_kept() {
        let mut fixture = app(SPARSE);
        open(&mut fixture, "General");
        to_row(&mut fixture, "silence timeout");
        press(&mut fixture.app, "x");
        let paths = paths(fixture.dir.path());
        save(&fixture.app.config, &paths, false).expect("saved");
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            SPARSE.replace("silence_timeout_ms = 4500\n", ""),
        );

        let mut fixture = app(SPARSE);
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        press(&mut fixture.app, "x");
        let paths = crate::tui::testing::paths(fixture.dir.path());
        save(&fixture.app.config, &paths, false).expect("saved");
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            SPARSE.replace("language = \"de\"\n", ""),
            "the key goes; its comment stays, above the key after it"
        );
    }

    /// A value toggled and toggled back is no change: nothing is written.
    /// A save with no edits at all writes nothing either.
    #[test]
    fn a_change_and_back_writes_nothing() {
        let mut fixture = app(SPARSE);
        let paths = paths(fixture.dir.path());
        let before = inode(&paths.config);
        open(&mut fixture, "General");
        to_row(&mut fixture, "notifications");
        press(&mut fixture.app, "  ");
        assert!(!fixture.app.config.form.is_dirty());
        save(&fixture.app.config, &paths, false).expect("a no-op");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), SPARSE);
        assert_eq!(inode(&paths.config), before, "the file was not rewritten");
        // The app does not even ask the writer.
        assert_eq!(press(&mut fixture.app, "w"), Vec::<Effect>::new());
        assert_eq!(
            fixture.app.status.as_ref().unwrap().text,
            "no changes to save"
        );
    }

    /// An agent added writes that entry, with what it set and nothing more;
    /// the entry already there keeps its bytes.
    #[test]
    fn an_added_agent_writes_only_its_entry() {
        let mut fixture = app(SPARSE);
        open(&mut fixture, "Agents");
        press(&mut fixture.app, "a");
        type_text(&mut fixture.app, "friday");
        press(&mut fixture.app, "\n\x1b");
        let paths = paths(fixture.dir.path());
        save(&fixture.app.config, &paths, false).expect("saved");
        let written = std::fs::read_to_string(&paths.config).unwrap();
        assert!(written.starts_with(SPARSE), "{written}");
        assert_eq!(
            written[SPARSE.len()..].trim(),
            "[[agents]]\nname = \"friday\""
        );
    }

    /// A cloud backend with no key in the file is written, with a warning
    /// saying where the daemon must find it instead.
    #[test]
    fn a_missing_key_is_written_with_a_warning() {
        // Validation reads this process's env.
        if std::env::var_os("WHISRS_DEEPGRAM_API_KEY").is_some() {
            return;
        }
        let toml = "[general]\nbackend = \"deepgram\"\n[deepgram]\napi_key = \"dg_1\"\n";
        let mut fixture = app(toml);
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "deepgram api_key");
        press(&mut fixture.app, "x");
        let paths = paths(fixture.dir.path());
        let saved = save(&fixture.app.config, &paths, false).expect("written");
        assert!(saved.warnings.iter().any(|w| w.contains(
            "the daemon needs WHISRS_DEEPGRAM_API_KEY in its own environment (systemd \
             Environment=, environment.d, or the OpenRC conf.d file)"
        )));
        let written: whisrs::Config =
            toml::from_str(&std::fs::read_to_string(&paths.config).unwrap()).unwrap();
        assert_eq!(written.deepgram.unwrap().api_key, "");
        fixture.app.update(crate::tui::app::Msg::Saved(Ok(saved)));
        assert!(fixture
            .app
            .notices
            .iter()
            .any(|n| n.tone == Tone::Warn && n.text.contains("WHISRS_DEEPGRAM_API_KEY")));
    }

    /// A changed vocabulary goes whole to `vocabulary.txt` when the file is
    /// in play, and an unrelated save leaves the file alone.
    #[test]
    fn a_save_follows_the_vocabulary_rules() {
        let toml = "[general]\nbackend = \"groq\"\nvocabulary = [\"a\"]\n[groq]\napi_key = \"k\"\n";
        let mut fixture = app(toml);
        let paths = paths(fixture.dir.path());
        std::fs::write(&paths.vocabulary, "# mine\nb\n").unwrap();
        // Load again, now with the file.
        fixture.app = crate::tui::app::App::prepare(
            crate::tui::Entry::Config,
            paths.clone(),
            Env::default(),
            whisrs::service::ServiceManager::Systemd,
        )
        .unwrap();
        assert_eq!(config(&fixture).general.vocabulary, ["a", "b"]);

        open(&mut fixture, "General");
        to_row(&mut fixture, "history");
        press(&mut fixture.app, " ");
        let saved = save(&fixture.app.config, &paths, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.vocabulary).unwrap(),
            "# mine\nb\n"
        );
        fixture.app.update(crate::tui::app::Msg::Saved(Ok(saved)));

        open(&mut fixture, "Vocabulary");
        to_row(&mut fixture, "vocabulary");
        press(&mut fixture.app, "\na");
        type_text(&mut fixture.app, "c");
        press(&mut fixture.app, "\n");
        save(&fixture.app.config, &paths, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.vocabulary).unwrap(),
            "a\nb\nc\n"
        );
        // The terms moved to vocabulary.txt: config.toml's list is empty,
        // its default, so the key leaves the file.
        let out = std::fs::read_to_string(&paths.config).unwrap();
        assert!(!out.contains("vocabulary"), "{out}");
        let on_disk: whisrs::Config = toml::from_str(&out).unwrap();
        assert!(on_disk.general.vocabulary.is_empty());
    }

    /// A save finds config.toml changed by something else (the daemon's
    /// `llm-command-set`) and asks: overwrite, load it, or cancel.
    #[test]
    fn a_change_on_disk_is_not_overwritten_unasked() {
        let toml = "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n";
        let mut fixture = app(toml);
        let paths = paths(fixture.dir.path());
        open(&mut fixture, "General");
        to_row(&mut fixture, "history");
        press(&mut fixture.app, " ");
        let changed = format!(
            "{toml}[[llm_commands]]\nname = \"de\"\nhotkey = \"F9\"\ninstruction = \"x\"\n"
        );
        std::fs::write(&paths.config, &changed).unwrap();

        let refused = save(&fixture.app.config, &paths, false);
        assert!(matches!(refused, Err(SaveError::Changed("config.toml"))));
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), changed);
        fixture.app.update(crate::tui::app::Msg::Saved(refused));
        assert_eq!(fixture.app.dialog, Some(Dialog::Changed("config.toml")));
        let drawn = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(drawn.iter().any(|r| r.contains("changed on disk")));

        // Esc cancels; `w` asks again; `l` loads it, edits dropped.
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.dialog, None);
        assert!(fixture.app.config.form.is_dirty());
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        fixture.app.update(crate::tui::app::Msg::Saved(save(
            &fixture.app.config,
            &paths,
            false,
        )));
        assert_eq!(press(&mut fixture.app, "l"), [Effect::Reload]);
        fixture
            .app
            .update(crate::tui::app::Msg::Reloaded(reload(&paths)));
        assert!(!fixture.app.config.form.is_dirty());
        assert_eq!(config(&fixture).llm_commands.len(), 1);

        // What was loaded is the new baseline: a save goes through.
        press(&mut fixture.app, "\t");
        open(&mut fixture, "General");
        to_row(&mut fixture, "history");
        press(&mut fixture.app, " ");
        let saved = save(&fixture.app.config, &paths, false).expect("saves");
        assert!(saved.loaded.form.config().llm_commands.len() == 1);

        // o overwrites.
        std::fs::write(&paths.config, toml).unwrap();
        fixture.app.update(crate::tui::app::Msg::Saved(Ok(saved)));
        open(&mut fixture, "General");
        to_row(&mut fixture, "notifications");
        press(&mut fixture.app, " ");
        fixture.app.update(crate::tui::app::Msg::Saved(save(
            &fixture.app.config,
            &paths,
            false,
        )));
        assert_eq!(press(&mut fixture.app, "o"), [Effect::SaveOver]);
        save(&fixture.app.config, &paths, true).expect("overwrites");
        let out = std::fs::read_to_string(&paths.config).unwrap();
        assert!(out.contains("notify = false"), "{out}");
    }

    /// vocabulary.txt is in the snapshot too, read through a symlink, and a
    /// save writes through the link and keeps it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_vocabulary_is_kept_and_watched() {
        let toml = "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n";
        let mut fixture = app(toml);
        let paths = paths(fixture.dir.path());
        std::fs::create_dir(fixture.dir.path().join("dotfiles")).unwrap();
        let target = fixture.dir.path().join("dotfiles/vocabulary.txt");
        std::fs::write(&target, "NixOS\n").unwrap();
        std::os::unix::fs::symlink(&target, &paths.vocabulary).unwrap();
        fixture
            .app
            .update(crate::tui::app::Msg::Reloaded(reload(&paths)));
        assert_eq!(config(&fixture).general.vocabulary, ["NixOS"]);

        open(&mut fixture, "Vocabulary");
        to_row(&mut fixture, "vocabulary");
        press(&mut fixture.app, "\na");
        type_text(&mut fixture.app, "Arch");
        press(&mut fixture.app, "\n");
        let saved = save(&fixture.app.config, &paths, false).expect("saves");
        let link = std::fs::symlink_metadata(&paths.vocabulary).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "NixOS\nArch\n");
        fixture.app.update(crate::tui::app::Msg::Saved(Ok(saved)));

        // Edited outside (its own target): the next save asks first.
        std::fs::write(&target, "NixOS\nArch\nGentoo\n").unwrap();
        open(&mut fixture, "General");
        to_row(&mut fixture, "history");
        press(&mut fixture.app, " ");
        assert!(matches!(
            save(&fixture.app.config, &paths, false),
            Err(SaveError::Changed("vocabulary.txt"))
        ));
    }

    /// While config.toml does not load, `r` is refused too: the daemon
    /// would start on defaults.
    #[test]
    fn r_is_refused_while_the_file_does_not_load() {
        let mut fixture = app("");
        fixture.app.config.broken = Some("TOML parse error".into());
        assert!(press(&mut fixture.app, "r").is_empty());
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert!(status.text.starts_with("not restarted"), "{}", status.text);
        fixture.app.config.broken = None;
        assert_eq!(press(&mut fixture.app, "r"), [Effect::Restart]);
    }

    /// Enter on a number fills the input with the exact value, so a commit
    /// without a change changes nothing.
    #[test]
    fn a_float_is_edited_from_its_exact_value() {
        let mut fixture = app("[general]\naudio_feedback_volume = 0.333\n");
        open(&mut fixture, "General");
        to_row(&mut fixture, "feedback volume");
        press(&mut fixture.app, "\n");
        assert_eq!(
            fixture.app.config.editing.as_ref().unwrap().input.text(),
            "0.333"
        );
        press(&mut fixture.app, "\n");
        assert!(!fixture.app.config.form.is_dirty());
    }

    // Opening.

    /// #134: a file that cannot be read is refused before the TUI opens,
    /// with the hand fix, and nothing is written.
    #[test]
    fn an_unreadable_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        // A directory where the file goes cannot be read as one.
        std::fs::create_dir(&paths.config).unwrap();
        let refusal = load(&paths).err().expect("refused");
        assert!(refusal.starts_with("cannot read config at "), "{refusal}");
        assert!(refusal.contains("Fix it by hand"), "{refusal}");
        assert!(paths.config.is_dir());
    }

    /// #134: a file that does not deserialize is refused by `config` (only
    /// `setup` starts fresh), and left as it is.
    #[test]
    fn an_undeserializable_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let broken = "[general]\nsilence_timeout_ms = \"soon\"\n";
        std::fs::write(&paths.config, broken).unwrap();
        let refusal = load(&paths).err().expect("refused");
        assert!(refusal.starts_with("cannot parse config at "), "{refusal}");
        assert!(refusal.contains("whisrs setup"), "{refusal}");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), broken);
    }

    /// #116: unknown keys are named at load, on Home and Config.
    #[test]
    fn unknown_keys_are_named_at_load() {
        let fixture = app("[general]\nsilense_timeout_ms = 1\n[tts]\nvoise = \"x\"\n");
        let texts: Vec<&str> = fixture
            .app
            .notices
            .iter()
            .map(|n| n.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "unknown key [general] silense_timeout_ms is ignored",
                "unknown key [tts] voise is ignored"
            ]
        );
    }

    #[test]
    fn no_file_opens_on_the_defaults_and_says_so() {
        let mut fixture = app_on(None, &[]);
        assert!(fixture.app.config.fresh);
        assert!(fixture.app.notices[0]
            .text
            .starts_with("No config.toml yet"));
        // Nothing changed, and still `w` writes it.
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
    }

    #[test]
    fn an_unreadable_vocabulary_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(&paths.config, "").unwrap();
        std::fs::create_dir(&paths.vocabulary).unwrap();
        let loaded = load(&paths).unwrap();
        assert!(loaded.notices[0].text.starts_with("cannot read "));
        assert!(!loaded.vocabulary.uses_file());
    }

    /// After `$EDITOR`, the file is read again and its unknown keys named;
    /// one that no longer loads turns `w` off.
    #[test]
    fn a_reload_names_unknown_keys_and_a_broken_file_stops_saves() {
        let mut fixture = app("");
        let paths = paths(fixture.dir.path());
        std::fs::write(&paths.config, "[general]\nsilense_timeout_ms = 1\n").unwrap();
        let reread = reload(&paths);
        fixture.app.update(crate::tui::app::Msg::Reloaded(reread));
        assert!(fixture
            .app
            .notices
            .iter()
            .any(|n| n.text == "unknown key [general] silense_timeout_ms is ignored"));

        std::fs::write(&paths.config, "[general\n").unwrap();
        let reread = reload(&paths);
        assert!(reread.is_err());
        fixture.app.update(crate::tui::app::Msg::Reloaded(reread));
        assert!(fixture.app.config.broken.is_some());
        open(&mut fixture, "General");
        to_row(&mut fixture, "history");
        press(&mut fixture.app, " ");
        assert!(press(&mut fixture.app, "w").is_empty());
        assert_eq!(fixture.app.status.as_ref().unwrap().tone, Tone::Error);
    }

    // Picking, stepping and the hotkey dialog.

    fn drawn(fixture: &Fixture) -> Vec<String> {
        crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30))
    }

    fn mode(fixture: &Fixture) -> Mode {
        fixture.app.config.mode()
    }

    fn status(fixture: &Fixture) -> Notice {
        fixture.app.status.clone().expect("a status")
    }

    /// A language is picked by its name, stepped in place, or typed as a
    /// code nobody lists, through the check.
    #[test]
    fn a_language_is_picked_by_name() {
        let mut fixture = app("");
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        // Letters filter; q is not quit here.
        type_text(&mut fixture.app, "ger");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("de") && r.contains("German")));
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Browse);
        assert_eq!(config(&fixture).general.language, "de");
        // h/l step through the list in place: auto, en, zh, de, es.
        press(&mut fixture.app, "l");
        assert_eq!(config(&fixture).general.language, "es");
        press(&mut fixture.app, "hhh");
        assert_eq!(config(&fixture).general.language, "en");
        // A code no list has is offered as itself, normalized by the check.
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "pt_br");
        assert!(drawn(&fixture).iter().any(|r| r.contains("use \"pt_br\"")));
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).general.language, "pt-BR");
        // One the check refuses keeps the picker open, with why.
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "xyz1");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        assert_eq!(status(&fixture).tone, Tone::Error);
        // Esc leaves with nothing changed.
        press(&mut fixture.app, "\x1b");
        assert_eq!(mode(&fixture), Mode::Browse);
        assert_eq!(config(&fixture).general.language, "pt-BR");
    }

    /// An unset-able row has unset first, saying what then applies.
    #[test]
    fn unset_is_a_row_of_its_own() {
        let mut fixture = app("");
        open(&mut fixture, "Read aloud (TTS)");
        to_row(&mut fixture, "voice");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("backend default (autumn)")));
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "troy");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).tts.as_ref().unwrap().voice.as_deref(),
            Some("troy")
        );
        // The cursor opens on the value set now; Home goes to unset.
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Home, KeyModifiers::NONE);
        press(&mut fixture.app, "\n");
        assert!(config(&fixture).tts.is_none());
        // The voices follow the backend.
        to_row(&mut fixture, "backend");
        press(&mut fixture.app, "l");
        assert_eq!(config(&fixture).tts.as_ref().unwrap().backend, "openai");
        to_row(&mut fixture, "voice");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "marin");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).tts.as_ref().unwrap().voice.as_deref(),
            Some("marin")
        );
    }

    /// The backends not built or not implemented are skipped by h/l and
    /// refused by the picker with why.
    #[test]
    fn a_backend_that_cannot_run_is_refused() {
        let mut fixture = app("");
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "backend");
        press(&mut fixture.app, "h");
        assert_eq!(config(&fixture).general.backend, "asr-sidecar");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::End, KeyModifiers::NONE);
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Warn);
        assert!(said.text.contains("not implemented yet"), "{}", said.text);
        assert!(drawn(&fixture).iter().any(|r| r.contains("coming soon")));
        press(&mut fixture.app, "\x1b");
        assert_eq!(config(&fixture).general.backend, "asr-sidecar");
    }

    /// Numbers step by their step, H/L by the big one, stop at a limit and
    /// show their unit or name.
    #[test]
    fn numbers_step_and_show_their_unit() {
        let mut fixture = app("");
        open(&mut fixture, "General");
        to_row(&mut fixture, "silence timeout");
        assert!(drawn(&fixture).iter().any(|r| r.contains("2000 ms")));
        press(&mut fixture.app, "L");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 3000);
        chord(&mut fixture.app, KeyCode::Left, KeyModifiers::SHIFT);
        press(&mut fixture.app, "HH");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 0);
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("silence timeout") && r.contains("off")));
        // At the limit it stops; it never wraps.
        press(&mut fixture.app, "h");
        assert_eq!(config(&fixture).general.silence_timeout_ms, 0);
        let footer = drawn(&fixture).join("\n");
        assert!(footer.contains("h/l \u{b1}250"), "{footer}");
        assert!(footer.contains("H/L \u{b1}1000"), "{footer}");

        to_row(&mut fixture, "feedback volume");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "0.4");
        press(&mut fixture.app, "\n");
        let bar = "\u{25B0}".repeat(4) + &"\u{25B1}".repeat(6) + " 40%";
        assert!(drawn(&fixture).iter().any(|r| r.contains(&bar)), "{bar}");
        press(&mut fixture.app, "L");
        assert!((config(&fixture).general.audio_feedback_volume - 0.65).abs() < 1e-9);
    }

    #[test]
    fn named_values_and_units_read_as_words() {
        let steps = |unit, named| Steps {
            min: -1,
            max: 3000,
            step: 1,
            big: 10,
            unit,
            named,
        };
        let days = steps(
            "days",
            &[(-1, "keep forever"), (0, "delete once transcribed")],
        );
        assert_eq!(show_int(-1, &days), "keep forever");
        assert_eq!(show_int(1, &days), "1 day");
        assert_eq!(show_int(30, &days), "30 days");
        let min = steps("min", &[]);
        assert_eq!(show_int(45, &min), "45 min");
        assert_eq!(show_int(90, &min), "90 min (1 h 30)");
        assert_eq!(show_int(180, &min), "180 min (3 h)");
        assert_eq!(show_int(3, &steps("", &[(0, "auto")])), "3");
    }

    /// The hotkey dialog builds a hotkey from toggles and a key, checks it
    /// with the parser, and names a clash without refusing it.
    #[test]
    fn a_hotkey_is_built_in_its_dialog() {
        let mut fixture = app("[hotkeys]\ntoggle = \"Alt+D\"\nrecording = \"Alt+Shift+M\"\n");
        open(&mut fixture, "Hotkeys");
        to_row(&mut fixture, "toggle");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Hotkey);
        // q and w are not quit and save while it is up.
        assert!(press(&mut fixture.app, "qw").is_empty());
        press(&mut fixture.app, " ");
        assert!(drawn(&fixture).iter().any(|r| r.contains("Super+Alt+D")));
        press(&mut fixture.app, "\t\t\t\t\t\n");
        assert_eq!(mode(&fixture), Mode::Browse);
        let hotkeys = |f: &Fixture| config(f).hotkeys.clone().unwrap();
        assert_eq!(hotkeys(&fixture).toggle.as_deref(), Some("Super+Alt+D"));

        // The key from its picker; a clash is a warning.
        press(&mut fixture.app, "\n");
        press(&mut fixture.app, " \t\t\t \t\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        type_text(&mut fixture.app, "m");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Hotkey);
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("Alt+Shift+M is already recording")));
        press(&mut fixture.app, "\n");
        assert_eq!(hotkeys(&fixture).toggle.as_deref(), Some("Alt+Shift+M"));
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Warn);
        assert_eq!(said.text, "Alt+Shift+M is also recording");

        // A bare key is refused, by the parser's own rule.
        to_row(&mut fixture, "cancel");
        press(&mut fixture.app, "\n\t\t\t\t\n");
        type_text(&mut fixture.app, "d");
        press(&mut fixture.app, "\n\n");
        assert_eq!(mode(&fixture), Mode::Hotkey);
        assert_eq!(status(&fixture).tone, Tone::Error);
        assert!(status(&fixture).text.contains("ScrollLock"));
        press(&mut fixture.app, "\x1b");
        assert_eq!(hotkeys(&fixture).cancel, None);

        // t types it instead; x unsets it.
        to_row(&mut fixture, "toggle");
        press(&mut fixture.app, "\nt");
        assert_eq!(mode(&fixture), Mode::Text);
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "Super+F9");
        press(&mut fixture.app, "\n");
        assert_eq!(hotkeys(&fixture).toggle.as_deref(), Some("Super+F9"));
        press(&mut fixture.app, "\nx");
        assert_eq!(hotkeys(&fixture).toggle, None);
    }

    /// A hotkey that does not parse opens as text, saying so; a typed one
    /// is checked by the parser too.
    #[test]
    fn a_hotkey_that_does_not_parse_is_typed() {
        let mut fixture = app("[hotkeys]\nspeak = \"Super+Nope\"\n");
        open(&mut fixture, "Hotkeys");
        to_row(&mut fixture, "speak");
        assert!(drawn(&fixture).iter().any(|r| r.contains("does not parse")));
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Text);
        assert_eq!(status(&fixture).tone, Tone::Warn);
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Text, "still refused");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "Ctrl+Alt+S");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).hotkeys.as_ref().unwrap().speak.as_deref(),
            Some("Ctrl+Alt+S")
        );
    }

    /// Every hotkey in the form is a candidate for a clash, entries too.
    #[test]
    fn a_clash_is_found_in_any_section() {
        let mut fixture = app(
            "[hotkeys]\ntoggle = \"Alt+D\"\n[[agents]]\nname = \"jarvis\"\nhotkey = \"Super+J\"\n",
        );
        let others = fixture.app.config.hotkeys_but("hotkeys.toggle");
        assert!(others.contains(&("agent jarvis hotkey".to_string(), "Super+J".to_string())));
        open(&mut fixture, "Hotkeys");
        to_row(&mut fixture, "toggle");
        press(&mut fixture.app, "\nt");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut fixture.app, "super+j");
        press(&mut fixture.app, "\n");
        assert_eq!(status(&fixture).text, "super+j is also agent jarvis hotkey");
    }

    /// A worker's list: asked for when the picker opens, "looking..." until
    /// it comes, then picked from; a failure says why and typing still
    /// works.
    #[test]
    fn devices_load_on_a_worker() {
        use crate::tui::app::Msg;
        use crate::tui::options::Device;

        let mut fixture = app("");
        // Asked for as soon as the cursor lands on the row.
        let index = SECTIONS.iter().position(|s| s.title == "Audio").unwrap();
        press(&mut fixture.app, &"j".repeat(index));
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::List(ListKind::Devices)]
        );
        // On its way already: the picker does not ask twice.
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert!(drawn(&fixture).iter().any(|r| r.contains("looking...")));
        fixture
            .app
            .update(Msg::Listed(Listing::Devices(Ok(vec![Device {
                id: "alsa_input.usb-yeti".into(),
                description: "Blue Yeti".into(),
                monitor: false,
            }]))));
        assert!(drawn(&fixture).iter().any(|r| r.contains("Blue Yeti")));
        type_text(&mut fixture.app, "yeti");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).audio.device, "alsa_input.usb-yeti");

        // Asked again on the next open; this time it fails.
        assert_eq!(
            press(&mut fixture.app, "\n"),
            [Effect::List(ListKind::Devices)]
        );
        fixture.app.update(Msg::Listed(Listing::Devices(Err(
            "no input devices found".into()
        ))));
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("no input devices found")));
        type_text(&mut fixture.app, "hw:1");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).audio.device, "hw:1");
    }

    /// The value set on a device row stays while the list is not here:
    /// `h`/`l` only ask for it. Once it is, a value it lacks steps to its
    /// first (or last) item.
    #[test]
    fn a_device_is_kept_until_its_list_is_here() {
        use crate::tui::app::Msg;
        use crate::tui::options::Device;

        let mut fixture = app("[audio]\ndevice = \"alsa_input.usb-yeti\"\n");
        open(&mut fixture, "Audio");
        for keys in ["l", "h"] {
            press(&mut fixture.app, keys);
            assert_eq!(
                config(&fixture).audio.device,
                "alsa_input.usb-yeti",
                "{keys}"
            );
        }
        assert_eq!(status(&fixture).text, "looking...");
        // A failed list is asked for again by the next press.
        fixture.app.update(Msg::Listed(Listing::Devices(Err(
            "no input devices found".into()
        ))));
        assert_eq!(
            press(&mut fixture.app, "l"),
            [Effect::List(ListKind::Devices)]
        );
        assert_eq!(config(&fixture).audio.device, "alsa_input.usb-yeti");
        fixture
            .app
            .update(Msg::Listed(Listing::Devices(Ok(vec![Device {
                id: "alsa_input.pci-builtin".into(),
                description: "Built-in".into(),
                monitor: false,
            }]))));
        assert!(fixture.app.status.is_none(), "looking... is gone");
        press(&mut fixture.app, "h");
        assert_eq!(config(&fixture).audio.device, "alsa_input.pci-builtin");
        press(&mut fixture.app, "l");
        assert_eq!(config(&fixture).audio.device, "default");
    }

    /// The recording devices too: an unset-able one is not unset, a
    /// monitor not replaced by none, while their list is on its way.
    #[cfg(feature = "recording")]
    #[test]
    fn recording_devices_are_kept_until_their_list_is_here() {
        let mut fixture = app("[recording]\nmic_device = \"x\"\nsystem_device = \"y.monitor\"\n");
        open(&mut fixture, "Recording");
        let recording = |f: &Fixture| f.app.config.form.config().recording.clone().unwrap();
        to_row(&mut fixture, "mic device");
        press(&mut fixture.app, "l");
        assert_eq!(recording(&fixture).mic_device.as_deref(), Some("x"));
        to_row(&mut fixture, "system device");
        press(&mut fixture.app, "h");
        assert_eq!(
            recording(&fixture).system_device.as_deref(),
            Some("y.monitor")
        );
        assert_eq!(status(&fixture).text, "looking...");
    }

    /// `system_device = "default"` is the default monitor, as unset is.
    #[cfg(feature = "recording")]
    #[test]
    fn a_system_device_of_default_reads_as_unset() {
        let mut fixture = app("[recording]\nsystem_device = \"default\"\n");
        open(&mut fixture, "Recording");
        to_row(&mut fixture, "system device");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("system device") && r.contains("default monitor")));
        let field = SECTIONS
            .iter()
            .find_map(|s| match s.body {
                Body::Rows(fields) => fields.iter().find(|f| f.path == "recording.system_device"),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            fixture
                .app
                .config
                .current_text(field, "recording.system_device"),
            None
        );
    }

    /// A list that comes while another row's picker is open leaves that
    /// picker as it is.
    #[test]
    fn a_list_refreshes_only_its_own_picker() {
        use crate::tui::app::Msg;

        let mut fixture = app("");
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        // auto, en (the cursor), zh, de.
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Down, KeyModifiers::NONE);
        chord(&mut fixture.app, KeyCode::Down, KeyModifiers::NONE);
        fixture.app.update(Msg::Listed(Listing::Devices(Err(
            "no input devices found".into()
        ))));
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).general.language, "de");
    }

    /// Space is the next value of a closed set.
    #[test]
    fn space_steps_a_closed_set() {
        let mut fixture = app("");
        open(&mut fixture, "Overlay");
        to_row(&mut fixture, "theme");
        press(&mut fixture.app, " ");
        assert_eq!(fixture.app.overlay().theme, "ember");
        to_row(&mut fixture, "agent overlay");
        press(&mut fixture.app, " ");
        assert_eq!(
            config(&fixture).agent_overlay.as_ref().unwrap().enabled,
            Some(true)
        );
    }

    /// The model path's key is not `~`-expanded by the daemon, so a typed
    /// `~` is written out.
    #[test]
    fn a_tilde_in_the_model_path_is_written_out() {
        let mut fixture = app_on(Some(""), &[("HOME", "/home/ada")]);
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "whisper model");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "~/models/ggml-x.bin");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).local_whisper.as_ref().unwrap().model_path,
            "/home/ada/models/ggml-x.bin"
        );
    }

    /// An expected language is a language: `auto` is refused, and not
    /// offered.
    #[test]
    fn expected_languages_refuse_auto() {
        let mut fixture = app("");
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "openai languages");
        press(&mut fixture.app, "\na");
        type_text(&mut fixture.app, "auto");
        assert!(!drawn(&fixture)
            .iter()
            .any(|r| r.contains("detect the language")));
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        assert!(status(&fixture).text.contains("auto is no language"));
        assert!(config(&fixture).openai.is_none());
    }

    /// `?` in a picker or the hotkey dialog lists its keys, and closes
    /// back onto it; the lines wrap at 60 columns rather than being cut.
    #[test]
    fn help_inside_a_picker_and_the_hotkey_dialog() {
        use crate::tui::app::HelpFor;

        let mut fixture = app("[hotkeys]\ntoggle = \"Alt+D\"\n");
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        press(&mut fixture.app, "\n?");
        assert_eq!(fixture.app.dialog, Some(Dialog::Help(HelpFor::Picker)));
        let small = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 60, 18));
        assert!(small.iter().any(|r| r.contains("picker keys")));
        assert!(!small.iter().any(|r| r.contains('\u{2026}')), "{small:#?}");
        assert!(
            small.iter().any(|r| r.contains("of what it is")),
            "{small:#?}"
        );
        press(&mut fixture.app, "x");
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(mode(&fixture), Mode::Picker);
        press(&mut fixture.app, "\x1b");

        open(&mut fixture, "Hotkeys");
        to_row(&mut fixture, "toggle");
        press(&mut fixture.app, "\n?");
        assert_eq!(fixture.app.dialog, Some(Dialog::Help(HelpFor::Hotkey)));
        press(&mut fixture.app, "\x1b");
        assert_eq!(mode(&fixture), Mode::Hotkey);
    }

    /// The model files load on a worker and set a full path; Tab in the
    /// filter completes a path.
    #[test]
    fn a_model_file_is_picked_or_its_path_completed() {
        use crate::tui::app::Msg;
        use crate::tui::options::ModelFile;

        let mut fixture = app("");
        let models = fixture.dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        open(&mut fixture, "Backend & keys");
        let row = fixture
            .app
            .config
            .items()
            .iter()
            .position(
                |item| matches!(item, Item::Field { field, .. } if field.label == "whisper model"),
            )
            .unwrap();
        press(&mut fixture.app, "g");
        // Asked for when the cursor lands on the row.
        let effects = press(&mut fixture.app, &"j".repeat(row));
        assert_eq!(effects, [Effect::List(ListKind::Models)]);
        assert!(press(&mut fixture.app, "\n").is_empty());
        let small = models.join("ggml-small.en.bin");
        fixture
            .app
            .update(Msg::Listed(Listing::Models(Ok(vec![ModelFile {
                path: small.clone(),
                size: 466_000_000,
            }]))));
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("ggml-small.en.bin") && r.contains("466 MB")));
        type_text(&mut fixture.app, "small");
        press(&mut fixture.app, "\n");
        let path = |f: &Fixture| config(f).local_whisper.as_ref().unwrap().model_path.clone();
        assert_eq!(path(&fixture), small.to_string_lossy());

        std::fs::write(models.join("ggml-tiny.bin"), b"x").unwrap();
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, &format!("{}/ggml-t", models.display()));
        chord(&mut fixture.app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut fixture.app, "\n");
        assert_eq!(
            path(&fixture),
            models.join("ggml-tiny.bin").to_string_lossy()
        );
    }

    /// Tab completes a typed path in a path input. A path the entry
    /// needs, not set yet, says so, not `empty`.
    #[test]
    fn a_path_input_completes_with_tab() {
        let mut fixture = app("[[agents]]\nname = \"jarvis\"\n");
        let root = fixture.dir.path().to_path_buf();
        std::fs::create_dir(root.join("Documents")).unwrap();
        open(&mut fixture, "Agents");
        press(&mut fixture.app, "\n");
        to_row(&mut fixture, "dir");
        let form = drawn(&fixture).join("\n");
        assert!(
            form.lines()
                .any(|r| r.contains("dir") && r.contains("not set") && !r.contains("empty")),
            "{form}"
        );
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, &format!("{}/Doc", root.display()));
        chord(&mut fixture.app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(drawn(&fixture).join("\n").contains("tab complete"));
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).agents[0].dir,
            format!("{}/Documents/", root.display())
        );
    }

    /// A list whose items are languages adds and changes them through the
    /// picker.
    #[test]
    fn list_items_are_picked() {
        let mut fixture = app("");
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "openai languages");
        press(&mut fixture.app, "\na");
        assert_eq!(mode(&fixture), Mode::Picker);
        type_text(&mut fixture.app, "ukr");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).openai.as_ref().unwrap().languages, ["uk"]);
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "ru");
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).openai.as_ref().unwrap().languages, ["ru"]);
    }

    /// A colour shows a swatch of itself, or of the theme's while unset.
    #[test]
    fn a_colour_shows_its_swatch() {
        let fixture =
            app("[overlay]\ntheme = \"custom\"\n[overlay.colors]\nrecording = \"#F97316\"\n");
        let mut fixture = fixture;
        open(&mut fixture, "Overlay");
        let buf = crate::tui::testing::draw(&fixture.app, 90, 30);
        let rows = crate::tui::testing::rows(&buf);
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("#F97316"))
            .expect("the colour row");
        let x = row.chars().position(|c| c == '\u{2588}').expect("a swatch") as u16;
        assert_eq!(
            buf[(x, y as u16)].fg,
            ratatui::style::Color::Rgb(0xF9, 0x73, 0x16)
        );
        // Unset: the theme's own (ember's, under custom) beside "theme".
        assert!(rows
            .iter()
            .any(|r| r.contains("speaking color") && r.contains("theme \u{2588}")));
    }

    /// A harness whose binary is not on PATH says so.
    #[test]
    fn a_missing_harness_is_noted() {
        let mut fixture = app("[[agents]]\nname = \"jarvis\"\n");
        open(&mut fixture, "Agents");
        press(&mut fixture.app, "\n");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("claude not on PATH")));
    }

    /// A typed language must be one whisrs knows: `zz` is refused in the
    /// picker, a region tag of a known code and `multi` are taken.
    #[test]
    fn a_typed_language_must_be_known() {
        let mut fixture = app("");
        open(&mut fixture, "General");
        to_row(&mut fixture, "language");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "zz");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Picker);
        let said = status(&fixture);
        assert_eq!(said.tone, Tone::Error);
        assert!(
            said.text.contains("zz is not a language whisrs knows"),
            "{}",
            said.text
        );
        press(&mut fixture.app, "\x1b");
        assert_eq!(config(&fixture).general.language, "en");
        for (typed, stored) in [("en-US", "en-US"), ("multi", "multi")] {
            press(&mut fixture.app, "\n");
            type_text(&mut fixture.app, typed);
            // The typed value's row is the last one.
            chord(&mut fixture.app, KeyCode::End, KeyModifiers::NONE);
            press(&mut fixture.app, "\n");
            assert_eq!(config(&fixture).general.language, stored, "{typed}");
        }
    }

    /// Move the cursor to the row whose key is `path`.
    fn to_path(fixture: &mut Fixture, path: &str) {
        press(&mut fixture.app, "g");
        for _ in 0..40 {
            let screen = &fixture.app.config;
            let at = screen.items().get(screen.view().cursor()).cloned();
            if matches!(at, Some(Item::Field { path: ref p, .. }) if p == path) {
                return;
            }
            press(&mut fixture.app, "j");
        }
        panic!("no row {path}");
    }

    fn labels(fixture: &Fixture) -> Vec<String> {
        let screen = &fixture.app.config;
        screen
            .items()
            .iter()
            .map(|item| describe(screen, item, &fixture.app.env, &ui()).0)
            .collect()
    }

    fn ui() -> Ui<'static> {
        static THEME: std::sync::OnceLock<crate::tui::theme::Theme> = std::sync::OnceLock::new();
        Ui {
            theme: THEME.get_or_init(crate::tui::testing::theme),
            glyphs: &crate::tui::glyphs::Glyphs::UNICODE,
        }
    }

    /// With no entries, Hotkeys is its `[hotkeys]` rows, as before.
    #[test]
    fn hotkeys_without_entries_are_the_hotkeys_rows() {
        let mut fixture = app("");
        open(&mut fixture, "Hotkeys");
        let paths: Vec<String> = fixture
            .app
            .config
            .items()
            .iter()
            .map(|item| match item {
                Item::Field { path, .. } => path.clone(),
                _ => panic!("not a row"),
            })
            .collect();
        let rows: Vec<String> = SECTIONS
            .iter()
            .find(|s| s.title == "Hotkeys")
            .map(|s| match s.body {
                Body::Rows(fields) => fields.iter().map(|f| f.path.to_string()).collect(),
                _ => Vec::new(),
            })
            .unwrap();
        assert_eq!(paths, rows);
    }

    /// Every binding shows in Hotkeys: each agent's and command's keys,
    /// under a heading, named by the entry.
    #[test]
    fn hotkeys_show_every_entrys_keys() {
        let mut fixture = app(
            "[[agents]]\nname = \"jarvis\"\n[[llm_commands]]\nname = \"translate-de\"\n\
             hotkey = \"F9\"\ninstruction = \"x\"\n",
        );
        open(&mut fixture, "Hotkeys");
        let labels = labels(&fixture);
        assert_eq!(
            labels[6..],
            [
                "Agents",
                "jarvis",
                "jarvis new session",
                "LLM commands",
                "translate-de",
                "translate-de set",
            ]
        );
        // The cursor passes over the headings, both ways.
        to_path(&mut fixture, "hotkeys.next_profile");
        press(&mut fixture.app, "j");
        let screen = &fixture.app.config;
        assert!(matches!(
            screen.items().get(screen.view().cursor()),
            Some(Item::Field { path, .. }) if path == "agents.0.hotkey"
        ));
        press(&mut fixture.app, "k");
        let screen = &fixture.app.config;
        assert!(matches!(
            screen.items().get(screen.view().cursor()),
            Some(Item::Field { path, .. }) if path == "hotkeys.next_profile"
        ));
    }

    /// An agent's key set from Hotkeys is the agent's own: its form shows
    /// it, a save writes it under [[agents]], and x resets it.
    #[test]
    fn an_agent_hotkey_set_from_hotkeys_is_the_agents_own() {
        let mut fixture = app(
            "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n[[agents]]\nname =              \"jarvis\"\ndir = \"/tmp\"\n",
        );
        open(&mut fixture, "Hotkeys");
        to_path(&mut fixture, "agents.0.hotkey");
        press(&mut fixture.app, "\n");
        assert_eq!(mode(&fixture), Mode::Hotkey);
        assert!(drawn(&fixture).iter().any(|r| r.contains("jarvis hotkey")));
        press(&mut fixture.app, "t");
        type_text(&mut fixture.app, "Super+J");
        press(&mut fixture.app, "\n");
        assert_eq!(
            config(&fixture).agents[0].hotkey.as_deref(),
            Some("Super+J")
        );

        // The agent's form shows it.
        open(&mut fixture, "Agents");
        press(&mut fixture.app, "\n");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("hotkey") && r.contains("Super+J")));
        // An edit there shows in Hotkeys.
        to_row(&mut fixture, "new session key");
        press(&mut fixture.app, "\nt");
        type_text(&mut fixture.app, "Super+Shift+J");
        press(&mut fixture.app, "\n\x1b");
        open(&mut fixture, "Hotkeys");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("jarvis new session") && r.contains("Super+Shift+J")));

        let paths = paths(fixture.dir.path());
        save(&fixture.app.config, &paths, false).expect("saves");
        let out = std::fs::read_to_string(&paths.config).unwrap();
        assert!(out.contains("[[agents]]"), "{out}");
        assert!(out.contains("hotkey = \"Super+J\""), "{out}");
        assert!(!out.contains("[hotkeys]"), "{out}");

        to_path(&mut fixture, "agents.0.hotkey");
        press(&mut fixture.app, "x");
        assert_eq!(config(&fixture).agents[0].hotkey, None);
    }

    /// An agent's key that a [hotkeys] row has too warns, in the dialog
    /// and after.
    #[test]
    fn an_agent_hotkey_clash_with_a_hotkeys_row_warns() {
        let mut fixture = app("[hotkeys]\ntoggle = \"Alt+D\"\n[[agents]]\nname = \"jarvis\"\n");
        open(&mut fixture, "Hotkeys");
        to_path(&mut fixture, "agents.0.hotkey");
        press(&mut fixture.app, "\n");
        // Alt, then D from the key picker.
        press(&mut fixture.app, "\t\t ");
        press(&mut fixture.app, "\t\t\n");
        type_text(&mut fixture.app, "d");
        press(&mut fixture.app, "\n");
        assert!(drawn(&fixture)
            .iter()
            .any(|r| r.contains("Alt+D is already toggle")));
        press(&mut fixture.app, "\n");
        assert_eq!(config(&fixture).agents[0].hotkey.as_deref(), Some("Alt+D"));
        assert_eq!(status(&fixture).text, "Alt+D is also toggle");
    }

    /// Built without the recording feature, Recording says so and keeps
    /// the section; with it, it has rows.
    #[test]
    fn the_recording_section_follows_the_feature() {
        let mut fixture = app("");
        open(&mut fixture, "Recording");
        let screen = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        let missing = screen
            .iter()
            .any(|r| r.contains("built without the recording"));
        assert_eq!(missing, !cfg!(feature = "recording"));
        assert_eq!(
            fixture.app.config.items().is_empty(),
            !cfg!(feature = "recording")
        );
    }
}

/// Config in profile mode, and base Config while a profile is active.
#[cfg(test)]
mod profile_tests {
    use std::os::unix::fs::PermissionsExt;

    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::tui::app::{After, Dialog, Effect, Msg, Screen};
    use crate::tui::testing::{
        assert_snapshot, chord, daemon_on, draw, mockup, mockup_profiles, mockup_with,
        open_profile, open_profiles, press, rows, type_text, with_profiles, Fixture, PROFILES,
    };

    fn on(fixture: &Fixture) -> &ConfigScreen {
        fixture.app.profile_config.as_ref().expect("profile mode")
    }

    /// Profile mode on `name` of [`PROFILES`], `active` in the state file.
    fn profile_mode(name: &str, active: Option<&str>) -> Fixture {
        let fixture = mockup_profiles(PROFILES, active);
        entered(fixture, name)
    }

    /// `fixture` on Profiles, then in profile mode on `name`.
    fn entered(mut fixture: Fixture, name: &str) -> Fixture {
        open_profiles(&mut fixture);
        open_profile(&mut fixture, name);
        assert_eq!(fixture.app.screen(), Screen::Profile);
        fixture
    }

    /// Open section `title`, the focus on its rows.
    fn section(fixture: &mut Fixture, title: &str) {
        let index = SECTIONS.iter().position(|s| s.title == title).unwrap();
        if on(fixture).focus == Focus::Detail {
            press(&mut fixture.app, "\t");
        }
        press(&mut fixture.app, &format!("g{}\n", "j".repeat(index)));
        assert_eq!(on(fixture).current().title, title);
    }

    /// The cursor onto the row of `path`.
    fn to(fixture: &mut Fixture, path: &str) {
        let at = on(fixture)
            .items()
            .iter()
            .position(|item| matches!(item, Item::Field { path: p, .. } if p == path))
            .unwrap_or_else(|| panic!("no row {path}"));
        press(&mut fixture.app, &format!("g{}", "j".repeat(at)));
    }

    /// The tag of the row of `path` in the current section of `screen`.
    fn tag_of(screen: &ConfigScreen, path: &str) -> Option<(String, Tone)> {
        let item = screen
            .items()
            .into_iter()
            .find(|item| matches!(item, Item::Field { path: p, .. } if p == path))
            .unwrap_or_else(|| panic!("no row {path}"));
        tag(screen, &item)
    }

    fn said(fixture: &Fixture) -> Notice {
        fixture.app.status.clone().expect("a status")
    }

    fn drawn(fixture: &Fixture, w: u16, h: u16) -> Vec<String> {
        rows(&draw(&fixture.app, w, h))
    }

    fn file(fixture: &Fixture, name: &str) -> String {
        std::fs::read_to_string(fixture.app.paths.profile_paths().profile(name)).unwrap()
    }

    /// Profile mode's `w`, as the loop runs it.
    fn save(fixture: &mut Fixture) -> Option<Effect> {
        assert_eq!(press(&mut fixture.app, "w"), [Effect::SaveProfile]);
        let saved = save_profile(on(fixture), &fixture.app.paths);
        fixture.app.update(Msg::ProfileSaved(saved))
    }

    fn profile() -> (String, Tone) {
        ("profile".to_string(), Tone::Accent)
    }

    fn base() -> (String, Tone) {
        ("base".to_string(), Tone::Muted)
    }

    fn base_only() -> (String, Tone) {
        ("base only".to_string(), Tone::Muted)
    }

    /// Each row says whose value it shows: the profile's, base's, or base
    /// only; the values are the merge's.
    #[test]
    fn each_row_says_whose_value_it_is() {
        let mut fixture = profile_mode("offline", None);
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("config \u{00B7} profile offline \u{00B7} \u{25CF}")));
        let config = on(&fixture).form.config();
        assert_eq!(config.general.backend, "local-whisper");
        assert_eq!(config.general.language, "de");
        assert_eq!(
            config.deepgram.as_ref().unwrap().api_key,
            "dg-0123456789abcdef9f2c"
        );
        section(&mut fixture, "General");
        let screen = on(&fixture);
        assert_eq!(tag_of(screen, "general.language"), Some(profile()));
        assert_eq!(tag_of(screen, "general.silence_timeout_ms"), Some(base()));
        section(&mut fixture, "Backend & keys");
        let row = drawn(&fixture, 90, 30)
            .into_iter()
            .find(|r| r.contains("local-whisper"))
            .expect("the backend row");
        assert!(
            row.trim_end_matches(['\u{2502}', ' ']).ends_with("profile"),
            "{row}"
        );
        let screen = on(&fixture);
        assert_eq!(tag_of(screen, "general.backend"), Some(profile()));
        assert_eq!(tag_of(screen, "deepgram.api_key"), Some(base_only()));
        assert_eq!(tag_of(screen, "local-whisper.model_path"), Some(profile()));
        section(&mut fixture, "Vocabulary");
        assert_eq!(
            tag_of(on(&fixture), "general.vocabulary"),
            Some(base_only())
        );
        assert_eq!(tag_of(on(&fixture), "general.prompt"), Some(base()));
        section(&mut fixture, "Hotkeys");
        assert_eq!(tag_of(on(&fixture), "hotkeys.toggle"), Some(base_only()));
    }

    /// Base-only rows, the vocabulary list and the Replacements section
    /// refuse every edit, saying why; nothing is edited.
    #[test]
    fn base_only_rows_refuse_edits() {
        let mut fixture = profile_mode("offline", None);
        section(&mut fixture, "Backend & keys");
        to(&mut fixture, "deepgram.api_key");
        for key in ["\n", "x"] {
            assert!(press(&mut fixture.app, key).is_empty());
            assert_eq!(
                said(&fixture),
                Notice::new(Tone::Warn, profile::BASE_ONLY_NOTE),
                "{key:?}"
            );
            assert_eq!(on(&fixture).mode(), Mode::Browse);
        }
        // t still tests it, on this profile's config.
        assert!(matches!(
            press(&mut fixture.app, "t").as_slice(),
            [Effect::TestKey {
                origin: crate::tui::app::Origin::Profile,
                ..
            }]
        ));
        section(&mut fixture, "Hotkeys");
        to(&mut fixture, "hotkeys.toggle");
        press(&mut fixture.app, "\n");
        assert_eq!(said(&fixture).text, profile::BASE_ONLY_NOTE);
        assert_eq!(on(&fixture).mode(), Mode::Browse);

        let vocabulary = "the vocabulary is edited on base, as vocabulary.txt is every \
                          profile's: e sets a list in profiles/offline.toml";
        section(&mut fixture, "Vocabulary");
        // The list opens to be looked at; a and d are refused.
        to(&mut fixture, "general.vocabulary");
        press(&mut fixture.app, "\n");
        assert!(matches!(on(&fixture).view(), View::List { .. }));
        for key in ["a", "d", "\n"] {
            fixture.app.status = None;
            press(&mut fixture.app, key);
            assert_eq!(said(&fixture).text, vocabulary, "{key:?}");
        }
        press(&mut fixture.app, "\x1b");
        section(&mut fixture, "Replacements");
        press(&mut fixture.app, "a");
        assert_eq!(
            said(&fixture).text,
            "Replacements is edited on base: e sets it in profiles/offline.toml"
        );
        section(&mut fixture, "Agents");
        press(&mut fixture.app, "a");
        assert_eq!(said(&fixture).text, profile::BASE_ONLY_NOTE);
        assert!(!on(&fixture).is_dirty());
    }

    /// An edit goes into the profile even at config.toml's value, and `w`
    /// writes only the profile's file: its comments kept, 0600, config.toml
    /// as it was.
    #[test]
    fn an_edit_is_written_into_the_profile_alone() {
        let mut fixture = profile_mode("offline", None);
        let paths = fixture.app.paths.clone();
        let base_before = std::fs::read(&paths.config).unwrap();
        section(&mut fixture, "General");
        to(&mut fixture, "general.notify");
        assert_eq!(tag_of(on(&fixture), "general.notify"), Some(base()));
        // Off and on again: config.toml's value, set by the profile now.
        press(&mut fixture.app, "  ");
        assert!(on(&fixture).form.config().general.notify);
        assert_eq!(tag_of(on(&fixture), "general.notify"), Some(profile()));
        assert!(on(&fixture).is_dirty());
        to(&mut fixture, "general.silence_timeout_ms");
        press(&mut fixture.app, "l");
        assert_eq!(on(&fixture).form.config().general.silence_timeout_ms, 2250);

        assert_eq!(save(&mut fixture), Some(Effect::ReadProfiles));
        assert_eq!(
            said(&fixture).text,
            "saved profiles/offline.toml (not active)"
        );
        let text = file(&fixture, "offline");
        assert!(
            text.starts_with(
                "# on the train\n[general]\nbackend = \"local-whisper\"\nlanguage = \"de\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("notify = true"), "{text}");
        assert!(text.contains("silence_timeout_ms = 2250"), "{text}");
        assert!(
            text.contains("[local-whisper]\nmodel_path = \"/m/ggml-base.bin\""),
            "{text}"
        );
        let file_path = paths.profile_paths().profile("offline");
        assert_eq!(
            std::fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&paths.config).unwrap(), base_before);
        // Edited on from the file.
        assert!(!on(&fixture).is_dirty());
        assert_eq!(tag_of(on(&fixture), "general.notify"), Some(profile()));
        // A save with nothing to save says so.
        assert!(press(&mut fixture.app, "w").is_empty());
        assert_eq!(said(&fixture).text, "no changes to save");
    }

    /// `x` takes a key out of the profile, back to config.toml's value; on
    /// a key it does not set it says so. A table left empty leaves the
    /// file, the comment over the first one stays.
    #[test]
    fn x_takes_a_key_out_of_the_profile() {
        let mut fixture = profile_mode("offline", None);
        section(&mut fixture, "General");
        to(&mut fixture, "general.silence_timeout_ms");
        assert!(press(&mut fixture.app, "x").is_empty());
        assert_eq!(
            said(&fixture),
            Notice::new(Tone::Muted, "offline does not set this")
        );
        to(&mut fixture, "general.language");
        press(&mut fixture.app, "x");
        assert_eq!(on(&fixture).form.config().general.language, "en");
        assert_eq!(tag_of(on(&fixture), "general.language"), Some(base()));
        assert!(on(&fixture).is_dirty());
        section(&mut fixture, "Backend & keys");
        to(&mut fixture, "local-whisper.model_path");
        press(&mut fixture.app, "x");
        assert_eq!(
            on(&fixture)
                .form
                .config()
                .local_whisper
                .as_ref()
                .unwrap()
                .model_path,
            "~/.local/share/whisrs/models/ggml-base.en.bin"
        );
        save(&mut fixture);
        assert_eq!(
            file(&fixture, "offline"),
            "# on the train\n[general]\nbackend = \"local-whisper\"\n"
        );
        assert_eq!(
            tag_of(on(&fixture), "local-whisper.model_path"),
            Some(base())
        );
    }

    /// A merged config that does not validate is refused: nothing is
    /// written, every edit stays.
    #[test]
    fn a_merged_config_that_does_not_validate_keeps_the_edits() {
        let mut fixture = profile_mode("offline", None);
        section(&mut fixture, "Backend & keys");
        to(&mut fixture, "general.backend");
        press(&mut fixture.app, "\n");
        type_text(&mut fixture.app, "asr-sidecar");
        press(&mut fixture.app, "\n");
        assert_eq!(on(&fixture).form.config().general.backend, "asr-sidecar");
        to(&mut fixture, "asr-sidecar.url");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut fixture.app, "\n");
        let before = file(&fixture, "offline");
        assert_eq!(save(&mut fixture), None);
        let status = said(&fixture);
        assert_eq!(status.tone, Tone::Error);
        assert!(status.text.starts_with("not saved: "), "{}", status.text);
        assert_eq!(file(&fixture, "offline"), before);
        assert!(on(&fixture).is_dirty());
        assert_eq!(on(&fixture).form.config().general.backend, "asr-sidecar");
    }

    /// A profile's file holds values, so it cannot unset a key config.toml
    /// sets; the edit is refused, the value kept.
    #[test]
    fn a_profile_cannot_unset_what_config_toml_sets() {
        let base = mockup_with("\n[overlay.colors]\nrecording = \"#ff0000\"\n");
        let mut fixture = entered(with_profiles(base, PROFILES, None), "offline");
        section(&mut fixture, "Overlay");
        to(&mut fixture, "overlay.colors.recording");
        press(&mut fixture.app, "\n");
        chord(&mut fixture.app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut fixture.app, "\n");
        assert!(said(&fixture)
            .text
            .contains("a profile cannot unset what config.toml sets"));
        let colors = on(&fixture)
            .form
            .config()
            .overlay
            .clone()
            .unwrap()
            .colors
            .unwrap();
        assert_eq!(colors.recording.as_deref(), Some("#ff0000"));
    }

    /// Esc and q with unsaved edits ask: Esc stays, d drops them, w saves
    /// them first; `e` asks the same. Clean, Esc is back on Profiles.
    #[test]
    fn leaving_with_unsaved_edits_asks() {
        let edited = || {
            let mut fixture = profile_mode("offline", None);
            section(&mut fixture, "General");
            to(&mut fixture, "general.notify");
            press(&mut fixture.app, " ");
            fixture
        };
        let unsaved = |after| Some(Dialog::UnsavedProfile(after, "offline".into()));
        let mut fixture = edited();
        press(&mut fixture.app, "\x1b\x1b");
        assert_eq!(fixture.app.dialog, unsaved(After::Leave));
        assert!(drawn(&fixture, 90, 30)
            .iter()
            .any(|r| r.contains("Unsaved changes to profiles/offline.toml.")));
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(fixture.app.screen(), Screen::Profile);
        assert!(on(&fixture).is_dirty());
        press(&mut fixture.app, "\x1b");
        assert!(press(&mut fixture.app, "d").is_empty());
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        assert!(fixture.app.profile_config.is_none());
        assert!(!file(&fixture, "offline").contains("notify"));

        let mut fixture = edited();
        press(&mut fixture.app, "q");
        assert_eq!(fixture.app.dialog, unsaved(After::Quit));
        assert_eq!(press(&mut fixture.app, "d"), [Effect::Quit]);

        let mut fixture = edited();
        press(&mut fixture.app, "\x1b\x1b");
        assert_eq!(press(&mut fixture.app, "w"), [Effect::SaveProfile]);
        let saved = save_profile(on(&fixture), &fixture.app.paths);
        fixture.app.update(Msg::ProfileSaved(saved));
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        assert!(file(&fixture, "offline").contains("notify = false"));

        let mut fixture = edited();
        press(&mut fixture.app, "e");
        assert_eq!(fixture.app.dialog, unsaved(After::Editor));
        assert_eq!(
            press(&mut fixture.app, "d"),
            [Effect::EditProfile("offline".into())]
        );

        let mut clean = profile_mode("offline", None);
        assert_eq!(
            press(&mut clean.app, "e"),
            [Effect::EditProfile("offline".into())]
        );
        press(&mut clean.app, "\x1b");
        assert_eq!(clean.app.screen(), Screen::Profiles);
    }

    /// Saving the active profile restarts the daemon, as a base save does.
    #[test]
    fn saving_the_active_profile_restarts_the_daemon() {
        let mut fixture = profile_mode("offline", Some("offline"));
        section(&mut fixture, "General");
        to(&mut fixture, "general.notify");
        press(&mut fixture.app, " ");
        assert_eq!(
            save(&mut fixture),
            Some(Effect::All(vec![Effect::ReadProfiles, Effect::Restart]))
        );
        assert_eq!(
            said(&fixture).text,
            "saved profiles/offline.toml, restarting the daemon"
        );
    }

    /// A profile whose file is not TOML does not open, saying why.
    #[test]
    fn a_broken_profile_does_not_open() {
        let mut fixture = mockup_profiles(PROFILES, None);
        open_profiles(&mut fixture);
        open_profile(&mut fixture, "broken");
        assert_eq!(fixture.app.screen(), Screen::Profiles);
        let status = said(&fixture);
        assert_eq!(status.tone, Tone::Error);
        assert!(status
            .text
            .starts_with("cannot open broken in Config: profiles/broken.toml is not valid TOML"));
    }

    /// The help names profile mode's keys.
    #[test]
    fn profile_mode_has_its_help() {
        let mut fixture = profile_mode("offline", None);
        press(&mut fixture.app, "?");
        let drawn = drawn(&fixture, 90, 30);
        assert!(drawn.iter().any(|r| r.contains("profile keys")));
        assert!(drawn
            .iter()
            .any(|r| r.contains("take it out of the profile")));
    }

    /// On base, a row the active profile sets says so, and its help that
    /// an edit applies once base is active; the header says base.
    #[test]
    fn base_config_names_what_the_active_profile_sets() {
        let mut fixture = mockup_profiles(PROFILES, Some("offline"));
        daemon_on(&mut fixture.app, Some("offline"), None, 1);
        let config = &fixture.app.config;
        let tagged = |path: &str| {
            let item = Item::Field {
                field: schema::row(path),
                path: path.to_string(),
            };
            tag(config, &item)
        };
        assert_eq!(
            tagged("general.language"),
            Some(("offline sets this".to_string(), Tone::Muted))
        );
        assert_eq!(tagged("general.silence_timeout_ms"), None);
        press(&mut fixture.app, "\n");
        let drawn = drawn(&fixture, 90, 30);
        assert!(drawn
            .iter()
            .any(|r| r.contains("config \u{00B7} base \u{00B7} \u{25CF}")));
        assert!(drawn
            .iter()
            .any(|r| r.contains("language") && r.contains("offline sets this")));
        assert!(row_help(&fixture.app.config)
            .contains(" offline sets this, so an edit here applies once base is active."));
        // With no profile active, nothing is tagged.
        let plain = mockup();
        assert!(plain.app.config.overrides.is_none());
        assert!(!rows(&draw(&plain.app, 90, 30))
            .iter()
            .any(|r| r.contains("config \u{00B7} base")));
    }

    /// Mic's `w` with the active profile setting the device says that one
    /// stays in use.
    #[test]
    fn mic_says_the_active_profile_keeps_its_device() {
        use crate::tui::options::{Device, Listing};
        let mut fixture = mockup_profiles(
            &[("loud", "[audio]\ndevice = \"alsa_input.usb-yeti\"\n")],
            Some("loud"),
        );
        daemon_on(&mut fixture.app, Some("loud"), None, 1);
        press(&mut fixture.app, "\x1bj\n");
        assert_eq!(fixture.app.screen(), Screen::Mic);
        fixture
            .app
            .update(Msg::Listed(Listing::Devices(Ok(vec![Device {
                id: "alsa_input.usb-mic".into(),
                description: "USB mic".into(),
                monitor: false,
            }]))));
        press(&mut fixture.app, "l");
        assert_eq!(
            press(&mut fixture.app, "w"),
            [Effect::SaveDevice { overwrite: false }]
        );
        let keys = ["audio.device".to_string()];
        let saved = save_keys(&fixture.app.config, &keys, false, &fixture.app.paths, false);
        fixture.app.update(Msg::DeviceSaved(saved));
        assert!(said(&fixture)
            .text
            .ends_with("; loud sets its own device, which stays in use while it is active"));
    }

    /// The rest of the Vocabulary section is plain `[general]` keys, set in
    /// the profile as any: the prompt typed, the filler words a list.
    #[test]
    fn the_prompt_and_the_filler_words_go_into_the_profile() {
        let mut fixture = profile_mode("offline", None);
        section(&mut fixture, "Vocabulary");
        to(&mut fixture, "general.prompt");
        press(&mut fixture.app, "\n");
        assert_eq!(on(&fixture).mode(), Mode::Text);
        type_text(&mut fixture.app, "Ein Diktat.");
        press(&mut fixture.app, "\n");
        assert_eq!(tag_of(on(&fixture), "general.prompt"), Some(profile()));
        to(&mut fixture, "general.remove_filler_words");
        press(&mut fixture.app, " ");
        assert_eq!(
            tag_of(on(&fixture), "general.remove_filler_words"),
            Some(profile())
        );
        to(&mut fixture, "general.filler_words");
        press(&mut fixture.app, "\na");
        type_text(&mut fixture.app, "halt");
        press(&mut fixture.app, "\n\x1b");
        assert_eq!(
            tag_of(on(&fixture), "general.filler_words"),
            Some(profile())
        );
        save(&mut fixture);
        let text = file(&fixture, "offline");
        assert!(text.contains("prompt = \"Ein Diktat.\""), "{text}");
        assert!(text.contains("filler_words = [\"halt\"]"), "{text}");
        assert!(text.contains("remove_filler_words = "), "{text}");
    }

    /// A commit or a pick of the value a row has already is no edit: the
    /// row stays config.toml's, the screen clean, and nothing is pinned.
    #[test]
    fn the_value_there_already_pins_nothing() {
        let mut fixture = profile_mode("offline", None);
        section(&mut fixture, "General");
        to(&mut fixture, "general.silence_timeout_ms");
        press(&mut fixture.app, "\n\n");
        assert_eq!(
            tag_of(on(&fixture), "general.silence_timeout_ms"),
            Some(base())
        );
        section(&mut fixture, "Typing");
        to(&mut fixture, "input.backend");
        press(&mut fixture.app, "\n");
        assert_eq!(on(&fixture).mode(), Mode::Picker);
        press(&mut fixture.app, "\n");
        assert_eq!(on(&fixture).mode(), Mode::Browse);
        assert_eq!(tag_of(on(&fixture), "input.backend"), Some(base()));
        assert!(!on(&fixture).is_dirty());
        assert!(press(&mut fixture.app, "w").is_empty());
        assert_eq!(said(&fixture).text, "no changes to save");
        assert!(!file(&fixture, "offline").contains("silence_timeout_ms"));
    }

    /// The merged config is checked as a client checks it: a cloud backend
    /// whose key may be in whisrsd's environment alone saves, warned of.
    #[test]
    fn a_key_in_whisrsds_environment_does_not_stop_a_save() {
        let fixture = mockup_profiles(&[("cloud", "[general]\nbackend = \"openai\"\n")], None);
        let mut fixture = entered(fixture, "cloud");
        section(&mut fixture, "General");
        to(&mut fixture, "general.notify");
        press(&mut fixture.app, " ");
        assert_eq!(save(&mut fixture), Some(Effect::ReadProfiles));
        assert_eq!(
            said(&fixture).text,
            "saved profiles/cloud.toml (not active)"
        );
        if std::env::var_os("WHISRS_OPENAI_API_KEY").is_none() {
            assert!(fixture
                .app
                .profile_notices
                .iter()
                .any(|n| n.text.contains("WHISRS_OPENAI_API_KEY")));
        }
    }

    #[test]
    fn profile_mode_snapshots() {
        let mut fixture = profile_mode("offline", Some("offline"));
        daemon_on(&mut fixture.app, Some("offline"), None, 1);
        // Backend & keys: rows the profile sets, base's and base only.
        press(&mut fixture.app, "j\n");
        // Of the default build: without whisper.cpp, its backend says so.
        if !schema::Gate::LocalWhisper.built() {
            return;
        }
        for (w, h) in [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)] {
            assert_snapshot(&format!("profile-config-{w}x{h}"), &drawn(&fixture, w, h));
        }
    }
}
