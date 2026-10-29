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
/// live inside it, so it is written with the same private-file helper (0600)
/// rather than through `fs::write`, which leaves it world-readable under the
/// usual umask.
pub fn write_vocabulary_file(path: &Path, terms: &[String]) -> io::Result<()> {
    let mut contents = String::new();
    for term in terms {
        if matches!(term.trim_start().chars().next(), Some('#') | Some('\\')) {
            contents.push('\\');
        }
        contents.push_str(term);
        contents.push('\n');
    }
    write::write_private_file(path, &contents)
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
/// The only place either store is written on save. `save_and_restart` calls
/// this and does nothing else with the vocabulary, so there is no second copy
/// of the decision for a call-site edit to get wrong.
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
) -> Result<Option<PathBuf>> {
    let plan = vocabulary_save_plan(vocab, &config.general.vocabulary);

    let mut vocabulary_written = None;
    if let Some(terms) = &plan.vocabulary_txt {
        write_vocabulary_file(vocab_path, terms)
            .with_context(|| format!("failed to write {}", vocab_path.display()))?;
        vocabulary_written = Some(vocab_path.to_path_buf());
    }

    let mut on_disk = config.clone();
    on_disk.general.vocabulary = plan.config_toml;

    if let Err(e) = write::write_config_to(&on_disk, config_path) {
        return Err(match &vocabulary_written {
            Some(path) => e.context(format!(
                "failed to write config: your vocabulary edits were already saved to {}, \
                 but config.toml was not updated",
                path.display()
            )),
            None => e.context("failed to write config"),
        });
    }

    Ok(vocabulary_written)
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

    #[test]
    fn parse_skips_blanks_and_comments_and_trims() {
        let contents = "\
# Deepgram nova-3 keyterms: this casing comes back.
whisrs

  Claude Code  \n\
\t
NixOS
# trailing comment
";
        assert_eq!(
            parse_vocabulary(contents),
            vec!["whisrs", "Claude Code", "NixOS"]
        );
    }

    #[test]
    fn parse_keeps_inline_hash() {
        assert_eq!(parse_vocabulary("C# dev\n"), vec!["C# dev"]);
    }

    #[test]
    fn parse_empty_and_comment_only_files_yield_no_terms() {
        assert!(parse_vocabulary("").is_empty());
        assert!(parse_vocabulary("# only a comment\n\n").is_empty());
    }

    /// `str::trim` strips `\r` but not U+FEFF, so a BOM used to ride into the
    /// term itself and reach Deepgram as `%EF%BB%BFNixOS`.
    #[test]
    fn parse_strips_a_leading_utf8_bom() {
        assert_eq!(
            parse_vocabulary("\u{feff}NixOS\nwhisrs\n"),
            vec!["NixOS", "whisrs"]
        );
    }

    /// A BOM-only file is an empty file. Trimming per line would leave the BOM
    /// standing as a one-character term.
    #[test]
    fn parse_bom_only_file_yields_no_terms() {
        assert!(parse_vocabulary("\u{feff}").is_empty());
        assert!(parse_vocabulary("\u{feff}\n").is_empty());
    }

    #[test]
    fn merge_puts_config_first_and_drops_duplicates() {
        let config = vec!["whisrs".to_string(), "GNOME".to_string()];
        let file = vec![
            "Deepgram".to_string(),
            "whisrs".to_string(),
            "NixOS".to_string(),
        ];
        assert_eq!(
            merge_vocabulary(config, file),
            vec!["whisrs", "GNOME", "Deepgram", "NixOS"]
        );
    }

    #[test]
    fn merge_is_case_sensitive() {
        let merged = merge_vocabulary(vec!["whisrs".to_string()], vec!["Whisrs".to_string()]);
        assert_eq!(merged, vec!["whisrs", "Whisrs"]);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("whisrs-vocab-test-missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            load_vocabulary_file(&dir.join("vocabulary.txt")).unwrap(),
            None
        );
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = std::env::temp_dir().join("whisrs-vocab-test-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocabulary.txt");

        write_vocabulary_file(&path, &[]).unwrap();
        assert_eq!(load_vocabulary_file(&path).unwrap(), Some(Vec::new()));

        let terms = vec!["whisrs".to_string(), "Claude Code".to_string()];
        write_vocabulary_file(&path, &terms).unwrap();
        assert_eq!(load_vocabulary_file(&path).unwrap(), Some(terms));

        let _ = std::fs::remove_dir_all(&dir);
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
    /// both ignore them; the `whisrs config` editor drops them in
    /// `parse_csv_list`. A blank entry is not a term anywhere in whisrs.
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

    /// The file holds vocabulary that used to live in the 0600 config.toml, so
    /// splitting it out must not widen who can read it. `fs::write` under the
    /// usual umask leaves 0644.
    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vocabulary.txt");

        write_vocabulary_file(&path, &["NixOS".to_string()]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A file that already drifted to laxer permissions is tightened, not
        // left as it was found.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_vocabulary_file(&path, &["NixOS".to_string(), "whisrs".to_string()]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// No vocabulary.txt means no migration, however the list was edited: the
    /// terms stay in config.toml, which is the pre-feature behavior.
    #[test]
    fn without_a_vocabulary_file_nothing_migrates() {
        assert!(!should_migrate_vocabulary(
            false,
            &terms(&["whisrs"]),
            &terms(&["whisrs", "NixOS"])
        ));
    }

    /// The bug this gate exists for: a save that never touched the vocabulary
    /// used to rewrite vocabulary.txt flat and blank `[general] vocabulary` in
    /// config.toml. An unchanged list must leave both stores alone.
    #[test]
    fn an_unchanged_vocabulary_does_not_migrate() {
        // Equal by value, not the same Vec: the save path compares the list
        // the editor started from against the one it ends with.
        assert!(!should_migrate_vocabulary(
            true,
            &terms(&["whisrs", "Hyprland"]),
            &terms(&["whisrs", "Hyprland"])
        ));
    }

    /// An empty vocabulary.txt is a legitimate way to opt in, so its mere
    /// existence is not a reason to write anything. Nothing on either side, or
    /// terms that only ever lived in config.toml, both stay put.
    #[test]
    fn an_empty_vocabulary_file_alone_does_not_migrate() {
        assert!(!should_migrate_vocabulary(true, &[], &[]));
        assert!(!should_migrate_vocabulary(
            true,
            &terms(&["whisrs"]),
            &terms(&["whisrs"])
        ));
    }

    /// Adding, removing or reordering a term is an edit, and an edit is what
    /// moves the whole list into vocabulary.txt.
    #[test]
    fn any_vocabulary_edit_migrates() {
        let baseline = terms(&["whisrs", "Hyprland"]);
        for current in [
            terms(&["whisrs", "Hyprland", "NixOS"]),
            terms(&["whisrs"]),
            terms(&["Hyprland", "whisrs"]),
        ] {
            assert!(
                should_migrate_vocabulary(true, &baseline, &current),
                "editing {baseline:?} into {current:?} must migrate"
            );
        }
    }

    /// Deleting every term still migrates, so vocabulary.txt is written empty
    /// rather than left holding the terms the user just removed.
    #[test]
    fn clearing_the_vocabulary_migrates() {
        assert!(should_migrate_vocabulary(
            true,
            &terms(&["whisrs", "Hyprland"]),
            &[]
        ));
    }

    /// Casing is part of the term: Deepgram echoes each keyterm back in the
    /// casing configured, so `Whisrs` is a different term from `whisrs`.
    #[test]
    fn a_case_only_vocabulary_change_migrates() {
        assert!(should_migrate_vocabulary(
            true,
            &terms(&["whisrs"]),
            &terms(&["Whisrs"])
        ));
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

    /// The regression a naive "just don't migrate" fix introduces: the editor
    /// holds the merged list, so an untouched save must put config.toml's own
    /// half back rather than write the merge into it. Otherwise vocabulary.txt's
    /// terms are silently copied into config.toml and stored twice.
    #[test]
    fn an_untouched_save_leaves_config_toml_holding_only_its_own_terms() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let current = vocab.merged.clone();
        assert_eq!(
            config_toml_vocabulary(&vocab, &current),
            terms(&["whisrs"]),
            "NixOS lives in vocabulary.txt and must not be copied into config.toml"
        );
    }

    /// Once the user edits the list, vocabulary.txt becomes the single store
    /// and config.toml is emptied, so deleting a term cannot resurrect it.
    #[test]
    fn an_edited_vocabulary_empties_config_toml() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        assert!(
            config_toml_vocabulary(&vocab, &terms(&["whisrs", "NixOS", "Hyprland"])).is_empty()
        );
        assert!(config_toml_vocabulary(&vocab, &[]).is_empty());
    }

    /// With no vocabulary.txt there was no merge, so the edited list is what
    /// config.toml gets, the behavior from before the file existed.
    #[test]
    fn without_a_vocabulary_file_config_toml_keeps_the_edited_list() {
        let vocab = baseline(false, &["whisrs"], &[]);
        assert_eq!(
            config_toml_vocabulary(&vocab, &terms(&["whisrs", "Hyprland"])),
            terms(&["whisrs", "Hyprland"])
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

    /// A `whisrs config` session that only ever loaded the config: the baseline
    /// and the in-memory list are the same raw merged list, padding and all.
    /// Nothing may migrate.
    #[test]
    fn a_save_that_never_opened_the_vocabulary_editor_does_not_migrate() {
        let vocab = baseline(true, &[" whisrs ", "Claude, Code"], &["NixOS"]);
        let untouched = vocab.merged.clone();
        assert_eq!(
            vocabulary_save_plan(&vocab, &untouched).vocabulary_txt,
            None
        );
    }

    /// A config.toml + vocabulary.txt pair in a fresh temp dir, driven through
    /// the real save path. Returns the two paths and what the vocabulary write
    /// reported.
    fn run_save(
        vocab: &VocabularyBaseline,
        current: &[&str],
        vocabulary_txt: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, Option<PathBuf>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        let vocab_path = dir.path().join("vocabulary.txt");
        if let Some(contents) = vocabulary_txt {
            std::fs::write(&vocab_path, contents).expect("seed vocabulary.txt");
        }

        let mut config: Config = toml::from_str("").expect("empty config uses defaults");
        config.general.vocabulary = terms(current);
        let written = write_config_and_vocabulary(&config, vocab, &config_path, &vocab_path)
            .expect("save succeeds");

        (dir, config_path, vocab_path, written)
    }

    fn config_toml_terms(path: &Path) -> Vec<String> {
        let contents = std::fs::read_to_string(path).expect("config.toml exists");
        let parsed: Config = toml::from_str(&contents).expect("config.toml reparses");
        parsed.general.vocabulary
    }

    /// The composition, not the pieces: an edit must put the *whole merged
    /// list* in vocabulary.txt and an empty list in config.toml, in that order.
    ///
    /// Writing the blanked config.toml list to the file, skipping the file
    /// write, or writing the merged list into config.toml each destroy or
    /// duplicate the user's terms, and each is a one-line slip at this call
    /// site. All three fail here.
    #[test]
    fn a_migrating_save_writes_the_whole_list_to_the_file_and_blanks_config_toml() {
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let (_dir, config_path, vocab_path, written) = run_save(
            &vocab,
            &["whisrs", "NixOS", "Deepgram"],
            Some("# hand-written\nNixOS\n"),
        );

        assert_eq!(written.as_deref(), Some(vocab_path.as_path()));
        assert_eq!(
            load_vocabulary_file(&vocab_path).expect("readable"),
            Some(terms(&["whisrs", "NixOS", "Deepgram"])),
            "vocabulary.txt must hold every term the editor showed"
        );
        assert!(
            config_toml_terms(&config_path).is_empty(),
            "config.toml must be blanked so no term is stored twice"
        );
    }

    /// The other half of the composition: with no edit, neither store is
    /// touched. vocabulary.txt keeps its comments byte for byte and config.toml
    /// gets back exactly its own half of the list, not the merge.
    #[test]
    fn a_save_without_a_vocabulary_edit_leaves_both_stores_alone() {
        let seeded = "# Deepgram keyterms\n\n# proper nouns\nNixOS\n";
        let vocab = baseline(true, &["whisrs"], &["NixOS"]);
        let (_dir, config_path, vocab_path, written) =
            run_save(&vocab, &["whisrs", "NixOS"], Some(seeded));

        assert_eq!(written, None);
        assert_eq!(
            std::fs::read_to_string(&vocab_path).expect("readable"),
            seeded,
            "an untouched vocabulary must not cost the user their comments"
        );
        assert_eq!(
            config_toml_terms(&config_path),
            terms(&["whisrs"]),
            "config.toml keeps its own half; the file's terms must not leak in"
        );
    }

    /// FIX for the editor round trip: opening "Vocabulary & prompt" to change
    /// only the prompt and pressing Enter over the vocabulary line is not an
    /// edit, even when config.toml's terms have padding or a comma in them.
    /// Before this, `parse_csv_list` normalized them, the lists compared
    /// unequal, and a migration the user never asked for flattened their
    /// hand-maintained vocabulary.txt.
    #[test]
    fn an_unedited_pass_through_the_editor_does_not_migrate() {
        let seeded = "# grouped by project\nNixOS\n\n# people\nClaude\n";
        let vocab = baseline(true, &[" whisrs ", "Claude, Code"], &["NixOS", "Claude"]);
        // Exactly what the editor hands back when the user accepts the default.
        let echoed = normalized_vocabulary(&vocab.merged);
        let echoed: Vec<&str> = echoed.iter().map(String::as_str).collect();

        let (_dir, config_path, vocab_path, written) = run_save(&vocab, &echoed, Some(seeded));

        assert_eq!(written, None, "an unedited pass must not write the file");
        assert_eq!(
            std::fs::read_to_string(&vocab_path).expect("readable"),
            seeded
        );
        assert_eq!(
            config_toml_terms(&config_path),
            terms(&[" whisrs ", "Claude, Code"]),
            "config.toml must keep its terms verbatim"
        );
    }

    /// Deleting a term is the case blanking config.toml exists for: the term
    /// must be in neither store afterwards, so the daemon's startup merge
    /// cannot bring it back. This is the resurrection scenario end to end:
    /// save, then re-run the merge the daemon does.
    #[test]
    fn a_deleted_term_cannot_resurrect_from_config_toml() {
        let vocab = baseline(true, &["whisrs", "ghost"], &["NixOS"]);
        let (_dir, config_path, vocab_path, _) =
            run_save(&vocab, &["whisrs", "NixOS"], Some("NixOS\n"));

        let from_file = load_vocabulary_file(&vocab_path)
            .expect("readable")
            .expect("written");
        let merged = merge_vocabulary(config_toml_terms(&config_path), from_file);
        assert_eq!(
            merged,
            terms(&["whisrs", "NixOS"]),
            "the daemon must load exactly what the editor showed after the delete"
        );
    }

    /// A term starting with `#` survives the migration. It used to be written
    /// as a line the parser reads as a comment, so it disappeared from
    /// vocabulary.txt in the same save that blanked config.toml, gone from
    /// both stores, silently.
    #[test]
    fn a_hash_leading_term_survives_the_migration() {
        let vocab = baseline(true, &["#1", "NixOS"], &[]);
        let (_dir, config_path, vocab_path, _) =
            run_save(&vocab, &["#1", "NixOS", "Deepgram"], Some(""));

        let from_file = load_vocabulary_file(&vocab_path)
            .expect("readable")
            .expect("written");
        let merged = merge_vocabulary(config_toml_terms(&config_path), from_file);
        assert_eq!(merged, terms(&["#1", "NixOS", "Deepgram"]));
    }
}
