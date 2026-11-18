use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::write;
use crate::Config;

pub fn vocabulary_path() -> PathBuf {
    crate::config_path().with_file_name("vocabulary.txt")
}

/// One term per line; blank lines and `#` comment lines are skipped.
///
/// A leading UTF-8 BOM is stripped from the whole file, not per line, because
/// that is the only place it can appear and because stripping it here is what
/// makes a BOM-only file yield nothing. `str::trim` does not remove U+FEFF, so
/// without this an editor that writes a BOM shipped `\u{feff}NixOS` to Deepgram
/// as a keyterm that can never match, and a BOM-only file produced one bogus
/// single-character term.
///
/// A single leading `\` is an escape and is stripped after the comment check,
/// so [`write_vocabulary_file`] can store a term that begins with `#` (or with
/// `\`) without the parser reading it back as a comment. That is the whole
/// grammar: `\` anywhere else, and `#` anywhere but the first non-whitespace
/// column, are literal — `C# dev` stays one term.
pub fn parse_vocabulary(contents: &str) -> Vec<String> {
    contents
        .strip_prefix('\u{feff}')
        .unwrap_or(contents)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.strip_prefix('\\').unwrap_or(line))
        // A lone `\` unescapes to nothing, which is not a term.
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn load_vocabulary_file(path: &Path) -> io::Result<Option<Vec<String>>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(parse_vocabulary(&contents))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Case-sensitive: Deepgram returns each keyterm in the casing configured.
pub fn merge_vocabulary(config_terms: Vec<String>, file_terms: Vec<String>) -> Vec<String> {
    let mut merged = Vec::with_capacity(config_terms.len() + file_terms.len());
    for term in config_terms.into_iter().chain(file_terms) {
        if !merged.contains(&term) {
            merged.push(term);
        }
    }
    merged
}

/// One term per line, 0600 like the config it was split out of.
///
/// A term whose first non-whitespace character is `#` or `\` is written with a
/// single extra leading `\`, which [`parse_vocabulary`] strips back off. Without
/// that escape the migration destroyed such a term outright: `#1` was written as
/// a line the parser reads as a comment, so it vanished from `vocabulary.txt`
/// while `[general] vocabulary` was blanked in the same save — gone from both
/// stores, with no warning. Nothing else is escaped, so `C# dev` is written
/// as-is.
///
/// The file sits next to `config.toml` and now holds vocabulary that used to
/// live inside it, so it is written the way config.toml is: atomically (a
/// 0600 temp file renamed over it, so a crash never leaves it half written)
/// rather than through `fs::write`, which leaves it world-readable under the
/// usual umask.
///
/// A symlink (stow, home-manager) is written through, to its target: a rename
/// over the link itself would replace it with a regular file.
pub fn write_vocabulary_file(path: &Path, terms: &[String]) -> io::Result<()> {
    let mut contents = String::new();
    for term in terms {
        if matches!(term.trim_start().chars().next(), Some('#') | Some('\\')) {
            contents.push('\\');
        }
        contents.push_str(term);
        contents.push('\n');
    }
    write::atomic_write(&link_target(path), &contents)
}

/// Where a write to `path` lands: the file a symlink points to (a dangling
/// one too), or `path` itself.
fn link_target(path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return path.to_path_buf();
    }
    std::fs::canonicalize(path).unwrap_or_else(|_| match std::fs::read_link(path) {
        Ok(target) => match path.parent() {
            Some(dir) => dir.join(target),
            None => target,
        },
        Err(_) => path.to_path_buf(),
    })
}

/// Where the vocabulary stood when this `whisrs config` session started.
///
/// Two lists, because the editor works on the merged view while config.toml
/// only ever held its own half. A save that did not touch the vocabulary has to
/// put `config_toml` back, or the file's terms leak into config.toml and end up
/// stored twice.
pub struct VocabularyBaseline {
    /// `vocabulary.txt` exists and was read successfully. False when it is
    /// missing (feature not opted into) or unreadable, in which case the file
    /// is never written and vocabulary edits stay in config.toml.
    use_file: bool,
    /// `[general] vocabulary` as read from config.toml, before the merge.
    config_toml: Vec<String>,
    /// The merged list the vocabulary editor started from.
    merged: Vec<String>,
}

impl VocabularyBaseline {
    /// Merge the `vocabulary.txt` at `path` into `config`'s vocabulary, the
    /// view an editor works on, and record where both stores stood.
    ///
    /// A file that cannot be read comes back as the error beside the baseline,
    /// for the caller to show: its terms are left out, and vocabulary edits
    /// stay in config.toml, as with no file at all.
    pub fn load(config: &mut Config, path: &Path) -> (Self, Option<io::Error>) {
        let config_toml = config.general.vocabulary.clone();
        let (use_file, unreadable) = match load_vocabulary_file(path) {
            Ok(Some(terms)) => {
                config.general.vocabulary =
                    merge_vocabulary(std::mem::take(&mut config.general.vocabulary), terms);
                (true, None)
            }
            Ok(None) => (false, None),
            Err(e) => (false, Some(e)),
        };
        let baseline = Self {
            use_file,
            config_toml,
            merged: config.general.vocabulary.clone(),
        };
        (baseline, unreadable)
    }

    /// Whether `vocabulary.txt` is in play: it exists and was read.
    pub fn uses_file(&self) -> bool {
        self.use_file
    }
}

/// Whether this save should move the vocabulary into `vocabulary.txt`.
///
/// The migration is gated on an actual edit, not just on the file existing.
/// Rewriting on every save made two unrelated saves destructive: it blanked
/// `[general] vocabulary` in config.toml the first time anyone changed an
/// unrelated setting (so deleting `vocabulary.txt` later lost the terms), and
/// it flattened a hand-maintained `vocabulary.txt`, dropping its comments and
/// grouping, for a save that had nothing to do with vocabulary.
///
/// Gating on the edit is also what makes an empty `vocabulary.txt` behave. An
/// empty file is a legitimate way to opt in, and it stays untouched until the
/// user actually adds a term, at which point the file is written non-empty,
/// which is what the daemon-side merge requires.
///
/// The comparison is made on both lists *after* [`normalized_vocabulary`], so
/// it is like-for-like.
fn should_migrate_vocabulary(
    use_vocab_file: bool,
    baseline: &[String],
    current: &[String],
) -> bool {
    use_vocab_file && normalized_vocabulary(baseline) != normalized_vocabulary(current)
}

/// A vocabulary list as the editor would hand it back.
///
/// The baseline is the raw merged list; anything that came out of "Vocabulary &
/// prompt" has been through `join(", ")` and [`parse_csv_list`], and that round
/// trip is not the identity: it trims padding and splits on commas. Comparing
/// the two directly made an *unedited* pass through the editor look like an
/// edit: a config.toml term with padding (`vocabulary = [" whisrs "]`, which is
/// exactly what the templated-config setups this feature exists for produce) or
/// with a comma in it fired the destructive migration when the user opened the
/// section to change only the prompt and pressed Enter over the vocabulary
/// line. Normalizing both sides removes the difference the editor itself
/// introduced, leaving only the differences the user actually made.
///
/// The transform is idempotent (its output has no commas, no padding and no
/// empty entries) so normalizing a list the editor already produced is a
/// no-op, and the never-opened-the-editor path compares equal too.
fn normalized_vocabulary(terms: &[String]) -> Vec<String> {
    parse_csv_list(&terms.join(", "))
}

/// What `[general] vocabulary` config.toml should hold after this save.
///
/// The editor works on the merged view, so writing the in-memory list straight
/// out would copy `vocabulary.txt`'s terms into config.toml and store every one
/// of them twice. On a migration config.toml is emptied, since the file is now
/// the single store. On an untouched vocabulary it gets back exactly the half
/// it started with. With no `vocabulary.txt` in play there was no merge, so the
/// edited list is written as-is, which is the pre-feature behavior.
fn config_toml_vocabulary(vocab: &VocabularyBaseline, current: &[String]) -> Vec<String> {
    if should_migrate_vocabulary(vocab.use_file, &vocab.merged, current) {
        Vec::new()
    } else if vocab.use_file {
        vocab.config_toml.clone()
    } else {
        current.to_vec()
    }
}

/// Everything a save does about the vocabulary, decided in one place.
///
/// The two halves are not independent (blanking `[general] vocabulary` is only
/// safe *because* the same save wrote the terms to `vocabulary.txt`), so they
/// are computed together and handed to the writer as a finished plan. Splitting
/// the decision across the call site is what let a one-line slip there (write
/// the blanked list to the file, skip the file write, write the merged list to
/// config.toml) destroy every term the user had with every unit test still
/// green.
#[derive(Debug, PartialEq, Eq)]
struct VocabularySavePlan {
    /// The list `[general] vocabulary` is written with.
    config_toml: Vec<String>,
    /// The list `vocabulary.txt` is written with, or `None` to leave the file
    /// exactly as it is (including its comments and grouping).
    vocabulary_txt: Option<Vec<String>>,
}

/// Decide both halves of the save. Pure: it touches no disk.
fn vocabulary_save_plan(vocab: &VocabularyBaseline, current: &[String]) -> VocabularySavePlan {
    VocabularySavePlan {
        config_toml: config_toml_vocabulary(vocab, current),
        vocabulary_txt: should_migrate_vocabulary(vocab.use_file, &vocab.merged, current)
            .then(|| current.to_vec()),
    }
}

/// Execute a [`VocabularySavePlan`] and write config.toml, in that order.
///
/// The only place either store is written on save. The Config screen's save
/// calls this and does nothing else with the vocabulary, so there is no second
/// copy of the decision for a call-site edit to get wrong.
///
/// Ordering: `vocabulary.txt` goes first because it is the direction that
/// cannot lose a term. If config.toml then fails to write, every term the user
/// kept is still on disk somewhere: the new list in `vocabulary.txt`, the old
/// one still in config.toml. What that does *not* guarantee is that the save
/// took effect: on a deletion config.toml keeps the removed term, and the next
/// daemon start merges it straight back in. The error context says which half
/// landed so the user is not left thinking the save rolled back cleanly.
pub fn write_config_and_vocabulary(
    config: &Config,
    vocab: &VocabularyBaseline,
    config_path: &Path,
    vocab_path: &Path,
) -> Result<Wrote> {
    let plan = vocabulary_save_plan(vocab, &config.general.vocabulary);

    let mut vocabulary_written = None;
    if let Some(terms) = &plan.vocabulary_txt {
        write_vocabulary_file(vocab_path, terms)
            .with_context(|| format!("failed to write {}", vocab_path.display()))?;
        vocabulary_written = Some(vocab_path.to_path_buf());
    }

    let mut on_disk = config.clone();
    on_disk.general.vocabulary = plan.config_toml;

    match write::write_config_noting(&on_disk, config_path) {
        Ok(regenerated) => Ok(Wrote {
            vocabulary: vocabulary_written,
            regenerated,
        }),
        Err(e) => Err(match &vocabulary_written {
            Some(path) => e.context(format!(
                "failed to write config: your vocabulary edits were already saved to {}, \
                 but config.toml was not updated",
                path.display()
            )),
            None => e.context("failed to write config"),
        }),
    }
}

/// What [`write_config_and_vocabulary`] wrote besides config.toml.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wrote {
    /// `vocabulary.txt`, when the save wrote it.
    pub vocabulary: Option<PathBuf>,
    /// config.toml could not be merged into, and was written from scratch.
    /// Not printed: the caller owns the screen.
    pub regenerated: Option<write::Regenerated>,
}

/// Parse a comma-separated list, trimming whitespace and dropping empty entries.
pub(crate) fn parse_csv_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_file_lives_next_to_config_toml() {
        let vocab = vocabulary_path();
        assert_eq!(vocab.file_name().unwrap(), "vocabulary.txt");
        assert_eq!(vocab.parent(), crate::config_path().parent());
    }

    /// One term per line: blank and `#` comment lines skipped, every line
    /// trimmed, a `#` inside a term kept. A leading BOM is stripped from the
    /// whole file: `str::trim` keeps U+FEFF, so it would ride into the first
    /// term, and a BOM-only file would leave a one-character term.
    #[test]
    fn parse_reads_one_term_per_line() {
        let rows: &[(&str, &str, &[&str])] = &[
            (
                "blanks, comments and padding",
                "\
# Deepgram nova-3 keyterms: this casing comes back.
whisrs

  Claude Code  \n\
\t
NixOS
# trailing comment
",
                &["whisrs", "Claude Code", "NixOS"],
            ),
            ("a # inside a term", "C# dev\n", &["C# dev"]),
            ("empty file", "", &[]),
            ("comments only", "# only a comment\n\n", &[]),
            (
                "leading BOM",
                "\u{feff}NixOS\nwhisrs\n",
                &["NixOS", "whisrs"],
            ),
            ("BOM only", "\u{feff}", &[]),
            ("BOM and a newline", "\u{feff}\n", &[]),
        ];
        for (row, contents, expected) in rows {
            assert_eq!(parse_vocabulary(contents), *expected, "{row}");
        }
    }

    /// config.toml's terms first, then the file's, each once. Case-sensitive:
    /// Deepgram returns each keyterm in the casing configured.
    #[test]
    fn merge_puts_config_first_and_drops_duplicates() {
        for (row, config, file, expected) in [
            (
                "config first, duplicates dropped",
                terms(&["whisrs", "GNOME"]),
                terms(&["Deepgram", "whisrs", "NixOS"]),
                terms(&["whisrs", "GNOME", "Deepgram", "NixOS"]),
            ),
            (
                "case-sensitive",
                terms(&["whisrs"]),
                terms(&["Whisrs"]),
                terms(&["whisrs", "Whisrs"]),
            ),
        ] {
            assert_eq!(merge_vocabulary(config, file), expected, "{row}");
        }
    }

    /// The property the migration depends on: whatever the writer puts on disk,
    /// the parser reads back as the same list. Before the `\` escape, `#1` was
    /// written as a comment line and dropped — and since the same save blanks
    /// `[general] vocabulary`, the term ended up in neither store.
    ///
    /// The corpus is deliberately adversarial about the two characters the
    /// format gives meaning to, plus ordinary terms that must stay untouched.
    #[test]
    fn write_then_parse_round_trips_every_term() {
        let terms: Vec<String> = [
            "#1",
            "#rust",
            "\\#literal",
            "\\backslash",
            "\\",
            "  #padded",
            "C# dev",
            "Claude Code",
            "NixOS",
            "whisrs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");
        write_vocabulary_file(&path, &terms).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            parse_vocabulary(&written),
            terms,
            "written form did not round-trip:\n{written}"
        );
    }

    /// The two terms that do *not* survive the round trip, pinned so the gap is
    /// deliberate rather than discovered later: an empty term and a
    /// whitespace-only term are both dropped, because the parser trims each
    /// line and skips blanks.
    ///
    /// Nothing downstream wants them either. `usable_keyterms`
    /// (`src/transcription/deepgram.rs`) trims and drops blanks before any
    /// counting or budgeting, so `Config::validate` and the request builder
    /// both ignore them; the `whisrs config` list editor drops a blank item.
    /// A blank entry is not a term anywhere in whisrs. An empty list still
    /// writes the file, which loads as no terms, not as a missing file.
    #[test]
    fn empty_and_whitespace_only_terms_are_dropped_not_round_tripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");
        let terms = vec![
            String::new(),
            "   ".to_string(),
            "\t".to_string(),
            "NixOS".to_string(),
        ];
        write_vocabulary_file(&path, &terms).unwrap();

        assert_eq!(
            load_vocabulary_file(&path).unwrap(),
            Some(vec!["NixOS".to_string()])
        );

        write_vocabulary_file(&path, &[]).unwrap();
        assert_eq!(load_vocabulary_file(&path).unwrap(), Some(Vec::new()));
    }

    /// A `#` term written by the migration is a term, not a comment, and a
    /// hand-written comment is still a comment.
    #[test]
    fn a_leading_hash_term_is_escaped_and_comments_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");
        write_vocabulary_file(&path, &["#1".to_string(), "NixOS".to_string()]).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\\#1\nNixOS\n");
        assert_eq!(
            parse_vocabulary("# a real comment\n\\#1\nNixOS\n"),
            vec!["#1", "NixOS"]
        );
    }

    /// The file is replaced, not truncated and rewritten: a new inode is
    /// renamed over the old one, so a crash mid-write leaves the old list
    /// whole, and no temp file stays behind. It holds what lived in the 0600
    /// config.toml, so one that drifted to 0644 comes out 0600.
    #[cfg(unix)]
    #[test]
    fn the_file_is_replaced_atomically() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();
        write_vocabulary_file(&path, &["new".to_string()]).unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_ne!(after.ino(), before, "the file was rewritten in place");
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["vocabulary.txt"]);
    }

    /// A symlinked file (stow, home-manager) stays a symlink: the new list
    /// lands in its target, atomically and 0600.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_written_through() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dotfiles")).unwrap();
        let target = dir.path().join("dotfiles/vocabulary.txt");
        std::fs::write(&target, "old\n").unwrap();
        let link = dir.path().join("vocabulary.txt");
        std::os::unix::fs::symlink("dotfiles/vocabulary.txt", &link).unwrap();

        write_vocabulary_file(&link, &["new".to_string()]).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A dangling one gets its target created.
        std::fs::remove_file(&target).unwrap();
        write_vocabulary_file(&link, &["again".to_string()]).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "again\n");
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn baseline(use_file: bool, config_toml: &[&str], file: &[&str]) -> VocabularyBaseline {
        VocabularyBaseline {
            use_file,
            config_toml: terms(config_toml),
            merged: merge_vocabulary(terms(config_toml), terms(file)),
        }
    }

    /// A config whose `[general] vocabulary` is `list`.
    fn config_with(list: &[&str]) -> Config {
        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.vocabulary = terms(list);
        config
    }

    /// `load` is what the editor starts from: with a file, the merged view in
    /// the config and both halves in the baseline; without one, or with one
    /// it cannot read, config.toml's list alone and no file in play.
    #[test]
    fn load_merges_the_file_and_remembers_both_stores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");
        std::fs::write(&path, "# notes\nNixOS\nwhisrs\n").unwrap();
        let mut config = config_with(&["whisrs", "Hyprland"]);
        let (vocab, unreadable) = VocabularyBaseline::load(&mut config, &path);
        assert!(unreadable.is_none());
        assert!(vocab.uses_file());
        assert_eq!(
            config.general.vocabulary,
            terms(&["whisrs", "Hyprland", "NixOS"])
        );
        assert_eq!(vocab.config_toml, terms(&["whisrs", "Hyprland"]));
        assert_eq!(vocab.merged, config.general.vocabulary);

        let mut config = config_with(&["whisrs"]);
        let (vocab, unreadable) =
            VocabularyBaseline::load(&mut config, &dir.path().join("absent.txt"));
        assert!(unreadable.is_none());
        assert!(!vocab.uses_file());
        assert_eq!(config.general.vocabulary, terms(&["whisrs"]));

        // A directory at the path reads as an error, not as a missing file.
        let unreadable_path = dir.path().join("dir.txt");
        std::fs::create_dir(&unreadable_path).unwrap();
        let mut config = config_with(&["whisrs"]);
        let (vocab, unreadable) = VocabularyBaseline::load(&mut config, &unreadable_path);
        assert!(unreadable.is_some());
        assert!(!vocab.uses_file());
        assert_eq!(config.general.vocabulary, terms(&["whisrs"]));
        assert_eq!(
            vocabulary_save_plan(&vocab, &terms(&["whisrs", "NixOS"])).vocabulary_txt,
            None,
            "an unreadable file is never written"
        );
    }

    /// One row of the save-time state table: what the two stores were, what the
    /// editor ended up holding, and what each store must be written with.
    struct PlanRow {
        what: &'static str,
        /// `vocabulary.txt` exists and was read.
        use_file: bool,
        /// `[general] vocabulary` as config.toml held it at startup.
        config_toml: &'static [&'static str],
        /// The terms `vocabulary.txt` held at startup.
        file: &'static [&'static str],
        /// The list in memory when the user hit "Save & exit".
        current: &'static [&'static str],
        /// What `[general] vocabulary` must be written with.
        expect_config_toml: &'static [&'static str],
        /// What `vocabulary.txt` must be written with, `None` to leave it be.
        expect_file: Option<&'static [&'static str]>,
    }

    /// The whole save-time decision, one row per reachable state.
    ///
    /// This is the table [`vocabulary_save_plan`] exists to satisfy. It is the
    /// only decision the save makes about the vocabulary, so a slip in
    /// `write_config_and_vocabulary` has nothing to reinterpret; see
    /// `a_migrating_save_writes_the_whole_list_to_the_file_and_blanks_config_toml`
    /// for the execution half.
    #[test]
    fn the_vocabulary_save_plan_covers_every_state() {
        let rows = [
            PlanRow {
                what: "no vocabulary.txt, vocabulary untouched",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "no vocabulary.txt, term added, stays in config.toml",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &["whisrs", "NixOS"],
                expect_file: None,
            },
            PlanRow {
                what: "no vocabulary.txt, list cleared",
                use_file: false,
                config_toml: &["whisrs"],
                file: &[],
                current: &[],
                expect_config_toml: &[],
                expect_file: None,
            },
            PlanRow {
                what: "vocabulary.txt present, vocabulary untouched, neither store moves",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "term added: the whole merged list migrates to the file",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs", "NixOS", "Deepgram"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs", "NixOS", "Deepgram"]),
            },
            PlanRow {
                what: "term removed: the file gets the survivors, config.toml is blanked",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["whisrs"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs"]),
            },
            PlanRow {
                what: "reordered: order is part of the list (Deepgram budgets in order)",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["NixOS", "whisrs"],
                expect_config_toml: &[],
                expect_file: Some(&["NixOS", "whisrs"]),
            },
            PlanRow {
                what: "case-only change: Deepgram echoes the casing configured",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &["Whisrs", "NixOS"],
                expect_config_toml: &[],
                expect_file: Some(&["Whisrs", "NixOS"]),
            },
            PlanRow {
                what: "everything deleted: the file is written empty, not left holding terms",
                use_file: true,
                config_toml: &["whisrs"],
                file: &["NixOS"],
                current: &[],
                expect_config_toml: &[],
                expect_file: Some(&[]),
            },
            PlanRow {
                what: "empty vocabulary.txt, nothing anywhere: its existence is not an edit",
                use_file: true,
                config_toml: &[],
                file: &[],
                current: &[],
                expect_config_toml: &[],
                expect_file: None,
            },
            PlanRow {
                what: "empty vocabulary.txt, config.toml terms untouched: they stay put",
                use_file: true,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs"],
                expect_config_toml: &["whisrs"],
                expect_file: None,
            },
            PlanRow {
                what: "empty vocabulary.txt, term added: now the file is written",
                use_file: true,
                config_toml: &["whisrs"],
                file: &[],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &[],
                expect_file: Some(&["whisrs", "NixOS"]),
            },
            PlanRow {
                // The editor round-trips through `join(", ")` + `parse_csv_list`,
                // which trims. A padded config.toml term must not read as an edit.
                what: "padded config.toml term, editor opened and left alone",
                use_file: true,
                config_toml: &[" whisrs "],
                file: &["NixOS"],
                current: &["whisrs", "NixOS"],
                expect_config_toml: &[" whisrs "],
                expect_file: None,
            },
            PlanRow {
                // Same, for the other half of that round trip: the editor's
                // input format splits on commas.
                what: "comma-containing config.toml term, editor opened and left alone",
                use_file: true,
                config_toml: &["Claude, Code"],
                file: &["NixOS"],
                current: &["Claude", "Code", "NixOS"],
                expect_config_toml: &["Claude, Code"],
                expect_file: None,
            },
            PlanRow {
                // A session that only ever loaded the config: the list in
                // memory is the raw merge, padding and all.
                what: "padded and comma terms, editor never opened",
                use_file: true,
                config_toml: &[" whisrs ", "Claude, Code"],
                file: &["NixOS"],
                current: &[" whisrs ", "Claude, Code", "NixOS"],
                expect_config_toml: &[" whisrs ", "Claude, Code"],
                expect_file: None,
            },
        ];

        for row in rows {
            let vocab = baseline(row.use_file, row.config_toml, row.file);
            let plan = vocabulary_save_plan(&vocab, &terms(row.current));
            assert_eq!(
                plan,
                VocabularySavePlan {
                    config_toml: terms(row.expect_config_toml),
                    vocabulary_txt: row.expect_file.map(terms),
                },
                "{}",
                row.what
            );
        }
    }

    /// A config.toml + vocabulary.txt pair in a fresh temp dir, driven through
    /// the real save path. Returns the two paths and what the vocabulary write
    /// reported. `row` names the case in a failure.
    fn run_save(
        row: &str,
        vocab: &VocabularyBaseline,
        current: &[String],
        vocabulary_txt: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, Option<PathBuf>) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{row}: temp dir: {e}"));
        let config_path = dir.path().join("config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        if let Some(contents) = vocabulary_txt {
            std::fs::write(&vocab_path, contents)
                .unwrap_or_else(|e| panic!("{row}: seed vocabulary.txt: {e}"));
        }

        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.vocabulary = current.to_vec();
        let written = write_config_and_vocabulary(&config, vocab, &config_path, &vocab_path)
            .unwrap_or_else(|e| panic!("{row}: save failed: {e:#}"))
            .vocabulary;

        (dir, config_path, vocab_path, written)
    }

    fn config_toml_terms(row: &str, path: &Path) -> Vec<String> {
        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{row}: config.toml unreadable: {e}"));
        let parsed: Config = toml::from_str(&contents)
            .unwrap_or_else(|e| panic!("{row}: config.toml does not reparse: {e}"));
        parsed.general.vocabulary
    }

    /// The composition, not the pieces: an edit must put the *whole merged
    /// list* in vocabulary.txt and an empty list in config.toml, in that order,
    /// so what the daemon merges at its next start is exactly what the editor
    /// showed. A deleted term is then in neither store and cannot come back,
    /// and a term starting with `#` is a term in the file, not a comment.
    ///
    /// Writing the blanked config.toml list to the file, skipping the file
    /// write, or writing the merged list into config.toml each destroy or
    /// duplicate the user's terms, and each is a one-line slip at this call
    /// site. All three fail here.
    #[test]
    fn a_migrating_save_writes_the_whole_list_to_the_file_and_blanks_config_toml() {
        let rows = [
            (
                "term added",
                baseline(true, &["whisrs"], &["NixOS"]),
                terms(&["whisrs", "NixOS", "Deepgram"]),
                "# hand-written\nNixOS\n",
            ),
            (
                "term deleted",
                baseline(true, &["whisrs", "ghost"], &["NixOS"]),
                terms(&["whisrs", "NixOS"]),
                "NixOS\n",
            ),
            (
                "#-leading term",
                baseline(true, &["#1", "NixOS"], &[]),
                terms(&["#1", "NixOS", "Deepgram"]),
                "",
            ),
        ];
        for (row, vocab, current, seeded) in rows {
            let (_dir, config_path, vocab_path, written) =
                run_save(row, &vocab, &current, Some(seeded));

            assert_eq!(written.as_deref(), Some(vocab_path.as_path()), "{row}");
            let from_file = load_vocabulary_file(&vocab_path)
                .unwrap_or_else(|e| panic!("{row}: vocabulary.txt unreadable: {e}"));
            assert_eq!(
                from_file.as_ref(),
                Some(&current),
                "{row}: vocabulary.txt must hold every term the editor showed"
            );
            let in_config_toml = config_toml_terms(row, &config_path);
            assert!(
                in_config_toml.is_empty(),
                "{row}: config.toml must be blanked so no term is stored twice"
            );
            assert_eq!(
                merge_vocabulary(in_config_toml, from_file.unwrap_or_default()),
                current,
                "{row}: the daemon must load exactly what the editor showed"
            );
        }
    }

    /// The other half of the composition: with no edit, neither store is
    /// touched. vocabulary.txt keeps its comments byte for byte and config.toml
    /// gets back exactly its own half of the list, not the merge. That holds
    /// for a pass through the editor that changed nothing, too: opening
    /// "Vocabulary & prompt" to change only the prompt and pressing Enter over
    /// the vocabulary line, with config.toml terms that have padding or a
    /// comma in them, which the editor's round trip trims and splits.
    #[test]
    fn a_save_without_a_vocabulary_edit_leaves_both_stores_alone() {
        let untouched = baseline(true, &["whisrs"], &["NixOS"]);
        let padded = baseline(true, &[" whisrs ", "Claude, Code"], &["NixOS", "Claude"]);
        // Exactly what the editor hands back when the user accepts the default.
        let echoed = normalized_vocabulary(&padded.merged);
        let rows = [
            (
                "untouched",
                &untouched,
                terms(&["whisrs", "NixOS"]),
                "# Deepgram keyterms\n\n# proper nouns\nNixOS\n",
                terms(&["whisrs"]),
            ),
            (
                "editor pass, padded and comma terms",
                &padded,
                echoed,
                "# grouped by project\nNixOS\n\n# people\nClaude\n",
                terms(&[" whisrs ", "Claude, Code"]),
            ),
        ];
        for (row, vocab, current, seeded, own_half) in rows {
            let (_dir, config_path, vocab_path, written) =
                run_save(row, vocab, &current, Some(seeded));

            assert_eq!(written, None, "{row}: the file must not be written");
            assert_eq!(
                std::fs::read_to_string(&vocab_path)
                    .unwrap_or_else(|e| panic!("{row}: vocabulary.txt unreadable: {e}")),
                seeded,
                "{row}: an untouched vocabulary must not cost the user their comments"
            );
            assert_eq!(
                config_toml_terms(row, &config_path),
                own_half,
                "{row}: config.toml keeps its own half verbatim; the file's terms must not leak in"
            );
        }
    }
}
