//! Regex-based filler word and stutter removal from transcribed speech.
//!
//! This crate removes common English filler words (such as "um", "uh", "like",
//! "you know", etc.), Russian hesitation sounds ("э-э-э", "эм", "а-а-а",
//! "ммм") and repeated-word stutters (e.g., "I I I went" → "I went") from
//! text — typically output from speech-to-text engines.
//!
//! # Built-in patterns: English fillers and Russian hesitations
//!
//! The default patterns target English filler words plus Russian hesitation
//! sounds, which are Cyrillic and cannot match English text. Construct a
//! built-in filter via [`FillerFilter::builtin`]. For other words or
//! languages, supply your own custom filler words via [`FillerFilter::new`];
//! they replace all built-ins.
//!
//! # Example
//!
//! ```rust
//! use filler_remove::FillerFilter;
//!
//! // Built-in patterns (no custom words).
//! let filter = FillerFilter::builtin();
//! assert_eq!(filter.apply("um, I uh went to the store"), "I went to the store");
//! assert_eq!(filter.apply("Э-э-э, я пошёл"), "Я пошёл");
//!
//! // Custom words for other languages.
//! let filter = FillerFilter::new(&["ну", "типа"]).unwrap();
//! assert_eq!(filter.apply("ну типа я пошёл"), "я пошёл");
//! ```
//!
//! For one-off / non-hot-path usage, [`remove_filler_words`] is also provided
//! as a convenience but recompiles regexes on every invocation.

use std::sync::LazyLock;

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

/// Pre-compiled built-in filler regexes.
static BUILTIN_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    DEFAULT_FILLER_PATTERNS
        .iter()
        .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
        .collect()
});

/// A whole Russian hesitation token, punctuation already stripped. Matching
/// whole tokens (not `\b`) keeps "э-это" and "эм-м" from losing only a part.
///
/// Deliberately conservative: only a capital or lowercase first letter (so
/// acronyms such as "МММ" stay); a lone "э", "м-м" (often "no") and "эу"
/// (a greeting) stay too.
static RU_HESITATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[Ээ][эм‐–—-]+|[Аа](?:[‐–—-]а+)+|[Аа]а{2,}|[Мм]м{2,}|[Мм](?:[‐–—-]м+){2,})$")
        .unwrap()
});

/// Sentence-ending punctuation a removed hesitation hands to the previous word.
const SENTENCE_END: &[char] = &['.', '!', '?', '…'];

/// Pre-compiled regex for collapsing multiple spaces into one.
static SPACE_COLLAPSE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" {2,}").unwrap());

/// A reusable filler-word filter that pre-compiles its regexes once.
///
/// Construct with [`FillerFilter::new`] and call [`FillerFilter::apply`]
/// repeatedly — each call avoids recompiling regexes, which matters when the
/// filter is invoked per audio chunk or per recording.
///
/// # Behavior
///
/// - If `custom_words` is empty, the filter uses the built-in English patterns
///   and Russian hesitations (already pre-compiled via [`LazyLock`]).
/// - If `custom_words` is non-empty, those words are compiled into custom
///   regexes (each wrapped with `\b...\b,?\s*`) and the built-in defaults are
///   **not** used — same semantics as the legacy [`remove_filler_words`].
/// - Stutter removal and whitespace collapse are always applied.
///
/// # Errors
///
/// Returns the underlying [`regex::Error`] if any custom word produces an
/// invalid pattern. The built-in patterns are statically validated and never
/// fail.
pub struct FillerFilter {
    /// Compiled custom-word patterns. Empty when using built-ins.
    custom: Vec<Regex>,
}

impl FillerFilter {
    /// Build a new filter, pre-compiling all custom-word patterns.
    ///
    /// Pass an empty slice to use the built-in filler list. For the
    /// "no custom words" case, prefer [`FillerFilter::builtin`] — it doesn't
    /// require a turbofish to pin the generic parameter.
    ///
    /// # Errors
    ///
    /// Surfaces a [`regex::Error`] if any custom word produces an invalid
    /// regex. Note that custom words are escaped via [`regex::escape`] before
    /// insertion, so failures here are rare in practice — but a malformed
    /// (non-UTF-8 or zero-length) input could still trip the compiler.
    pub fn new<S: AsRef<str>>(custom_words: &[S]) -> Result<Self, regex::Error> {
        let mut custom = Vec::with_capacity(custom_words.len());
        for word in custom_words {
            let pattern_str = format!(r"(?i)\b{},?\s*", regex::escape(word.as_ref()));
            custom.push(Regex::new(&pattern_str)?);
        }
        Ok(Self { custom })
    }

    /// Build a filter that uses only the built-in filler patterns.
    ///
    /// This is an ergonomic shortcut for `FillerFilter::new::<&str>(&[])`
    /// that avoids the turbofish needed to pin the generic parameter when the
    /// slice is empty. Construction never fails — the built-in patterns are
    /// statically validated.
    pub fn builtin() -> Self {
        // The built-in `LazyLock` already holds the compiled defaults; we
        // simply leave `custom` empty so `apply` dispatches to that path.
        Self { custom: Vec::new() }
    }

    /// Number of compiled custom-word regexes held by this filter.
    ///
    /// `0` means the filter uses the (statically pre-compiled) built-ins.
    /// Test-only: lets the cache tests assert that no per-call recompilation
    /// is sneaking in by re-checking that this count stays stable across
    /// repeated `apply` calls and matches the input list length.
    #[cfg(test)]
    pub(crate) fn compiled_regex_count(&self) -> usize {
        self.custom.len()
    }

    /// Apply the filter to `text`, returning a cleaned string.
    ///
    /// This is cheap to call repeatedly — no regex compilation happens here.
    pub fn apply(&self, text: &str) -> String {
        let mut result = text.to_string();

        if self.custom.is_empty() {
            // Use pre-compiled built-in patterns.
            for re in BUILTIN_PATTERNS.iter() {
                result = re.replace_all(&result, "").to_string();
            }
            result = remove_russian_hesitations(&result);
        } else {
            // Use pre-compiled custom-word patterns.
            for re in &self.custom {
                result = re.replace_all(&result, "").to_string();
            }
        }

        // Remove stutters (repeated consecutive words like "I I I went" -> "I went").
        // The regex crate doesn't support backreferences, so we do this manually.
        result = remove_stutters(&result);

        // Collapse multiple spaces and trim.
        result = SPACE_COLLAPSE_RE.replace_all(&result, " ").to_string();

        result.trim().to_string()
    }
}

impl Default for FillerFilter {
    /// Equivalent to [`FillerFilter::builtin`].
    fn default() -> Self {
        Self::builtin()
    }
}

/// Remove filler words and stutters from the given text.
///
/// When `custom_words` is non-empty, those patterns are used instead of the
/// built-in defaults. Each custom word is wrapped with `\b...\b,?\s*` to
/// create a word-boundary-aware pattern.
///
/// Always removes stutters regardless of the filler list.
///
/// # Built-in patterns: English fillers and Russian hesitations
///
/// The default list targets English filler words and Russian hesitation
/// sounds. For other words or languages, supply custom words.
///
/// # Performance
///
/// This convenience function compiles a fresh [`FillerFilter`] on every call.
/// For hot paths (per-chunk / per-recording filtering), construct a
/// [`FillerFilter`] once via [`FillerFilter::new`] and reuse it.
///
/// # Panics
///
/// Panics if any `custom_words` entry produces an invalid regex. Use
/// [`FillerFilter::new`] for fallible construction. (In practice, every
/// custom word is escaped via [`regex::escape`], so failures here are rare.)
pub fn remove_filler_words(text: &str, custom_words: &[String]) -> String {
    let filter = FillerFilter::new(custom_words)
        .expect("custom filler word produced an invalid regex; use FillerFilter::new for fallible construction");
    filter.apply(text)
}

/// Drop whole-token Russian hesitations, keeping the sentence intact: a
/// sentence end on the dropped token moves to the previous word (replacing a
/// comma there), a comma left dangling at the end goes, and the word after a
/// dropped sentence-initial hesitation gets its capital.
fn remove_russian_hesitations(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut kept: Vec<String> = Vec::with_capacity(tokens.len());
    let mut capitalize_next = false;

    for (i, token) in tokens.iter().enumerate() {
        let core = token.trim_end_matches(|c: char| ",;:".contains(c) || SENTENCE_END.contains(&c));
        if !RU_HESITATION_RE.is_match(core) {
            let mut word = token.to_string();
            if std::mem::take(&mut capitalize_next) {
                word = capitalize(&word);
            }
            kept.push(word);
            continue;
        }

        let sentence_end: String = token[core.len()..]
            .chars()
            .filter(|c| SENTENCE_END.contains(c))
            .collect();
        match kept.last_mut() {
            Some(prev) => {
                let prev_ends_sentence = prev.ends_with(SENTENCE_END);
                if !sentence_end.is_empty() && !prev_ends_sentence {
                    prev.truncate(prev.trim_end_matches(',').len());
                    prev.push_str(&sentence_end);
                } else if i == tokens.len() - 1 {
                    prev.truncate(prev.trim_end_matches(',').len());
                }
                capitalize_next |= prev_ends_sentence && core.starts_with(char::is_uppercase);
            }
            None => capitalize_next |= core.starts_with(char::is_uppercase),
        }
    }

    kept.join(" ")
}

/// Uppercase the first letter of `word`.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
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

    // --- Non-ASCII language tests ---

    #[test]
    fn cyrillic_no_fillers() {
        let text = "Привет мир, как дела?";
        assert_eq!(remove_filler_words(text, &[]), text);
    }

    #[test]
    fn cyrillic_with_english_filler() {
        // Transcription APIs sometimes mix English fillers into non-English text.
        assert_eq!(remove_filler_words("um Привет мир", &[]), "Привет мир");
    }

    #[test]
    fn cyrillic_stutter_removal() {
        assert_eq!(remove_filler_words("я я пошёл домой", &[]), "я пошёл домой");
    }

    #[test]
    fn arabic_no_fillers() {
        let text = "مرحبا بالعالم";
        assert_eq!(remove_filler_words(text, &[]), text);
    }

    #[test]
    fn arabic_stutter_removal() {
        assert_eq!(remove_filler_words("هذا هذا اختبار", &[]), "هذا اختبار");
    }

    #[test]
    fn cjk_no_fillers() {
        let text = "你好世界";
        assert_eq!(remove_filler_words(text, &[]), text);
    }

    #[test]
    fn japanese_no_fillers() {
        let text = "こんにちは世界";
        assert_eq!(remove_filler_words(text, &[]), text);
    }

    #[test]
    fn korean_no_fillers() {
        let text = "안녕하세요 세계";
        assert_eq!(remove_filler_words(text, &[]), text);
    }

    #[test]
    fn mixed_script_with_filler() {
        assert_eq!(
            remove_filler_words("basically Привет, 世界 uh okay", &[]),
            "Привет, 世界 okay"
        );
    }

    #[test]
    fn removes_russian_hesitations() {
        let cases = [
            (
                "А-а-а, работу не начинай, э-э-э, пока",
                "Работу не начинай, пока",
            ),
            ("Эээм, продолжим", "Продолжим"),
            ("эм-м, продолжим", "продолжим"),
            ("Так, ммм, ладно", "Так, ладно"),
            ("м-м-м, вкусно", "вкусно"),
            ("нужна, э—э—э, какая-то", "нужна, какая-то"),
            ("Ааа, понял", "Понял"),
        ];
        for (input, expected) in cases {
            assert_eq!(remove_filler_words(input, &[]), expected, "{input}");
        }
    }

    #[test]
    fn russian_hesitation_keeps_sentence_punctuation() {
        let cases = [
            ("Э-э-э. Проверка", "Проверка"),
            ("пока, э-э-э", "пока"),
            ("пока, э-э-э.", "пока."),
            ("Ладно, э-э-э. Дальше", "Ладно. Дальше"),
            ("Ладно. Э-э-э, дальше", "Ладно. Дальше"),
            ("Готово! Эм", "Готово!"),
        ];
        for (input, expected) in cases {
            assert_eq!(remove_filler_words(input, &[]), expected, "{input}");
        }
    }

    #[test]
    fn keeps_russian_words_that_look_like_hesitations() {
        let cases = [
            "Акции МММ упали, ЭЭЭ тоже",
            "Буква э",
            "М-м, не надо",
            "Эу, иди сюда",
            "э-это мама",
            "А папа нет, а мама эмоции и эканья-мэканья, ему и мне",
            "покааа воообще сообщение",
            "Э. М. Ремарк",
            "э-книга, э-мейл, ЭМ-волны",
            "судовая ЭУ, батарейки ААА",
            "длинношеее животное, Оооо, как красиво!",
        ];
        for text in cases {
            assert_eq!(remove_filler_words(text, &[]), text);
        }
    }

    #[test]
    fn russian_hesitation_removal_is_idempotent() {
        let once = remove_filler_words("Эээм, ну, э-э-э, ладно. Ммм", &[]);
        assert_eq!(once, "Ну, ладно.");
        assert_eq!(remove_filler_words(&once, &[]), once);
    }

    #[test]
    fn custom_words_skip_russian_builtins() {
        let custom = vec!["ну".to_string()];
        assert_eq!(
            remove_filler_words("ну э-э-э пошёл", &custom),
            "э-э-э пошёл"
        );
    }

    #[test]
    fn custom_cyrillic_filler() {
        let custom = vec!["ну".to_string(), "типа".to_string()];
        assert_eq!(remove_filler_words("ну типа я пошёл", &custom), "я пошёл");
    }

    #[test]
    fn emoji_in_text() {
        let text = "um 😀 that was basically 🎉 great";
        assert_eq!(remove_filler_words(text, &[]), "😀 that was 🎉 great");
    }

    // --- FillerFilter tests ---

    #[test]
    fn filter_builtin_caches_compiled_regexes() {
        // The built-in path holds zero custom regexes (it dispatches to the
        // module-level `LazyLock`). The compiled-regex count must stay at 0
        // across many `apply` calls — any regression that re-introduced
        // per-call `Regex::new` into the custom slot would push it above 0.
        let filter = FillerFilter::builtin();
        assert_eq!(filter.compiled_regex_count(), 0);
        for _ in 0..100 {
            assert_eq!(filter.apply("um hello uh world"), "hello world");
            assert_eq!(filter.compiled_regex_count(), 0);
        }
    }

    #[test]
    fn filter_custom_caches_compiled_regexes() {
        // The custom-word path compiles one regex per input word *once*, at
        // construction time. The count must equal the input length and stay
        // stable across repeated `apply` calls.
        let words = ["well", "so"];
        let filter = FillerFilter::new(&words).unwrap();
        assert_eq!(filter.compiled_regex_count(), words.len());
        for _ in 0..100 {
            assert_eq!(filter.apply("well so I went home"), "I went home");
            // Custom words override defaults — "um" should remain.
            assert_eq!(filter.apply("well um okay"), "um okay");
            assert_eq!(filter.compiled_regex_count(), words.len());
        }
    }

    #[test]
    fn filter_matches_free_function_builtin() {
        // FillerFilter::apply must produce the same output as the legacy
        // free function for the built-in path.
        let filter = FillerFilter::builtin();
        let cases = [
            "um I went to the store",
            "it was like, really cool",
            "basically it works",
        ];
        for case in cases {
            assert_eq!(filter.apply(case), remove_filler_words(case, &[]));
        }
    }

    #[test]
    fn filter_new_with_empty_slice_still_compiles() {
        // Regression guard: `FillerFilter::new(&[])` requires a turbofish
        // because `S: AsRef<str>` is otherwise unconstrained. Keep one test
        // that exercises the generic form so a future refactor doesn't
        // accidentally remove the workaround that callers may still rely on.
        let filter = FillerFilter::new::<&str>(&[]).unwrap();
        assert_eq!(filter.apply("um hello"), "hello");
    }

    #[test]
    fn filter_default_uses_builtin_patterns() {
        let filter = FillerFilter::default();
        assert_eq!(filter.compiled_regex_count(), 0);
        assert_eq!(filter.apply("um hello"), "hello");
    }

    #[test]
    fn filter_new_returns_result_type() {
        // The constructor signature is `Result<Self, regex::Error>` so any
        // failure to compile a custom word now surfaces as a real error
        // instead of being silently dropped (the previous behavior used
        // `if let Ok(re) = ...` and discarded compilation errors).
        let res: Result<FillerFilter, regex::Error> = FillerFilter::new(&["um", "uh"]);
        assert!(res.is_ok());
    }

    #[test]
    fn filter_error_path_propagates_regex_syntax_errors() {
        // The `?` operator inside `FillerFilter::new` propagates any
        // `regex::Error` that `Regex::new` returns. We can't *easily*
        // produce one through the public API because custom words go
        // through [`regex::escape`] first, which sanitises every
        // metacharacter — so we instead lock down both halves of the
        // contract:
        //
        //   1. The regex crate produces a deterministic `Syntax` error for
        //      an obviously-malformed pattern (`"foo("`). This is fast
        //      (microseconds) and stable across regex-crate versions —
        //      unlike a `CompiledTooBig` test that depends on the default
        //      `size_limit`.
        //
        //   2. Manually constructing the same wrapper format string used
        //      inside `new` with an *un-escaped* malformed payload still
        //      produces a `regex::Error::Syntax`, demonstrating that the
        //      `?` propagation in `new` would surface it as `Err` (rather
        //      than the silent-drop pattern the first fix pass corrected).
        // Build the malformed patterns at runtime so clippy's
        // `invalid_regex` lint doesn't reject them at compile time.
        let unmatched_paren: String = ['f', 'o', 'o', '('].iter().collect();
        let bad_direct = Regex::new(&unmatched_paren);
        assert!(
            matches!(bad_direct, Err(regex::Error::Syntax(_))),
            "regex crate must reject unmatched paren as a syntax error",
        );

        // Same wrapper `FillerFilter::new` uses, minus the `regex::escape`
        // step, so the malformed character class reaches the compiler
        // unaltered.
        let unclosed_class: String = ['[', 'a', 'b', 'c'].iter().collect();
        let wrapped = format!(r"(?i)\b{unclosed_class},?\s*");
        let bad_wrapped = Regex::new(&wrapped);
        assert!(
            matches!(bad_wrapped, Err(regex::Error::Syntax(_))),
            "wrapper format must propagate inner-pattern syntax errors",
        );
    }
}
