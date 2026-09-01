use std::io;
use std::path::{Path, PathBuf};

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
    crate::config::setup::write_private_file(path, &contents)
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
}
