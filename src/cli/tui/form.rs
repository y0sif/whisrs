//! The Config screen's edit buffer: the config as a `toml::Table`, read and
//! written by dotted path (`overlay.colors.recording`, `agents.0.harness`).
//!
//! An edit sets one path, and the table is deserialized back into a
//! [`Config`]: a value of the wrong kind is refused there, with every other
//! edit kept. The table is then serialized again from that `Config`, so it
//! always describes a config that loads. A section the file does not have
//! stays out of the table until a value in it changes.

use toml::{Table, Value};
use whisrs::Config;

/// Every section, empty: what each key is when the file does not set it.
/// An entry of each array gives the defaults of an entry's fields.
const DEFAULTS: &str = whisrs::config::write::SECTION_DEFAULTS;

pub struct Form {
    /// The config being edited.
    table: Table,
    /// The config as loaded, for the unsaved-changes check.
    baseline: Table,
    /// [`DEFAULTS`], serialized.
    defaults: Table,
    /// `table`, deserialized.
    config: Config,
}

impl Form {
    pub fn new(config: Config) -> Result<Form, String> {
        let table = to_table(&config)?;
        let defaults = toml::from_str::<Config>(DEFAULTS)
            .map_err(|e| e.to_string())
            .and_then(|config| to_table(&config))?;
        Ok(Form {
            baseline: table.clone(),
            table,
            defaults,
            config,
        })
    }

    /// The config as edited.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether anything changed since the load.
    pub fn is_dirty(&self) -> bool {
        self.table != self.baseline
    }

    /// What is edited now is what is on disk: a save that could not be read
    /// back still wrote it.
    pub fn mark_saved(&mut self) {
        self.baseline = self.table.clone();
    }

    /// Drop every edit.
    pub fn discard(&mut self) {
        self.table = self.baseline.clone();
        self.config = self
            .table
            .clone()
            .try_into()
            .expect("the baseline was serialized from a config");
    }

    /// The value at `path` in the config as last loaded or saved, unsaved
    /// edits left out: what the daemon runs on.
    pub fn saved(&self, path: &str) -> Option<&Value> {
        lookup(&self.baseline, path)
    }

    /// The value at `path` in the config, if it has one.
    pub fn value(&self, path: &str) -> Option<&Value> {
        lookup(&self.table, path)
    }

    /// The value at `path`, or the default when the config has none. An
    /// entry's fields default as any entry's do.
    pub fn effective(&self, path: &str) -> Option<&Value> {
        self.value(path).or_else(|| self.defaults_at(path))
    }

    /// The default at `path`: [`DEFAULTS`]' first entry stands for every
    /// entry of an array.
    fn defaults_at(&self, path: &str) -> Option<&Value> {
        let any_entry: Vec<&str> = path
            .split('.')
            .map(|seg| {
                if seg.parse::<usize>().is_ok() {
                    "0"
                } else {
                    seg
                }
            })
            .collect();
        lookup(&self.defaults, &any_entry.join("."))
    }

    /// Set `path` to `value`, or remove it for `None`. `Ok(true)` when the
    /// config changed; a value the config refuses is an `Err` and changes
    /// nothing. Setting a value a key already has is no change, so an absent
    /// section stays absent.
    pub fn set(&mut self, path: &str, value: Option<Value>) -> Result<bool, String> {
        if value.as_ref() == self.effective(path) && (value.is_some() || self.value(path).is_none())
        {
            return Ok(false);
        }
        let mut table = self.table.clone();
        put(&mut table, path, value)?;
        self.replace(table)
    }

    /// Put `path` back to its default.
    pub fn reset(&mut self, path: &str) -> Result<bool, String> {
        let default = self.defaults_at(path).cloned();
        if default.is_some() && self.value(path).is_none() {
            return Ok(false);
        }
        self.set(path, default)
    }

    /// What is edited at `path` is what is on disk now: that key alone was
    /// written. The other edits stay unsaved.
    pub fn settle(&mut self, path: &str) {
        let value = lookup(&self.table, path).cloned();
        let _ = put(&mut self.baseline, path, value);
    }

    /// Whether `path` changed since the load.
    pub fn is_dirty_at(&self, path: &str) -> bool {
        lookup(&self.table, path) != lookup(&self.baseline, path)
    }

    /// The entries of the array of tables at `path`.
    pub fn entries(&self, path: &str) -> &[Value] {
        self.value(path)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Append `entry` to the array of tables at `path`; its index.
    pub fn push_entry(&mut self, path: &str, entry: Table) -> Result<usize, String> {
        let mut entries = self.entries(path).to_vec();
        entries.push(Value::Table(entry));
        let index = entries.len() - 1;
        let mut table = self.table.clone();
        put(&mut table, path, Some(Value::Array(entries)))?;
        self.replace(table)?;
        Ok(index)
    }

    /// Remove entry `index` of the array of tables at `path`.
    pub fn remove_entry(&mut self, path: &str, index: usize) -> Result<(), String> {
        let mut entries = self.entries(path).to_vec();
        if index >= entries.len() {
            return Ok(());
        }
        entries.remove(index);
        let value = (!entries.is_empty()).then_some(Value::Array(entries));
        let mut table = self.table.clone();
        put(&mut table, path, value)?;
        self.replace(table).map(|_| ())
    }

    /// Take `table` if it deserializes, normalized through the config.
    fn replace(&mut self, table: Table) -> Result<bool, String> {
        let config: Config = table
            .try_into()
            .map_err(|e: toml::de::Error| e.message().to_string())?;
        let mut table = to_table(&config)?;
        // A section the file did not have, back at its defaults, leaves
        // again: a value changed there and back is no change.
        table.retain(|key, value| {
            self.baseline.contains_key(key) || self.defaults.get(key) != Some(value)
        });
        let config: Config = table
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| e.message().to_string())?;
        let changed = table != self.table;
        self.table = table;
        self.config = config;
        Ok(changed)
    }
}

fn to_table(config: &Config) -> Result<Table, String> {
    match Value::try_from(config) {
        Ok(Value::Table(table)) => Ok(table),
        Ok(_) => Err("the config did not serialize to a table".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The value at a dotted path. A numeric segment indexes an array.
pub fn lookup<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let mut current = table.get(segments.next()?)?;
    for seg in segments {
        current = match current {
            Value::Table(t) => t.get(seg)?,
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Set or remove the value at a dotted path, creating the tables on the way.
/// An array index must exist.
fn put(table: &mut Table, path: &str, value: Option<Value>) -> Result<(), String> {
    let segments: Vec<&str> = path.split('.').collect();
    put_in_table(table, &segments, value).map_err(|why| format!("{path}: {why}"))
}

fn put_in_table(table: &mut Table, segments: &[&str], value: Option<Value>) -> Result<(), String> {
    let Some((first, rest)) = segments.split_first() else {
        return Err("empty path".to_string());
    };
    if rest.is_empty() {
        match value {
            Some(value) => table.insert(first.to_string(), value),
            None => table.remove(*first),
        };
        return Ok(());
    }
    let next = table
        .entry(first.to_string())
        .or_insert_with(|| Value::Table(Table::new()));
    put_in_value(next, rest, value)
}

fn put_in_value(target: &mut Value, segments: &[&str], value: Option<Value>) -> Result<(), String> {
    let items = match target {
        Value::Table(table) => return put_in_table(table, segments, value),
        Value::Array(items) => items,
        _ => return Err("not a table".to_string()),
    };
    let Some((first, rest)) = segments.split_first() else {
        return Err("empty path".to_string());
    };
    let index: usize = first
        .parse()
        .map_err(|_| format!("{first} is not an entry index"))?;
    if index >= items.len() {
        return Err(format!("there is no entry {index}"));
    }
    if rest.is_empty() {
        match value {
            Some(value) => items[index] = value,
            None => {
                items.remove(index);
            }
        }
        return Ok(());
    }
    put_in_value(&mut items[index], rest, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(toml: &str) -> Form {
        Form::new(toml::from_str(toml).expect("fixture parses")).expect("form")
    }

    #[test]
    fn the_defaults_parse() {
        let form = form("");
        assert_eq!(
            form.effective("deepgram.model"),
            Some(&Value::from("nova-3"))
        );
        assert_eq!(
            form.effective("agents.4.harness"),
            Some(&Value::from("claude"))
        );
        assert_eq!(form.effective("overlay.colors.recording"), None);
    }

    #[test]
    fn an_edit_is_kept_and_marks_the_form_dirty() {
        let mut form = form("[general]\nsilence_timeout_ms = 2000\n");
        assert!(!form.is_dirty());
        assert_eq!(
            form.set("general.silence_timeout_ms", Some(Value::Integer(500))),
            Ok(true)
        );
        assert!(form.is_dirty());
        assert_eq!(form.config().general.silence_timeout_ms, 500);
        // Setting it back is clean again.
        form.set("general.silence_timeout_ms", Some(Value::Integer(2000)))
            .unwrap();
        assert!(!form.is_dirty());
    }

    /// A value of the wrong kind is refused, and the form keeps every
    /// other edit.
    #[test]
    fn a_refused_value_changes_nothing() {
        let mut form = form("");
        form.set("general.language", Some(Value::from("de")))
            .unwrap();
        let refused = form.set("general.silence_timeout_ms", Some(Value::Integer(-5)));
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(form.config().general.language, "de");
        assert_eq!(form.config().general.silence_timeout_ms, 2000);
        assert!(form
            .set("input.backend", Some(Value::from("telepathy")))
            .is_err());
    }

    /// An absent section stays absent unless a value in it changed.
    #[test]
    fn an_absent_section_appears_only_when_changed() {
        let mut form = form("");
        assert_eq!(
            form.set("deepgram.model", Some(Value::from("nova-3"))),
            Ok(false)
        );
        assert!(form.value("deepgram").is_none());
        assert_eq!(form.set("tts.voice", None), Ok(false));
        assert!(form.value("tts").is_none());
        assert!(form.reset("recording.summary").is_ok());
        assert!(form.value("recording").is_none());
        // There and back is no change either.
        form.set("hotkeys.toggle", Some(Value::from("Alt+D")))
            .unwrap();
        assert!(form.is_dirty());
        form.set("hotkeys.toggle", None).unwrap();
        assert!(form.value("hotkeys").is_none());
        form.set("overlay.theme", Some(Value::from("ember")))
            .unwrap();
        form.set("overlay.theme", Some(Value::from("carbon")))
            .unwrap();
        assert!(form.value("overlay").is_none());
        assert!(!form.is_dirty());

        assert_eq!(
            form.set("deepgram.model", Some(Value::from("nova-2"))),
            Ok(true)
        );
        assert!(form.config().deepgram.is_some());
        assert_eq!(
            form.set("overlay.colors.ring", Some(Value::from("#fff"))),
            Ok(true)
        );
        assert_eq!(
            form.config()
                .overlay
                .as_ref()
                .and_then(|o| o.colors.as_ref())
                .and_then(|c| c.ring.as_deref()),
            Some("#fff")
        );
    }

    #[test]
    fn an_optional_value_is_removed_and_a_plain_one_reset() {
        let mut form = form("[general]\nprompt = \"p\"\nlanguage = \"de\"\n");
        assert_eq!(form.set("general.prompt", None), Ok(true));
        assert_eq!(form.config().general.prompt, None);
        assert_eq!(form.reset("general.language"), Ok(true));
        assert_eq!(form.config().general.language, "en");
    }

    #[test]
    fn entries_are_added_edited_and_removed() {
        let mut form = form("");
        assert!(form.entries("agents").is_empty());
        let mut entry = Table::new();
        entry.insert("name".into(), Value::from("jarvis"));
        assert_eq!(form.push_entry("agents", entry), Ok(0));
        assert_eq!(form.config().agents[0].harness, "claude");
        assert_eq!(
            form.set("agents.0.harness", Some(Value::from("codex"))),
            Ok(true)
        );
        assert_eq!(form.config().agents[0].harness, "codex");
        form.remove_entry("agents", 0).unwrap();
        assert!(form.config().agents.is_empty());
        assert!(form.value("agents").is_none());
        assert!(!form.is_dirty());

        // An entry missing a required key is refused whole.
        assert!(form.push_entry("llm_commands", Table::new()).is_err());
        assert!(form.entries("llm_commands").is_empty());
    }

    #[test]
    fn discard_drops_every_edit() {
        let mut form = form("[general]\nlanguage = \"de\"\n");
        form.set("general.language", Some(Value::from("fr")))
            .unwrap();
        form.set("tts.enabled", Some(Value::Boolean(true))).unwrap();
        form.discard();
        assert!(!form.is_dirty());
        assert_eq!(form.config().general.language, "de");
        assert!(form.config().tts.is_none());
    }
}
