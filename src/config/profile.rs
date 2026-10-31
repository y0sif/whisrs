//! Config profiles (#189): a profile is a partial `config.toml` in
//! `profiles/<name>.toml` beside it, holding only what differs. The daemon
//! merges the active one over the base config at startup; the active name is
//! one line in `$XDG_STATE_HOME/whisrs/active-profile` (no file, or `base`:
//! the base config alone).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use toml::{Table, Value};

use super::types::{
    config_path, unknown_config_keys, unknown_config_keys_with_paths, unknown_keys_warning,
    with_position, Config,
};

/// The base config's name: `config.toml` alone.
pub const BASE: &str = "base";

/// Names no profile can have: `whisrs profile` takes them as its own words.
pub const RESERVED: [&str; 3] = ["base", "list", "next"];

/// The longest profile name.
pub const NAME_MAX: usize = 32;

/// What stays in `config.toml`: a profile that sets one has it dropped, with
/// a warning naming it. A dotted path covers everything under it; `*` is any
/// one section but `[replacements]`, whose keys are words. Hotkeys stay so a
/// profile can never drop the key that switches out of it; `[recording] dir`
/// stays so every reader of the recordings folder finds the same one.
pub const BASE_ONLY: [&str; 5] = [
    "hotkeys",
    "llm_commands",
    "agents",
    "*.api_key",
    "recording.dir",
];

/// Why a key in [`BASE_ONLY`] is ignored in a profile.
pub const BASE_ONLY_NOTE: &str =
    "api keys, hotkeys, [[llm_commands]], [[agents]] and [recording] dir stay in config.toml";

/// What `next` answers with no profile to switch to.
pub const NO_PROFILES: &str =
    "no profiles to switch to: add one as profiles/<name>.toml next to config.toml";

/// What a client says to a switch while whisrsd restarts ([`unanswered`]).
pub const RESTARTING: &str = "whisrsd is restarting; try again in a moment";

/// How long a client waits for a restarting whisrsd to answer again.
pub const RESTART_WAIT: Duration = Duration::from_secs(3);

/// How recent a write of the active name has to be for a whisrsd that does
/// not answer to count as restarting on it.
const RECENT_SWITCH: Duration = Duration::from_secs(5);

/// The schema's serde aliases, as (the table they are in, alias, the field's
/// own name). Both sides of a merge are renamed, so a merge never holds a
/// field twice; a writer into a profile's file keeps the spelling it has.
pub const ALIASES: [(&[&str], &str, &str); 4] = [
    (&[], "local", "local-whisper"),
    (&[], "asr", "asr-sidecar"),
    (&[], "vibevoice", "asr-sidecar"),
    (&["hotkeys"], "read", "speak"),
];

/// Whether `name` can name a profile: `[a-z0-9_-]{1,32}`, not reserved. The
/// error says why not.
pub fn check_name(name: &str) -> Result<(), String> {
    if RESERVED.contains(&name) {
        return Err(format!("{name} is a reserved word, not a profile name"));
    }
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
    if name.is_empty() || name.len() > NAME_MAX || !name.bytes().all(allowed) {
        return Err(format!(
            "'{name}' is not a profile name: use 1 to {NAME_MAX} of a-z, 0-9, - and _"
        ));
    }
    Ok(())
}

/// Whether the dotted `path` (`deepgram.api_key`, `hotkeys.toggle`,
/// `agents.0.harness`) stays in config.toml ([`BASE_ONLY`]).
pub fn is_base_only(path: &str) -> bool {
    base_only(&path.split('.').collect::<Vec<_>>())
}

fn base_only<S: AsRef<str>>(path: &[S]) -> bool {
    BASE_ONLY
        .iter()
        .any(|pattern| matches_pattern(pattern, path, |path, pattern| path >= pattern))
}

/// Whether a key below `path` is base-only: `path` is a strict prefix of a
/// [`BASE_ONLY`] pattern (any section but `[replacements]`).
fn leads_to_base_only<S: AsRef<str>>(path: &[S]) -> bool {
    BASE_ONLY
        .iter()
        .any(|pattern| matches_pattern(pattern, path, |path, pattern| path < pattern))
}

/// Whether `path` and `pattern` agree key by key as far as both go, with
/// `lengths` (path's, pattern's) saying how far that has to be.
fn matches_pattern<S: AsRef<str>>(
    pattern: &str,
    path: &[S],
    lengths: impl Fn(usize, usize) -> bool,
) -> bool {
    let pattern: Vec<&str> = pattern.split('.').collect();
    lengths(path.len(), pattern.len())
        && pattern.iter().zip(path).all(|(want, key)| {
            let key = key.as_ref();
            *want == key || (*want == "*" && key != "replacements")
        })
}

/// Whether a profile's `value` at the dotted `path` is dropped by a merge, so
/// a view of the file names the key and never shows the value: a base-only
/// key ([`is_base_only`]), or anything but a table where a base-only key sits
/// below. An array or a string at `[groq]` would fill the section's fields
/// in order, `api_key` with them, and its value would reach serde's errors.
pub fn is_dropped(path: &str, value: &Value) -> bool {
    dropped_at(&path.split('.').collect::<Vec<_>>(), value)
}

fn dropped_at<S: AsRef<str>>(path: &[S], value: &Value) -> bool {
    base_only(path) || (!value.is_table() && leads_to_base_only(path))
}

/// How a profile's file is named in messages: `profiles/offline.toml`.
pub fn label(name: &str) -> String {
    format!("profiles/{name}.toml")
}

/// The one line every client shows for a profile that did not load.
pub fn not_loaded(name: &str, why: &str) -> String {
    format!("profile {name} did not load, using base: {why}")
}

/// `<config dir>/whisrs/profiles`.
pub fn profiles_dir() -> PathBuf {
    config_path().with_file_name("profiles")
}

/// `$XDG_STATE_HOME/whisrs/active-profile`.
pub fn active_profile_path() -> PathBuf {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("~/.local/state"))
        .join("whisrs")
        .join("active-profile")
}

/// Where the base config, the profiles and the active name live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub profiles: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    /// This user's: [`config_path`], [`profiles_dir`], [`active_profile_path`].
    pub fn current() -> Self {
        Self {
            config: config_path(),
            profiles: profiles_dir(),
            state: active_profile_path(),
        }
    }

    /// All three under `root`: `config.toml`, `profiles/` and
    /// `state/active-profile`.
    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config.toml"),
            profiles: root.join("profiles"),
            state: root.join("state").join("active-profile"),
        }
    }

    /// The file of profile `name`.
    pub fn profile(&self, name: &str) -> PathBuf {
        self.profiles.join(format!("{name}.toml"))
    }
}

/// The profiles in a folder: the names, sorted, and the files skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub names: Vec<String>,
    pub skipped: Vec<Skipped>,
}

/// A file in `profiles/` that is not a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Its file name.
    pub file: String,
    pub why: String,
}

impl Skipped {
    /// The warning that names it.
    pub fn warning(&self) -> String {
        format!("skipped profiles/{}: {}", self.file, self.why)
    }
}

/// The profiles in `dir`. A missing folder has none. A file that is not
/// `.toml`, or whose stem is not a profile name, is skipped.
pub fn list(dir: &Path) -> io::Result<Listing> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(e) => return Err(e),
    };
    let mut listing = Listing::default();
    for entry in entries {
        let path = entry?.path();
        let file = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let skip = |why: String| Skipped {
            file: file.clone(),
            why,
        };
        if path.extension().and_then(|e| e.to_str()) != Some("toml") || !path.is_file() {
            listing.skipped.push(skip("not a .toml file".to_string()));
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match check_name(&stem) {
            Ok(()) => listing.names.push(stem),
            Err(why) => listing.skipped.push(skip(why)),
        }
    }
    listing.names.sort();
    listing.skipped.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(listing)
}

/// The active profile the state file names: `None` is base (no file, an
/// empty one, or `base`). A name that breaks the rule is an error.
pub fn read_active(state: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(state) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", state.display())),
    };
    let name = text.trim();
    if name.is_empty() || name == BASE {
        return Ok(None);
    }
    check_name(name).map_err(|why| format!("{} names no profile: {why}", state.display()))?;
    Ok(Some(name.to_string()))
}

/// Write profile `name`'s file whole at `paths`: atomically, 0600, the
/// profiles folder made first. The text is not checked here.
pub fn write_profile(paths: &Paths, name: &str, text: &str) -> io::Result<()> {
    check_name(name).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    std::fs::create_dir_all(&paths.profiles)?;
    super::write::atomic_write(&paths.profile(name), text)
}

/// Make `name` (`None`: base) the active profile: one line, written
/// atomically, its folder made first.
pub fn set_active(state: &Path, name: Option<&str>) -> io::Result<()> {
    if let Some(name) = name {
        check_name(name).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    }
    if let Some(dir) = state.parent() {
        std::fs::create_dir_all(dir)?;
    }
    super::write::atomic_write(state, &format!("{}\n", name.unwrap_or(BASE)))
}

/// The profile after `current` (`None`: base) in the order base, then
/// `names` (sorted), wrapping. A current name no longer listed is followed by
/// the next one after it.
pub fn next(names: &[String], current: Option<&str>) -> Option<String> {
    match current {
        None => names.first().cloned(),
        Some(current) => names.iter().find(|n| n.as_str() > current).cloned(),
    }
}

/// What `next` switches to from `current` (`None`: base): the first target
/// after it, in the order base, then `names`, wrapping, that
/// [`switch_check`] passes with `check`, and the ones skipped on the way,
/// which did not. With none passing before the order comes back to
/// `current`, the refusal names each with a short why.
pub fn next_usable(
    paths: &Paths,
    names: &[String],
    current: Option<&str>,
    check: Check,
) -> Result<(Option<String>, Vec<String>), String> {
    if names.is_empty() {
        return Err(NO_PROFILES.to_string());
    }
    let mut skipped = Vec::new();
    let mut whys = Vec::new();
    let mut target = next(names, current);
    // Each target once: base and every name, `current` too when it is not
    // listed (its file is gone).
    for _ in 0..=names.len() {
        if target.as_deref() == current {
            break;
        }
        let shown = target.as_deref().unwrap_or(BASE).to_string();
        match switch_check(paths, target.as_deref(), check) {
            Ok(()) => return Ok((target, skipped)),
            Err(refusal) => {
                whys.push(format!("{shown} ({})", short_why(&shown, &refusal)));
                skipped.push(shown);
            }
        }
        target = next(names, target.as_deref());
    }
    Err(format!("no other profile loads: {}", whys.join(", ")))
}

/// A [`switch_check`] refusal of `shown`, short: its first line, without
/// the profile's name, file or the position in it.
fn short_why(shown: &str, refusal: &str) -> String {
    let why = refusal
        .strip_prefix(&format!("cannot switch to {shown}: "))
        .unwrap_or(refusal);
    let why = why.lines().next().unwrap_or(why);
    let label = label(shown);
    let why = match why.strip_prefix(&format!("the config with {label} is not valid: ")) {
        Some(why) => why.strip_prefix("configuration error: ").unwrap_or(why),
        None => why.strip_prefix(&format!("{label} ")).unwrap_or(why),
    };
    let why = why.strip_prefix("is ").unwrap_or(why);
    let why = why.find(" (line ").map_or(why, |at| &why[..at]);
    why.trim_end_matches('.').to_string()
}

/// What a switch says it does: `switching to clean`, and the profiles
/// `next` skipped on the way ([`skipped_note`]).
pub fn switching_line(shown: &str, skipped: &[String]) -> String {
    format!("switching to {shown}{}", skipped_note(skipped))
}

/// ` (skipped broken, cloud: they do not load)`, or nothing when `next`
/// skipped none.
pub fn skipped_note(skipped: &[String]) -> String {
    match skipped {
        [] => String::new(),
        [one] => format!(" (skipped {one}: it does not load)"),
        many => format!(" (skipped {}: they do not load)", many.join(", ")),
    }
}

/// What a switch asked for `asked` goes to (`None`: base), given the
/// profiles there are and the one in use: `next` (or nothing), `base`, or a
/// listed name.
pub fn resolve_target(
    names: &[String],
    current: Option<&str>,
    asked: Option<&str>,
) -> Result<Option<String>, String> {
    match asked.map(str::trim) {
        None | Some("next") if names.is_empty() => Err(NO_PROFILES.to_string()),
        None | Some("next") => Ok(next(names, current)),
        Some(BASE) => Ok(None),
        Some(name) => {
            check_name(name)?;
            if names.iter().any(|n| n == name) {
                Ok(Some(name.to_string()))
            } else {
                Err(format!(
                    "no profile named {name}: {} does not exist",
                    label(name)
                ))
            }
        }
    }
}

/// `text` as a table, the schema's aliases renamed to their fields' names.
/// A table that spells a field both ways keeps both, for serde to refuse.
pub fn read_table(text: &str) -> Result<Table, toml::de::Error> {
    let mut table = text.parse::<Table>()?;
    rename_aliases(&mut table);
    Ok(table)
}

fn rename_aliases(table: &mut Table) {
    for (at, alias, name) in ALIASES {
        let mut inner = Some(&mut *table);
        for key in at {
            inner = match inner.and_then(|t| t.get_mut(*key)) {
                Some(Value::Table(t)) => Some(t),
                _ => None,
            };
        }
        let Some(inner) = inner else { continue };
        if inner.contains_key(name) {
            continue;
        }
        if let Some(value) = inner.remove(alias) {
            inner.insert(name.to_string(), value);
        }
    }
}

/// A profile over a base config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Merged {
    /// The base with the profile merged in.
    pub table: Table,
    /// The dotted paths the profile sets (a leaf, or an array it replaces).
    pub set: BTreeSet<String>,
    /// The [`BASE_ONLY`] keys the profile set, dropped.
    pub dropped: Vec<String>,
}

/// Merge `profile` over `base` (both from [`read_table`]): what stays in
/// base is dropped from the profile first; tables merge key by key, and any
/// other value, an array too, replaces the base value.
pub fn merge(base: &Table, profile: &Table) -> Merged {
    let mut over = profile.clone();
    let mut dropped = Vec::new();
    drop_base_only(&mut over, &mut Vec::new(), &mut dropped);
    let mut table = base.clone();
    let mut set = BTreeSet::new();
    merge_into(&mut table, &over, &mut Vec::new(), &mut set);
    Merged {
        table,
        set,
        dropped,
    }
}

fn drop_base_only(table: &mut Table, at: &mut Vec<String>, dropped: &mut Vec<String>) {
    let keys: Vec<String> = table.keys().cloned().collect();
    for key in keys {
        at.push(key.clone());
        if table.get(&key).is_some_and(|value| dropped_at(at, value)) {
            table.remove(&key);
            dropped.push(at.join("."));
        } else if let Some(Value::Table(inner)) = table.get_mut(&key) {
            drop_base_only(inner, at, dropped);
        }
        at.pop();
    }
}

fn merge_into(base: &mut Table, over: &Table, at: &mut Vec<String>, set: &mut BTreeSet<String>) {
    for (key, value) in over {
        at.push(key.clone());
        match (base.get_mut(key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge_into(base, over, at, set),
            _ => {
                leaves(value, at, set);
                base.insert(key.clone(), value.clone());
            }
        }
        at.pop();
    }
}

fn leaves(value: &Value, at: &mut Vec<String>, set: &mut BTreeSet<String>) {
    match value {
        Value::Table(table) => {
            for (key, value) in table {
                at.push(key.clone());
                leaves(value, at, set);
                at.pop();
            }
        }
        _ => {
            set.insert(at.join("."));
        }
    }
}

/// Whether the merged-in profile `over` set the key at `path`: it has a
/// value on the way there that replaced the base's, or the key itself.
fn profile_sets(over: &Table, path: &[String]) -> bool {
    let mut table = over;
    for key in path {
        match table.get(key) {
            None => return false,
            Some(Value::Table(inner)) => table = inner,
            Some(_) => return true,
        }
    }
    true
}

/// The config a profile makes over the base, and what the merge found.
#[derive(Debug, Clone)]
pub struct Effective {
    /// What to run on: the merge, or the base when the profile did not load
    /// ([`Self::error`]).
    pub config: Config,
    /// The table `config` came from, aliases renamed.
    pub table: Table,
    /// The dotted paths the profile sets ([`Merged::set`]); empty when its
    /// file did not parse.
    pub set: BTreeSet<String>,
    /// The base-only keys the profile set, dropped ([`Merged::dropped`]).
    pub dropped: Vec<String>,
    /// Unknown keys that came from config.toml.
    pub unknown_base: Vec<String>,
    /// Unknown keys that came from the profile.
    pub unknown_profile: Vec<String>,
    /// One per dropped key, then the profile's unknown keys.
    pub warnings: Vec<String>,
    /// Why the profile is not applied, if it is not: its file is missing or
    /// unreadable, it is not valid TOML, the merge does not deserialize, or
    /// it does not validate. `config` is then the base.
    pub error: Option<String>,
}

impl Effective {
    /// Whether the profile is applied.
    pub fn applied(&self) -> bool {
        self.error.is_none()
    }
}

/// Who checks a profile, and so how a missing API key counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The daemon, which decides: a cloud backend with no API key in
    /// config.toml or its environment fails ([`Config::validate`]).
    Daemon,
    /// A client (the CLI, the TUI), which only predicts: such a key can live
    /// in the daemon's environment alone (a unit's `Environment=`), so it is
    /// no failure here ([`Config::validate_to_write`]). The daemon's own
    /// dry run still decides the switch.
    Client,
}

impl Check {
    fn validate(self, config: &Config) -> Result<(), String> {
        match self {
            Check::Daemon => config.validate(),
            Check::Client => config.validate_to_write(),
        }
        .map(drop)
        .map_err(|e| e.to_string())
    }
}

/// The config profile `name` (its file's text, `None` when the file is
/// missing) makes over `base_text`, with the merge's findings, as the daemon
/// checks it ([`Check::Daemon`]). Every way the profile can fail falls back
/// to the base, with [`Effective::error`] set. `Err` only when the base
/// itself does not load.
pub fn effective(
    base_text: &str,
    name: &str,
    profile_text: Option<&str>,
) -> Result<Effective, String> {
    effective_checked(base_text, name, profile_text, Check::Daemon)
}

/// [`effective`], checked by `check`: a client passes [`Check::Client`].
pub fn effective_checked(
    base_text: &str,
    name: &str,
    profile_text: Option<&str>,
    check: Check,
) -> Result<Effective, String> {
    let (base, base_config) = load_base(base_text)?;
    let label = label(name);
    let fallback = |merged: Option<Merged>, error: String| {
        let (set, dropped) = merged.map(|m| (m.set, m.dropped)).unwrap_or_default();
        Effective {
            config: base_config.clone(),
            table: base.clone(),
            warnings: dropped_warnings(&label, &dropped),
            set,
            dropped,
            unknown_base: unknown_config_keys(base_text),
            unknown_profile: Vec::new(),
            error: Some(error),
        }
    };

    let Some(text) = profile_text else {
        return Ok(fallback(None, format!("{label} does not exist")));
    };
    let profile = match read_table(text) {
        Ok(profile) => profile,
        Err(e) => {
            let why = with_position("is not valid TOML", text, e.span());
            return Ok(fallback(None, format!("{label} {why}")));
        }
    };
    let merged = merge(&base, &profile);
    let config = match to_config(&merged.table) {
        Ok(config) => config,
        Err(e) => {
            return Ok(fallback(
                Some(merged),
                format!("{label} does not load over config.toml: {e}"),
            ))
        }
    };
    // The base's own error is not the profile's: the base would fail the
    // same way, so the profile still applies.
    if let Err(e) = check.validate(&config) {
        if check.validate(&base_config).err().as_ref() != Some(&e) {
            return Ok(fallback(
                Some(merged),
                format!("the config with {label} is not valid: {e}"),
            ));
        }
    }

    let mut over = profile;
    drop_base_only(&mut over, &mut Vec::new(), &mut Vec::new());
    let (mut unknown_base, mut unknown_profile) = (Vec::new(), Vec::new());
    if let Ok(text) = toml::to_string(&merged.table) {
        for (path, key) in unknown_config_keys_with_paths(&text) {
            if profile_sets(&over, &path) {
                unknown_profile.push(key);
            } else {
                unknown_base.push(key);
            }
        }
    }
    let mut warnings = dropped_warnings(&label, &merged.dropped);
    warnings.extend(unknown_keys_warning(Path::new(&label), &unknown_profile));
    Ok(Effective {
        config,
        table: merged.table,
        set: merged.set,
        dropped: merged.dropped,
        unknown_base,
        unknown_profile,
        warnings,
        error: None,
    })
}

/// [`effective`] over the files at `paths`: a missing config.toml is an
/// empty one; an unreadable profile falls back like a missing one. `Err`
/// when config.toml cannot be read or does not load.
pub fn effective_at(paths: &Paths, name: &str) -> Result<Effective, String> {
    effective_at_checked(paths, name, Check::Daemon)
}

/// [`effective_at`], checked by `check`: a client passes [`Check::Client`].
pub fn effective_at_checked(paths: &Paths, name: &str, check: Check) -> Result<Effective, String> {
    let base_text = read_base(paths)?;
    match std::fs::read_to_string(paths.profile(name)) {
        Ok(text) => effective_checked(&base_text, name, Some(&text), check),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            effective_checked(&base_text, name, None, check)
        }
        Err(e) => effective_checked(&base_text, name, None, check).map(|mut eff| {
            eff.error = Some(format!("{} cannot be read: {e}", label(name)));
            eff
        }),
    }
}

/// Whether profile `name` would load at `paths`, as the daemon checks it:
/// the switch's dry run. The error is the refusal.
pub fn dry_run(paths: &Paths, name: &str) -> Result<(), String> {
    switch_check(paths, Some(name), Check::Daemon)
}

/// Whether a switch to `target` (`None`: base) can go ahead at `paths`: the
/// profile loads over config.toml, and config.toml loads (a switch to base
/// on a broken one would run on the built-in defaults). The error is the
/// refusal. The daemon passes [`Check::Daemon`], a client [`Check::Client`].
pub fn switch_check(paths: &Paths, target: Option<&str>, check: Check) -> Result<(), String> {
    let shown = target.unwrap_or(BASE);
    let refuse = |why: String| format!("cannot switch to {shown}: {why}");
    let Some(name) = target else {
        return read_base(paths)
            .and_then(|text| load_base(&text))
            .map(drop)
            .map_err(refuse);
    };
    match effective_at_checked(paths, name, check) {
        Ok(eff) => eff.error.map_or(Ok(()), |why| Err(refuse(why))),
        Err(why) => Err(refuse(why)),
    }
}

/// config.toml's text at `paths`: a missing file is an empty one.
fn read_base(paths: &Paths) -> Result<String, String> {
    match std::fs::read_to_string(&paths.config) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("cannot read config.toml: {e}")),
    }
}

/// config.toml as a table (aliases renamed) and as a config, or why it does
/// not load.
fn load_base(base_text: &str) -> Result<(Table, Config), String> {
    let base = read_table(base_text).map_err(|e| {
        format!(
            "config.toml {}",
            with_position("is not valid TOML", base_text, e.span())
        )
    })?;
    let config = to_config(&base).map_err(|e| format!("config.toml does not load: {e}"))?;
    Ok((base, config))
}

/// `table` as a config. The error is serde's message alone (the text the
/// merge was serialized from is no file the user has), with every
/// base-only value hidden.
fn to_config(table: &Table) -> Result<Config, String> {
    Value::Table(table.clone())
        .try_into::<Config>()
        .map_err(|e| hide_base_only(e.message(), table))
}

/// `message` with each base-only string of `table` (an API key, a hotkey)
/// hidden, so no error quotes one. Strings shorter than 4 are left: they
/// would hide ordinary words.
fn hide_base_only(message: &str, table: &Table) -> String {
    let mut secrets = Vec::new();
    base_only_strings(table, &mut Vec::new(), &mut secrets);
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets
        .into_iter()
        .filter(|s| s.chars().count() >= 4)
        .fold(message.to_string(), |message, secret| {
            message.replace(secret, "***")
        })
}

fn base_only_strings<'a>(table: &'a Table, at: &mut Vec<String>, out: &mut Vec<&'a str>) {
    for (key, value) in table {
        at.push(key.clone());
        if base_only(at) {
            strings_in(value, out);
        } else if let Value::Table(inner) = value {
            base_only_strings(inner, at, out);
        }
        at.pop();
    }
}

fn strings_in<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| strings_in(v, out)),
        Value::Table(table) => table.values().for_each(|v| strings_in(v, out)),
        _ => {}
    }
}

fn dropped_warnings(label: &str, dropped: &[String]) -> Vec<String> {
    dropped
        .iter()
        .map(|key| {
            if is_base_only(key) {
                format!("{label}: {key} is ignored ({BASE_ONLY_NOTE})")
            } else {
                format!("{label}: {key} is ignored: a section has to be a table ({BASE_ONLY_NOTE})")
            }
        })
        .collect()
}

/// Why whisrsd did not answer a switch at `socket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswered {
    /// Not running: a switch is for its next start.
    Down,
    /// It was restarting and answers again: send the switch to it.
    Back,
    /// Still restarting after [`RESTART_WAIT`]: say [`RESTARTING`], and write
    /// nothing.
    Restarting,
}

/// What a client makes of a whisrsd that did not answer a switch at
/// `socket`. It looks to be restarting when its socket is set aside for a
/// re-exec (`service::socket_aside`), or the active name at `state` was
/// written in the last few seconds; it is then waited for, up to
/// [`RESTART_WAIT`]. Otherwise it is down. Blocks while it waits.
pub fn unanswered(socket: &Path, state: &Path) -> Unanswered {
    unanswered_within(socket, state, RECENT_SWITCH, RESTART_WAIT)
}

fn unanswered_within(socket: &Path, state: &Path, recent: Duration, wait: Duration) -> Unanswered {
    let aside = crate::service::socket_aside(socket).exists();
    let switched = std::fs::metadata(state)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|written| written.elapsed().ok())
        .is_some_and(|age| age < recent);
    if !aside && !switched {
        return Unanswered::Down;
    }
    let deadline = Instant::now() + wait;
    loop {
        let probe = crate::ipc::request_blocking_at(
            socket,
            &crate::ipc::Command::Status,
            Some(Duration::from_secs(1)),
        );
        if probe.is_ok() {
            return Unanswered::Back;
        }
        if Instant::now() >= deadline {
            return Unanswered::Restarting;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_TEXT: &str = "\
[general]
backend = \"groq\"
language = \"en\"
vocabulary = [\"whisrs\", \"Hyprland\"]

[groq]
api_key = \"gsk_base\"

[hotkeys]
toggle = \"Super+Shift+D\"

[recording]
dir = \"/base/recordings\"
max_duration_minutes = 90
";

    fn eff(profile: Option<&str>) -> Effective {
        effective(BASE_TEXT, "offline", profile).expect("the base loads")
    }

    #[test]
    fn names_follow_the_rule() {
        for good in ["offline", "a", "work-2", "my_laptop", &"x".repeat(32)] {
            assert_eq!(check_name(good), Ok(()), "{good}");
        }
        for bad in ["", "Offline", "with space", "dot.ted", "ü", &"x".repeat(33)] {
            let why = check_name(bad).expect_err(bad);
            assert!(why.contains("not a profile name"), "{bad}: {why}");
        }
        for reserved in RESERVED {
            let why = check_name(reserved).expect_err(reserved);
            assert!(why.contains("reserved"), "{reserved}: {why}");
        }
    }

    #[test]
    fn base_only_covers_its_list() {
        for path in [
            "hotkeys",
            "hotkeys.toggle",
            "hotkeys.next_profile",
            "llm_commands",
            "llm_commands.0.hotkey",
            "agents",
            "agents.0.harness",
            "deepgram.api_key",
            "llm.api_key",
            "tts.api_key",
            "recording.dir",
        ] {
            assert!(is_base_only(path), "{path}");
        }
        for path in [
            "general.backend",
            "deepgram.model",
            "recording.backend",
            "recording.directory",
            "replacements.api_key",
            "api_key",
        ] {
            assert!(!is_base_only(path), "{path}");
        }
    }

    /// Every `api_key` the schema has is base-only, so a new section's key
    /// cannot slip into a profile.
    #[test]
    fn every_api_key_in_the_schema_is_base_only() {
        // An api_key in every section: serde keeps the ones a section has.
        let mut text = String::new();
        for line in crate::config::write::SECTION_DEFAULTS.lines() {
            text.push_str(line);
            text.push('\n');
            if line.starts_with('[') && !line.starts_with("[[") {
                text.push_str("api_key = \"k\"\n");
            }
        }
        let config: Config = toml::from_str(&text).unwrap();
        let mut set = BTreeSet::new();
        leaves(
            &Value::try_from(&config).unwrap(),
            &mut Vec::new(),
            &mut set,
        );
        let keys: Vec<&String> = set.iter().filter(|p| p.ends_with(".api_key")).collect();
        assert!(keys.len() >= 7, "{keys:?}");
        for key in keys {
            assert!(is_base_only(key), "{key}");
        }
    }

    /// The renames name exactly the schema's aliases (pinned in types.rs by
    /// `every_serde_alias_names_an_optional_section`), and each alias reads
    /// as its field does.
    #[test]
    fn aliases_are_the_schemas() {
        let mut aliases: Vec<&str> = ALIASES.iter().map(|(_, alias, _)| *alias).collect();
        aliases.sort();
        assert_eq!(aliases, ["asr", "local", "read", "vibevoice"]);
        for (at, alias, name) in ALIASES {
            let section = |key: &str| match at {
                [] => format!("[{key}]\n"),
                [table] => format!("[{table}]\n{key} = \"x\"\n"),
                _ => unreachable!(),
            };
            let renamed = read_table(&section(alias)).unwrap();
            assert_eq!(renamed, read_table(&section(name)).unwrap(), "{alias}");
        }
    }

    #[test]
    fn tables_merge_key_by_key_and_other_values_replace() {
        let eff = eff(Some(
            "[general]\nlanguage = \"de\"\nvocabulary = [\"NixOS\"]\n\
             [local-whisper]\nmodel_path = \"/m/ggml-base.bin\"\n",
        ));
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(eff.config.general.backend, "groq", "kept from base");
        assert_eq!(eff.config.general.language, "de");
        assert_eq!(
            eff.config.general.vocabulary,
            ["NixOS"],
            "an array replaces"
        );
        assert_eq!(
            eff.config.local_whisper.as_ref().unwrap().model_path,
            "/m/ggml-base.bin"
        );
        assert_eq!(eff.config.groq.as_ref().unwrap().api_key, "gsk_base");
        assert_eq!(
            eff.set.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "general.language",
                "general.vocabulary",
                "local-whisper.model_path"
            ]
        );
        assert!(eff.dropped.is_empty() && eff.warnings.is_empty());
    }

    /// An alias on either side merges into the field itself: one section,
    /// never two.
    #[test]
    fn aliases_merge_on_both_sides() {
        let base = "[general]\nbackend = \"local-whisper\"\n[local]\nmodel_path = \"/base.bin\"\n";
        let eff = effective(
            base,
            "p",
            Some("[local-whisper]\nmodel_path = \"/p.bin\"\n"),
        )
        .unwrap();
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(eff.config.local_whisper.unwrap().model_path, "/p.bin");
        assert!(!eff.table.contains_key("local"));

        let base = "[general]\nbackend = \"asr-sidecar\"\n\
                    [asr-sidecar]\nurl = \"http://127.0.0.1:1\"\nmodel = \"a\"\n";
        let eff = effective(base, "p", Some("[vibevoice]\nmodel = \"b\"\n")).unwrap();
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(eff.config.asr_sidecar.unwrap().model, "b");
        assert_eq!(eff.set.iter().collect::<Vec<_>>(), ["asr-sidecar.model"]);

        // Both spellings in one file stay both, and serde refuses them, as it
        // does without a profile.
        let eff = effective(
            "",
            "p",
            Some("[asr]\nmodel = \"a\"\n[asr-sidecar]\nmodel = \"b\"\n"),
        )
        .unwrap();
        assert!(eff
            .error
            .unwrap()
            .contains("does not load over config.toml"));
    }

    #[test]
    fn base_only_keys_are_dropped_with_a_warning_each() {
        let eff = eff(Some(
            "[hotkeys]\ntoggle = \"Super+P\"\n\
             [groq]\napi_key = \"gsk_profile\"\nmodel = \"whisper-large-v3\"\n\
             [recording]\ndir = \"/profile/recordings\"\nmax_duration_minutes = 30\n\
             [[llm_commands]]\nname = \"x\"\nhotkey = \"Super+X\"\ninstruction = \"y\"\n\
             [[agents]]\nname = \"jarvis\"\n",
        ));
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(
            eff.dropped,
            [
                "agents",
                "groq.api_key",
                "hotkeys",
                "llm_commands",
                "recording.dir"
            ]
        );
        assert_eq!(eff.warnings.len(), 5, "{:?}", eff.warnings);
        assert_eq!(
            eff.warnings[1],
            format!("profiles/offline.toml: groq.api_key is ignored ({BASE_ONLY_NOTE})")
        );
        let config = &eff.config;
        assert_eq!(
            config.hotkeys.as_ref().unwrap().toggle.as_deref(),
            Some("Super+Shift+D")
        );
        let groq = config.groq.as_ref().unwrap();
        assert_eq!(
            (groq.api_key.as_str(), groq.model.as_str()),
            ("gsk_base", "whisper-large-v3")
        );
        let recording = config.recording.as_ref().unwrap();
        assert_eq!(recording.dir.as_deref(), Some("/base/recordings"));
        assert_eq!(recording.max_duration_minutes, 30);
        assert!(config.llm_commands.is_empty() && config.agents.is_empty());
        assert!(eff.set.iter().all(|p| !is_base_only(p)), "{:?}", eff.set);
        // No warning quotes a key.
        assert!(eff.warnings.iter().all(|w| !w.contains("gsk_")));
    }

    /// Anything but a table at a section would fill the section's fields in
    /// order, `api_key` with them: it is dropped like a base-only key, and
    /// its value reaches no message.
    #[test]
    fn a_section_that_is_not_a_table_is_dropped() {
        for (profile, dropped) in [
            ("groq = [\"gsk_FROM_PROFILE\"]\n", "groq"),
            ("groq = []\n", "groq"),
            ("groq = \"gsk_FROM_PROFILE\"\n", "groq"),
            ("recording = [\"/profile/recordings\"]\n", "recording"),
            ("general = 5\n", "general"),
            ("[[deepgram]]\napi_key = \"gsk_FROM_PROFILE\"\n", "deepgram"),
        ] {
            let eff = eff(Some(profile));
            assert!(eff.applied(), "{profile}: {:?}", eff.error);
            assert_eq!(eff.dropped, [dropped], "{profile}");
            let groq = eff.config.groq.as_ref().unwrap();
            assert_eq!(groq.api_key, "gsk_base", "{profile}");
            let recording = eff.config.recording.as_ref().unwrap();
            assert_eq!(
                recording.dir.as_deref(),
                Some("/base/recordings"),
                "{profile}"
            );
            assert_eq!(eff.config.general.backend, "groq", "{profile}");
            assert!(eff.config.deepgram.is_none(), "{profile}");
            assert_eq!(
                eff.warnings,
                [format!(
                    "profiles/offline.toml: {dropped} is ignored: a section has to be a table \
                     ({BASE_ONLY_NOTE})"
                )],
                "{profile}"
            );
            assert!(eff.set.is_empty(), "{profile}: {:?}", eff.set);
            for text in eff.warnings.iter().chain(&eff.error) {
                assert!(!text.contains("FROM_PROFILE"), "{text}");
            }
        }
        // [replacements] holds words, not settings: its own error stands.
        let eff = eff(Some("replacements = 5\n"));
        assert!(eff.dropped.is_empty());
        assert!(eff
            .error
            .unwrap()
            .contains("does not load over config.toml"));
    }

    /// What the merge drops is what a view of the file names without its
    /// value.
    #[test]
    fn is_dropped_matches_the_merge() {
        let table = |text: &str| Value::Table(text.parse::<Table>().unwrap());
        let string = Value::String("x".into());
        assert!(is_dropped("groq.api_key", &string));
        assert!(is_dropped("hotkeys", &table("toggle = \"x\"")));
        assert!(is_dropped("recording.dir", &string));
        assert!(is_dropped("groq", &string));
        assert!(is_dropped("groq", &Value::Array(Vec::new())));
        assert!(is_dropped("recording", &Value::Integer(1)));
        assert!(!is_dropped("groq", &table("model = \"x\"")));
        assert!(!is_dropped("groq.model", &string));
        assert!(!is_dropped("replacements", &Value::Integer(1)));
        assert!(!is_dropped("replacements.api_key", &string));
        assert!(!is_dropped("general.vocabulary", &Value::Array(Vec::new())));
    }

    /// A serde error never quotes a base-only value.
    #[test]
    fn errors_hide_base_only_values() {
        let table: Table =
            "[groq]\napi_key = \"gsk_SECRET_KEY\"\n[hotkeys]\ntoggle = \"Super+D\"\n\
                            [general]\nlanguage = \"gsk_SECRET_KEY\"\n"
                .parse()
                .unwrap();
        assert_eq!(
            hide_base_only("bad `gsk_SECRET_KEY` and Super+D, not ok", &table),
            "bad `***` and ***, not ok"
        );
    }

    /// A client's check takes a missing key for one the daemon's own
    /// environment may hold; the daemon's refuses it.
    #[test]
    fn a_client_does_not_refuse_a_key_only_the_daemon_may_have() {
        if std::env::var_os("WHISRS_DEEPGRAM_API_KEY").is_some() {
            return;
        }
        let profile = Some("[general]\nbackend = \"deepgram\"\n");
        let daemon = effective(BASE_TEXT, "cloud", profile).unwrap();
        assert!(
            daemon
                .error
                .unwrap()
                .contains("Deepgram backend selected but no API key"),
            "the daemon decides"
        );
        let client = effective_checked(BASE_TEXT, "cloud", profile, Check::Client).unwrap();
        assert!(client.applied(), "{:?}", client.error);
        assert_eq!(client.config.general.backend, "deepgram");
        // Any other error still fails for a client.
        let client = effective_checked(
            BASE_TEXT,
            "p",
            Some("[general]\nbackend = \"nope\"\n"),
            Check::Client,
        )
        .unwrap();
        assert!(client.error.is_some());
    }

    /// A switch to base is checked too: config.toml has to load.
    #[test]
    fn a_switch_to_base_needs_a_config_toml_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert_eq!(
            switch_check(&paths, None, Check::Daemon),
            Ok(()),
            "none is empty"
        );
        std::fs::write(&paths.config, BASE_TEXT).unwrap();
        assert_eq!(switch_check(&paths, None, Check::Client), Ok(()));
        std::fs::write(&paths.config, "[general]\nnotify = \"loud\"\n").unwrap();
        let refusal = switch_check(&paths, None, Check::Daemon).unwrap_err();
        assert!(
            refusal.starts_with("cannot switch to base: config.toml does not load: "),
            "{refusal}"
        );
        std::fs::remove_file(&paths.config).unwrap();
        std::fs::create_dir(&paths.config).unwrap();
        let refusal = switch_check(&paths, None, Check::Daemon).unwrap_err();
        assert!(refusal.starts_with("cannot switch to base: cannot read config.toml"));
    }

    /// A daemon that does not answer is down, unless its socket is set
    /// aside or the active name was just written: then it is waited for,
    /// back once it answers, still restarting when the wait runs out.
    #[test]
    fn a_daemon_that_does_not_answer_is_down_or_restarting() {
        let short = Duration::from_millis(200);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("whisrs.sock");
        let state = dir.path().join("active-profile");
        assert_eq!(
            unanswered_within(&socket, &state, RECENT_SWITCH, short),
            Unanswered::Down,
            "no state file"
        );
        std::fs::write(&state, "offline\n").unwrap();
        assert_eq!(
            unanswered_within(&socket, &state, Duration::ZERO, short),
            Unanswered::Down,
            "written long enough ago"
        );
        let started = Instant::now();
        assert_eq!(
            unanswered_within(&socket, &state, RECENT_SWITCH, short),
            Unanswered::Restarting,
            "just written"
        );
        assert!(started.elapsed() >= short);
        std::fs::remove_file(&state).unwrap();
        std::fs::write(crate::service::socket_aside(&socket), "").unwrap();
        assert_eq!(
            unanswered_within(&socket, &state, RECENT_SWITCH, short),
            Unanswered::Restarting,
            "set aside for a re-exec"
        );

        // It binds again and answers.
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let daemon = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            stream.read_exact(&mut body).unwrap();
            let answer = crate::ipc::Response::Ok {
                state: crate::ipc::State::Idle,
            };
            stream
                .write_all(&crate::ipc::encode_message(&answer).unwrap())
                .unwrap();
        });
        assert_eq!(
            unanswered_within(&socket, &state, RECENT_SWITCH, Duration::from_secs(5)),
            Unanswered::Back
        );
        daemon.join().unwrap();
    }

    #[test]
    fn unknown_keys_name_the_file_they_came_from() {
        let base = format!("{BASE_TEXT}[input]\npast = true\n");
        let eff = effective(
            &base,
            "offline",
            Some("[general]\nsilense_timeout_ms = 1\n[bogus]\nx = 1\n"),
        )
        .unwrap();
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(eff.unknown_base, ["input.past"]);
        assert_eq!(
            eff.unknown_profile,
            ["bogus.x", "general.silense_timeout_ms"]
        );
        assert_eq!(
            eff.warnings,
            [
                "Unknown keys in config at profiles/offline.toml ignored: bogus.x, \
                 general.silense_timeout_ms"
            ]
        );
    }

    /// Each way a profile fails falls back to the base, saying why.
    #[test]
    fn every_failure_falls_back_to_the_base() {
        for (profile, why) in [
            (None, "profiles/offline.toml does not exist"),
            (
                Some("[general\nbackend = \"x\""),
                "profiles/offline.toml is not valid TOML (line 1, column 9)",
            ),
            (
                Some("[general]\nnotify = \"loud\"\n"),
                "profiles/offline.toml does not load over config.toml: ",
            ),
            (
                Some("[general]\nbackend = \"nope\"\n"),
                "the config with profiles/offline.toml is not valid: ",
            ),
        ] {
            let eff = eff(profile);
            let error = eff.error.clone().expect("the profile fails");
            assert!(error.starts_with(why), "{profile:?}: {error}");
            assert_eq!(eff.config.general.backend, "groq", "{profile:?}");
            assert_eq!(eff.config.general.language, "en", "{profile:?}");
            assert_eq!(eff.table, read_table(BASE_TEXT).unwrap(), "{profile:?}");
        }
        // A broken base is no fallback.
        assert!(effective("[general", "p", Some("")).is_err());
        assert!(effective("[general]\nnotify = 1\n", "p", Some(""))
            .unwrap_err()
            .starts_with("config.toml does not load"));
    }

    /// A bad profile line is placed, never quoted: it can hold a key.
    #[test]
    fn a_bad_toml_profile_is_not_quoted() {
        let eff = eff(Some("[groq]\napi_key = \"gsk_SECRET\n"));
        let error = eff.error.unwrap();
        assert!(!error.contains("gsk_SECRET"), "{error}");
        assert!(error.contains("(line 2,"), "{error}");
    }

    #[test]
    fn an_empty_profile_is_the_base() {
        let eff = eff(Some(""));
        assert!(eff.applied());
        assert!(eff.set.is_empty());
        assert_eq!(eff.table, read_table(BASE_TEXT).unwrap());
    }

    /// A base that does not validate is not the profile's fault: one that
    /// keeps the base's error applies; one that brings its own does not.
    #[test]
    fn only_the_profiles_own_validation_error_falls_back() {
        let base = "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"\"\n";
        assert!(toml::from_str::<Config>(base).unwrap().validate().is_err());
        for profile in ["", "[general]\nlanguage = \"de\"\n"] {
            let eff = effective(base, "p", Some(profile)).unwrap();
            assert!(eff.applied(), "{profile:?}: {:?}", eff.error);
        }
        let eff = effective(base, "p", Some("[general]\nbackend = \"nope\"\n")).unwrap();
        assert!(eff.error.unwrap().contains("Unknown backend 'nope'"));
    }

    #[test]
    fn next_wraps_through_base_and_the_names() {
        let names: Vec<String> = ["meeting", "offline", "work"].map(String::from).to_vec();
        assert_eq!(next(&names, None).as_deref(), Some("meeting"));
        assert_eq!(next(&names, Some("meeting")).as_deref(), Some("offline"));
        assert_eq!(next(&names, Some("work")), None, "wraps to base");
        assert_eq!(next(&names, Some("nope")).as_deref(), Some("offline"));
        assert_eq!(next(&[], None), None);
    }

    /// `next` passes over what would not load, in the order and wrapping,
    /// and says what it skipped; with nothing else loading it refuses,
    /// naming each with a short why.
    #[test]
    fn next_skips_what_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(
            &paths.config,
            "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"http://127.0.0.1:1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(&paths.profiles).unwrap();
        for (name, text) in [
            ("broken", "[general\n"),
            ("clean", ""),
            ("nope", "[general]\nbackend = \"nope\"\n"),
            ("quiet", ""),
        ] {
            std::fs::write(paths.profile(name), text).unwrap();
        }
        let names: Vec<String> = ["broken", "clean", "nope", "quiet"]
            .map(String::from)
            .to_vec();
        let next = |current| next_usable(&paths, &names, current, Check::Daemon);
        assert_eq!(
            next(None),
            Ok((Some("clean".into()), vec!["broken".into()]))
        );
        assert_eq!(
            next(Some("clean")),
            Ok((Some("quiet".into()), vec!["nope".into()]))
        );
        assert_eq!(next(Some("quiet")), Ok((None, vec![])), "wraps to base");
        // A current profile whose file is gone: the one after it.
        assert_eq!(
            next(Some("dead")),
            Ok((Some("quiet".into()), vec!["nope".into()]))
        );

        // Nothing else loads: from clean, with quiet broken too and
        // config.toml broken (so base fails as well).
        std::fs::write(paths.profile("quiet"), "[general\n").unwrap();
        std::fs::write(&paths.config, "[general\n").unwrap();
        let refusal = next(Some("clean")).unwrap_err();
        assert_eq!(
            refusal,
            "no other profile loads: nope (config.toml is not valid TOML), quiet (config.toml is \
             not valid TOML), base (config.toml is not valid TOML), broken (config.toml is not \
             valid TOML)"
        );
        std::fs::write(
            &paths.config,
            "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"http://127.0.0.1:1\"\n",
        )
        .unwrap();
        std::fs::write(paths.profile("clean"), "[general\n").unwrap();
        assert_eq!(
            next(Some("broken")),
            Ok((None, vec!["clean".into(), "nope".into(), "quiet".into()]))
        );
        let only: Vec<String> = vec!["broken".into()];
        std::fs::write(&paths.config, "[general]\nnotify = \"loud\"\n").unwrap();
        let refusal = next_usable(&paths, &only, Some("broken"), Check::Daemon).unwrap_err();
        assert!(
            refusal.starts_with("no other profile loads: base (config.toml does not load: "),
            "{refusal}"
        );
        assert_eq!(
            next_usable(&paths, &[], None, Check::Daemon),
            Err(NO_PROFILES.into())
        );
    }

    #[test]
    fn short_whys_drop_the_name_file_and_position() {
        let why = |shown, refusal| short_why(shown, refusal);
        assert_eq!(
            why(
                "broken",
                "cannot switch to broken: profiles/broken.toml is not valid TOML (line 1, column 9)"
            ),
            "not valid TOML"
        );
        assert_eq!(
            why(
                "cloud",
                "cannot switch to cloud: the config with profiles/cloud.toml is not valid: \
                 configuration error: Groq backend selected but no API key configured.\n\
                 Set WHISRS_GROQ_API_KEY or add [groq] api_key to config.toml."
            ),
            "Groq backend selected but no API key configured"
        );
        assert_eq!(
            why(
                "gone",
                "cannot switch to gone: profiles/gone.toml does not exist"
            ),
            "does not exist"
        );
        assert_eq!(
            why(
                "base",
                "cannot switch to base: config.toml is not valid TOML (line 2, column 1)"
            ),
            "config.toml is not valid TOML"
        );
    }

    #[test]
    fn a_switch_names_what_next_skipped() {
        assert_eq!(switching_line("clean", &[]), "switching to clean");
        assert_eq!(
            switching_line("clean", &["broken".into()]),
            "switching to clean (skipped broken: it does not load)"
        );
        assert_eq!(
            switching_line("clean", &["broken".into(), "cloud".into()]),
            "switching to clean (skipped broken, cloud: they do not load)"
        );
    }

    #[test]
    fn targets_resolve() {
        let names: Vec<String> = ["offline", "work"].map(String::from).to_vec();
        let resolve = |current, asked| resolve_target(&names, current, asked);
        assert_eq!(resolve(None, None), Ok(Some("offline".into())));
        assert_eq!(resolve(Some("work"), Some("next")), Ok(None));
        assert_eq!(resolve(Some("work"), Some("base")), Ok(None));
        assert_eq!(resolve(None, Some("work")), Ok(Some("work".into())));
        assert_eq!(
            resolve(None, Some("gone")),
            Err("no profile named gone: profiles/gone.toml does not exist".into())
        );
        assert!(resolve(None, Some("list"))
            .unwrap_err()
            .contains("reserved"));
        assert!(resolve(None, Some("Bad"))
            .unwrap_err()
            .contains("not a profile name"));
        assert_eq!(resolve_target(&[], None, None), Err(NO_PROFILES.into()));
        assert_eq!(resolve_target(&[], None, Some("base")), Ok(None));
    }

    #[test]
    fn list_sorts_and_skips_what_is_no_profile() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            list(&dir.path().join("missing")).unwrap(),
            Listing::default()
        );
        for file in [
            "work.toml",
            "offline.toml",
            "Bad Name.toml",
            "notes.txt",
            "list.toml",
        ] {
            std::fs::write(dir.path().join(file), "").unwrap();
        }
        std::fs::create_dir(dir.path().join("dir.toml")).unwrap();
        let listing = list(dir.path()).unwrap();
        assert_eq!(listing.names, ["offline", "work"]);
        let skipped: Vec<(&str, &str)> = listing
            .skipped
            .iter()
            .map(|s| (s.file.as_str(), s.why.as_str()))
            .collect();
        assert_eq!(skipped.len(), 4, "{skipped:?}");
        assert_eq!(skipped[0].0, "Bad Name.toml");
        assert!(skipped[0].1.contains("not a profile name"));
        assert_eq!(skipped[1], ("dir.toml", "not a .toml file"));
        assert_eq!(skipped[2].0, "list.toml");
        assert!(skipped[2].1.contains("reserved"));
        assert_eq!(skipped[3], ("notes.txt", "not a .toml file"));
        assert_eq!(
            listing.skipped[3].warning(),
            "skipped profiles/notes.txt: not a .toml file"
        );
    }

    #[test]
    fn the_state_file_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let state = Paths::under(dir.path()).state;
        assert_eq!(read_active(&state), Ok(None), "no file is base");

        set_active(&state, Some("offline")).unwrap();
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "offline\n");
        assert_eq!(read_active(&state), Ok(Some("offline".into())));
        let mode = std::fs::metadata(&state).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        set_active(&state, None).unwrap();
        assert_eq!(read_active(&state), Ok(None));
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "base\n");

        // Written whole: no temp file is left beside it.
        let files: Vec<_> = std::fs::read_dir(state.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["active-profile"]);

        assert!(set_active(&state, Some("Bad")).is_err());
        assert_eq!(
            read_active(&state),
            Ok(None),
            "a refused name writes nothing"
        );

        std::fs::write(&state, "  work \n").unwrap();
        assert_eq!(read_active(&state), Ok(Some("work".into())));
        std::fs::write(&state, "\n").unwrap();
        assert_eq!(read_active(&state), Ok(None));
        std::fs::write(&state, "../etc\n").unwrap();
        assert!(read_active(&state)
            .unwrap_err()
            .contains("names no profile"));
    }

    #[test]
    fn effective_at_reads_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.profiles).unwrap();

        // No config.toml: the profile goes over the defaults. (local-whisper
        // validates with no key, whatever this shell's environment holds.)
        std::fs::write(
            paths.profile("p"),
            "[general]\nbackend = \"local-whisper\"\nlanguage = \"de\"\n",
        )
        .unwrap();
        let eff = effective_at(&paths, "p").unwrap();
        assert!(eff.applied(), "{:?}", eff.error);
        assert_eq!(eff.config.general.language, "de");
        assert_eq!(dry_run(&paths, "p"), Ok(()));

        std::fs::write(&paths.config, BASE_TEXT).unwrap();
        let eff = effective_at(&paths, "p").unwrap();
        assert_eq!(eff.config.groq.unwrap().api_key, "gsk_base");

        // Unreadable (a directory stands in: chmod is a no-op for root).
        std::fs::create_dir(paths.profile("dir")).unwrap();
        let eff = effective_at(&paths, "dir").unwrap();
        assert!(eff
            .error
            .unwrap()
            .starts_with("profiles/dir.toml cannot be read"));

        assert_eq!(
            dry_run(&paths, "gone"),
            Err("cannot switch to gone: profiles/gone.toml does not exist".into())
        );
        std::fs::write(paths.profile("bad"), "[general]\nbackend = \"nope\"\n").unwrap();
        assert!(dry_run(&paths, "bad")
            .unwrap_err()
            .starts_with("cannot switch to bad: the config with profiles/bad.toml is not valid"));
    }

    #[test]
    fn paths_sit_where_the_spec_puts_them() {
        let paths = Paths::current();
        assert_eq!(paths.profiles, paths.config.with_file_name("profiles"));
        assert!(paths.state.ends_with("whisrs/active-profile"));
        assert_eq!(
            Paths::under(Path::new("/r")).profile("offline"),
            Path::new("/r/profiles/offline.toml")
        );
    }
}
