//! Config: every config.toml key, by section ([`schema::SECTIONS`]), on the
//! left the sections and on the right the rows of the one picked. At 90
//! columns and up both panes show; below, the list drills into a section.
//!
//! Edits go into a [`Form`] and stay there until `w`, which validates the
//! config, writes it through the format-preserving writer (comments, key
//! order and unknown keys kept, 0600, atomic, `vocabulary.txt` by its own
//! rules) and restarts the daemon. An invalid config is refused with the
//! error shown and every edit kept.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use toml::Value;
use whisrs::config::keys::{self, KeySummary};
use whisrs::config::vocabulary::{self, VocabularyBaseline};
use whisrs::config::write::{self, ExistingConfig};

use crate::tui::app::{App, Dialog};
use crate::tui::form::Form;
use crate::tui::keys::Action;
use crate::tui::schema::{self, Body, Entries, Field, Kind, Section, SECTIONS};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::input::Input;
use crate::tui::widgets::row::{Row, Value as Shown};
use crate::tui::widgets::{fit, scroll, width, wrap, Notice, Tone, Ui};
use crate::tui::{Env, Paths};

/// The section list's width in the two-pane layout.
const LIST_WIDTH: u16 = super::home::LIST_WIDTH;

/// The widest a label column gets.
const MAX_LABEL: usize = 24;

/// The most help lines under the rows.
const MAX_HELP: usize = 4;

/// A config read from disk, ready to edit.
pub struct Loaded {
    pub form: Form,
    pub vocabulary: VocabularyBaseline,
    /// No file at the path: these are the defaults.
    pub fresh: bool,
    /// Unknown keys, an unreadable `vocabulary.txt`, a fresh config.
    pub notices: Vec<Notice>,
    /// config.toml's bytes as read (`None`: no file), to tell at save
    /// whether something else wrote it since, such as the daemon's
    /// `llm-command-set`.
    pub disk: Option<Vec<u8>>,
}

/// What a save did.
pub struct Saved {
    /// config.toml.
    pub path: PathBuf,
    pub warnings: Vec<String>,
    /// config.toml could not be merged into and was written from scratch.
    pub regenerated: Option<write::Regenerated>,
    /// Both stores as they are now on disk, to edit on from.
    pub loaded: Loaded,
}

/// Why a save did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// Refused or failed: nothing was written, or not all of it.
    Failed(String),
    /// config.toml changed on disk since it was read; nothing was written.
    Changed,
    /// Written, but reading it back failed.
    Reread(String),
}

/// Load config.toml (and `vocabulary.txt`) to edit. A file that cannot be
/// used is refused with the text `whisrs config` exits with (#134): nothing
/// opens on defaults over a file the user has.
pub fn load(paths: &Paths) -> Result<Loaded, String> {
    read(paths)
        .map_err(|(kind, message)| write::unusable_config_refusal(kind, &message, &paths.config))
}

/// [`load`] again after `$EDITOR`, with what validation says about the
/// result. A file that no longer loads is its error's first line.
pub fn reload(paths: &Paths) -> Result<Loaded, String> {
    let mut loaded = read(paths)
        .map_err(|(_, message)| message.lines().next().unwrap_or_default().to_string())?;
    match loaded.form.config().validate() {
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

fn read(paths: &Paths) -> Result<Loaded, (write::UnusableKind, String)> {
    // Before the load: a write between the two then reads as a change at
    // save, which only asks.
    let disk = std::fs::read(&paths.config).ok();
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
    if fresh {
        notices.push(Notice::new(
            Tone::Muted,
            "No config.toml yet: these are the defaults, and w writes them. whisrs setup does \
             the whole onboarding.",
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
/// `overwrite`, a config.toml that changed on disk since it was read is left
/// alone: writing it would undo what changed it.
pub fn save(screen: &ConfigScreen, paths: &Paths, overwrite: bool) -> Result<Saved, SaveError> {
    if !overwrite && std::fs::read(&paths.config).ok() != screen.disk {
        return Err(SaveError::Changed);
    }
    let config = screen.form.config();
    let warnings = config
        .validate()
        .map_err(|e| SaveError::Failed(one_line(&e.to_string())))?;
    let wrote = vocabulary::write_config_and_vocabulary(
        config,
        &screen.vocabulary,
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
        loaded,
    })
}

/// A message on one line: its line breaks are spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sections,
    Detail,
}

/// A level of the detail pane.
#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    /// The section's own rows, entries or map.
    Section { cursor: usize },
    /// One entry of the section's array of tables.
    Entry { index: usize, cursor: usize },
    /// The items of a list key.
    List {
        path: String,
        field: &'static str,
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

/// What a key on Config asks of the app.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Back to Home.
    pub leave: bool,
    pub status: Option<Notice>,
    pub flash: Option<String>,
    pub dialog: Option<Dialog>,
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
    /// config.toml as last read ([`Loaded::disk`]).
    disk: Option<Vec<u8>>,
    section: usize,
    focus: Focus,
    /// The detail pane's levels, never empty.
    views: Vec<View>,
    editing: Option<Editing>,
    /// Rows whose label or path holds this, in any case.
    filter: String,
}

impl ConfigScreen {
    pub fn new(loaded: Loaded) -> ConfigScreen {
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
            filter: String::new(),
        }
    }

    /// Whether a text input has the keys.
    pub fn typing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn discard(&mut self) {
        self.form.discard();
        self.editing = None;
        self.clamp();
    }

    /// Edit on from what a save wrote.
    pub fn saved(&mut self, loaded: Loaded) {
        self.reloaded(loaded);
    }

    /// Edit on from config.toml as read again.
    pub fn reloaded(&mut self, loaded: Loaded) {
        self.form = loaded.form;
        self.vocabulary = loaded.vocabulary;
        self.fresh = loaded.fresh;
        self.disk = loaded.disk;
        self.broken = None;
        self.editing = None;
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
                        Body::Rows(fields) => fields.iter().any(|f| self.passes(section, f)),
                        Body::Entries(entries) => entries.path.contains(&query),
                        Body::Map(map) => map.path.contains(&query),
                    }
            })
            .collect()
    }

    /// The rows of the detail pane's current level.
    fn items(&self) -> Vec<Item> {
        let section = self.current();
        match self.view() {
            _ if !self.built() => Vec::new(),
            View::Section { .. } => match section.body {
                Body::Rows(fields) => fields
                    .iter()
                    .filter(|f| self.passes(section, f))
                    .map(|field| Item::Field {
                        field,
                        path: field.path.to_string(),
                    })
                    .collect(),
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
        let len = self.items().len();
        let view = self.view_mut();
        view.set_cursor(view.cursor().min(len.saturating_sub(1)));
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
        if self.editing.is_some() {
            return self.edit_key(action);
        }
        if action == Action::Filter {
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
                return Outcome::default();
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
            Action::First => 0,
            Action::Last => last,
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
        self.view_mut().set_cursor(to);
        Outcome::default()
    }

    fn item_key(&mut self, action: Action, item: Option<Item>) -> Outcome {
        if let Some(Item::Field { field, path }) = &item {
            return self.field_key(field, path, action);
        }
        let view = self.view().clone();
        match (&view, self.current().body, action) {
            (View::List { path, .. }, _, Action::Add) => {
                self.type_into(Target::Element {
                    path: path.clone(),
                    index: None,
                });
            }
            (View::List { path, .. }, _, Action::Enter | Action::Right) => {
                if let Some(Item::Element(index)) = item {
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
                    self.views.push(View::Entry { index, cursor: 0 });
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
            (_, _, Action::Left) => self.focus = Focus::Sections,
            _ => {}
        }
        Outcome::default()
    }

    fn field_key(&mut self, field: &'static Field, path: &str, action: Action) -> Outcome {
        let current = self.form.effective(path).cloned();
        let step = match action {
            Action::Left => -1,
            Action::Right | Action::Enter | Action::Toggle => 1,
            _ => 0,
        };
        match (&field.kind, action) {
            (_, Action::Reset) => {
                let result = self.form.reset(path);
                return self.apply(path, field, result);
            }
            (Kind::Bool, Action::Enter | Action::Toggle | Action::Left | Action::Right) => {
                let on = current.and_then(|v| v.as_bool()).unwrap_or(false);
                return self.set(path, field, Some(Value::Boolean(!on)));
            }
            (Kind::MaybeBool { .. }, _) if step != 0 => {
                let ring = [None, Some(true), Some(false)];
                let at = ring
                    .iter()
                    .position(|v| *v == current.as_ref().and_then(Value::as_bool))
                    .unwrap_or(0);
                let next = ring[cycle(at, ring.len(), step)].map(Value::Boolean);
                return self.set(path, field, next);
            }
            (Kind::Choice(choices), _) if step != 0 => {
                let at = current
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(|v| choices.iter().position(|c| *c == v));
                let next = match at {
                    Some(at) => cycle(at, choices.len(), step),
                    None if step > 0 => 0,
                    None => choices.len() - 1,
                };
                return self.set(path, field, Some(Value::from(choices[next])));
            }
            (Kind::MaybeChoice { choices, .. }, _) if step != 0 => {
                let at = match current.as_ref().and_then(Value::as_str) {
                    None => 0,
                    Some(v) => choices.iter().position(|c| *c == v).map_or(0, |i| i + 1),
                };
                let next = cycle(at, choices.len() + 1, step);
                let value = next.checked_sub(1).map(|i| Value::from(choices[i]));
                return self.set(path, field, value);
            }
            (Kind::Int { min, max, step: by }, Action::Left | Action::Right) => {
                let n = current.and_then(|v| v.as_integer()).unwrap_or(*min);
                let n = (n + by * step).clamp(*min, *max);
                return self.set(path, field, Some(Value::Integer(n)));
            }
            (Kind::Float { min, max, step: by }, Action::Left | Action::Right) => {
                let n = current.and_then(|v| v.as_float()).unwrap_or(*min);
                let n = ((n + by * step as f64) * 100.0).round() / 100.0;
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
            (Kind::Int { .. } | Kind::Float { .. } | Kind::Text { .. }, Action::Enter) => {
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
                    field: field.label,
                    cursor: 0,
                });
            }
            (_, Action::Left) => self.focus = Focus::Sections,
            _ => {}
        }
        Outcome::default()
    }

    /// Start typing a new value for `target`.
    fn type_into(&mut self, target: Target) {
        self.editing = Some(Editing {
            target,
            input: Input::new("", false),
        });
    }

    fn set(&mut self, path: &str, field: &Field, value: Option<Value>) -> Outcome {
        let result = self.form.set(path, value);
        self.apply(path, field, result)
    }

    fn apply(&mut self, path: &str, field: &Field, result: Result<bool, String>) -> Outcome {
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
            Ok(_) => Outcome::default(),
            Err(why) => Outcome::say(Tone::Error, why),
        }
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
            Target::Field { field, path } => match parse(field, trimmed) {
                Err(why) => {
                    self.editing = Some(editing.clone());
                    Outcome::say(Tone::Error, format!("{}: {why}", field.label))
                }
                Ok(None) => Outcome::default(),
                Ok(Some(value)) => {
                    let outcome = self.set(path, field, value);
                    if outcome.status.is_some() {
                        self.editing = Some(editing.clone());
                    }
                    outcome
                }
            },
            Target::Element { path, index } => {
                let mut list = self.list(path);
                match (index, trimmed.is_empty()) {
                    (Some(i), _) if *i >= list.len() => return Outcome::default(),
                    (Some(i), true) => {
                        list.remove(*i);
                    }
                    (Some(i), false) => list[*i] = trimmed.to_string(),
                    (None, true) => return Outcome::default(),
                    (None, false) => list.push(trimmed.to_string()),
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
                        self.views.push(View::Entry { index, cursor: 0 });
                        Outcome::default()
                    }
                    Err(why) => Outcome::say(Tone::Error, why),
                }
            }
        }
    }
}

/// `at` moved `step` around a ring of `len`.
fn cycle(at: usize, len: usize, step: i64) -> usize {
    (at as i64 + step).rem_euclid(len as i64) as usize
}

/// What typing `text` into `field` sets: `Ok(None)` changes nothing,
/// `Ok(Some(None))` unsets the key.
fn parse(field: &Field, text: &str) -> Result<Option<Option<Value>>, String> {
    match &field.kind {
        Kind::Int { min, max, .. } => {
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
        Kind::Text { unset, check } => {
            if text.is_empty() {
                return Ok(Some(match unset {
                    Some(_) => None,
                    None => Some(Value::from("")),
                }));
            }
            let text = match check {
                Some(check) => check(text)?,
                None => text.to_string(),
            };
            Ok(Some(Some(Value::String(text))))
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
    let on_off = |on: bool| Shown::plain(if on { "on" } else { "off" });
    let with_note = |text: &str| {
        let shown = Shown::plain(text);
        match schema::choice_note(text) {
            Some(note) => shown.then(note, Tone::Muted),
            None => shown,
        }
    };
    match &field.kind {
        Kind::Bool => on_off(value.and_then(Value::as_bool).unwrap_or(false)),
        Kind::MaybeBool { unset } => match value.and_then(Value::as_bool) {
            Some(on) => on_off(on),
            None => Shown::muted(*unset),
        },
        Kind::Choice(_) => with_note(value.and_then(Value::as_str).unwrap_or_default()),
        Kind::MaybeChoice { unset, .. } => match value.and_then(Value::as_str) {
            Some(text) => with_note(text),
            None => Shown::muted(*unset),
        },
        Kind::Int { .. } => Shown::plain(value.map(|v| v.to_string()).unwrap_or_default()),
        Kind::Float { .. } => Shown::plain(format!(
            "{:.2}",
            value.and_then(Value::as_float).unwrap_or(0.0)
        )),
        Kind::Text { unset, .. } => match value.and_then(Value::as_str) {
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
        },
        Kind::Secret => secret(screen, path, env),
        Kind::List { empty } => {
            let list = screen.list(path);
            if list.is_empty() {
                Shown::muted(*empty)
            } else {
                Shown::plain(list.join(", "))
            }
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

/// A row's label, value, and whether it is a choice.
fn describe(screen: &ConfigScreen, item: &Item, env: &Env, ui: &Ui) -> (String, Shown, bool) {
    match item {
        Item::Field { field, path } => {
            let choice = matches!(
                field.kind,
                Kind::Choice(_) | Kind::MaybeChoice { .. } | Kind::MaybeBool { .. }
            );
            (
                field.label.to_string(),
                shown(screen, field, path, env, ui),
                choice,
            )
        }
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
                false,
            )
        }
        Item::Pair(key) => {
            let path = match screen.current().body {
                Body::Map(map) => map.path,
                _ => "",
            };
            let value = screen.map(path).remove(key).unwrap_or_default();
            (key.clone(), Shown::plain(value), false)
        }
        Item::Element(index) => {
            let path = match screen.view() {
                View::List { path, .. } => path.as_str(),
                _ => "",
            };
            let text = screen.list(path).get(*index).cloned().unwrap_or_default();
            (String::new(), Shown::plain(text), false)
        }
    }
}

/// The key an item flashes under.
fn flash_key(item: &Item) -> Option<&str> {
    match item {
        Item::Field { path, .. } => Some(path),
        _ => None,
    }
}

/// The help under the rows, for the row with the cursor.
fn help(screen: &ConfigScreen, item: Option<&Item>) -> String {
    let section = screen.current();
    match (screen.view(), item) {
        (View::List { .. }, _) => "a adds an item, enter edits it, d deletes it.".to_string(),
        (_, Some(Item::Field { field, .. })) => {
            let mut text = field.help.to_string();
            if let Some(gate) = field.gate.filter(|gate| !gate.built()) {
                text.push_str(&format!(
                    " This whisrs was built without the {} feature, so the daemon ignores it.",
                    gate.name()
                ));
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
    let screen = &app.config;
    let area = f.area();
    let max = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, max);
    let Some(placed) = frame::place(area, Some(body_rows(messages.len()))) else {
        return frame::too_small(f, ui);
    };
    let name = if screen.filter.is_empty() || screen.typing_filter() {
        "config".to_string()
    } else {
        format!("config /{}", screen.filter)
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: &name,
            daemon: app.daemon_up(),
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
    if two_panes {
        let list = Rect {
            width: LIST_WIDTH.min(panes.width),
            ..panes
        };
        let detail = Rect {
            x: panes.x + list.width,
            width: panes.width - list.width,
            ..panes
        };
        sections(screen, f, list, ui);
        self::detail(app, f, detail, ui, false);
    } else if screen.focus == Focus::Sections {
        sections(screen, f, panes, ui);
    } else {
        self::detail(app, f, panes, ui, true);
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
}

impl ConfigScreen {
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

/// The detail pane. `crumb` heads it with where it is, for the one-pane
/// layout, where the section list is not on screen; a nested level always
/// has one.
fn detail(app: &App, f: &mut Frame, area: Rect, ui: &Ui, crumb: bool) {
    let screen = &app.config;
    let section = screen.current();
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
        View::List { field, .. } => {
            Some(format!("{} {} {field}", section.title, ui.glyphs.separator))
        }
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
    let rows: Vec<(String, Shown, bool)> = items
        .iter()
        .map(|item| describe(screen, item, &app.env, ui))
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
    let label_width = rows
        .iter()
        .map(|(label, _, _)| width(label))
        .chain(new_row.map(|(label, _)| width(label)))
        .max()
        .map_or(0, |w| if w == 0 { 0 } else { (w + 2).min(MAX_LABEL) });

    let help_text = if focused {
        help(screen, items.get(cursor))
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
    for (i, (label, value, choice)) in rows.into_iter().enumerate().skip(offset).take(height) {
        let item = &items[i];
        let row = Row {
            focused: focused && i == cursor && new_row.is_none(),
            choice,
            flash: app
                .flash
                .as_deref()
                .is_some_and(|key| Some(key) == flash_key(item)),
            input: input_on(item),
            ..Row::new(&label, value)
        };
        let y = top + (i - offset) as u16;
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

fn footer(screen: &ConfigScreen, f: &mut Frame, rect: Rect, ui: &Ui) {
    if let Some(editing) = &screen.editing {
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
        return frame::footer(
            f,
            rect,
            ui,
            &[("enter", "keep"), ("esc", "cancel"), ("ctrl+u", "clear")],
        );
    }
    let hints: Vec<(&str, &str)> = match screen.focus {
        Focus::Sections => vec![
            ("j/k", "move"),
            ("tab", "pane"),
            ("enter", "open"),
            ("w", "save"),
            ("e", "$EDITOR"),
            ("/", "filter"),
            ("?", "help"),
        ],
        Focus::Detail => {
            let items = screen.items();
            let item = items.get(screen.view().cursor());
            let change = match item {
                Some(Item::Field { field, .. }) => match field.kind {
                    Kind::Bool => ("space", "toggle"),
                    Kind::Choice(_) | Kind::MaybeChoice { .. } | Kind::MaybeBool { .. } => {
                        ("h/l", "change")
                    }
                    Kind::Int { .. } | Kind::Float { .. } => ("h/l", "change"),
                    Kind::List { .. } => ("enter", "open"),
                    Kind::Text { .. } | Kind::Secret => ("enter", "edit"),
                },
                Some(Item::Entry(_)) => ("enter", "open"),
                _ => ("enter", "edit"),
            };
            let lists = matches!(screen.view(), View::List { .. })
                || matches!(
                    (screen.view(), screen.current().body),
                    (View::Section { .. }, Body::Entries(_) | Body::Map(_))
                );
            if lists {
                vec![
                    ("j/k", "move"),
                    ("a", "add"),
                    change,
                    ("d", "delete"),
                    ("esc", "back"),
                    ("w", "save"),
                    ("?", "help"),
                ]
            } else {
                vec![
                    ("j/k", "move"),
                    ("tab", "pane"),
                    change,
                    ("w", "save"),
                    ("e", "$EDITOR"),
                    ("/", "filter"),
                    ("?", "help"),
                ]
            }
        }
    };
    frame::footer(f, rect, ui, &hints);
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::tui::app::{Dialog, Effect};
    use crate::tui::testing::{app, app_on, chord, mockup, paths, press, type_text, Fixture};

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
        let toml = "[general]\nbackend = \"deepgram\"\n[deepgram]\napi_key = \"dg_1\"\n";
        let mut fixture = app(toml);
        open(&mut fixture, "Backend & keys");
        to_row(&mut fixture, "deepgram api_key");
        press(&mut fixture.app, "x");
        open(&mut fixture, "General");
        to_row(&mut fixture, "notifications");
        press(&mut fixture.app, " ");

        let paths = paths(fixture.dir.path());
        let why = match save(&fixture.app.config, &paths, false) {
            Err(SaveError::Failed(why)) => why,
            Err(other) => panic!("refused for another reason: {other:?}"),
            Ok(_) => panic!("saved an invalid config"),
        };
        assert!(why.contains("Deepgram backend selected"), "{why}");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), toml);
        fixture
            .app
            .update(crate::tui::app::Msg::Saved(Err(SaveError::Failed(why))));
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert!(status.text.starts_with("not saved: "), "{}", status.text);
        assert!(fixture.app.config.form.is_dirty());
        assert!(!config(&fixture).general.notify);
        assert_eq!(config(&fixture).deepgram.as_ref().unwrap().api_key, "");
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
        let out = std::fs::read_to_string(&paths.config).unwrap();
        assert!(out.contains("vocabulary = []"), "{out}");
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
        assert!(matches!(refused, Err(SaveError::Changed)));
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), changed);
        fixture.app.update(crate::tui::app::Msg::Saved(refused));
        assert_eq!(fixture.app.dialog, Some(Dialog::Changed));
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
