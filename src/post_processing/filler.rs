//! Filler word removal from transcribed text.
//!
//! Strips common filler words and stutters (repeated words) from text,
//! using word-boundary-aware regexes for accuracy.

use regex::Regex;

/// Built-in filler patterns (case-insensitive, word-boundary-aware).
///
/// "like" requires a trailing comma to avoid removing the verb form.
const DEFAULT_FILLER_PATTERNS: &[&str] = &[
    r"\bum\b,?\s*",
    r"\buh\b,?\s*",
    r"\blike,\s*",
    r"\byou know,?\s*",
    r"\bbasically,?\s*",
    r"\bactually,?\s*",
    r"\bI mean,?\s*",
    r"\bsort of\b",
    r"\bkind of\b",
];

/// Remove filler words and stutters from the given text.
///
/// When `custom_words` is non-empty, those patterns are used instead of the
/// built-in defaults. Each custom word is wrapped with `\b...\b,?\s*` to
/// create a word-boundary-aware pattern.
///
/// Always removes stutters regardless of the filler list.
pub fn remove_filler_words(text: &str, custom_words: &[String]) -> String {
    let mut result = text.to_string();

    if custom_words.is_empty() {
        // Use built-in patterns.
        for pattern_str in DEFAULT_FILLER_PATTERNS {
            if let Ok(re) = Regex::new(&format!("(?i){pattern_str}")) {
                result = re.replace_all(&result, "").to_string();
            }
        }
    } else {
        // Use custom words, wrapping each with word boundaries.
        for word in custom_words {
            let pattern_str = format!(r"(?i)\b{},?\s*", regex::escape(word));
            if let Ok(re) = Regex::new(&pattern_str) {
                result = re.replace_all(&result, "").to_string();
            }
        }
    }

    // Remove stutters (repeated consecutive words like "I I I went" -> "I went").
    // The regex crate doesn't support backreferences, so we do this manually.
    result = remove_stutters(&result);

    // Collapse multiple spaces and trim.
    if let Ok(re) = Regex::new(r" {2,}") {
        result = re.replace_all(&result, " ").to_string();
    }

    result.trim().to_string()
}

/// Remove consecutive repeated words (case-insensitive).
/// "I I I went" -> "I went", "the the cat" -> "the cat".
fn remove_stutters(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }

    let mut result = Vec::with_capacity(words.len());
    result.push(words[0]);

    for word in &words[1..] {
        if let Some(prev) = result.last() {
            if !prev.eq_ignore_ascii_case(word) {
                result.push(word);
            }
        }
    }

    result.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_um() {
        assert_eq!(
            remove_filler_words("um I went to the store", &[]),
            "I went to the store"
        );
    }

    #[test]
    fn removes_uh() {
        assert_eq!(remove_filler_words("I uh went home", &[]), "I went home");
    }

    #[test]
    fn removes_like_filler() {
        // "like," with comma is treated as filler.
        assert_eq!(
            remove_filler_words("it was like, really cool", &[]),
            "it was really cool"
        );
    }

    #[test]
    fn preserves_like_as_verb() {
        // "like" followed by end of string (no trailing space/comma) is preserved
        assert_eq!(remove_filler_words("I like cats", &[]), "I like cats");
    }

    #[test]
    fn removes_you_know() {
        assert_eq!(
            remove_filler_words("it was, you know, pretty good", &[]),
            "it was, pretty good"
        );
    }

    #[test]
    fn removes_basically() {
        assert_eq!(remove_filler_words("basically it works", &[]), "it works");
    }

    #[test]
    fn removes_actually() {
        assert_eq!(
            remove_filler_words("actually I think so", &[]),
            "I think so"
        );
    }

    #[test]
    fn removes_i_mean() {
        assert_eq!(
            remove_filler_words("I mean it was fine", &[]),
            "it was fine"
        );
    }

    #[test]
    fn removes_sort_of() {
        assert_eq!(
            remove_filler_words("it was sort of okay", &[]),
            "it was okay"
        );
    }

    #[test]
    fn removes_kind_of() {
        assert_eq!(
            remove_filler_words("it was kind of nice", &[]),
            "it was nice"
        );
    }

    #[test]
    fn removes_stutters() {
        assert_eq!(
            remove_filler_words("I I I went to the store", &[]),
            "I went to the store"
        );
    }

    #[test]
    fn removes_double_stutter() {
        assert_eq!(remove_filler_words("the the cat sat", &[]), "the cat sat");
    }

    #[test]
    fn removes_multiple_fillers() {
        assert_eq!(
            remove_filler_words("um uh like, you know basically it works", &[]),
            "it works"
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(remove_filler_words("Um I went", &[]), "I went");
        assert_eq!(remove_filler_words("UH okay", &[]), "okay");
    }

    #[test]
    fn collapses_spaces() {
        assert_eq!(remove_filler_words("I  um  went  home", &[]), "I went home");
    }

    #[test]
    fn empty_input() {
        assert_eq!(remove_filler_words("", &[]), "");
    }

    #[test]
    fn no_fillers() {
        assert_eq!(
            remove_filler_words("the cat sat on the mat", &[]),
            "the cat sat on the mat"
        );
    }

    #[test]
    fn custom_words() {
        let custom = vec!["well".to_string(), "so".to_string()];
        assert_eq!(
            remove_filler_words("well so I went home", &custom),
            "I went home"
        );
    }

    #[test]
    fn custom_words_ignores_defaults() {
        // With custom words, default fillers like "um" should NOT be removed.
        let custom = vec!["well".to_string()];
        assert_eq!(remove_filler_words("well um I went", &custom), "um I went");
    }

    #[test]
    fn trims_result() {
        assert_eq!(remove_filler_words("  um hello  ", &[]), "hello");
    }

    #[test]
    fn filler_with_comma() {
        assert_eq!(remove_filler_words("like, it was good", &[]), "it was good");
    }
}
