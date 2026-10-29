//! Reading and writing `config.toml` for the interactive flows: the load they
//! start from, the refusals for a file they cannot use, and the
//! format-preserving write (merged into the file on disk, atomic, 0600, with a
//! `.bak` when it cannot merge). The daemon persists through the same write.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::config::types::{unknown_config_keys, PreservedKeys};
use crate::Config;

/// What the interactive flows found at the config path.
///
/// Before issue #134 this was an `Option` and collapsed four outcomes into
/// `None`: no file, an unreadable file, invalid TOML, and valid TOML that does
/// not deserialize into [`Config`]. Both callers read `None` as "no config
/// file" and went on to write one from defaults, which deleted every section
/// the defaults leave `None`, API keys included, with no backup. The three
/// cases that differ in what the caller must do are separate variants now.
// `Loaded` makes the enum `Config`-sized (~1.2 KB). It is returned by value
// exactly once per `whisrs setup` / `whisrs config` invocation and never
// collected, so boxing would buy an allocation and cost the callers a deref.
#[allow(clippy::large_enum_variant)]
pub enum ExistingConfig {
    /// No file at the path: first run, nothing to preserve.
    Missing,
    /// A file is there and cannot be used. `message` is what to show the user,
    /// underlying error verbatim: the `toml` crate's error names the line,
    /// column and offending value, which is the single most useful thing the
    /// user gets out of this. `kind` decides what the caller can offer to do
    /// about it.
    Unusable { kind: UnusableKind, message: String },
    /// A usable file, together with the keys in it the schema does not know.
    Loaded {
        config: Config,
        unknown: Vec<String>,
    },
}

/// Why an [`ExistingConfig::Unusable`] file is unusable: the distinction that
/// decides whether writing a fresh config over it can work at all.
///
/// Fusing the two was the second half of issue #134: `run_setup` warned that the
/// old file would be "backed up when the new config is written", asked every
/// wizard question, and only then hit `write_config_to`'s own
/// `fs::read_to_string`, which failed exactly as the load had. Nothing written,
/// no `.bak`, every answer discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnusableKind {
    /// The bytes could not be read at all: mode bits, a root-owned file left by
    /// a `sudo whisrs setup`, a directory at the path, non-UTF-8 content in it.
    /// Rewriting cannot help, because
    /// the write reads the file first and fails the same way. Only a hand fix
    /// gets out of this, so neither flow may offer `whisrs setup`.
    Unreadable,
    /// Read fine (valid TOML or not) but does not deserialize into [`Config`].
    /// Regenerating from scratch works, and [`backup_and_regenerate`] preserves
    /// the original as `.bak`.
    Undeserializable,
}

/// Try to load an existing config from disk, together with the keys in it the
/// schema does not know.
///
/// The daemon warns about those keys via `tracing`, which prints nothing in the
/// CLI flows, so both interactive entry points get the list here and say so
/// themselves (issue #116).
pub fn load_existing_config() -> ExistingConfig {
    load_existing_config_from(&crate::config_path())
}

/// Implementation of [`load_existing_config`] against an explicit path
/// (testable).
pub fn load_existing_config_from(path: &Path) -> ExistingConfig {
    // `NotFound` is the missing file, rather than a preceding `path.exists()`:
    // the pre-check left a window in which a file deleted between the two calls
    // came back as `Unusable("...No such file or directory")`, i.e. `whisrs
    // config` telling the user to hand-fix a file that is not there. One `open`
    // makes the function total and removes the TOCTOU.
    //
    // Wording is shared with the daemon's loader (`daemon::startup::load_config`
    // says "Failed to read/parse config at {path}: {e}" and uses defaults), so
    // the two name the same broken file the same way. Lowercase and without the
    // trailing fallback: neither CLI flow silently falls back to defaults, and
    // `src/cli/main.rs` prints this after "config failed: " / "setup failed: ".
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ExistingConfig::Missing,
        Err(e) => {
            return ExistingConfig::Unusable {
                kind: UnusableKind::Unreadable,
                message: format!("cannot read config at {}: {e}", path.display()),
            };
        }
    };
    match toml::from_str::<Config>(&contents) {
        Ok(config) => ExistingConfig::Loaded {
            config,
            unknown: unknown_config_keys(&contents),
        },
        // `toml::de::Error` renders with a trailing newline. Trimmed here rather
        // than at each consumer: every one of them continues the sentence, so
        // the newline would leave a blank line mid-message.
        Err(e) => ExistingConfig::Unusable {
            kind: UnusableKind::Undeserializable,
            message: format!("cannot parse config at {}: {e}", path.display())
                .trim_end()
                .to_string(),
        },
    }
}

/// Where [`write_config_to`] parks the previous contents of `config_path` when
/// it has to regenerate the file from scratch.
fn config_backup_path(config_path: &Path) -> PathBuf {
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    config_path.with_file_name(format!("{file_name}.bak"))
}

/// The full text `whisrs config` fails with when the file on disk is
/// [`ExistingConfig::Unusable`]: the message, then what to do about it, which
/// depends on whether rewriting the file could work at all.
///
/// Returned as the error rather than printed, because `src/cli/main.rs` already
/// renders it once as `config failed: {e:#}` and exits non-zero; printing it
/// here as well would show the user the same serde error twice.
pub fn unusable_config_refusal(kind: UnusableKind, message: &str, config_path: &Path) -> String {
    match kind {
        UnusableKind::Unreadable => unreadable_config_refusal(message, config_path),
        UnusableKind::Undeserializable => format!(
            "{message}\n\nNothing was changed. Fix {} by hand, or run `whisrs setup` to start from \
             a fresh config (it backs the current file up to {} first).",
            config_path.display(),
            config_backup_path(config_path).display()
        ),
    }
}

/// The refusal both flows share for [`UnusableKind::Unreadable`], because both
/// can only refuse: the write reads the file first, so `whisrs setup` fails on
/// exactly the read that just failed. Offering it would send the user around a
/// loop that cannot terminate, which is why this text names the hand fix only.
pub(crate) fn unreadable_config_refusal(message: &str, config_path: &Path) -> String {
    let path = config_path.display();
    format!(
        "{message}\n\nNothing was changed, and rewriting the file cannot help: `whisrs setup` \
         writes the config by reading it first, so it fails on the same read. Fix it by hand. The \
         usual cause is ownership or permissions: a config written by `sudo whisrs setup` belongs \
         to root. Check that {path} is a regular file, owned by you and readable at mode 0600:\
         \n\n    sudo chown $USER {path}\n    chmod 600 {path}"
    )
}

/// The warning `whisrs setup` prints for an [`UnusableKind::Undeserializable`]
/// file. Setup's job is to produce a config, so it says what it found and
/// carries on into the wizard; the write at the end keeps the old file as `.bak`.
pub fn undeserializable_config_setup_warning(message: &str, config_path: &Path) -> String {
    format!(
        "{message}\nThe existing config cannot be used, so setup starts fresh; the current file is \
         backed up to {} when the new config is written.",
        config_backup_path(config_path).display()
    )
}

/// Write the config to `~/.config/whisrs/config.toml` with `chmod 0600`.
///
/// Format-preserving (issue #82): when a config file already exists on disk it
/// is parsed with `toml_edit` and updated in place, so the user's comments,
/// section order, and formatting of unchanged values all survive. The daemon
/// calls this on every `set_hotkey` press, which must not shred a hand-tuned
/// file. The write itself is atomic (0600 temp file in the same directory +
/// rename), so a crash or full disk mid-write can never leave a truncated
/// config and the API keys inside are never world-readable, even transiently.
pub fn write_config(config: &Config) -> Result<PathBuf> {
    let config_path = crate::config_path();
    write_config_to(config, &config_path)?;
    Ok(config_path)
}

/// Implementation of [`write_config`] against an explicit path (testable).
///
/// Takes the path so the `whisrs config` save path can be driven end to end in
/// a temp dir: the composition of "write vocabulary.txt, then write config.toml
/// with the right list" is where the destructive bugs live, not in either half.
pub fn write_config_to(config: &Config, config_path: &Path) -> Result<()> {
    let config_dir = config_path
        .parent()
        .expect("config path should have a parent directory");

    // Create the config directory if it doesn't exist.
    fs::create_dir_all(config_dir)
        .with_context(|| format!("failed to create config directory {}", config_dir.display()))?;

    // Serialize the struct, then parse that back into a TOML document. This
    // "fresh" document is the source of truth for *which* keys exist and what
    // their values are; the on-disk document is the source of truth for
    // comments, ordering, and formatting.
    let fresh_str = toml::to_string_pretty(config).context("failed to serialize config to TOML")?;
    let mut output = match fs::read_to_string(config_path) {
        Ok(existing_str) => match existing_str.parse::<DocumentMut>() {
            // Derived from the bytes on disk, on every write: a set built from
            // `fresh_str`, or cached across writes, would describe keys that are
            // not there and break byte-stability.
            //
            // "Every write" includes the daemon's LLM-command `set` path, not
            // just the interactive flows. Measured cost of the scan: +175 us on a
            // clean synthetic config, +2.2 ms on a real one, 148 ms on a
            // pathological 200-unknown-section file. The confirmation pass only
            // runs for keys the cheap reserialize diff already flagged, so a clean
            // config pays for the diff and nothing else. Fine in practice, no
            // optimization wanted.
            //
            // This one call also decides *whether* to merge. The set is what the
            // merge's stale-key pass consults, so asking it directly is the only
            // condition that cannot drift from what the merge will do: an empty
            // set means "understood, nothing unknown", an error means "these bytes
            // cannot be described". Re-deriving the answer here from a separate
            // `toml::from_str::<Config>` agreed only by coincidence: it missed
            // the reserialization failure, which would have merged against an
            // empty set and deleted the unknown sections all over again (#134).
            Ok(mut existing) => match PreservedKeys::from_config_str(&existing_str) {
                Ok(preserved) => {
                    let fresh: DocumentMut = fresh_str
                        .parse()
                        .context("failed to reparse serialized config")?;
                    merge_table(existing.as_table_mut(), fresh.as_table(), &preserved);
                    existing.to_string()
                }
                // A file with no preserve set cannot be merged into: the
                // stale-key pass would delete every section the struct does not
                // carry, API keys included, and the `.bak` backstop below never
                // fired, because it was reached only for a file that is not TOML
                // at all. That was issue #134. Same treatment here: back the file
                // up byte for byte and regenerate.
                //
                // Not a merge with a smarter preserve set on purpose: keeping an
                // aliased section (`[local]`, `[asr]`, `[vibevoice]`) beside the
                // canonical one the writer emits makes the result fail to load
                // with `duplicate field`, which is total config loss (see
                // [`is_preserved`]).
                Err(reason) => {
                    backup_and_regenerate(config_path, &existing_str, &reason.to_string())?;
                    fresh_str
                }
            },
            // Unparseable on-disk file: there is no layout to preserve, so
            // fall back to regenerating it from the struct (pre-#82
            // behavior). This branch skips the merge entirely, so unknown
            // keys are lost here by design; the file is kept verbatim as
            // `.bak`, which is the recovery path for them.
            Err(e) => {
                backup_and_regenerate(
                    config_path,
                    &existing_str,
                    &format!("is not valid TOML ({e})"),
                )?;
                fresh_str
            }
        },
        // First-time setup: nothing on disk yet, plain serialization is fine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fresh_str,
        Err(e) => {
            return Err(e).with_context(|| {
                format!("failed to read existing config {}", config_path.display())
            });
        }
    };

    // Appended as a trailing comment: `merge_table` only touches keys, so
    // unrecognized trailing text survives the round-trip.  Only shown when the
    // user hasn't configured hooks yet, and only if the hint isn't already
    // present (idempotent: writing twice must produce the same bytes).
    const HOOKS_HINT: &str = "\n# [hooks]\n\
             # media_auto_pause = true   # pause playing MPRIS media while dictating, resume it after\n\
             # on_record_start = \"\"     # shell command on recording start\n\
             # on_record_stop = \"\"      # shell command on recording stop\n";
    if config.hooks.is_none() && !output.contains(HOOKS_HINT) {
        output.push_str(HOOKS_HINT);
    }

    atomic_write(config_path, &output)
        .with_context(|| format!("failed to write config to {}", config_path.display()))
}

/// Park the current contents of `config_path` in a private `.bak` beside it and
/// tell the user the file is about to be regenerated from the struct.
///
/// The shared body of the two [`write_config_to`] branches that cannot merge
/// into the file on disk: `toml_edit` could not parse it, or it has no
/// [`PreservedKeys`] set describing it. `reason` completes the sentence "existing
/// config at `<path>` …" and is the only thing that differs between them.
///
/// The broken file is the only copy of the user's hand-edits, so it is saved
/// first, at 0600 because it may hold API keys, and an error here aborts the
/// write rather than overwriting the only copy.
fn backup_and_regenerate(config_path: &Path, existing_str: &str, reason: &str) -> Result<()> {
    let backup_path = config_backup_path(config_path);
    write_private_file(&backup_path, existing_str).with_context(|| {
        format!(
            "existing config {} {reason}, and backing it up to {} failed; refusing to overwrite \
             the only copy",
            config_path.display(),
            backup_path.display()
        )
    })?;
    // `tracing` alone is invisible in the CLI flows (`whisrs setup` / `whisrs
    // config` install no subscriber that prints warnings), so tell the user on
    // stderr as well.
    tracing::warn!(
        "existing config at {} {reason}; rewriting it from scratch (backup saved to {})",
        config_path.display(),
        backup_path.display()
    );
    eprintln!(
        "warning: existing config at {} {reason}; rewriting it from scratch (backup saved to {})",
        config_path.display(),
        backup_path.display()
    );
    Ok(())
}

/// Sync `existing` (the user's on-disk TOML, decor intact) to hold exactly the
/// keys and values of `fresh` (the reserialized struct), plus whatever
/// `preserved` marks as confirmed-unknown. Matching keys keep their comments
/// and formatting, recursing into sub-tables; keys missing from `fresh` are
/// removed unless preserved; new keys are appended.
///
/// `preserved` is the node for *this* table. A file the preserve set cannot
/// describe (not TOML, not a `Config`, or a `Config` that does not reserialize)
/// used to arrive here as an *empty* set, which made the merge delete every
/// section the struct does not carry: issue #134. That cannot happen now.
/// [`write_config_to`] branches on `PreservedKeys::from_config_str` itself and
/// diverts such a file to [`backup_and_regenerate`] before the merge is reached,
/// so an empty set here always means "described, nothing unknown".
///
/// Two deliberate gaps remain:
///
/// * `llm_commands = [{ name = "x", bogus = 1 }]` written as an array of
///   *inline* tables takes `merge_item`'s catch-all and is replaced wholesale,
///   losing `bogus`. Pre-existing; the `[[llm_commands]]` spelling is handled.
/// * A section that is *entirely* unknown but whose name is a serde alias for
///   one the writer emits (`[asr]`, `[vibevoice]`, `[local]`) is warned about
///   and then deleted anyway. Keeping it would put both spellings in the file
///   and make it fail to load with `duplicate field`, so the alias loses. See
///   [`is_preserved`].
fn merge_table(existing: &mut Table, fresh: &Table, preserved: &PreservedKeys) {
    // Drop keys the struct no longer carries. `Config` has no catch-all field,
    // so this used to delete keys it never knew about as well, including the
    // very typo the load-time warning had just told the user to fix (issue
    // #116). Confirmed-unknown keys are exempt now; everything else still goes.
    let stale: Vec<String> = existing
        .iter()
        .filter(|(key, item)| !fresh.contains_key(key) && !is_preserved(preserved, key, item))
        .map(|(key, _)| key.to_string())
        .collect();
    for key in stale {
        existing.remove(&key);
    }

    for (key, fresh_item) in fresh.iter() {
        match existing.get_mut(key) {
            Some(existing_item) => merge_item(existing_item, fresh_item, preserved.table(key)),
            None => {
                // A header-less parent (e.g. `[overlay.colors]` without an
                // explicit `[overlay]`) must gain its header once it holds a
                // plain value.
                if fresh_item.is_value() && existing.is_implicit() {
                    existing.set_implicit(false);
                }
                existing.insert(key, fresh_item.clone());
            }
        }
    }
}

/// Whether `key` names something the writer must keep even though `fresh` no
/// longer carries it: a confirmed-unknown key, or a subtree made entirely of
/// them.
///
/// This is the *only* place a stale key is judged, and it is reached only for
/// keys `fresh` does not carry, which makes it the one place that can safely
/// ask `PreservedKeys::table_prunable`, i.e. "would deleting this whole table
/// change how the file parses?". Two rules, both learned the hard way:
///
/// * "Every leaf of the on-disk subtree is confirmed-unknown" is the rule, and
///   "prefixes a confirmed-unknown path" is *not* good enough. `[local]` is a
///   serde alias for `[local-whisper]` (likewise `[asr]`/`[vibevoice]` for
///   `[asr-sidecar]`), so keeping a `[local]` that still holds real settings
///   while the merge also writes the canonical `[local-whisper]` makes the file
///   fail to deserialize with `duplicate field`, and the daemon then silently
///   falls back to defaults, throwing away the user's entire config. Losing one
///   stray key next to a real one is by far the lesser evil.
/// * Even an *all*-unknown table can be an alias in disguise (`[asr] bogus = 1`
///   prunes to an empty table that still means `[asr-sidecar]`), so a whole
///   table is kept only when removing it provably changes nothing. Asking that
///   question anywhere but here is what deleted typo'd keys out of `[hooks]`
///   and `[hotkeys]`: those names *are* in `fresh`, so this function is never
///   called for them and the merge simply recurses into them.
fn is_preserved(preserved: &PreservedKeys, key: &str, item: &Item) -> bool {
    if preserved.is_empty() {
        return false;
    }
    match item {
        Item::None => false,
        Item::Value(value) => value_is_preserved(preserved, key, value),
        Item::Table(table) => {
            let child = preserved.table(key);
            // `table_prunable()` is what carries the safety property here: a
            // leafless on-disk table contributes no `PreservedKeys` node, so
            // an empty `[asr]` is already refused by that flag before the
            // emptiness check is consulted. `!table.is_empty()` is a cheap
            // early-out and defence in depth, not the guard: replacing it
            // with `true` leaves the suite green. Kept anyway: nothing should
            // be *kept whole* when there is nothing in it to keep.
            child.table_prunable()
                && !table.is_empty()
                && table
                    .iter()
                    .all(|(key, item)| subtree_is_preserved(child, key, item))
        }
        // Only an array of tables under a wholly unknown key can be stale; the
        // schema's own (`[[llm_commands]]`) is always present in `fresh`.
        Item::ArrayOfTables(_) => preserved.contains_leaf(key),
    }
}

/// [`is_preserved`] for a key *inside* a subtree the caller is already keeping
/// whole. Same rules, plus one that only makes sense there: a table with no
/// leaves at all is preserved vacuously.
///
/// `t = {}`, `a = { b = {} }` and a bare `[bogus.emptysub]` header name nothing,
/// so "every leaf of this subtree is confirmed-unknown" is trivially true of
/// them, but they contribute no leaf, so `PreservedKeys` has no node for them
/// and the flag-based rules read them as *not* preserved. One stray `{}`
/// anywhere inside `[bogus]` then vetoed the whole section, and the writer
/// deleted it: the comment, the real keys, and (because the rewritten file no
/// longer held the keys) the warning that would have mentioned them. Issue
/// #116's exact symptom, produced by the fix for issue #116.
///
/// Deliberately *not* applied at the top level, where [`is_preserved`] is
/// reached from the stale filter. An empty `[asr]` beside any other unknown key
/// would then be kept next to the canonical `[asr-sidecar]` the writer emits,
/// and serde rejects that file with `duplicate field`: total config loss. Up
/// there a leafless table must keep failing; the `table_prunable` check is the
/// only thing standing between an aliased section and a config the daemon
/// cannot load.
fn subtree_is_preserved(preserved: &PreservedKeys, key: &str, item: &Item) -> bool {
    match item {
        Item::Value(value) => subtree_value_is_preserved(preserved, key, value),
        Item::Table(table) if table_has_no_leaves(table) => true,
        _ => is_preserved(preserved, key, item),
    }
}

/// The value half of [`is_preserved`]: inline tables recurse, everything else
/// is a leaf. A whole inline table takes the same prunability check as a header
/// table: `asr = { bogus = 1 }` is `[asr] bogus = 1` in another spelling.
fn value_is_preserved(preserved: &PreservedKeys, key: &str, value: &Value) -> bool {
    match value {
        Value::InlineTable(inline) => {
            let child = preserved.table(key);
            // Same division of labour as in [`is_preserved`]: the flag is the
            // guard against `asr = {}` surviving beside `[asr-sidecar]`,
            // `!inline.is_empty()` only the cheap early-out beside it.
            child.table_prunable()
                && !inline.is_empty()
                && inline
                    .iter()
                    .all(|(key, value)| subtree_value_is_preserved(child, key, value))
        }
        _ => preserved.contains_leaf(key),
    }
}

/// The value half of [`subtree_is_preserved`].
fn subtree_value_is_preserved(preserved: &PreservedKeys, key: &str, value: &Value) -> bool {
    match value {
        Value::InlineTable(inline) if inline_has_no_leaves(inline) => true,
        _ => value_is_preserved(preserved, key, value),
    }
}

/// Whether `table` holds no leaf keys at all, only (possibly nested) empty
/// tables. Mirrors `types::has_no_leaves`, over the `toml_edit` types.
fn table_has_no_leaves(table: &Table) -> bool {
    table.iter().all(|(_, item)| match item {
        Item::Table(inner) => table_has_no_leaves(inner),
        Item::Value(Value::InlineTable(inner)) => inline_has_no_leaves(inner),
        _ => false,
    })
}

/// [`table_has_no_leaves`] for an inline table, whose entries are all values.
fn inline_has_no_leaves(inline: &InlineTable) -> bool {
    inline.iter().all(|(_, value)| match value {
        Value::InlineTable(inner) => inline_has_no_leaves(inner),
        _ => false,
    })
}

/// Merge one item of a table, dispatching on its structure. `preserved` is the
/// node for this item's own subtree.
fn merge_item(existing: &mut Item, fresh: &Item, preserved: &PreservedKeys) {
    match (existing, fresh) {
        (Item::Table(existing), Item::Table(fresh)) => merge_table(existing, fresh, preserved),
        (Item::ArrayOfTables(existing), Item::ArrayOfTables(fresh)) => {
            merge_array_of_tables(existing, fresh, preserved)
        }
        // The user wrote a section as an inline table (`colors = { ... }`);
        // the serializer always produces a header table. Keep their spelling,
        // at every depth. Bailing out to the catch-all when the fresh table was
        // not flat is what ate `overlay = { colors = { bogos = 1 } }`: the root
        // inline spelling of the one section with a sub-section, replaced
        // wholesale by the fresh item, unknown keys and all.
        (Item::Value(Value::InlineTable(existing)), Item::Table(fresh)) => {
            merge_inline_table(existing, fresh, preserved)
        }
        (Item::Value(existing), Item::Value(fresh)) => merge_value(existing, fresh),
        // Structural change (e.g. `llm_commands = []` becoming a populated
        // `[[llm_commands]]` array, or vice versa): take the fresh item as-is.
        (existing, fresh) => *existing = fresh.clone(),
    }
}

/// Replace a scalar/array value only when it actually changed, carrying the
/// old decor (surrounding whitespace + trailing inline comment) over to the
/// new value. Untouched values keep their exact user spelling (quoting style,
/// number format, array layout).
fn merge_value(existing: &mut Value, fresh: &Value) {
    if values_equal(existing, fresh) {
        return;
    }
    let decor = existing.decor().clone();
    let mut new_value = fresh.clone();
    *new_value.decor_mut() = decor;
    *existing = new_value;
}

/// Structural equality of two TOML values, ignoring formatting/decor.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, av)| b.get(key).is_some_and(|bv| values_equal(av, bv)))
        }
        _ => false,
    }
}

/// Sync an array-of-tables (e.g. `[[llm_commands]]`). Fresh entries are
/// matched to their on-disk counterpart by `name` first (so deleting or
/// reordering entries keeps each survivor's comments), falling back to
/// position for tables without a usable `name`. Fresh entries with no match
/// are appended; on-disk entries with no match are dropped (whole, unknown
/// keys included) because the user deleted them.
fn merge_array_of_tables(
    existing: &mut ArrayOfTables,
    fresh: &ArrayOfTables,
    preserved: &PreservedKeys,
) {
    let mut consumed = vec![false; existing.len()];
    let mut merged: Vec<Table> = Vec::with_capacity(fresh.len());

    for (fresh_idx, fresh_table) in fresh.iter().enumerate() {
        let by_name = entry_name(fresh_table).and_then(|name| {
            (0..existing.len()).find(|&i| {
                !consumed[i] && existing.get(i).is_some_and(|t| entry_name(t) == Some(name))
            })
        });
        let matched = by_name
            .or_else(|| (fresh_idx < existing.len() && !consumed[fresh_idx]).then_some(fresh_idx));
        match matched.and_then(|i| existing.get(i).cloned().map(|t| (i, t))) {
            Some((i, mut table)) => {
                consumed[i] = true;
                // `i`, never `fresh_idx`: `Seg::Index` is the position in the
                // on-disk document, and deleting an earlier entry shifts every
                // later one. Indexing with the fresh position would apply a
                // deleted entry's unknown keys to its successor and delete the
                // successor's own.
                merge_table(&mut table, fresh_table, preserved.element(i));
                merged.push(table);
            }
            None => merged.push(fresh_table.clone()),
        }
    }

    existing.clear();
    for table in merged {
        existing.push(table);
    }
}

/// The `name` key of an array-of-tables entry, if it is a string.
fn entry_name(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

/// Sync a user-written inline table against the header table the serializer
/// produced for the same section.
///
/// Carries its own copy of `merge_table`'s stale filter, so it needs its own
/// copy of the preserve exemption too: `colors = { theme = "x", bogos = 1 }`
/// reaches this path and nothing else.
///
/// A sub-section of `fresh` (`[overlay.colors]` under a root-level
/// `overlay = { ... }`) has to be re-spelled inline to go inside an inline
/// table. Where the user already wrote one, recurse into it so its unknown keys
/// take the same exemption; otherwise `Item::into_value` does the conversion
/// (`Table` -> `InlineTable`, `ArrayOfTables` -> array of inline tables) and
/// normalizes the decor, so the header-table blank lines do not leak into a
/// value position.
fn merge_inline_table(existing: &mut InlineTable, fresh: &Table, preserved: &PreservedKeys) {
    let stale: Vec<String> = existing
        .iter()
        .filter(|(key, value)| {
            !fresh.contains_key(key) && !value_is_preserved(preserved, key, value)
        })
        .map(|(key, _)| key.to_string())
        .collect();
    for key in stale {
        existing.remove(&key);
    }
    for (key, fresh_item) in fresh.iter() {
        if let (Some(Value::InlineTable(nested)), Item::Table(fresh_table)) =
            (existing.get_mut(key), fresh_item)
        {
            merge_inline_table(nested, fresh_table, preserved.table(key));
            continue;
        }
        // `Item::None` is the only shape with no value form, and a serialized
        // `Config` never produces one.
        let Ok(fresh_value) = fresh_item.clone().into_value() else {
            continue;
        };
        match existing.get_mut(key) {
            Some(existing_value) => merge_value(existing_value, &fresh_value),
            None => {
                existing.insert(key, fresh_value);
            }
        }
    }
}

/// Write `contents` to `path` atomically: create a 0600 temp file in the same
/// directory, fsync it, then rename it over `path`. Interrupted writes can
/// only ever leave the temp file behind, never a truncated config, and the
/// mode is set at creation so the file is private for its entire lifetime.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .expect("config path should have a parent directory");
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    // Per-process temp name so a concurrent `whisrs config` save and a daemon
    // persist do not scribble on the same temp file.
    let tmp_path = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let result = write_private_file(&tmp_path, contents).and_then(|()| fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Create (or truncate) `path` with mode 0600 and write `contents`, fsyncing
/// before returning.
///
/// Public because every file whisrs writes next to `config.toml` gets the same
/// treatment, `vocabulary.txt` included, since the feature moves user data out
/// of the 0600 config into a sibling file.
pub fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // `mode(0o600)` only applies at creation; enforce it again in case a
    // stale temp file from an interrupted run survived with laxer permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A config the way a user actually writes it: header comments, inline
    /// comments, custom section order ([audio] before [general]), and
    /// commented [[llm_commands]] entries pasted from the docs.
    const COMMENTED_CONFIG: &str = r#"# whisrs config - hand-tuned, do not regenerate
# see docs/configuration.md before touching anything below

[audio]
device = "default" # usb mic drops out, stick to default

[general]
backend = "groq" # fastest cloud option
language = "en"

[hotkeys]
toggle = "Super+D"

# translate the selection into german
[[llm_commands]]
name = "german"
hotkey = "Super+Shift+G"
set_hotkey = "Super+Shift+S"
instruction = "Translate to German."

# tidy up prose without changing meaning
[[llm_commands]]
name = "polish"
hotkey = "Super+Shift+P"
instruction = "Polish the text."
"#;

    /// The issue #134 repro: valid TOML, so `toml_edit` parses it happily, but
    /// `silence_timeout_ms` is quoted and so it does not deserialize into
    /// `Config`. The API key is what the user stood to lose.
    const CONFIG_THAT_DOES_NOT_DESERIALIZE: &str = r#"[general]
backend = "groq"
silence_timeout_ms = "2000"

[groq]
api_key = "sk-my-real-secret-key"
model = "whisper-large-v3-turbo"
"#;

    fn parse_config(toml_str: &str) -> Config {
        toml::from_str(toml_str).expect("fixture should deserialize")
    }

    fn write_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).expect("write fixture");
        path
    }

    #[test]
    fn value_change_keeps_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // The #82 scenario: a set_hotkey press persists a new instruction.
        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands[0].instruction = "Translate to French.".to_string();
        // Plus a changed scalar that carries an inline comment.
        config.general.backend = "openai".to_string();
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        assert!(out.contains("# see docs/configuration.md before touching anything below"));
        assert!(out.contains("# usb mic drops out, stick to default"));
        assert!(out.contains("# translate the selection into german"));
        assert!(out.contains("# tidy up prose without changing meaning"));
        // The changed value keeps its trailing comment.
        assert!(out.contains(r#"backend = "openai" # fastest cloud option"#));
        assert!(out.contains("Translate to French."));
        assert!(!out.contains("Translate to German."));
        // The user's section order survives ([audio] written above [general]).
        assert!(out.find("[audio]").unwrap() < out.find("[general]").unwrap());
        // And the result still round-trips into the struct.
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.general.backend, "openai");
        assert_eq!(reparsed.llm_commands[0].instruction, "Translate to French.");
    }

    /// A `whisrs config` save keeps the user's `[[agents]]` entries and their
    /// comments (defaults are filled in, as in every other section).
    #[test]
    fn agents_survive_a_write_untouched() {
        const WITH_AGENTS: &str = r#"[general]
backend = "groq"

# my coding agent
[[agents]]
name = "jarvis" # says hi
dir = "~/Projects/whisrs"
args = ["--model", "haiku"]
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, WITH_AGENTS).unwrap();
        let mut config = parse_config(WITH_AGENTS);
        config.general.backend = "openai".to_string();
        write_config_to(&config, &path).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("# my coding agent"), "{out}");
        assert!(out.contains(r#"name = "jarvis" # says hi"#), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.agents, config.agents);
        assert_eq!(reparsed.agents[0].args, ["--model", "haiku"]);
    }

    #[test]
    fn added_keys_appear() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        let mut config = parse_config(COMMENTED_CONFIG);
        config.general.prompt = Some("Vocabulary: whisrs, Hyprland".to_string());
        config.hotkeys.as_mut().unwrap().speak = Some("Super+R".to_string());
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        // New key in an existing section.
        assert!(out.contains(r#"prompt = "Vocabulary: whisrs, Hyprland""#));
        assert!(out.contains(r#"speak = "Super+R""#));
        // A whole section the file never had (serialized from defaults).
        assert!(out.contains("[input]"));
        // Comments still intact.
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed.general.prompt.as_deref(),
            Some("Vocabulary: whisrs, Hyprland")
        );
        assert_eq!(reparsed.hotkeys.unwrap().speak.as_deref(), Some("Super+R"));
    }

    #[test]
    fn removed_llm_command_disappears_and_survivor_keeps_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // Remove the *first* entry, so the survivor only keeps its comment if
        // entries are matched by name rather than by position.
        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands.retain(|c| c.name != "german");
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("Translate to German."));
        assert!(!out.contains(r#"name = "german""#));
        // The removed entry's comment goes with it.
        assert!(!out.contains("# translate the selection into german"));
        // The survivor keeps its own comment and content.
        assert!(out.contains("# tidy up prose without changing meaning"));
        assert!(out.contains(r#"name = "polish""#));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm_commands.len(), 1);
        assert_eq!(reparsed.llm_commands[0].name, "polish");
    }

    #[test]
    fn clearing_llm_commands_removes_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        let mut config = parse_config(COMMENTED_CONFIG);
        config.llm_commands.clear();
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("[[llm_commands]]"));
        // Comments elsewhere survive the structural change.
        assert!(out.contains("# whisrs config - hand-tuned, do not regenerate"));
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.llm_commands.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_create_and_rewrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = parse_config(COMMENTED_CONFIG);

        // Fresh create (no file on disk yet).
        write_config_to(&config, &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Rewrite over a file that drifted to laxer permissions.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_config_to(&config, &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn first_time_write_creates_parseable_file() {
        let dir = tempfile::tempdir().unwrap();
        // Parent directory does not exist yet: exercised create_dir_all.
        let path = dir.path().join("whisrs").join("config.toml");

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");
        assert_eq!(reparsed.llm_commands.len(), 2);
    }

    #[test]
    fn unparseable_existing_file_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is [ not toml").unwrap();

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");

        // The broken file was the only copy of the user's hand-edits: it must
        // survive, byte for byte, as a private backup next to the config.
        let backup = dir.path().join("config.toml.bak");
        assert_eq!(fs::read(&backup).unwrap(), b"this is [ not toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "backup may hold API keys, must be 0600");
        }
    }

    /// Issue #134: valid TOML that does not deserialize used to reach the merge
    /// with an empty preserve set, which deleted every section the struct does
    /// not carry, and the `.bak` backstop never fired because it only covered
    /// files that are not TOML at all.
    #[test]
    fn existing_file_that_does_not_deserialize_is_backed_up_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        // What both CLI flows do next: write a config built without any
        // knowledge of the file on disk (the wizard's, or `default_config()`).
        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();

        // The rewritten file loads: it is a config again, not a second broken one.
        let reparsed: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.general.backend, "groq");

        // And the file it replaced survives byte for byte, 0600 because it may
        // hold API keys (this fixture does).
        let backup = dir.path().join("config.toml.bak");
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes()
        );
        #[cfg(unix)]
        {
            let mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "backup may hold API keys, must be 0600");
        }
    }

    /// The user-visible stake of issue #134: the `[groq] api_key` in a file the
    /// flow could not read still leaves the live config (the struct being
    /// written has no `[groq]` at all), but it is recoverable from the backup
    /// instead of gone without a trace.
    #[test]
    fn api_key_of_an_undeserializable_config_survives_in_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        write_config_to(&parse_config(COMMENTED_CONFIG), &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(
            !out.contains("sk-my-real-secret-key"),
            "the struct being written has no [groq]; nothing should invent one\n{out}"
        );
        let backup = fs::read_to_string(dir.path().join("config.toml.bak")).unwrap();
        assert!(backup.contains("[groq]"), "{backup}");
        assert!(
            backup.contains(r#"api_key = "sk-my-real-secret-key""#),
            "{backup}"
        );
    }

    /// The second write after a divert must take the merge path, because the
    /// file it now finds is one it just wrote.
    ///
    /// Safe today only because `fresh_str` round-trips and `HOOKS_HINT` is
    /// comments: if any `Config` field ever stops round-tripping, every
    /// subsequent write diverts again and overwrites the `.bak` with the
    /// regenerated file, destroying the user's only copy of the original. The
    /// backup has to survive writes that follow the one that made it.
    #[test]
    fn a_second_write_after_a_divert_keeps_the_first_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup = dir.path().join("config.toml.bak");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();
        let after_divert = fs::read(&path).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes()
        );

        // Same struct again: a `whisrs config` save, or a `set_hotkey` press.
        write_config_to(&config, &path).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE.as_bytes(),
            "the second write re-backed-up, clobbering the only copy of the user's original"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            after_divert,
            "the second write did not take the merge path: the regenerated file is not stable"
        );
    }

    /// A serde alias beside the canonical section: `[local]` is an alias for
    /// `[local-whisper]`, so a file carrying both fails to deserialize with
    /// `duplicate field`, a different failure class from the type error every
    /// other non-deserializing fixture here uses, and the one that makes the
    /// "merge with a smarter preserve set" design lose the whole config.
    /// Preserving `[local]` alongside the `[local-whisper]` the writer emits
    /// would produce a file the daemon cannot load at all. Divert instead.
    const CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE: &str = r#"[general]
backend = "local-whisper"

[groq]
api_key = "sk-my-real-secret-key"

# Hand-written before the section was renamed, and kept afterwards.
[local]
model_path = "/home/u/.local/share/whisrs/models/ggml-base.en.bin"

[local-whisper]
model_path = "/home/u/.local/share/whisrs/models/ggml-small.en.bin"
"#;

    #[test]
    fn an_alias_section_beside_the_canonical_one_is_backed_up_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE).unwrap();
        // The premise: this is a `duplicate field` failure, not a type error.
        let err = toml::from_str::<Config>(CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE)
            .expect_err("fixture must not deserialize")
            .to_string();
        assert!(err.contains("duplicate field `local-whisper`"), "{err}");

        write_config_to(&parse_config(COMMENTED_CONFIG), &path).unwrap();

        // The rewritten file loads. A merge that kept `[local]` would not: both
        // spellings in one file is the `duplicate field` above, i.e. the daemon
        // discarding the user's entire config.
        let out = fs::read_to_string(&path).unwrap();
        let reparsed: Config =
            toml::from_str(&out).unwrap_or_else(|e| panic!("rewrite does not load: {e}\n{out}"));
        assert_eq!(reparsed.general.backend, "groq");
        assert!(!out.contains("[local]"), "{out}");

        // And the original survives byte for byte, which is where the user's
        // `api_key` and both `model_path`s are recoverable from.
        assert_eq!(
            fs::read(dir.path().join("config.toml.bak")).unwrap(),
            CONFIG_WITH_AN_ALIAS_SECTION_BESIDE_THE_CANONICAL_ONE.as_bytes()
        );
    }

    /// The write half of issue #134's read failure: `write_config_to` reads the
    /// file before rewriting it, so a file it cannot read stops the write. It
    /// must stop *without* side effects: no truncated config, and no `.bak`
    /// holding whatever it managed to read (`run_setup` refuses before the wizard
    /// for exactly this reason, so this path should now only be reachable by a
    /// permission change mid-session).
    #[test]
    #[cfg(unix)]
    fn write_config_to_an_unreadable_file_errors_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits, as in `unreadable_config_is_unusable_not_missing`.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        let err = write_config_to(&parse_config(COMMENTED_CONFIG), &path)
            .expect_err("an unreadable config must not be silently overwritten");
        let rendered = format!("{err:#}");
        assert!(rendered.contains(&path.display().to_string()), "{rendered}");

        // No backup: nothing was read, so there is nothing to have saved, and a
        // partial one would look like a recovery point that is not.
        assert!(
            !dir.path().join("config.toml.bak").exists(),
            "a failed read must not leave a .bak"
        );
        // And the file itself is exactly as it was.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED_CONFIG);
    }

    /// The `?` in `backup_and_regenerate` is the whole safety argument for the
    /// divert: the broken file is the only copy of the user's hand-edits, so a
    /// backup that cannot be written must abort the write rather than let
    /// `atomic_write` replace the original anyway. Pinned here because that
    /// property is now shared by two branches instead of one.
    #[test]
    fn a_divert_whose_backup_cannot_be_written_leaves_the_original_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();
        // A directory where the `.bak` goes: `write_private_file` cannot open it.
        fs::create_dir(dir.path().join("config.toml.bak")).unwrap();

        let err = write_config_to(&parse_config(COMMENTED_CONFIG), &path)
            .expect_err("a failed backup must abort the write");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("refusing to overwrite the only copy"),
            "{rendered}"
        );

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            CONFIG_THAT_DOES_NOT_DESERIALIZE,
            "the only copy of the user's config must survive a failed backup"
        );
    }

    #[test]
    fn rewriting_identical_config_is_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir);

        // First write merges the struct into the commented fixture.
        let config = parse_config(COMMENTED_CONFIG);
        write_config_to(&config, &path).unwrap();
        let first = fs::read(&path).unwrap();

        // Writing the identical Config again (e.g. a set_hotkey press that
        // changes nothing) must not perturb a single byte.
        write_config_to(&config, &path).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(
            first, second,
            "second write of an identical Config must be byte-identical"
        );
    }

    /// A config carrying keys the schema does not know, in every shape the
    /// merge has to handle: a typo'd key with a trailing comment, a stray key
    /// beside a real one, a whole unknown section with a nested sub-section,
    /// and a stray key inside the *second* `[[llm_commands]]` entry.
    const CONFIG_WITH_UNKNOWN_KEYS: &str = r#"[general]
backend = "groq"

[input]
past = true # typo for `paste` - do not eat my comment

[deepgram]
api_key = "k"
bogus = 2

[bogus]
foo = 1

[bogus.nested]
bar = 2

[[llm_commands]]
name = "german"
hotkey = "Super+Shift+G"
instruction = "Translate to German."

[[llm_commands]]
name = "polish"
hotkey = "Super+Shift+P"
instruction = "Polish the text."
stray = "keep me"
"#;

    fn write_unknown_key_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_WITH_UNKNOWN_KEYS).expect("write fixture");
        path
    }

    /// Write `contents` to a fresh temp config and round-trip it through
    /// `write_config_to`, returning the rewritten file.
    fn rewrite(dir: &tempfile::TempDir, contents: &str) -> String {
        let path = dir.path().join("config.toml");
        fs::write(&path, contents).expect("write fixture");
        write_config_to(&parse_config(contents), &path).unwrap();
        fs::read_to_string(&path).unwrap()
    }

    /// [`rewrite`] plus the two invariants that outrank whatever the caller is
    /// actually asserting: the rewritten file still deserializes into `Config`
    /// (a file that stops loading is total config loss, strictly worse than the
    /// key-eating bug), and writing it again does not perturb a byte (a
    /// preserved key that moves or re-renders churns the file on every save).
    fn rewrite_checked(dir: &tempfile::TempDir, contents: &str) -> String {
        let out = rewrite(dir, contents);
        toml::from_str::<Config>(&out)
            .unwrap_or_else(|e| panic!("rewrite no longer deserializes: {e}\n{out}"));
        let path = dir.path().join("config.toml");
        write_config_to(&parse_config(&out), &path).unwrap();
        assert_eq!(
            out,
            fs::read_to_string(&path).unwrap(),
            "the second write is not byte-identical"
        );
        out
    }

    #[test]
    fn unknown_keys_and_their_comments_survive_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        write_config_to(&parse_config(CONFIG_WITH_UNKNOWN_KEYS), &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        // The typo the load-time warning points at, comment included.
        assert!(
            out.contains("past = true # typo for `paste` - do not eat my comment"),
            "{out}"
        );
        // A stray key beside a real one in a known section.
        assert!(out.contains("bogus = 2"), "{out}");
        // A whole unknown section, nested sub-section included.
        assert!(out.contains("[bogus]"), "{out}");
        assert!(out.contains("foo = 1"), "{out}");
        assert!(out.contains("[bogus.nested]"), "{out}");
        assert!(out.contains("bar = 2"), "{out}");
        // And one inside an array-of-tables entry.
        assert!(out.contains(r#"stray = "keep me""#), "{out}");

        // The known keys still merged normally and the file still loads.
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.deepgram.unwrap().api_key, "k");
        assert!(!reparsed.input.paste);
        // Still reported, so the warning keeps naming them.
        assert!(unknown_config_keys(&out).contains(&"input.past".to_string()));
    }

    #[test]
    fn rewriting_a_config_with_unknown_keys_is_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        let config = parse_config(CONFIG_WITH_UNKNOWN_KEYS);
        write_config_to(&config, &path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        write_config_to(&config, &path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(
            first, second,
            "a preserved key must not move, duplicate, or re-render on the next write"
        );
    }

    #[test]
    fn a_hotkey_alias_is_recanonicalized_while_the_typo_beside_it_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[hotkeys]\nread = \"Super+Shift+R\"\nbogus = \"Super+X\"\n",
        );

        // `read` is a serde alias for `speak`, so it is *known*, never
        // preserved, and replaced by the canonical spelling exactly once;
        // keeping both would be a `duplicate field` error on the next load.
        assert!(!out.contains("read ="), "alias key survived: {out}");
        assert_eq!(out.matches("speak = ").count(), 1, "{out}");
        assert!(out.contains(r#"bogus = "Super+X""#), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed.hotkeys.unwrap().speak.as_deref(),
            Some("Super+Shift+R")
        );
    }

    #[test]
    fn an_unknown_key_survives_the_deletion_of_an_earlier_llm_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        // `stray` lives in on-disk entry 1; deleting entry 0 makes `polish`
        // *fresh* entry 0. Descending with the fresh index would delete
        // `stray` and try to apply the removed entry's keys to `polish`.
        let mut config = parse_config(CONFIG_WITH_UNKNOWN_KEYS);
        config.llm_commands.retain(|c| c.name != "german");
        write_config_to(&config, &path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains(r#"name = "german""#), "{out}");
        assert!(out.contains(r#"name = "polish""#), "{out}");
        assert!(out.contains(r#"stray = "keep me""#), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm_commands.len(), 1);
    }

    #[test]
    fn an_unknown_key_in_an_inline_table_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\ntheme = \"custom\"\n\
             colors = { background = \"#000000\", bogos = 1 }\n",
        );

        // `merge_inline_table` carries its own stale filter, so it needs its
        // own copy of the exemption: the user's inline spelling survives, and
        // so does the stray key inside it.
        assert!(out.contains("colors = {"), "{out}");
        assert!(out.contains("bogos = 1"), "{out}");
        assert!(out.contains(r##"background = "#000000""##), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_mixed_alias_section_is_still_dropped_whole() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"local\"\n\n[local]\nmodel_path = \"/m.bin\"\nbogus = 1\n",
        );

        // `model_path` is a real setting reached through the `local` ->
        // `local-whisper` alias, so the subtree is mixed and goes wholesale.
        // Keeping `[local]` beside the canonical section would make the file
        // fail to deserialize with `duplicate field`, i.e. the daemon would
        // discard the user's entire config.
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[local]"), "{out}");
        assert!(out.contains("[local-whisper]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.local_whisper.unwrap().model_path, "/m.bin");
    }

    #[test]
    fn a_wholly_unknown_alias_section_is_dropped_rather_than_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(&dir, "[general]\nbackend = \"groq\"\n\n[asr]\nbogus = 1\n");

        // Every leaf of `[asr]` is confirmed-unknown, but `asr` is a serde
        // alias for the `asr-sidecar` section the writer emits. The file
        // still loading is the whole point of the check.
        assert!(!out.contains("bogus = 1"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());
    }

    /// The class of config every other fixture here misses: a section the
    /// schema *knows*, whose fields are all optional, holding nothing but
    /// typos. `[hotkeys] speek = "Super+R"` is the likeliest real-world shape
    /// of issue #116 and the one the first cut of the fix still ate: the
    /// section parses to `Some(default)`, deleting it would parse to `None`,
    /// and the whole-table guard read that as "not ignorable" and pruned the
    /// node, taking the keys and their comments with it. Worse, the rewritten
    /// file then reported *nothing* unknown, so even the warning stopped.
    #[test]
    fn a_typo_that_is_a_sections_only_key_survives_with_its_comments() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n\
             # pause music while I dictate\n\
             [hooks]\nmedia_autopause = true      # typo for media_auto_pause\n\n\
             # my hotkeys\n[hotkeys]\nbogus = \"Super+X\"\nspeek = \"Super+R\" # meant `speak`\n\n\
             [input]\npast = true\n\n\
             [llm]\nbogus = 1\n\n[tts]\nbogus = 2\n",
        );

        assert!(
            out.contains("media_autopause = true      # typo for media_auto_pause"),
            "{out}"
        );
        assert!(out.contains("# pause music while I dictate"), "{out}");
        assert!(
            out.contains(r#"speek = "Super+R" # meant `speak`"#),
            "{out}"
        );
        assert!(out.contains("# my hotkeys"), "{out}");
        assert!(out.contains(r#"bogus = "Super+X""#), "{out}");
        assert!(out.contains("past = true"), "{out}");
        assert!(out.contains("bogus = 1"), "{out}");
        assert!(out.contains("bogus = 2"), "{out}");

        // The file must still load, and the warning must keep naming the keys:
        // losing them silently also lost the only prompt to fix them.
        let _: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            unknown_config_keys(&out),
            vec![
                "hooks.media_autopause",
                "hotkeys.bogus",
                "hotkeys.speek",
                "input.past",
                "llm.bogus",
                "tts.bogus"
            ],
            "{out}"
        );
    }

    #[test]
    fn a_typo_in_a_nested_all_optional_section_survives() {
        let dir = tempfile::tempdir().unwrap();

        // As a standard table.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay.colors]\nbogos = 1\n",
        );
        assert!(out.contains("bogos = 1"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();

        // And as an inline table whose *every* key is unknown (`theme` is a
        // field of `[overlay]`, not of `colors`), which is what makes the
        // whole-table question apply one level down.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\ncolors = { theme = \"dark\", bogos = 1 }\n",
        );
        assert!(out.contains(r#"theme = "dark""#), "{out}");
        assert!(out.contains("bogos = 1"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_wholly_unknown_array_of_tables_survives() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[[bogus]]\nfoo = 1\n\n[[bogus]]\nfoo = 2\n",
        );

        // The array takes `is_preserved`'s `ArrayOfTables` arm, where the
        // confirmed-unknown *leaf* `bogus` carries the whole thing.
        assert_eq!(out.matches("[[bogus]]").count(), 2, "{out}");
        assert!(out.contains("foo = 1") && out.contains("foo = 2"), "{out}");
        let _: Config = toml::from_str(&out).unwrap();
    }

    #[test]
    fn a_wholly_unknown_alias_section_is_dropped_in_every_spelling() {
        let dir = tempfile::tempdir().unwrap();

        // `[vibevoice]` aliases `[asr-sidecar]` exactly as `[asr]` does.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[vibevoice]\nbogus = 1\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[vibevoice]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // Same section written as a root inline table, which reaches the
        // exemption through `value_is_preserved` instead of `is_preserved`.
        let out = rewrite(
            &dir,
            "asr = { bogus = 1 }\n\n[general]\nbackend = \"groq\"\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("asr = {"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // `[local]` aliases `[local-whisper]`, and since #137 gave
        // `model_path` a serde default it can be wholly unknown as well.
        let out = rewrite(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[local]\nbogus = 1\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[local]"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.local_whisper.is_some());

        // And its inline spelling, through `value_is_preserved`.
        let out = rewrite(
            &dir,
            "local = { bogus = 1 }\n\n[general]\nbackend = \"groq\"\n",
        );
        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("local = {"), "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.local_whisper.is_some());
    }

    /// The whole preserve rule as one table, driven through the real writer.
    ///
    /// The invariant that outranks every row: the rewritten file still
    /// deserializes into `Config`. A file that stops loading is total config
    /// loss, strictly worse than the key-eating bug this all exists to fix,
    /// so it is asserted for every row, including the ones that drop keys.
    #[test]
    fn the_preserve_matrix_holds_and_every_rewrite_still_loads() {
        struct Row {
            what: &'static str,
            contents: &'static str,
            /// Substrings the rewritten file must still contain.
            kept: &'static [&'static str],
            /// Substrings it must not contain.
            gone: &'static [&'static str],
            /// Substrings it must contain exactly once (no alias duplication).
            once: &'static [&'static str],
        }

        const GENERAL: &str = "[general]\nbackend = \"groq\"\n\n";
        let rows = [
            Row {
                what: "a typo that is a known section's only key",
                contents: "[hotkeys]\nspeek = \"Super+R\" # meant `speak`\n",
                kept: &["speek = \"Super+R\" # meant `speak`"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo in [hooks]",
                contents: "[hooks]\nmedia_autopause = true\n",
                kept: &["media_autopause = true"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "an all-unknown inline sub-table",
                contents: "[overlay]\ncolors = { theme = \"dark\", bogos = 1 }\n",
                kept: &["theme = \"dark\"", "bogos = 1"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo in a nested standard table",
                contents: "[overlay.colors]\nbogos = 1\n",
                kept: &["bogos = 1"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "typos in [llm] and [tts]",
                contents: "[llm]\nbogus = 1\n\n[tts]\nbogus = 2\n",
                kept: &["bogus = 1", "bogus = 2"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a wholly unknown alias section",
                contents: "[asr]\nbogus = 1\n",
                kept: &["[asr-sidecar]"],
                gone: &["bogus = 1", "[asr]\n"],
                once: &["[asr-sidecar]"],
            },
            Row {
                what: "the other alias for the same section",
                contents: "[vibevoice]\nbogus = 1\n",
                kept: &["[asr-sidecar]"],
                gone: &["bogus = 1", "[vibevoice]"],
                once: &["[asr-sidecar]"],
            },
            Row {
                what: "a mixed alias section",
                contents: "[local]\nmodel_path = \"/m.bin\"\nbogus = 1\n",
                kept: &["[local-whisper]", "/m.bin"],
                gone: &["bogus = 1", "[local]\n"],
                once: &["model_path"],
            },
            Row {
                what: "an unknown sub-table inside a known section",
                contents: "[hooks]\nmedia_auto_pause = true\n\n[hooks.bogus]\nx = 1\n",
                kept: &["[hooks.bogus]", "x = 1", "media_auto_pause = true"],
                gone: &[],
                once: &["[hooks.bogus]"],
            },
            Row {
                what: "a wholly unknown section with a nested one",
                contents: "[bogus]\nfoo = 1\n\n[bogus.nested]\nbar = 2\n",
                kept: &["[bogus]", "foo = 1", "[bogus.nested]", "bar = 2"],
                gone: &[],
                once: &[],
            },
            Row {
                what: "a typo beside real keys",
                contents: "[input]\npast = true\n",
                kept: &["past = true"],
                gone: &[],
                once: &["paste = "],
            },
            Row {
                what: "a hotkey alias",
                contents: "[hotkeys]\nread = \"Super+R\"\n",
                kept: &["speak = \"Super+R\""],
                gone: &["read = "],
                once: &["speak = "],
            },
        ];

        let dir = tempfile::tempdir().unwrap();
        for row in rows {
            let out = rewrite(&dir, &format!("{GENERAL}{}", row.contents));
            for needle in row.kept {
                assert!(out.contains(needle), "{}: lost `{needle}`\n{out}", row.what);
            }
            for needle in row.gone {
                assert!(
                    !out.contains(needle),
                    "{}: kept `{needle}`\n{out}",
                    row.what
                );
            }
            for needle in row.once {
                assert_eq!(
                    out.matches(needle).count(),
                    1,
                    "{}: `{needle}` is not written exactly once\n{out}",
                    row.what
                );
            }
            toml::from_str::<Config>(&out)
                .unwrap_or_else(|e| panic!("{}: rewrite no longer loads: {e}\n{out}", row.what));

            // And the write after that (a `set_hotkey` press, say) must not
            // perturb a byte: a preserved key that moves or re-renders would
            // churn the file on every save.
            let path = dir.path().join("config.toml");
            write_config_to(&parse_config(&out), &path).unwrap();
            assert_eq!(
                out,
                fs::read_to_string(&path).unwrap(),
                "{}: the second write is not byte-stable",
                row.what
            );
        }
    }

    /// One `{}` inside an unknown section used to delete the whole section.
    ///
    /// An empty table contributes no leaf, so `PreservedKeys` has no node for
    /// it, `covers` read the missing node as "not confirmed-unknown", and the
    /// writer dropped `[bogus]` entire: the comment above it and both real
    /// keys with it. And because the rewritten file no longer held those keys,
    /// `unknown_config_keys` went quiet too: the tool warned about the data
    /// once, ate it, and then stopped mentioning it. That is issue #116's exact
    /// symptom, shipped by the fix for issue #116, and the write is byte-stable
    /// afterwards so nothing ever notices.
    #[test]
    fn a_stray_empty_table_does_not_delete_the_section_around_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n\
             # my notes about this section\n\
             [bogus]\nimportant = \"please keep me\"      # user data\n\
             other = 42\nempty_table = {}\n",
        );

        assert!(out.contains("# my notes about this section"), "{out}");
        assert!(
            out.contains("important = \"please keep me\"      # user data"),
            "{out}"
        );
        assert!(out.contains("other = 42"), "{out}");
        // The warning has to keep firing: losing the keys also lost the only
        // prompt to fix them.
        assert_eq!(
            unknown_config_keys(&out),
            vec!["bogus.important", "bogus.other"],
            "{out}"
        );
    }

    /// Every spelling of "a table with no leaves in it", and the blast radius
    /// each one used to have. All of these lost data before the vacuous rule in
    /// `subtree_is_preserved`.
    #[test]
    fn a_leafless_table_never_vetoes_its_enclosing_subtree() {
        let dir = tempfile::tempdir().unwrap();

        // A bare sub-table header with no keys under it.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[bogus]\nimportant = \"keep\"\n\n[bogus.emptysub]\n",
        );
        assert!(out.contains(r#"important = "keep""#), "{out}");
        assert!(out.contains("[bogus.emptysub]"), "{out}");

        // An empty table nested inside an inline one, two levels down.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[bogus]\nimportant = \"keep\"\na = { b = {} }\n",
        );
        assert!(out.contains(r#"important = "keep""#), "{out}");
        assert!(out.contains("a = { b = {} }"), "{out}");

        // An unknown subtree of a *known* section: `[tts]` and `[overlay]` are
        // in `fresh`, so the merge recurses into them and the stale filter sees
        // the sub-table on its own.
        for section in ["tts", "overlay"] {
            let out = rewrite_checked(
                &dir,
                &format!(
                    "[general]\nbackend = \"groq\"\n\n[{section}.bogus]\nkeep_me = 1\nt = {{}}\n"
                ),
            );
            assert!(out.contains("keep_me = 1"), "{section}: {out}");
            assert_eq!(
                unknown_config_keys(&out),
                vec![format!("{section}.bogus.keep_me")],
                "{section}: {out}"
            );
        }

        // Blast radius: the top-most unknown table went, its siblings did not.
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[wipeme]\nx = 1\nt = {}\n\n[keepme]\ny = 2\n",
        );
        assert!(out.contains("[wipeme]") && out.contains("x = 1"), "{out}");
        assert!(out.contains("[keepme]") && out.contains("y = 2"), "{out}");
    }

    /// The other side of the vacuous rule: it is deliberately not applied by
    /// the top-level stale filter, so an empty table that is a *direct* key of
    /// a known section still goes. Keeping it there would mean keeping an empty
    /// `[asr]` too, beside the canonical `[asr-sidecar]` the writer emits, and
    /// serde rejects that file with `duplicate field`.
    #[test]
    fn an_empty_table_beside_a_known_sections_keys_is_still_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[overlay]\nbogus_tbl = {}\nother_bogus = 3\n",
        );

        assert!(!out.contains("bogus_tbl"), "{out}");
        assert!(out.contains("other_bogus = 3"), "{out}");
    }

    /// The vacuous rule must not weaken the alias guard, which is a separate
    /// condition (`table_prunable`, not `covers`). An empty sub-table now lets
    /// `[asr]` *cover* itself, so the whole-table question is asked where it
    /// previously was not, and the answer still has to be "no".
    #[test]
    fn an_alias_section_with_an_empty_subtable_is_still_dropped_whole() {
        let dir = tempfile::tempdir().unwrap();
        let out = rewrite_checked(
            &dir,
            "[general]\nbackend = \"groq\"\n\n[asr]\nbogus = 1\n\n[asr.empty]\n",
        );

        assert!(!out.contains("bogus = 1"), "{out}");
        assert!(!out.contains("[asr]"), "{out}");
        assert!(!out.contains("[asr.empty]"), "{out}");
        assert_eq!(out.matches("[asr-sidecar]").count(), 1, "{out}");
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert!(reparsed.asr_sidecar.is_some());

        // The shape that decides where the vacuous rule may be applied: an
        // aliased section that is *itself* leafless, beside an unknown key
        // elsewhere so the preserve set is non-empty and the stale filter
        // actually runs. Treating leafless tables as preserved at the top level
        // would keep this one next to the `[asr-sidecar]` the writer emits, and
        // serde rejects that with `duplicate field`; the daemon then falls
        // back to defaults and the user's whole config is gone.
        for contents in [
            // As a header, as a bare sub-table header, and as a root inline
            // table (which has to come before any section header to stay at the
            // root rather than becoming a key of `[general]`).
            "[general]\nbackend = \"groq\"\n\n[asr]\n\n[bogus]\nx = 1\n",
            "[general]\nbackend = \"groq\"\n\n[asr.empty]\n\n[bogus]\nx = 1\n",
            "asr = {}\n\n[general]\nbackend = \"groq\"\n\n[bogus]\nx = 1\n",
        ] {
            let spelling = contents;
            let out = rewrite_checked(&dir, contents);
            assert!(!out.contains("asr ="), "{spelling}: {out}");
            assert!(!out.contains("[asr]"), "{spelling}: {out}");
            assert!(!out.contains("[asr.empty]"), "{spelling}: {out}");
            assert_eq!(out.matches("[asr-sidecar]").count(), 1, "{spelling}: {out}");
            // The unrelated unknown section is untouched, so the drop above is
            // the alias guard doing its job, not the preserve set being empty.
            assert!(out.contains("[bogus]") && out.contains("x = 1"), "{out}");
        }
    }

    /// `overlay` is the only section with a sub-section, so it is the only one
    /// whose *root inline* spelling made `merge_item` bail to the catch-all
    /// (the fresh table is not flat) and replace the user's whole table with
    /// the fresh one, unknown keys and all. Every other section round-tripped
    /// this shape fine, which is why it took a nested fixture to find.
    #[test]
    fn a_root_inline_section_with_a_nested_inline_table_keeps_its_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();

        let out = rewrite_checked(
            &dir,
            "overlay = { theme = \"dark\", colors = { ring = \"#fff\", bogos = 1 } }\n\n\
             [general]\nbackend = \"groq\"\n",
        );
        assert!(out.contains("bogos = 1"), "{out}");
        assert!(out.contains(r##"ring = "#fff""##), "{out}");
        assert_eq!(
            unknown_config_keys(&out),
            vec!["overlay.colors.bogos"],
            "{out}"
        );
        // The user's inline spelling survives, so no header is synthesized,
        // which also disposes of the `[overlay ]` the conversion used to emit,
        // key decor and all.
        assert!(!out.contains("[overlay"), "{out}");

        // An unknown scalar in the outer inline table alongside the nested one.
        let out = rewrite_checked(
            &dir,
            "overlay = { theme = \"dark\", ov_bogus = 2, colors = { bogos = 3 } }\n\n\
             [general]\nbackend = \"groq\"\n",
        );
        assert!(out.contains("ov_bogus = 2"), "{out}");
        assert!(out.contains("bogos = 3"), "{out}");
        assert_eq!(
            unknown_config_keys(&out),
            vec!["overlay.colors.bogos", "overlay.ov_bogus"],
            "{out}"
        );
    }

    #[test]
    fn load_existing_config_from_reports_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_unknown_key_fixture(&dir);

        let ExistingConfig::Loaded { config, unknown } = load_existing_config_from(&path) else {
            panic!("fixture loads");
        };
        assert_eq!(config.general.backend, "groq");
        assert_eq!(
            unknown,
            vec![
                "bogus.foo",
                "bogus.nested.bar",
                "deepgram.bogus",
                "input.past",
                "llm_commands[1].stray",
            ]
        );

        // A path with nothing at it is `Missing`, and only that: the callers
        // key their "start from defaults" behavior off this variant alone
        // (issue #134).
        assert!(matches!(
            load_existing_config_from(&dir.path().join("absent.toml")),
            ExistingConfig::Missing
        ));
    }

    /// Issue #134: this file used to be indistinguishable from no file at all,
    /// so `whisrs config` opened on defaults and saved over it.
    #[test]
    fn config_that_does_not_deserialize_is_unusable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, CONFIG_THAT_DOES_NOT_DESERIALIZE).unwrap();

        let ExistingConfig::Unusable { kind, message: msg } = load_existing_config_from(&path)
        else {
            panic!("valid TOML that does not deserialize must be reported as unusable");
        };
        // Read fine, wrong shape: regenerating over it works, so the flows may
        // offer `whisrs setup`.
        assert_eq!(kind, UnusableKind::Undeserializable);
        // The path, so the user knows which file to open...
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        // ...and the serde error verbatim, which is the only thing that names
        // the offending key and value.
        assert!(
            msg.contains(r#"invalid type: string "2000", expected u64"#),
            "{msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_config_is_unusable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, COMMENTED_CONFIG).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode bits and there is no portable way to make a
        // file unreadable to root, so skip rather than assert something that
        // only holds for an unprivileged user.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        let ExistingConfig::Unusable { kind, message: msg } = load_existing_config_from(&path)
        else {
            panic!("an unreadable file must not be reported as missing");
        };
        // And not as a shape problem either: this is the kind no rewrite can
        // fix, so both flows must refuse rather than send the user to
        // `whisrs setup` (issue #134).
        assert_eq!(kind, UnusableKind::Unreadable);
        assert!(msg.contains("cannot read config at"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    /// A directory at the config path reads as `Unreadable` too, not as a
    /// missing file: `read_to_string` fails with `IsADirectory`, and only
    /// `NotFound` is `Missing`.
    #[test]
    fn a_directory_at_the_config_path_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::create_dir(&path).unwrap();

        let ExistingConfig::Unusable { kind, message } = load_existing_config_from(&path) else {
            panic!("a directory at the config path must not be reported as missing");
        };
        assert_eq!(kind, UnusableKind::Unreadable);
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    /// These messages are the whole of what the user gets on this path, so pin
    /// their contents: the reason first and verbatim, the path to edit, the
    /// `.bak` the file lands in, and (for `whisrs config`) that nothing was
    /// touched.
    #[test]
    fn undeserializable_config_messages_name_the_path_the_backup_and_the_way_out() {
        let config_path = Path::new("/home/u/.config/whisrs/config.toml");
        let reason = "cannot parse config at /home/u/.config/whisrs/config.toml: \
                      invalid type: string \"2000\", expected u64";

        let refusal = unusable_config_refusal(UnusableKind::Undeserializable, reason, config_path);
        assert!(refusal.starts_with(reason), "{refusal}");
        assert!(refusal.contains("Nothing was changed"), "{refusal}");
        assert!(
            refusal.contains("Fix /home/u/.config/whisrs/config.toml by hand"),
            "{refusal}"
        );
        assert!(
            refusal.contains("/home/u/.config/whisrs/config.toml.bak"),
            "{refusal}"
        );
        // Regenerating over this file works, so the way out is offered.
        assert!(refusal.contains("whisrs setup"), "{refusal}");

        let warning = undeserializable_config_setup_warning(reason, config_path);
        assert!(warning.starts_with(reason), "{warning}");
        assert!(
            warning.contains("/home/u/.config/whisrs/config.toml.bak"),
            "{warning}"
        );
    }

    /// The other kind (issue #134): a file whose bytes cannot be read is one
    /// `write_config_to` cannot read either, so the one thing this message must
    /// never do is send the user to `whisrs setup`: that path is guaranteed to
    /// fail on the same read, after the whole wizard. It has to name the likely
    /// cause and the hand fix instead.
    #[test]
    fn the_unreadable_refusal_names_the_hand_fix_and_never_whisrs_setup() {
        let config_path = Path::new("/home/u/.config/whisrs/config.toml");
        let reason = "cannot read config at /home/u/.config/whisrs/config.toml: \
                      Permission denied (os error 13)";

        let refusal = unreadable_config_refusal(reason, config_path);
        assert!(refusal.starts_with(reason), "{refusal}");
        assert!(refusal.contains("Nothing was changed"), "{refusal}");
        assert!(
            refusal.contains("rewriting the file cannot help"),
            "{refusal}"
        );
        // The cause and the remedy, concretely.
        assert!(refusal.contains("ownership or permissions"), "{refusal}");
        assert!(
            refusal.contains("chown $USER /home/u/.config/whisrs/config.toml"),
            "{refusal}"
        );
        assert!(
            refusal.contains("chmod 600 /home/u/.config/whisrs/config.toml"),
            "{refusal}"
        );
        // And never the way out that cannot work. The text does name `whisrs
        // setup`, to say why it cannot help, so the absence of one phrasing
        // proves nothing: "try `whisrs setup` instead" would slip past it.
        // Assert the explanation is present instead: every sentence mentioning
        // the command has to be the one ruling it out.
        assert!(
            refusal.contains("`whisrs setup` writes the config by reading it first"),
            "the refusal must say why `whisrs setup` cannot help: {refusal}"
        );
        assert_eq!(
            refusal.matches("whisrs setup").count(),
            2,
            "every mention of `whisrs setup` must be part of ruling it out; a new one \
             is a way out that fails on the same read: {refusal}"
        );
        assert!(
            !refusal.contains(".bak"),
            "nothing gets backed up on this path, so promising a .bak is a lie: {refusal}"
        );

        // `whisrs config` refuses with exactly this text, so the two cannot
        // drift apart.
        assert_eq!(
            unusable_config_refusal(UnusableKind::Unreadable, reason, config_path),
            refusal
        );
    }
}
