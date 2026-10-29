//! `Tab` in a path input: the typed path completed from what is on disk,
//! as far as the names that start the same agree.

use std::path::Path;

/// What a `Tab` did to a typed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The text to put back.
    pub text: String,
    /// The names it could still be, when there are several.
    pub candidates: Vec<String>,
}

/// Complete `typed` from the folder it names. `~` is expanded with `home`
/// for the lookup; `keep_tilde` writes it back as typed, else the home
/// folder is written out (for a key the daemon does not expand). A folder
/// that is the one match gets its `/`. Hidden names only match a `.`.
pub fn complete(typed: &str, home: Option<&str>, keep_tilde: bool) -> Completion {
    let unchanged = || Completion {
        text: typed.to_string(),
        candidates: Vec::new(),
    };
    let tilde = typed == "~" || typed.starts_with("~/");
    let expanded = match (tilde, home) {
        (true, Some(_)) => expand_home(typed, home),
        (true, None) => return unchanged(),
        (false, _) => typed.to_string(),
    };
    if expanded.is_empty() {
        return unchanged();
    }
    // `~` alone is the home folder itself.
    let expanded = if typed == "~" {
        format!("{expanded}/")
    } else {
        expanded
    };
    let (dir, prefix) = match expanded.rfind('/') {
        Some(at) => (&expanded[..=at], &expanded[at + 1..]),
        None => ("", expanded.as_str()),
    };
    let lookup = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = std::fs::read_dir(lookup) else {
        return unchanged();
    };
    let mut names: Vec<(String, bool)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let shown =
                name.starts_with(prefix) && (!name.starts_with('.') || prefix.starts_with('.'));
            shown.then(|| {
                let is_dir = Path::new(lookup).join(&name).is_dir();
                (name, is_dir)
            })
        })
        .collect();
    names.sort();
    let Some((first, _)) = names.first() else {
        return unchanged();
    };
    let common = names.iter().fold(first.clone(), |common, (name, _)| {
        common
            .chars()
            .zip(name.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect()
    });
    let mut done = common;
    if names.len() == 1 && names[0].1 {
        done.push('/');
    }
    let head = if tilde && keep_tilde {
        // The typed folder part, `~` and all.
        match typed.rfind('/') {
            Some(at) => typed[..=at].to_string(),
            None => "~/".to_string(),
        }
    } else {
        dir.to_string()
    };
    let candidates = if names.len() > 1 {
        names.into_iter().map(|(name, _)| name).collect()
    } else {
        Vec::new()
    };
    Completion {
        text: format!("{head}{done}"),
        candidates,
    }
}

/// `text` with a leading `~` (alone, or before a `/`) written out as
/// `home`; as it is without a home or a `~`.
pub fn expand_home(text: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if text == "~" || text.starts_with("~/") => {
            format!("{}{}", home.trim_end_matches('/'), &text[1..])
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Documents")).unwrap();
        std::fs::create_dir(dir.path().join("Downloads")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("ggml-base.en.bin"), b"").unwrap();
        dir
    }

    #[test]
    fn a_leading_tilde_is_written_out() {
        let home = Some("/home/ada");
        assert_eq!(expand_home("~/m/ggml.bin", home), "/home/ada/m/ggml.bin");
        assert_eq!(expand_home("~", home), "/home/ada");
        assert_eq!(expand_home("~ada/m", home), "~ada/m");
        assert_eq!(expand_home("/m/~/x", home), "/m/~/x");
        assert_eq!(expand_home("~/m", None), "~/m");
    }

    #[test]
    fn one_folder_is_completed_with_its_slash() {
        let dir = tree();
        let root = dir.path().to_str().unwrap();
        let done = complete(&format!("{root}/Doc"), None, true);
        assert_eq!(done.text, format!("{root}/Documents/"));
        assert!(done.candidates.is_empty());
        let file = complete(&format!("{root}/gg"), None, true);
        assert_eq!(file.text, format!("{root}/ggml-base.en.bin"));
    }

    #[test]
    fn several_complete_as_far_as_they_agree() {
        let dir = tree();
        let root = dir.path().to_str().unwrap();
        let done = complete(&format!("{root}/D"), None, true);
        assert_eq!(done.text, format!("{root}/Do"));
        assert_eq!(done.candidates, ["Documents", "Downloads"]);
        // Hidden names only for a typed dot.
        let all = complete(&format!("{root}/"), None, true);
        assert!(!all.candidates.contains(&".hidden".to_string()));
        assert_eq!(
            complete(&format!("{root}/.h"), None, true).text,
            format!("{root}/.hidden/")
        );
    }

    /// `~` is looked up in the home folder, and kept or written out.
    #[test]
    fn a_tilde_is_kept_or_written_out() {
        let dir = tree();
        let home = dir.path().to_str().unwrap();
        assert_eq!(complete("~/Doc", Some(home), true).text, "~/Documents/");
        assert_eq!(
            complete("~/Doc", Some(home), false).text,
            format!("{home}/Documents/")
        );
        assert_eq!(complete("~/nothing", Some(home), true).text, "~/nothing");
        assert_eq!(complete("~/Doc", None, true).text, "~/Doc");
    }
}
